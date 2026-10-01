//! Starting processes, either as the client itself or as the logged-in user.
//!
//! On a device the client is a service running as LocalSystem. There "as the
//! logged-in user" means the token of the user on the active console
//! session, a process started with `CreateProcessAsUserW` on that user's
//! interactive desktop. Started by hand (console mode) the client already is
//! the logged-in user and starts the process directly, the same way it
//! starts anything else.
//!
//! Only console mode can run on a development machine. The LocalSystem path
//! is on the manual test checklist; its invariants are spelled out where
//! they matter.

use std::ffi::{c_void, OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::path::Path;
use std::process::Stdio;
use std::ptr::{null, null_mut};
use std::sync::{Arc, OnceLock};

use protocol::RunAs;
use tokio::io::AsyncRead;
use tokio::net::windows::named_pipe::{PipeMode, ServerOptions};
use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, SetHandleInformation, ERROR_NO_TOKEN, HANDLE, HANDLE_FLAG_INHERIT,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, SecurityImpersonation, TokenPrimary,
    TokenUser, WinLocalSystemSid, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    CreateProcessAsUserW, DeleteProcThreadAttributeList, GetCurrentProcess, GetExitCodeProcess,
    InitializeProcThreadAttributeList, OpenProcessToken, ResumeThread, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject, CREATE_NEW_CONSOLE, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, INFINITE, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESHOWWINDOW, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use crate::platform::PlatformError;

pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `ShowWindow` values, without pulling in the whole windowing API.
const SW_HIDE: u16 = 0;
const SW_SHOWNORMAL: u16 = 1;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Window {
    /// No console window: scripts and setting snippets.
    Hidden,
    /// A console program gets its own console window; a windowed program
    /// is shown as usual.
    Normal,
}

/// What to start.
pub(crate) struct Spec<'a> {
    /// A full path, or a program name looked up the usual way.
    pub program: &'a Path,
    /// The command line after the program, already quoted
    /// ([`quote_arguments`]) or deliberately raw (the `cmd /c` line of a
    /// script).
    pub arguments: &'a str,
    /// Set in the environment on top of the usual variables.
    pub env: &'a [(&'a str, OsString)],
    /// Working directory of a process the client starts as itself. One
    /// started as another user starts in that user's profile directory.
    pub current_dir: Option<&'a Path>,
    pub window: Window,
    /// Pipes for stdout and stderr, and an empty stdin. Without them the
    /// process gets no standard handles from the client.
    pub capture_output: bool,
    /// Stop the process if the [`Child`] is dropped before it has exited.
    pub kill_on_drop: bool,
}

pub(crate) type Pipe = Box<dyn AsyncRead + Send + Unpin>;

/// A started process. A process started as another user is suspended until
/// [`Child::resume`], so that it can be put in a Job Object before it can
/// start anything itself.
pub(crate) struct Child {
    process: Process,
    pub stdout: Option<Pipe>,
    pub stderr: Option<Pipe>,
}

enum Process {
    Direct(Box<tokio::process::Child>),
    AsUser(UserProcess),
}

/// Starts `spec` as the client itself (`RunAs::System`) or as the
/// logged-in user (`RunAs::User`).
pub(crate) async fn spawn(spec: &Spec<'_>, run_as: RunAs) -> Result<Child, PlatformError> {
    match run_as {
        RunAs::User if is_local_system()? => spawn_as_user(spec).await,
        // Not LocalSystem: the client is the logged-in user already.
        RunAs::User | RunAs::System => spawn_direct(spec),
    }
}

impl Child {
    /// The process id, while the process is known to be there.
    pub fn id(&self) -> Option<u32> {
        match &self.process {
            Process::Direct(child) => child.id(),
            Process::AsUser(process) => Some(process.pid),
        }
    }

    pub fn raw_handle(&self) -> Option<RawHandle> {
        match &self.process {
            Process::Direct(child) => child.raw_handle(),
            Process::AsUser(process) => Some(process.handle.as_raw_handle()),
        }
    }

    /// Lets a process started suspended run. Does nothing for one that was
    /// not.
    pub fn resume(&mut self) -> Result<(), PlatformError> {
        let Process::AsUser(process) = &mut self.process else { return Ok(()) };
        let Some(thread) = &process.suspended_thread else { return Ok(()) };
        // SAFETY: `thread` is the process's main thread handle, owned here.
        if unsafe { ResumeThread(thread.as_raw_handle() as HANDLE) } == u32::MAX {
            // Still suspended, so dropping the process terminates it.
            let error = io::Error::last_os_error();
            return Err(PlatformError::Failed(format!("cannot resume the process: {error}")));
        }
        process.suspended_thread = None;
        Ok(())
    }

    /// Waits for the process to exit and returns its exit code.
    pub async fn wait(&mut self) -> io::Result<i32> {
        match &mut self.process {
            Process::Direct(child) => Ok(child.wait().await?.code().unwrap_or(-1)),
            Process::AsUser(process) => {
                let handle = process.handle.clone();
                // There is no asynchronous wait for a raw process handle in
                // tokio. If this future is dropped the thread keeps waiting
                // until the process ends, which whoever dropped it sees to.
                let code = tokio::task::spawn_blocking(move || wait_for_exit(&handle))
                    .await
                    .map_err(io::Error::other)??;
                process.exited = true;
                Ok(code)
            }
        }
    }

    /// Stops the process (not what it started; that is the Job Object's
    /// business).
    pub async fn kill(&mut self) {
        match &mut self.process {
            Process::Direct(child) => {
                let _ = child.kill().await;
            }
            Process::AsUser(process) => process.terminate(),
        }
    }
}

fn spawn_direct(spec: &Spec<'_>) -> Result<Child, PlatformError> {
    let mut command = tokio::process::Command::new(spec.program);
    if !spec.arguments.is_empty() {
        command.raw_arg(spec.arguments);
    }
    for (name, value) in spec.env {
        command.env(name, value);
    }
    if let Some(dir) = spec.current_dir {
        command.current_dir(dir);
    }
    let (stdout, stderr) = if spec.capture_output {
        (Stdio::piped(), Stdio::piped())
    } else {
        (Stdio::null(), Stdio::null())
    };
    command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .creation_flags(window_flag(spec.window))
        .kill_on_drop(spec.kill_on_drop);
    let mut child = command.spawn().map_err(|error| {
        PlatformError::Failed(format!("cannot start {}: {error}", spec.program.display()))
    })?;
    let stdout = child.stdout.take().map(|pipe| Box::new(pipe) as Pipe);
    let stderr = child.stderr.take().map(|pipe| Box::new(pipe) as Pipe);
    Ok(Child { process: Process::Direct(Box::new(child)), stdout, stderr })
}

fn window_flag(window: Window) -> u32 {
    match window {
        Window::Hidden => CREATE_NO_WINDOW,
        // Without it a console program would share the client's console,
        // or as a service have none the user can see.
        Window::Normal => CREATE_NEW_CONSOLE,
    }
}

// ------------------------------------------------------------ who am I

/// Whether the client runs as LocalSystem, i.e. as the service. Asked once;
/// an error is not cached, and makes anything that must run as the user
/// fail rather than risk running it as LocalSystem.
pub(crate) fn is_local_system() -> Result<bool, PlatformError> {
    static IS_LOCAL_SYSTEM: OnceLock<bool> = OnceLock::new();
    if let Some(known) = IS_LOCAL_SYSTEM.get() {
        return Ok(*known);
    }
    let token = process_token()?;
    let user = TokenUserInfo::of(&token)?;
    // SAFETY: the SID points into `user`'s buffer, which is alive here.
    let local_system = unsafe { IsWellKnownSid(user.sid(), WinLocalSystemSid) } != 0;
    Ok(*IS_LOCAL_SYSTEM.get_or_init(|| local_system))
}

/// The SID of the logged-in user, `S-1-5-21-…`: of the console session's
/// user as LocalSystem, of the client itself otherwise.
pub(crate) fn interactive_user_sid() -> Result<String, PlatformError> {
    let token = if is_local_system()? { user_token()? } else { process_token()? };
    TokenUserInfo::of(&token)?.sid_string()
}

fn last_error(what: &str) -> PlatformError {
    PlatformError::Failed(format!("{what} failed: {}", io::Error::last_os_error()))
}

fn process_token() -> Result<OwnedHandle, PlatformError> {
    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo handle of the current process needs no closing;
    // the token handle returned is owned by the `OwnedHandle`.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        Ok(OwnedHandle::from_raw_handle(token as RawHandle))
    }
}

/// A `TOKEN_USER` with the SID it points to, in one buffer.
struct TokenUserInfo {
    /// `u64`s for the alignment `TOKEN_USER` needs.
    buffer: Vec<u64>,
}

impl TokenUserInfo {
    fn of(token: &OwnedHandle) -> Result<TokenUserInfo, PlatformError> {
        let handle = token.as_raw_handle() as HANDLE;
        let mut length = 0u32;
        // SAFETY: the first call only reports the size needed; the second
        // writes at most `length` bytes into a buffer at least that large.
        unsafe {
            GetTokenInformation(handle, TokenUser, null_mut(), 0, &mut length);
            if length == 0 {
                return Err(last_error("GetTokenInformation"));
            }
            let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
            if GetTokenInformation(handle, TokenUser, buffer.as_mut_ptr().cast(), length, &mut length) == 0 {
                return Err(last_error("GetTokenInformation"));
            }
            Ok(TokenUserInfo { buffer })
        }
    }

    fn sid(&self) -> *mut c_void {
        // SAFETY: the buffer holds a `TOKEN_USER` written by the system.
        unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn sid_string(&self) -> Result<String, PlatformError> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: `ConvertSidToStringSidW` allocates a NUL-terminated string
        // that is read up to its terminator and freed with `LocalFree`.
        unsafe {
            if ConvertSidToStringSidW(self.sid(), &mut text) == 0 {
                return Err(last_error("ConvertSidToStringSidW"));
            }
            let length = (0..).take_while(|&i| *text.add(i) != 0).count();
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
            LocalFree(text.cast());
            Ok(sid)
        }
    }
}

// ------------------------------------------------------------ as the user (LocalSystem only)

/// The primary token of the user logged in on the console session.
///
/// Invariant: only called when the client is LocalSystem, which holds the
/// `SeTcbPrivilege` that `WTSQueryUserToken` needs. With UAC this is the
/// user's filtered token, so an administrator's process is not elevated.
/// Remote Desktop sessions are not considered: the device is used at its
/// own screen.
fn user_token() -> Result<OwnedHandle, PlatformError> {
    // SAFETY: plain calls; every handle returned is owned by an
    // `OwnedHandle` at once.
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        if session == u32::MAX {
            return Err(PlatformError::NoUserSession("no session is attached to the console".into()));
        }
        let mut token: HANDLE = null_mut();
        if WTSQueryUserToken(session, &mut token) == 0 {
            return Err(match GetLastError() {
                // The console shows the sign-in screen.
                ERROR_NO_TOKEN => PlatformError::NoUserSession("nobody is logged in to the device".into()),
                code => PlatformError::Failed(format!("WTSQueryUserToken failed with error {code}")),
            });
        }
        let token = OwnedHandle::from_raw_handle(token as RawHandle);
        let mut primary: HANDLE = null_mut();
        let duplicated = DuplicateTokenEx(
            token.as_raw_handle() as HANDLE,
            MAXIMUM_ALLOWED,
            null(),
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        );
        if duplicated == 0 {
            return Err(last_error("DuplicateTokenEx"));
        }
        Ok(OwnedHandle::from_raw_handle(primary as RawHandle))
    }
}

/// The environment a fresh logon of the token's user would have.
fn user_environment(token: &OwnedHandle) -> Result<Vec<OsString>, PlatformError> {
    let mut block: *mut c_void = null_mut();
    // SAFETY: the block is a sequence of NUL-terminated UTF-16 strings
    // ending with an empty one; it is read up to that and then destroyed.
    unsafe {
        if CreateEnvironmentBlock(&mut block, token.as_raw_handle() as HANDLE, 0) == 0 {
            return Err(last_error("CreateEnvironmentBlock"));
        }
        let mut entries = Vec::new();
        let mut cursor = block as *const u16;
        loop {
            let length = (0..).take_while(|&i| *cursor.add(i) != 0).count();
            if length == 0 {
                break;
            }
            entries.push(OsString::from_wide(std::slice::from_raw_parts(cursor, length)));
            cursor = cursor.add(length + 1);
        }
        DestroyEnvironmentBlock(block);
        Ok(entries)
    }
}

/// The name of a `NAME=value` entry. Entries such as `=C:=C:\dir` have a
/// name that starts with `=`.
fn variable_name(entry: &OsStr) -> String {
    let text = entry.to_string_lossy();
    let end = text.char_indices().skip(1).find(|&(_, c)| c == '=').map_or(text.len(), |(i, _)| i);
    text[..end].to_owned()
}

/// Builds a `CREATE_UNICODE_ENVIRONMENT` block from `base` with `extra`
/// added, replacing variables of the same name (names are not case
/// sensitive), sorted by name as Windows expects.
fn environment_block(base: &[OsString], extra: &[(&str, OsString)]) -> Vec<u16> {
    let mut entries: Vec<(String, OsString)> = base
        .iter()
        .map(|entry| (variable_name(entry), entry.clone()))
        .filter(|(name, _)| !extra.iter().any(|(extra, _)| extra.eq_ignore_ascii_case(name)))
        .collect();
    for (name, value) in extra {
        let mut entry = OsString::from(format!("{name}="));
        entry.push(value);
        entries.push((name.to_string(), entry));
    }
    entries.sort_by_key(|(name, _)| name.to_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (_, entry) in &entries {
        block.extend(entry.encode_wide());
        block.push(0);
    }
    if entries.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn environment_variable(entries: &[OsString], name: &str) -> Option<OsString> {
    entries.iter().find(|entry| variable_name(entry).eq_ignore_ascii_case(name)).map(|entry| {
        let wide: Vec<u16> = entry.encode_wide().skip(name.len() + 1).collect();
        OsString::from_wide(&wide)
    })
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

/// A process started as the user, owned by its handle.
struct UserProcess {
    /// Shared with a thread waiting for the process to exit.
    handle: Arc<OwnedHandle>,
    /// The main thread, until resumed.
    suspended_thread: Option<OwnedHandle>,
    pid: u32,
    kill_on_drop: bool,
    exited: bool,
}

impl UserProcess {
    fn terminate(&self) {
        // SAFETY: the handle is valid while `self` lives.
        unsafe {
            TerminateProcess(self.handle.as_raw_handle() as HANDLE, 1);
        }
    }
}

impl Drop for UserProcess {
    fn drop(&mut self) {
        // A process still suspended never ran: never leave it behind.
        if (self.kill_on_drop || self.suspended_thread.is_some()) && !self.exited {
            self.terminate();
        }
    }
}

fn wait_for_exit(handle: &OwnedHandle) -> io::Result<i32> {
    let handle = handle.as_raw_handle() as HANDLE;
    // SAFETY: the handle is kept alive by the caller's `Arc`.
    unsafe {
        if WaitForSingleObject(handle, INFINITE) != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        let mut code = 0u32;
        if GetExitCodeProcess(handle, &mut code) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(code as i32)
    }
}

/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`: the new process inherits exactly
/// these handles and no other inheritable handle the client happens to
/// have open.
struct InheritList {
    /// `u64`s for alignment; the list is opaque.
    buffer: Vec<u64>,
    /// Must stay at this address while the list is in use.
    _handles: Box<[HANDLE]>,
}

impl InheritList {
    fn new(handles: Vec<HANDLE>) -> Result<InheritList, PlatformError> {
        let handles = handles.into_boxed_slice();
        let mut size = 0usize;
        // SAFETY: the first call only reports the size; the list is then
        // initialised in a buffer of that size and deleted on drop. The
        // handle array lives in `self` for as long as the list.
        unsafe {
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut size);
            let mut buffer = vec![0u64; size.div_ceil(8)];
            if InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), 1, 0, &mut size) == 0 {
                return Err(last_error("InitializeProcThreadAttributeList"));
            }
            let list = InheritList { buffer, _handles: handles };
            let updated = UpdateProcThreadAttribute(
                list.as_ptr(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list._handles.as_ptr().cast(),
                std::mem::size_of_val(&*list._handles),
                null_mut(),
                null(),
            );
            if updated == 0 {
                return Err(last_error("UpdateProcThreadAttribute"));
            }
            Ok(list)
        }
    }

    fn as_ptr(&self) -> *mut c_void {
        self.buffer.as_ptr() as *mut c_void
    }
}

impl Drop for InheritList {
    fn drop(&mut self) {
        // SAFETY: initialised in `new`, deleted once.
        unsafe { DeleteProcThreadAttributeList(self.as_ptr()) }
    }
}

/// An inheritable handle for the child's side of a standard stream.
fn inheritable(file: &std::fs::File) -> Result<HANDLE, PlatformError> {
    let handle = file.as_raw_handle() as HANDLE;
    // Inheritable only matters for the one `CreateProcessAsUserW` call,
    // which inherits only the handles in its list; the client's own copy is
    // closed right after it.
    // SAFETY: the handle is valid while `file` lives.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
        return Err(last_error("SetHandleInformation"));
    }
    Ok(handle)
}

/// One output pipe: the client reads the server end asynchronously, the
/// child writes the client end. A named pipe rather than an anonymous one
/// because only those support overlapped reads; the name is random and the
/// one instance is taken at once.
async fn output_pipe() -> Result<(Pipe, std::fs::File), PlatformError> {
    let failed = |error: io::Error| PlatformError::Failed(format!("cannot create an output pipe: {error}"));
    let name = format!(r"\\.\pipe\surface-coordinator-{}", uuid::Uuid::new_v4().simple());
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(false)
        .max_instances(1)
        .pipe_mode(PipeMode::Byte)
        .create(&name)
        .map_err(failed)?;
    let client = std::fs::OpenOptions::new().write(true).open(&name).map_err(failed)?;
    // Already connected by the open above; this only completes the state.
    server.connect().await.map_err(failed)?;
    Ok((Box::new(server), client))
}

/// Starts `spec` in the console session as its user. Only called as
/// LocalSystem (see [`user_token`]).
async fn spawn_as_user(spec: &Spec<'_>) -> Result<Child, PlatformError> {
    spawn_with_token(spec, &user_token()?).await
}

/// Starts `spec` as the user of `token`, a primary token, on the
/// interactive desktop and with that user's environment and profile
/// directory.
///
/// Invariants: the process is created suspended and must be resumed by the
/// caller. Our copies of the pipe ends the child writes to are closed when
/// this returns, so reading ends when the child and whatever inherited from
/// it are gone.
async fn spawn_with_token(spec: &Spec<'_>, token: &OwnedHandle) -> Result<Child, PlatformError> {
    let base = user_environment(token)?;
    let environment = environment_block(&base, spec.env);
    let profile = environment_variable(&base, "USERPROFILE").map(|dir| wide(&dir));

    let (mut stdout, mut stderr, mut child_ends) = (None, None, Vec::new());
    if spec.capture_output {
        let failed = |error: io::Error| PlatformError::Failed(format!("cannot open NUL: {error}"));
        let (out, out_end) = output_pipe().await?;
        let (err, err_end) = output_pipe().await?;
        let nul = std::fs::OpenOptions::new().read(true).open("NUL").map_err(failed)?;
        stdout = Some(out);
        stderr = Some(err);
        child_ends = vec![nul, out_end, err_end];
    }
    let handles = child_ends.iter().map(inheritable).collect::<Result<Vec<_>, _>>()?;
    let inherit = if handles.is_empty() { None } else { Some(InheritList::new(handles.clone())?) };

    let program = quote_program(spec.program)?;
    let mut command_line = wide(OsStr::new(&format!("{program} {}", spec.arguments)));
    let mut desktop = wide(OsStr::new("winsta0\\default"));

    // SAFETY: every pointer passed points to a live, NUL-terminated buffer
    // or structure owned by this function for the duration of the call.
    // The handles in PROCESS_INFORMATION are owned by `OwnedHandle`s at once.
    unsafe {
        let mut startup: STARTUPINFOEXW = std::mem::zeroed();
        startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        startup.StartupInfo.dwFlags = STARTF_USESHOWWINDOW;
        startup.StartupInfo.wShowWindow = match spec.window {
            Window::Hidden => SW_HIDE,
            Window::Normal => SW_SHOWNORMAL,
        };
        let mut flags = CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED | window_flag(spec.window);
        if let [stdin, stdout, stderr] = handles[..] {
            startup.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
            startup.StartupInfo.hStdInput = stdin;
            startup.StartupInfo.hStdOutput = stdout;
            startup.StartupInfo.hStdError = stderr;
        }
        if let Some(inherit) = &inherit {
            startup.lpAttributeList = inherit.as_ptr();
            flags |= EXTENDED_STARTUPINFO_PRESENT;
        }
        let mut information: PROCESS_INFORMATION = std::mem::zeroed();
        let created = CreateProcessAsUserW(
            token.as_raw_handle() as HANDLE,
            null(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            i32::from(inherit.is_some()),
            flags,
            environment.as_ptr().cast(),
            profile.as_ref().map_or(null(), |dir| dir.as_ptr()),
            &startup.StartupInfo,
            &mut information,
        );
        let error = io::Error::last_os_error();
        drop(child_ends);
        if created == 0 {
            return Err(PlatformError::Failed(format!(
                "cannot start {} as the logged-in user: {error}",
                spec.program.display()
            )));
        }
        let process = UserProcess {
            handle: Arc::new(OwnedHandle::from_raw_handle(information.hProcess as RawHandle)),
            suspended_thread: Some(OwnedHandle::from_raw_handle(information.hThread as RawHandle)),
            pid: information.dwProcessId,
            kill_on_drop: spec.kill_on_drop,
            exited: false,
        };
        Ok(Child { process: Process::AsUser(process), stdout, stderr })
    }
}

// ------------------------------------------------------------ command lines

/// One argument quoted so that the C runtime's parser (and
/// `CommandLineToArgvW`) reads it back unchanged: backslashes are literal
/// unless they precede a quote, so those, and the ones before the closing
/// quote, are doubled.
pub(crate) fn quote_argument(argument: &str) -> String {
    let needs_quotes =
        argument.is_empty() || argument.contains([' ', '\t', '\n', '\u{b}', '"']);
    if !needs_quotes {
        return argument.to_owned();
    }
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for c in argument.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

/// The arguments of a command line, each quoted as needed.
pub(crate) fn quote_arguments<S: AsRef<str>>(arguments: &[S]) -> String {
    arguments.iter().map(|argument| quote_argument(argument.as_ref())).collect::<Vec<_>>().join(" ")
}

/// The program at the start of a command line. It follows simpler rules
/// than the arguments: everything up to the next quote, no escapes. A path
/// cannot contain a quote anyway.
pub(crate) fn quote_program(program: &Path) -> Result<String, PlatformError> {
    let text = program.to_str().ok_or_else(|| {
        PlatformError::Failed(format!("the path {} is not valid Unicode", program.display()))
    })?;
    if text.contains('"') || text.contains('\0') {
        return Err(PlatformError::Failed(format!("the path {text:?} contains a quote or NUL")));
    }
    Ok(format!("\"{text}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_by_the_windows_rules() {
        let cases: &[(&str, &str)] = &[
            ("plain", "plain"),
            ("", r#""""#),
            ("two words", r#""two words""#),
            ("tab\there", "\"tab\there\""),
            (r#"say "hi""#, r#""say \"hi\"""#),
            (r"C:\dir\file.txt", r"C:\dir\file.txt"),
            (r"C:\dir with space\", r#""C:\dir with space\\""#),
            (r#"back\"quote"#, r#""back\\\"quote""#),
            (r#"two\\"quotes"#, r#""two\\\\\"quotes""#),
            (r"trailing\\", r"trailing\\"),
            (r#"a\\b c"#, r#""a\\b c""#),
            ("\"", r#""\"""#),
            ("grüße ✓", "\"grüße ✓\""),
        ];
        for (argument, expected) in cases {
            assert_eq!(quote_argument(argument), *expected, "argument {argument:?}");
        }
        assert_eq!(quote_arguments(&["/c", "exit 0"]), r#"/c "exit 0""#);
        assert_eq!(quote_arguments::<&str>(&[]), "");
    }

    #[test]
    fn quoted_arguments_read_back_unchanged_by_the_system_parser() {
        let arguments = [
            "plain",
            "",
            "two words",
            r#"say "hi""#,
            r"C:\dir with space\",
            r#"back\"quote"#,
            r#"x\\\"y"#,
            r"\\server\share\file name",
            "  leading and trailing  ",
            r#"""#,
            r#"\"#,
            "grüße ✓",
        ];
        let line = format!("{} {}", quote_program(Path::new(r"C:\Fake\program.exe")).unwrap(), quote_arguments(&arguments));
        let parsed = command_line_to_argv(&line);
        assert_eq!(parsed[0], r"C:\Fake\program.exe");
        assert_eq!(&parsed[1..], &arguments.map(String::from)[..], "command line {line}");
    }

    /// The shell's parser, which follows the same rules as the C runtime.
    fn command_line_to_argv(line: &str) -> Vec<String> {
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
        let wide_line = wide(OsStr::new(line));
        let mut count = 0;
        // SAFETY: the returned array of `count` NUL-terminated strings is
        // read and then freed with `LocalFree`, as documented.
        unsafe {
            let argv = CommandLineToArgvW(wide_line.as_ptr(), &mut count);
            assert!(!argv.is_null());
            let parsed = (0..count as usize)
                .map(|i| {
                    let arg = *argv.add(i);
                    let length = (0..).take_while(|&j| *arg.add(j) != 0).count();
                    String::from_utf16(std::slice::from_raw_parts(arg, length)).unwrap()
                })
                .collect();
            LocalFree(argv.cast());
            parsed
        }
    }

    #[test]
    fn a_program_path_is_always_quoted_and_cannot_contain_a_quote() {
        assert_eq!(quote_program(Path::new(r"C:\Program Files\App\app.exe")).unwrap(), r#""C:\Program Files\App\app.exe""#);
        assert_eq!(quote_program(Path::new("notepad.exe")).unwrap(), r#""notepad.exe""#);
        assert!(quote_program(Path::new(r#"C:\bad"name.exe"#)).is_err());
    }

    #[test]
    fn extra_variables_replace_existing_ones_and_the_block_is_sorted() {
        let base: Vec<OsString> = ["Path=C:\\Windows", "=C:=C:\\work", "sc_value=old", "TEMP=C:\\Temp", "ALLUSERSPROFILE=C:\\ProgramData"]
            .iter()
            .map(OsString::from)
            .collect();
        let extra = [("SC_VALUE", OsString::from("new; not code")), ("SC_SCRIPT", OsString::from("C:\\s.ps1"))];
        let block = environment_block(&base, &extra);
        assert_eq!(&block[block.len() - 2..], &[0, 0]);
        let text = String::from_utf16(&block[..block.len() - 2]).unwrap();
        let entries: Vec<&str> = text.split('\0').collect();
        assert_eq!(
            entries,
            [
                "=C:=C:\\work",
                "ALLUSERSPROFILE=C:\\ProgramData",
                "Path=C:\\Windows",
                "SC_SCRIPT=C:\\s.ps1",
                "SC_VALUE=new; not code",
                "TEMP=C:\\Temp",
            ]
        );
        assert_eq!(environment_block(&[], &[]), [0, 0]);
    }

    #[test]
    fn variables_are_found_by_name_without_regard_to_case() {
        let entries: Vec<OsString> =
            ["=C:=C:\\work", "UserProfile=C:\\Users\\fake-user"].iter().map(OsString::from).collect();
        assert_eq!(environment_variable(&entries, "USERPROFILE"), Some(OsString::from("C:\\Users\\fake-user")));
        assert_eq!(environment_variable(&entries, "=C:"), Some(OsString::from("C:\\work")));
        assert_eq!(environment_variable(&entries, "TEMP"), None);
    }

    /// A primary token of this process's own user. `CreateProcessAsUserW`
    /// accepts it without the privileges LocalSystem has, so everything
    /// after `WTSQueryUserToken` can be exercised here.
    fn own_primary_token() -> OwnedHandle {
        use windows_sys::Win32::Security::TOKEN_DUPLICATE;
        // SAFETY: as in `user_token`.
        unsafe {
            let mut token: HANDLE = null_mut();
            assert_ne!(OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut token), 0);
            let token = OwnedHandle::from_raw_handle(token as RawHandle);
            let mut primary: HANDLE = null_mut();
            let duplicated = DuplicateTokenEx(
                token.as_raw_handle() as HANDLE,
                MAXIMUM_ALLOWED,
                null(),
                SecurityImpersonation,
                TokenPrimary,
                &mut primary,
            );
            assert_ne!(duplicated, 0, "{}", io::Error::last_os_error());
            OwnedHandle::from_raw_handle(primary as RawHandle)
        }
    }

    fn cmd_exe() -> std::path::PathBuf {
        super::super::system32().join("cmd.exe")
    }

    fn process_exists(pid: u32) -> bool {
        let tasklist = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&tasklist.stdout).contains(&pid.to_string())
    }

    async fn read_all(pipe: Option<Pipe>) -> String {
        use tokio::io::AsyncReadExt;
        let mut text = String::new();
        pipe.expect("output is captured").read_to_string(&mut text).await.unwrap();
        text
    }

    #[tokio::test]
    async fn a_process_started_with_a_token_has_its_output_environment_and_exit_code() {
        let cmd = cmd_exe();
        let env = [("SC_TEST_VALUE", OsString::from("fake-test-value"))];
        let spec = Spec {
            program: &cmd,
            arguments: r#"/d /s /c "echo %SC_TEST_VALUE%& cd& echo to stderr 1>&2& exit 7""#,
            env: &env,
            current_dir: None,
            window: Window::Hidden,
            capture_output: true,
            kill_on_drop: true,
        };
        let mut child = spawn_with_token(&spec, &own_primary_token()).await.unwrap();
        child.resume().unwrap();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let (stdout, stderr, code) = tokio::join!(read_all(stdout), read_all(stderr), child.wait());
        assert_eq!(code.unwrap(), 7);
        // The user's profile is the working directory.
        let profile = std::env::var("USERPROFILE").unwrap();
        assert_eq!(stdout, format!("fake-test-value\r\n{profile}\r\n"));
        assert_eq!(stderr, "to stderr \r\n");
    }

    #[tokio::test]
    async fn a_process_started_with_a_token_without_output_reports_its_exit_code() {
        let cmd = cmd_exe();
        let spec = Spec {
            program: &cmd,
            arguments: "/d /c exit 3",
            env: &[],
            current_dir: None,
            window: Window::Hidden,
            capture_output: false,
            kill_on_drop: false,
        };
        let mut child = spawn_with_token(&spec, &own_primary_token()).await.unwrap();
        assert!(child.stdout.is_none());
        child.resume().unwrap();
        assert_eq!(child.wait().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn a_process_dropped_before_it_was_resumed_is_not_left_behind() {
        let cmd = cmd_exe();
        let spec = Spec {
            program: &cmd,
            arguments: "/d /c exit 0",
            env: &[],
            current_dir: None,
            window: Window::Hidden,
            capture_output: false,
            // Even so: a process that never ran is never left suspended.
            kill_on_drop: false,
        };
        let child = spawn_with_token(&spec, &own_primary_token()).await.unwrap();
        let pid = child.id().unwrap();
        assert!(process_exists(pid));
        drop(child);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!process_exists(pid), "suspended process {pid} left behind");
    }

    #[tokio::test]
    async fn starting_a_missing_program_with_a_token_fails() {
        let missing = Path::new(r"C:\Fake Surface Coordinator Test\missing.exe");
        let spec = Spec {
            program: missing,
            arguments: "",
            env: &[],
            current_dir: None,
            window: Window::Normal,
            capture_output: false,
            kill_on_drop: false,
        };
        match spawn_with_token(&spec, &own_primary_token()).await {
            Err(PlatformError::Failed(message)) => assert!(message.contains("missing.exe"), "{message}"),
            Err(other) => panic!("unexpected error {other:?}"),
            Ok(_) => panic!("a missing program started"),
        }
    }

    #[test]
    fn this_test_process_is_not_local_system() {
        // Tests run as the developer, never as the service.
        assert_eq!(is_local_system(), Ok(false));
    }

    #[test]
    fn the_interactive_user_is_this_user_in_console_mode() {
        let sid = interactive_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let whoami = std::process::Command::new("whoami.exe").args(["/user", "/fo", "csv", "/nh"]).output().unwrap();
        let listing = String::from_utf8_lossy(&whoami.stdout);
        assert!(listing.contains(&format!("\"{sid}\"")), "{sid} not in {listing}");
    }
}
