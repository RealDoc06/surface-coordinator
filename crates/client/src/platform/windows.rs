//! Windows implementation of [`Platform`].

use std::ffi::{c_void, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use protocol::{OutputStream, PowerAction, RunAs, Shell};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};

use super::{
    executable, OutputChunk, Platform, PlatformError, ScriptOutcome, ScriptRequest, SystemInfo,
    Utf8Decoder, WifiConnection, WifiJoinRequest,
};
use session::{Spec, Window};
#[cfg(test)]
pub(crate) use session::command_line_to_argv;
pub(crate) use session::{quote_arguments, quote_program};

mod power;
mod session;
#[macro_use]
mod settings;
mod wifi;

/// How long output is still collected after the script itself has ended.
/// A process the script left running can hold the pipes open indefinitely.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// How much of a failed setting snippet's output goes into the error.
const MAX_SETTING_ERROR_CHARS: usize = 2000;

/// The shell is started through `cmd.exe` so that `chcp 65001` can switch the
/// (hidden) console to UTF-8 first; both shells then write UTF-8 to the
/// pipes. Paths are passed in environment variables, so no path character
/// needs escaping for `cmd`.
const POWERSHELL_COMMAND_LINE: &str = r#"/d /s /c ""%SC_CHCP%" 65001 >nul & "%SC_POWERSHELL%" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%SC_SCRIPT%"""#;
const CMD_COMMAND_LINE: &str = r#"/d /s /c ""%SC_CHCP%" 65001 >nul & call "%SC_SCRIPT%"""#;

pub struct WindowsPlatform {
    work_dir: PathBuf,
    restart: Option<Restart>,
}

/// How [`Platform::request_restart`] stops the client: by cancelling the
/// token the client runs with, after noting that a restart was asked for.
struct Restart {
    shutdown: CancellationToken,
    requested: Arc<AtomicBool>,
}

impl WindowsPlatform {
    /// `work_dir` is where script bodies are written while they run. It
    /// should not be writable by less privileged users than the client.
    /// Scripts run as the logged-in user are read from there by that user,
    /// so it must be readable by them.
    pub fn new(work_dir: impl Into<PathBuf>) -> WindowsPlatform {
        WindowsPlatform { work_dir: work_dir.into(), restart: None }
    }

    /// Lets `client.update` restart the client: a restart request cancels
    /// `shutdown`, and [`WindowsPlatform::restart_requested`] then tells the
    /// caller to exit with [`crate::RESTART_EXIT_CODE`] so that the service
    /// manager starts the client again.
    pub fn with_restart(mut self, shutdown: CancellationToken) -> WindowsPlatform {
        self.restart = Some(Restart { shutdown, requested: Arc::new(AtomicBool::new(false)) });
        self
    }

    pub fn restart_requested(&self) -> bool {
        self.restart.as_ref().is_some_and(|restart| restart.requested.load(Ordering::SeqCst))
    }
}

fn running_exe() -> Result<PathBuf, PlatformError> {
    std::env::current_exe()
        .map_err(|error| PlatformError::Failed(format!("cannot find the client's executable: {error}")))
}

/// Runs a file operation on the executable off the async worker threads.
async fn exe_operation(
    what: &'static str,
    operation: impl FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
) -> Result<(), PlatformError> {
    let exe = running_exe()?;
    tokio::task::spawn_blocking(move || operation(&exe))
        .await
        .unwrap_or_else(|error| Err(std::io::Error::other(error.to_string())))
        .map_err(|error| PlatformError::Failed(format!("{what} failed: {error}")))
}

fn system32() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    Path::new(&root).join("System32")
}

/// The full host name. `COMPUTERNAME` is the NetBIOS name, cut at 15
/// characters.
fn hostname() -> Option<String> {
    let mut length = 0u32;
    // SAFETY: the first call only asks for the required length; the second
    // writes at most `length` UTF-16 units into a buffer of that size.
    unsafe {
        GetComputerNameExW(ComputerNameDnsHostname, std::ptr::null_mut(), &mut length);
        let mut buffer = vec![0u16; length as usize];
        if GetComputerNameExW(ComputerNameDnsHostname, buffer.as_mut_ptr(), &mut length) == 0 {
            return None;
        }
        String::from_utf16(&buffer[..length as usize]).ok().filter(|name| !name.is_empty())
    }
}

/// A Job Object holding the script's process tree, so that a timeout or a
/// cancel can stop everything the script started and not just the shell.
///
/// Dropped while still armed, it stops the tree: that only happens when the
/// script's future is dropped before the script ended, which is how a job
/// is cancelled. Once the script has ended the tree is disarmed, so that
/// something a script deliberately left running survives it.
struct ProcessTree {
    job: HANDLE,
    armed: AtomicBool,
}

// The handle is only used through thread-safe kernel calls.
unsafe impl Send for ProcessTree {}
unsafe impl Sync for ProcessTree {}

impl ProcessTree {
    fn containing(process: *mut c_void) -> Option<ProcessTree> {
        // SAFETY: plain FFI calls; the job handle is owned by the returned
        // value and `process` is a live handle owned by the child.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let tree = ProcessTree { job, armed: AtomicBool::new(true) };
            (AssignProcessToJobObject(job, process as HANDLE) != 0).then_some(tree)
        }
    }

    fn terminate(&self) {
        // SAFETY: the handle is valid for the lifetime of `self`.
        unsafe {
            TerminateJobObject(self.job, 1);
        }
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst) {
            self.terminate();
        }
        // Closing the handle alone does not stop the processes.
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Reads a pipe to its end, forwarding decoded text.
async fn pump(
    mut pipe: impl AsyncRead + Unpin,
    stream: OutputStream,
    output: mpsc::Sender<OutputChunk>,
) {
    let mut decoder = Utf8Decoder::new();
    let mut buffer = vec![0u8; 8192];
    let mut at_start = true;
    loop {
        let read = match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut data = decoder.push(&buffer[..read]);
        if at_start && !data.is_empty() {
            at_start = false;
            if let Some(stripped) = data.strip_prefix('\u{feff}') {
                data = stripped.to_owned();
            }
        }
        if !data.is_empty() && output.send(OutputChunk { stream, data }).await.is_err() {
            return;
        }
    }
    let rest = decoder.finish();
    if !rest.is_empty() {
        let _ = output.send(OutputChunk { stream, data: rest }).await;
    }
}

impl WindowsPlatform {
    /// Runs a script with `extra_env` set in its environment on top of the
    /// usual variables. Dropping the future stops the script and everything
    /// it started.
    async fn run_script_with(
        &self,
        request: ScriptRequest,
        extra_env: &[(&str, OsString)],
        output: mpsc::Sender<OutputChunk>,
    ) -> Result<ScriptOutcome, PlatformError> {
        let failed =
            |what: &str, error: std::io::Error| PlatformError::Failed(format!("{what}: {error}"));

        let (extension, contents, command_line) = match request.shell {
            // Windows PowerShell reads a script without a BOM in the ANSI
            // code page, so give it one.
            Shell::PowerShell => {
                ("ps1", format!("\u{feff}{}", request.body), POWERSHELL_COMMAND_LINE)
            }
            // Batch files want CRLF line endings.
            Shell::Cmd => {
                ("cmd", request.body.replace("\r\n", "\n").replace('\n', "\r\n"), CMD_COMMAND_LINE)
            }
        };
        tokio::fs::create_dir_all(&self.work_dir)
            .await
            .map_err(|e| failed("cannot create the script directory", e))?;
        let script =
            self.work_dir.join(format!("job-{}.{extension}", uuid::Uuid::new_v4().simple()));
        let _remove_script = RemoveOnDrop(script.clone());
        tokio::fs::write(&script, contents)
            .await
            .map_err(|e| failed("cannot write the script file", e))?;

        let system32 = system32();
        let mut env = vec![
            ("SC_CHCP", system32.join("chcp.com").into_os_string()),
            ("SC_POWERSHELL", system32.join("WindowsPowerShell\\v1.0\\powershell.exe").into_os_string()),
            ("SC_SCRIPT", script.clone().into_os_string()),
        ];
        env.extend(extra_env.iter().cloned());
        let shell = system32.join("cmd.exe");
        let temp = std::env::temp_dir();
        let spec = Spec {
            program: &shell,
            arguments: command_line,
            env: &env,
            current_dir: Some(&temp),
            window: Window::Hidden,
            capture_output: true,
            kill_on_drop: true,
        };
        let mut child = session::spawn(&spec, request.run_as).await?;

        let tree = child.raw_handle().and_then(ProcessTree::containing);
        if tree.is_none() {
            tracing::warn!("could not create a job object; a timeout will only stop the shell");
        }
        // Started as the user, the shell is suspended until it is in the
        // job, so nothing it starts can escape it.
        child.resume()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let readers = [
            tokio::spawn(pump(stdout, OutputStream::Stdout, output.clone())),
            tokio::spawn(pump(stderr, OutputStream::Stderr, output)),
        ];

        let outcome = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(Ok(code)) => {
                if let Some(tree) = &tree {
                    tree.disarm();
                }
                Ok(ScriptOutcome::Exited(code))
            }
            Ok(Err(error)) => Err(failed("waiting for the shell failed", error)),
            Err(_) => {
                if let Some(tree) = &tree {
                    tree.terminate();
                }
                child.kill().await;
                Ok(ScriptOutcome::TimedOut)
            }
        };

        for mut reader in readers {
            if tokio::time::timeout(DRAIN_GRACE, &mut reader).await.is_err() {
                reader.abort();
                let _ = reader.await;
            }
        }
        outcome
    }

    /// Runs a script to its end and returns its outcome with stdout and
    /// stderr together.
    async fn run_collected(
        &self,
        request: ScriptRequest,
        extra_env: &[(&str, OsString)],
    ) -> (Result<ScriptOutcome, PlatformError>, String) {
        let (sender, mut receiver) = mpsc::channel::<OutputChunk>(16);
        let collect = async {
            let mut text = String::new();
            while let Some(chunk) = receiver.recv().await {
                text.push_str(&chunk.data);
            }
            text
        };
        tokio::join!(self.run_script_with(request, extra_env, sender), collect)
    }
}

impl Platform for WindowsPlatform {
    fn system_info(&self) -> SystemInfo {
        SystemInfo {
            hostname: hostname()
                .or_else(|| std::env::var("COMPUTERNAME").ok())
                .unwrap_or_else(|| "unknown".into()),
            os: "windows".into(),
        }
    }

    async fn run_script(
        &self,
        request: ScriptRequest,
        output: mpsc::Sender<OutputChunk>,
    ) -> Result<ScriptOutcome, PlatformError> {
        self.run_script_with(request, &[], output).await
    }

    async fn wifi_scan(&self) -> Result<Vec<String>, PlatformError> {
        blocking(wifi::scan).await
    }

    async fn wifi_join(&self, request: WifiJoinRequest) -> Result<(), PlatformError> {
        blocking(move || wifi::join(request)).await
    }

    async fn wifi_current(&self) -> Result<Option<WifiConnection>, PlatformError> {
        blocking(wifi::current).await
    }

    async fn launch_app(&self, path: String, arguments: Vec<String>) -> Result<u32, PlatformError> {
        if path.contains('\0') || arguments.iter().any(|argument| argument.contains('\0')) {
            return Err(PlatformError::Failed("the path or an argument contains NUL".into()));
        }
        let program = PathBuf::from(&path);
        let arguments = session::quote_arguments(&arguments);
        let spec = Spec {
            program: &program,
            arguments: &arguments,
            env: &[],
            current_dir: None,
            window: Window::Normal,
            capture_output: false,
            // The app is the user's from now on.
            kill_on_drop: false,
        };
        let mut child = session::spawn(&spec, RunAs::User).await?;
        child.resume()?;
        child.id().ok_or_else(|| PlatformError::Failed(format!("{path} ended at once")))
    }

    async fn apply_setting(&self, setting: String, value: serde_json::Value) -> Result<(), PlatformError> {
        let entry = settings::entry(&setting).ok_or_else(|| {
            PlatformError::Unsupported(format!("setting {setting} is not implemented on Windows"))
        })?;
        let (request, env) = settings::prepare(entry, &value, session::interactive_user_sid)?;
        let (outcome, output) = self.run_collected(request, &env).await;
        let output = output.trim();
        let detail = |summary: String| {
            let mut message = summary;
            if !output.is_empty() {
                message.push_str(": ");
                message.extend(output.chars().take(MAX_SETTING_ERROR_CHARS));
            }
            PlatformError::Failed(message)
        };
        match outcome? {
            ScriptOutcome::Exited(0) => Ok(()),
            ScriptOutcome::Exited(code) => Err(detail(format!("applying {setting} failed with exit code {code}"))),
            ScriptOutcome::TimedOut => Err(detail(format!("applying {setting} timed out"))),
        }
    }

    fn check_power(&self, action: PowerAction) -> Result<(), PlatformError> {
        power::check(action)
    }

    async fn power(&self, action: PowerAction) -> Result<(), PlatformError> {
        power::carry_out(action).await
    }

    fn current_exe(&self) -> Result<PathBuf, PlatformError> {
        running_exe()
    }

    async fn swap_executable(&self, new: PathBuf) -> Result<(), PlatformError> {
        exe_operation("replacing the executable", move |exe| executable::swap(exe, &new)).await
    }

    async fn restore_executable(&self) -> Result<(), PlatformError> {
        exe_operation("restoring the previous executable", executable::restore).await
    }

    async fn remove_old_executable(&self) -> Result<(), PlatformError> {
        exe_operation("removing the previous executable", executable::remove_leftovers).await
    }

    fn request_restart(&self) -> Result<(), PlatformError> {
        let restart = self.restart.as_ref().ok_or_else(|| {
            PlatformError::Unsupported("this client was started without a way to restart it".into())
        })?;
        restart.requested.store(true, Ordering::SeqCst);
        restart.shutdown.cancel();
        Ok(())
    }
}

/// Runs a blocking Wi-Fi call off the async worker threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, PlatformError> + Send + 'static,
) -> Result<T, PlatformError> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|error| Err(PlatformError::Failed(format!("Wi-Fi call failed: {error}"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Run {
        outcome: Result<ScriptOutcome, PlatformError>,
        stdout: String,
        stderr: String,
        work_dir: tempfile::TempDir,
    }

    async fn run_with(shell: Shell, body: &str, timeout: Duration, run_as: RunAs) -> Run {
        let work_dir = tempfile::tempdir().unwrap();
        let platform = WindowsPlatform::new(work_dir.path());
        let (sender, mut receiver) = mpsc::channel(16);
        let request = ScriptRequest { shell, body: body.to_owned(), timeout, run_as };
        let collect = async {
            let (mut stdout, mut stderr) = (String::new(), String::new());
            while let Some(OutputChunk { stream, data }) = receiver.recv().await {
                match stream {
                    OutputStream::Stdout => stdout.push_str(&data),
                    OutputStream::Stderr => stderr.push_str(&data),
                }
            }
            (stdout, stderr)
        };
        let (outcome, (stdout, stderr)) = tokio::join!(platform.run_script(request, sender), collect);
        Run { outcome, stdout, stderr, work_dir }
    }

    async fn run(shell: Shell, body: &str) -> Run {
        run_with(shell, body, Duration::from_secs(60), RunAs::System).await
    }

    fn process_exists(pid: u32) -> bool {
        let tasklist = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&tasklist.stdout).contains(&pid.to_string())
    }

    /// Starts a ping that runs for two minutes, prints its pid, then sleeps.
    const SCRIPT_LEAVING_A_CHILD: &str =
        "$p = Start-Process ping.exe -ArgumentList '-n','120','127.0.0.1' -NoNewWindow -PassThru -RedirectStandardOutput \"$env:TEMP\\sc-test-ping-$PID.txt\"\n\
         Write-Output \"pid=$($p.Id)\"\n\
         Start-Sleep -Seconds 120";

    fn parse_pid(stdout: &str) -> u32 {
        stdout
            .trim()
            .strip_prefix("pid=")
            .unwrap_or_else(|| panic!("unexpected output {stdout:?}"))
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn powershell_output_and_success() {
        let run = run(Shell::PowerShell, "Write-Output 'hello from powershell'\nWrite-Output (2 + 3)").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(run.stdout, "hello from powershell\r\n5\r\n");
        assert_eq!(run.stderr, "");
    }

    #[tokio::test]
    async fn powershell_exit_code_and_stderr() {
        let run = run(
            Shell::PowerShell,
            "Write-Output 'before'\n[Console]::Error.WriteLine('problem')\nexit 3",
        )
        .await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(3)));
        assert_eq!(run.stdout, "before\r\n");
        assert_eq!(run.stderr, "problem\r\n");
    }

    #[tokio::test]
    async fn powershell_terminating_error_is_a_non_zero_exit() {
        let run = run(Shell::PowerShell, "throw 'it broke'").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(1)));
        assert!(run.stderr.contains("it broke"), "stderr: {}", run.stderr);
    }

    #[tokio::test]
    async fn powershell_exit_code_of_an_earlier_native_command_does_not_fail_the_script() {
        let run = run(Shell::PowerShell, "cmd /c exit 5\nWrite-Output \"native said $LASTEXITCODE\"").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(run.stdout, "native said 5\r\n");
    }

    #[tokio::test]
    async fn powershell_unicode_survives_in_script_and_output() {
        let run = run(Shell::PowerShell, "Write-Output 'grüße ✓ 日本語'").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(run.stdout, "grüße ✓ 日本語\r\n");
    }

    #[tokio::test]
    async fn powershell_script_may_start_with_using_and_param() {
        let run = run(
            Shell::PowerShell,
            "using namespace System.Text\nparam([string]$Name = 'default')\nWrite-Output \"name=$Name\"",
        )
        .await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(run.stdout, "name=default\r\n");
    }

    #[tokio::test]
    async fn cmd_output_and_exit_code() {
        let run = run(Shell::Cmd, "@echo off\necho hello from cmd\necho oops 1>&2\nexit /b 4").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(4)));
        assert_eq!(run.stdout, "hello from cmd\r\n");
        assert_eq!(run.stderr.trim_end(), "oops");

        let run = super::tests::run(Shell::Cmd, "@echo grüße ✓").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(run.stdout, "grüße ✓\r\n");
    }

    #[tokio::test]
    async fn timeout_stops_the_script_and_what_it_started() {
        let started = std::time::Instant::now();
        let run =
            run_with(Shell::PowerShell, SCRIPT_LEAVING_A_CHILD, Duration::from_secs(4), RunAs::System).await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(30), "took {:?}", started.elapsed());

        // Output produced before the timeout was delivered.
        let pid = parse_pid(&run.stdout);
        // The process the script started is gone too.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!process_exists(pid), "ping {pid} still running");
    }

    #[tokio::test]
    async fn dropping_a_running_script_stops_it_and_what_it_started() {
        // This is how a job is cancelled.
        let work_dir = tempfile::tempdir().unwrap();
        let platform = WindowsPlatform::new(work_dir.path());
        let (sender, mut receiver) = mpsc::channel(16);
        let request = ScriptRequest {
            shell: Shell::PowerShell,
            body: SCRIPT_LEAVING_A_CHILD.into(),
            timeout: Duration::from_secs(120),
            run_as: RunAs::System,
        };
        let mut run = Box::pin(platform.run_script(request, sender));
        let mut stdout = String::new();
        let pid = loop {
            tokio::select! {
                chunk = receiver.recv() => {
                    let chunk = chunk.expect("output ended before the pid");
                    stdout.push_str(&chunk.data);
                    if stdout.ends_with('\n') {
                        break parse_pid(&stdout);
                    }
                }
                outcome = &mut run => panic!("the script ended early: {outcome:?}"),
                _ = tokio::time::sleep(Duration::from_secs(30)) => panic!("no pid within 30 s"),
            }
        };
        assert!(process_exists(pid));
        drop(run);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!process_exists(pid), "ping {pid} still running after the script was dropped");
        // The script file is gone too.
        assert_eq!(std::fs::read_dir(work_dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_process_left_running_by_a_finished_script_survives_it() {
        let run = run(
            Shell::PowerShell,
            "$p = Start-Process ping.exe -ArgumentList '-n','30','127.0.0.1' -NoNewWindow -PassThru -RedirectStandardOutput \"$env:TEMP\\sc-test-ping-$PID.txt\"\n\
             Write-Output \"pid=$($p.Id)\"",
        )
        .await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        let pid = parse_pid(&run.stdout);
        assert!(process_exists(pid), "the ping the script left running was stopped");
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/F"]).output();
    }

    #[tokio::test]
    async fn run_as_user_in_console_mode_runs_as_this_user() {
        let body = "Write-Output ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value)";
        let as_user = run_with(Shell::PowerShell, body, Duration::from_secs(60), RunAs::User).await;
        let as_system = run(Shell::PowerShell, body).await;
        assert_eq!(as_user.outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(as_user.stdout, as_system.stdout);
        assert_eq!(as_user.stdout.trim(), session::interactive_user_sid().unwrap());
    }

    #[tokio::test]
    async fn extra_environment_reaches_the_script_as_data() {
        let work_dir = tempfile::tempdir().unwrap();
        let platform = WindowsPlatform::new(work_dir.path());
        let hostile = "'; exit 9; $(exit 8) `\" & exit 7";
        let request = ScriptRequest {
            shell: Shell::PowerShell,
            body: "[Console]::Out.Write($env:SC_VALUE)".into(),
            timeout: Duration::from_secs(60),
            run_as: RunAs::System,
        };
        let (outcome, output) =
            platform.run_collected(request, &[("SC_VALUE", OsString::from(hostile))]).await;
        assert_eq!(outcome, Ok(ScriptOutcome::Exited(0)));
        assert_eq!(output, hostile);
    }

    #[tokio::test]
    async fn script_file_is_removed_afterwards() {
        let run = run(Shell::PowerShell, "Write-Output $PSCommandPath").await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::Exited(0)));
        let script = PathBuf::from(run.stdout.trim());
        assert_eq!(script.extension().and_then(|e| e.to_str()), Some("ps1"));
        assert!(!script.exists());
        assert_eq!(std::fs::read_dir(run.work_dir.path()).unwrap().count(), 0);
    }

    // ------------------------------------------------------------ launch_app

    #[tokio::test]
    async fn launch_app_starts_the_program_and_returns_its_pid() {
        let platform = WindowsPlatform::new(std::env::temp_dir());
        let cmd = system32().join("cmd.exe").to_string_lossy().into_owned();
        let pid = platform.launch_app(cmd, vec!["/c".into(), "exit 0".into()]).await.unwrap();
        assert!(pid > 0);
    }

    #[tokio::test]
    async fn launching_a_program_that_does_not_exist_fails() {
        let platform = WindowsPlatform::new(std::env::temp_dir());
        let missing = r"C:\Fake Surface Coordinator Test\does-not-exist.exe".to_owned();
        match platform.launch_app(missing, vec![]).await {
            Err(PlatformError::Failed(message)) => assert!(message.contains("does-not-exist.exe"), "{message}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    // ------------------------------------------------------------ settings

    #[tokio::test]
    async fn every_setting_snippet_is_valid_powershell() {
        let dir = tempfile::tempdir().unwrap();
        for entry in &settings::ENTRIES {
            std::fs::write(dir.path().join(format!("{}.ps1", entry.id)), entry.snippet).unwrap();
        }
        // Parsed, never run.
        let body = "$failed = $false\n\
            foreach ($file in Get-ChildItem -LiteralPath $env:SC_DIR -Filter *.ps1) {\n\
                $tokens = $null; $errors = $null\n\
                [System.Management.Automation.Language.Parser]::ParseFile($file.FullName, [ref]$tokens, [ref]$errors) | Out-Null\n\
                foreach ($e in $errors) { Write-Output \"$($file.Name) line $($e.Extent.StartLineNumber): $($e.Message)\"; $failed = $true }\n\
            }\n\
            if ($failed) { exit 1 }";
        let work_dir = tempfile::tempdir().unwrap();
        let platform = WindowsPlatform::new(work_dir.path());
        let request = ScriptRequest {
            shell: Shell::PowerShell,
            body: body.into(),
            timeout: Duration::from_secs(60),
            run_as: RunAs::System,
        };
        let (outcome, output) =
            platform.run_collected(request, &[("SC_DIR", dir.path().as_os_str().to_owned())]).await;
        assert_eq!(outcome, Ok(ScriptOutcome::Exited(0)), "{output}");
    }

    #[tokio::test]
    async fn an_unknown_setting_is_unsupported() {
        let platform = WindowsPlatform::new(std::env::temp_dir());
        let result = platform.apply_setting("display.night_light".into(), serde_json::json!(true)).await;
        assert!(matches!(result, Err(PlatformError::Unsupported(_))), "{result:?}");
    }

    /// The default output's level, exactly, and whether it is muted.
    #[derive(Debug, Clone, PartialEq)]
    struct Audio {
        level: String,
        muted: bool,
    }

    impl Audio {
        fn percent(&self) -> i64 {
            (self.level.parse::<f64>().unwrap() * 100.0).round() as i64
        }
    }

    const NO_AUDIO: &str = "this device has no audio output";

    /// `Ok(None)` if the machine has no audio output.
    async fn read_audio(platform: &WindowsPlatform) -> Result<Option<Audio>, String> {
        let body = concat!(
            audio_interop!(),
            "$culture = [Globalization.CultureInfo]::InvariantCulture\n\
             [Console]::Out.Write([ScAudio]::GetLevel().ToString('R', $culture) + ' ' + [ScAudio]::GetMute())\n"
        );
        let request = ScriptRequest {
            shell: Shell::PowerShell,
            body: body.into(),
            timeout: Duration::from_secs(120),
            run_as: RunAs::User,
        };
        match platform.run_collected(request, &[]).await {
            (Ok(ScriptOutcome::Exited(0)), output) => {
                let (level, muted) = output.trim().split_once(' ').ok_or(format!("unexpected {output:?}"))?;
                let muted = muted.eq_ignore_ascii_case("true");
                Ok(Some(Audio { level: level.to_owned(), muted }))
            }
            (Ok(ScriptOutcome::Exited(3)), output) if output.contains(NO_AUDIO) => Ok(None),
            (outcome, output) => Err(format!("reading the audio state: {outcome:?} {output}")),
        }
    }

    /// Puts back exactly what `read_audio` returned.
    async fn restore_audio(platform: &WindowsPlatform, audio: &Audio) -> Result<(), String> {
        let body = concat!(
            audio_interop!(),
            "$culture = [Globalization.CultureInfo]::InvariantCulture\n\
             [ScAudio]::SetLevel([float]::Parse($env:SC_LEVEL, $culture))\n\
             [ScAudio]::SetMute([bool]::Parse($env:SC_MUTE))\n"
        );
        let request = ScriptRequest {
            shell: Shell::PowerShell,
            body: body.into(),
            timeout: Duration::from_secs(120),
            run_as: RunAs::User,
        };
        let env = [("SC_LEVEL", OsString::from(&audio.level)), ("SC_MUTE", OsString::from(audio.muted.to_string()))];
        match platform.run_collected(request, &env).await {
            (Ok(ScriptOutcome::Exited(0)), _) => Ok(()),
            (outcome, output) => Err(format!("restoring the audio state: {outcome:?} {output}")),
        }
    }

    /// The only settings applied for real, because they can be put back
    /// exactly. Volume and mute in one test so that two tests never change
    /// them at the same time.
    #[tokio::test]
    async fn audio_volume_and_mute_are_applied_and_then_restored() {
        let work_dir = tempfile::tempdir().unwrap();
        let platform = WindowsPlatform::new(work_dir.path());
        let original = match read_audio(&platform).await {
            Ok(Some(audio)) => audio,
            Ok(None) => {
                eprintln!("no audio output on this machine; skipping");
                let result = platform.apply_setting("audio.volume".into(), serde_json::json!(30)).await;
                assert!(matches!(&result, Err(PlatformError::Failed(m)) if m.contains(NO_AUDIO)), "{result:?}");
                return;
            }
            Err(error) => panic!("{error}"),
        };

        // Nothing in here panics, so that the original state is put back
        // whatever happens.
        let exercise = async {
            let volume = if original.percent() == 37 { 38 } else { 37 };
            platform
                .apply_setting("audio.volume".into(), serde_json::json!(volume))
                .await
                .map_err(|error| format!("applying the volume: {error}"))?;
            let now = read_audio(&platform).await?.ok_or("the audio output went away")?;
            if now.percent() != volume {
                return Err(format!("volume is {} after setting {volume}", now.percent()));
            }

            let muted = !original.muted;
            platform
                .apply_setting("audio.mute".into(), serde_json::json!(muted))
                .await
                .map_err(|error| format!("applying mute: {error}"))?;
            let now = read_audio(&platform).await?.ok_or("the audio output went away")?;
            if now.muted != muted {
                return Err(format!("mute is {} after setting {muted}", now.muted));
            }
            Ok::<(), String>(())
        };
        let exercised = exercise.await;
        let restored = restore_audio(&platform, &original).await;
        let after = read_audio(&platform).await;

        exercised.unwrap();
        restored.unwrap();
        assert_eq!(after, Ok(Some(original)), "the audio state was not restored exactly");
    }

    // ------------------------------------------------------------ Wi-Fi

    // Read-only calls against the real adapter. Whether this machine has
    // Wi-Fi is unknown, so either a result or `Unsupported` is fine; joining
    // for real is on the manual checklist and never done here.

    #[tokio::test]
    async fn wifi_current_answers_on_this_machine() {
        let platform = WindowsPlatform::new(std::env::temp_dir());
        match platform.wifi_current().await {
            Ok(current) => eprintln!("current Wi-Fi connection: {current:?}"),
            Err(PlatformError::Unsupported(message)) => eprintln!("no Wi-Fi: {message}"),
            Err(error) => panic!("querying the Wi-Fi connection failed: {error}"),
        }
    }

    #[tokio::test]
    async fn wifi_scan_answers_on_this_machine() {
        let platform = WindowsPlatform::new(std::env::temp_dir());
        match platform.wifi_scan().await {
            Ok(ssids) => {
                eprintln!("networks in range: {ssids:?}");
                assert!(ssids.iter().all(|ssid| !ssid.is_empty()));
                let mut unique = ssids.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), ssids.len(), "duplicates in {ssids:?}");
            }
            Err(PlatformError::Unsupported(message)) => eprintln!("no Wi-Fi: {message}"),
            Err(error) => panic!("scanning failed: {error}"),
        }
    }

    #[test]
    fn system_info_reports_windows() {
        let info = WindowsPlatform::new(std::env::temp_dir()).system_info();
        assert_eq!(info.os, "windows");
        assert!(!info.hostname.is_empty());
    }
}
