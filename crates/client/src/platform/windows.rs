//! Windows implementation of [`Platform`].

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use protocol::{OutputStream, RunAs, Shell};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
};
use windows_sys::Win32::System::SystemInformation::{ComputerNameDnsHostname, GetComputerNameExW};

use super::{
    OutputChunk, Platform, PlatformError, ScriptOutcome, ScriptRequest, SystemInfo, Utf8Decoder,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long output is still collected after the script itself has ended.
/// A process the script left running can hold the pipes open indefinitely.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The shell is started through `cmd.exe` so that `chcp 65001` can switch the
/// (hidden) console to UTF-8 first; both shells then write UTF-8 to the
/// pipes. Paths are passed in environment variables, so no path character
/// needs escaping for `cmd`.
const POWERSHELL_COMMAND_LINE: &str = r#"/d /s /c ""%SC_CHCP%" 65001 >nul & "%SC_POWERSHELL%" -NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "%SC_SCRIPT%"""#;
const CMD_COMMAND_LINE: &str = r#"/d /s /c ""%SC_CHCP%" 65001 >nul & call "%SC_SCRIPT%"""#;

pub struct WindowsPlatform {
    work_dir: PathBuf,
}

impl WindowsPlatform {
    /// `work_dir` is where script bodies are written while they run. It
    /// should not be writable by less privileged users than the client.
    pub fn new(work_dir: impl Into<PathBuf>) -> WindowsPlatform {
        WindowsPlatform { work_dir: work_dir.into() }
    }
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

/// A Job Object holding the script's process tree, so that a timeout can
/// stop everything the script started and not just the shell.
struct ProcessTree(HANDLE);

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
            let tree = ProcessTree(job);
            (AssignProcessToJobObject(job, process as HANDLE) != 0).then_some(tree)
        }
    }

    fn terminate(&self) {
        // SAFETY: the handle is valid for the lifetime of `self`.
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        // Closing the handle does not stop the processes: something a
        // script deliberately left running survives the script.
        // SAFETY: the handle is owned by `self` and closed exactly once.
        unsafe {
            CloseHandle(self.0);
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
        if request.run_as == RunAs::User {
            return Err(PlatformError::Unsupported(
                "running as the logged-in user requires the client to run as a service".into(),
            ));
        }
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
        let mut command = Command::new(system32.join("cmd.exe"));
        command
            .raw_arg(command_line)
            .env("SC_CHCP", system32.join("chcp.com"))
            .env("SC_POWERSHELL", system32.join("WindowsPowerShell\\v1.0\\powershell.exe"))
            .env("SC_SCRIPT", &script)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|e| failed("cannot start the shell", e))?;

        let tree = child.raw_handle().and_then(ProcessTree::containing);
        if tree.is_none() {
            tracing::warn!("could not create a job object; a timeout will only stop the shell");
        }
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let readers = [
            tokio::spawn(pump(stdout, OutputStream::Stdout, output.clone())),
            tokio::spawn(pump(stderr, OutputStream::Stderr, output)),
        ];

        let outcome = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(Ok(status)) => Ok(ScriptOutcome::Exited(status.code().unwrap_or(-1))),
            Ok(Err(error)) => Err(failed("waiting for the shell failed", error)),
            Err(_) => {
                if let Some(tree) = &tree {
                    tree.terminate();
                }
                let _ = child.kill().await;
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
        let run = run_with(
            Shell::PowerShell,
            "$p = Start-Process ping.exe -ArgumentList '-n','120','127.0.0.1' -NoNewWindow -PassThru -RedirectStandardOutput \"$env:TEMP\\sc-test-ping-$PID.txt\"\n\
             Write-Output \"pid=$($p.Id)\"\n\
             Start-Sleep -Seconds 120",
            Duration::from_secs(4),
            RunAs::System,
        )
        .await;
        assert_eq!(run.outcome, Ok(ScriptOutcome::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(30), "took {:?}", started.elapsed());

        // Output produced before the timeout was delivered.
        let pid: u32 = run
            .stdout
            .trim()
            .strip_prefix("pid=")
            .unwrap_or_else(|| panic!("unexpected output {:?}", run.stdout))
            .parse()
            .unwrap();
        // The process the script started is gone too.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tasklist = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&tasklist.stdout);
        assert!(!listing.contains(&pid.to_string()), "ping still running: {listing}");
    }

    #[tokio::test]
    async fn run_as_user_is_unsupported() {
        let run = run_with(Shell::PowerShell, "Write-Output 'x'", Duration::from_secs(5), RunAs::User).await;
        assert!(matches!(run.outcome, Err(PlatformError::Unsupported(_))));
        assert_eq!(run.stdout, "");
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

    #[test]
    fn system_info_reports_windows() {
        let info = WindowsPlatform::new(std::env::temp_dir()).system_info();
        assert_eq!(info.os, "windows");
        assert!(!info.hostname.is_empty());
    }
}
