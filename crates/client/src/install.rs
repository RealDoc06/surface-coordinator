//! `client.exe install` and `client.exe uninstall`: the client installs
//! itself as a Windows service, no separate installer.
//!
//! Installing copies the running executable to the install directory,
//! creates the state directory readable only by SYSTEM and Administrators
//! (it holds the enrollment secret), writes `config.json` there, and
//! registers and starts the service. Running it again over an existing
//! installation replaces the executable and configuration and keeps the
//! device id.
//!
//! What can be decided without the system (paths, command lines, access
//! rules, messages) is in plain functions with tests; the rest needs an
//! elevated prompt and is on the manual checklist.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::config::{self, ConfigFile};
use crate::platform::windows::{quote_arguments, quote_program};

pub const SERVICE_NAME: &str = "SurfaceCoordinatorClient";
pub const SERVICE_DISPLAY_NAME: &str = "Surface Coordinator Client";
pub const SERVICE_DESCRIPTION: &str =
    "Keeps this device connected to its Surface Coordinator and runs the jobs sent to it.";
/// What `install` and `uninstall` say when not run elevated.
pub const NOT_ELEVATED: &str = "run this from an elevated (Administrator) prompt";
/// Name of the executable in the install directory.
pub const EXE_NAME: &str = "client.exe";
/// Scripts run as the logged-in user are written here by the client and
/// read by that user.
pub const SCRIPTS_DIR: &str = "scripts";

/// Well-known SIDs, so that the access rules do not depend on the
/// language of Windows' account names.
const SYSTEM: &str = "*S-1-5-18";
const ADMINISTRATORS: &str = "*S-1-5-32-544";
const USERS: &str = "*S-1-5-32-545";

/// How long a stopping service may take before install or uninstall gives up.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to keep trying to replace an executable that the stopped
/// service's process may still hold for a moment.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(15);
/// Windows error codes the service calls are expected to return.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;

/// `%ProgramFiles%\Surface Coordinator`.
pub fn default_install_dir(program_files: Option<OsString>) -> PathBuf {
    let base = program_files.filter(|dir| !dir.is_empty()).unwrap_or_else(|| r"C:\Program Files".into());
    Path::new(&base).join("Surface Coordinator")
}

/// `%ProgramData%\Surface Coordinator`.
pub fn default_state_dir(program_data: Option<OsString>) -> PathBuf {
    let base = program_data.filter(|dir| !dir.is_empty()).unwrap_or_else(|| r"C:\ProgramData".into());
    Path::new(&base).join("Surface Coordinator")
}

/// What the service is started with after the executable.
pub fn service_arguments(state_dir: &Path) -> Vec<OsString> {
    vec!["service".into(), "--state-dir".into(), state_dir.as_os_str().to_owned()]
}

/// The service's command line, as the service manager will run it. The
/// `windows-service` crate builds the same from the executable and
/// [`service_arguments`]; this is what `install` shows.
pub fn service_command_line(exe: &Path, state_dir: &Path) -> anyhow::Result<String> {
    let state_dir = state_dir.to_str().context("the state directory is not valid Unicode")?;
    let arguments = quote_arguments(&["service", "--state-dir", state_dir]);
    Ok(format!("{} {arguments}", quote_program(exe)?))
}

/// `icacls` arguments that make the state directory SYSTEM's and
/// Administrators' alone: inherited access (Users can read %ProgramData%)
/// is removed, and full control for the two is inherited by everything
/// inside.
pub fn state_dir_access(dir: &Path) -> Vec<OsString> {
    let mut arguments = vec![dir.as_os_str().to_owned()];
    arguments.extend(
        ["/inheritance:r", "/grant:r", &format!("{SYSTEM}:(OI)(CI)F"), &format!("{ADMINISTRATORS}:(OI)(CI)F")]
            .map(OsString::from),
    );
    arguments
}

/// `icacls` arguments that let users read the scripts directory as well,
/// because scripts run as the logged-in user are read from there by them.
pub fn scripts_dir_access(dir: &Path) -> Vec<OsString> {
    let mut arguments = vec![dir.as_os_str().to_owned()];
    arguments.extend(["/grant:r", &format!("{USERS}:(OI)(CI)RX")].map(OsString::from));
    arguments
}

/// The message of `install` or `uninstall` refusing to run unelevated.
pub fn not_elevated(command: &str) -> String {
    format!("client.exe {command} needs administrator rights: {NOT_ELEVATED}")
}

/// Whether this process runs elevated (with the Administrators group
/// enabled in its token).
pub fn is_elevated() -> io::Result<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = std::ptr::null_mut();
    // SAFETY: plain FFI calls; the token handle is closed below and the
    // information buffer is a correctly sized `TOKEN_ELEVATION`.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut length = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut length,
        );
        let error = io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(error);
        }
        Ok(elevation.TokenIsElevated != 0)
    }
}

/// Refuses to go on without elevation, before anything is touched.
pub fn require_elevation(command: &str) -> anyhow::Result<()> {
    let elevated = is_elevated().context("cannot tell whether this prompt is elevated")?;
    if !elevated {
        bail!(not_elevated(command));
    }
    Ok(())
}

pub struct InstallOptions {
    pub key_id: String,
    pub key_secret: String,
    pub coordinator_url: Option<String>,
    pub install_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// Installs (or reinstalls) and starts the service. Needs elevation.
pub fn install(options: &InstallOptions) -> anyhow::Result<()> {
    require_elevation("install")?;
    let source = std::env::current_exe().context("cannot find this executable")?;
    let exe = options.install_dir.join(EXE_NAME);
    let state_dir = &options.state_dir;
    let command_line = service_command_line(&exe, state_dir)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("cannot open the service manager")?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::CHANGE_CONFIG;
    let existing = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => Some(service),
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => None,
        Err(error) => return Err(error).context("cannot open the existing service"),
    };
    if let Some(service) = &existing {
        println!("stopping the installed service");
        stop(service)?;
    }

    std::fs::create_dir_all(&options.install_dir)
        .with_context(|| format!("cannot create {}", options.install_dir.display()))?;
    if is_same_file(&source, &exe) {
        println!("{} is already in place", exe.display());
    } else {
        replace_file(&source, &exe)?;
        println!("copied the client to {}", exe.display());
    }

    // Restricted before the secret is written into it.
    std::fs::create_dir_all(state_dir.join(SCRIPTS_DIR))
        .with_context(|| format!("cannot create {}", state_dir.display()))?;
    icacls(&state_dir_access(state_dir))?;
    icacls(&scripts_dir_access(&state_dir.join(SCRIPTS_DIR)))?;
    let device_id = config::existing_device_id(state_dir)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let config = ConfigFile {
        device_id: Some(device_id.clone()),
        key_id: Some(options.key_id.clone()),
        key_secret: Some(options.key_secret.clone()),
        coordinator_url: options.coordinator_url.clone(),
    };
    write_atomically(&state_dir.join(config::CONFIG_FILE), &config.to_json())?;
    println!("wrote the configuration to {} (device id {device_id})", state_dir.join(config::CONFIG_FILE).display());

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: service_arguments(state_dir),
        dependencies: Vec::new(),
        // LocalSystem.
        account_name: None,
        account_password: None,
    };
    let service = match existing {
        Some(service) => {
            service.change_config(&info).context("cannot update the service")?;
            service
        }
        None => manager.create_service(&info, access).context("cannot create the service")?,
    };
    service.set_description(SERVICE_DESCRIPTION).context("cannot set the service description")?;
    service
        .update_failure_actions(failure_actions())
        .context("cannot set the service's restart on failure")?;
    // Also restart after a clean exit with an error code, which is how the
    // client restarts itself after an update.
    service
        .set_failure_actions_on_non_crash_failures(true)
        .context("cannot set the service's restart on failure")?;
    service.start::<&OsStr>(&[]).context("cannot start the service")?;
    println!("service {SERVICE_NAME} installed and started: {command_line}");
    Ok(())
}

/// Restart after 5 s, 5 s, then 60 s; the count starts over after a day
/// without failures.
fn failure_actions() -> ServiceFailureActions {
    let restart = |seconds| ServiceAction { action_type: ServiceActionType::Restart, delay: Duration::from_secs(seconds) };
    ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart(5), restart(5), restart(60)]),
    }
}

/// Stops and deletes the service and removes the install directory; with
/// `purge` the state directory (configuration, device id, job ledger, log)
/// too. Needs elevation.
pub fn uninstall(install_dir: &Path, state_dir: &Path, purge: bool) -> anyhow::Result<()> {
    require_elevation("uninstall")?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("cannot open the service manager")?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            stop(&service)?;
            service.delete().context("cannot delete the service")?;
            println!("service {SERVICE_NAME} removed");
        }
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
            println!("service {SERVICE_NAME} is not installed")
        }
        Err(error) => return Err(error).context("cannot open the service"),
    }

    if install_dir.exists() {
        remove_install_dir(install_dir)?;
    }
    if purge && state_dir.exists() {
        std::fs::remove_dir_all(state_dir).with_context(|| format!("cannot remove {}", state_dir.display()))?;
        println!("removed {}", state_dir.display());
    } else if state_dir.exists() {
        println!("kept {} (configuration, device id); --purge removes it", state_dir.display());
    }
    Ok(())
}

/// Removes the install directory. Run from inside it, as is natural, this
/// executable cannot delete itself: it and the directory go at the next
/// restart of Windows.
fn remove_install_dir(install_dir: &Path) -> anyhow::Result<()> {
    let running = std::env::current_exe().ok().and_then(|exe| exe.canonicalize().ok());
    let dir = install_dir.canonicalize().unwrap_or_else(|_| install_dir.to_owned());
    let Some(running) = running.filter(|exe| exe.starts_with(&dir)) else {
        std::fs::remove_dir_all(install_dir).with_context(|| format!("cannot remove {}", install_dir.display()))?;
        println!("removed {}", install_dir.display());
        return Ok(());
    };
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let path = entry.path();
        if path == running {
            continue;
        }
        let removed = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        removed.with_context(|| format!("cannot remove {}", path.display()))?;
    }
    delete_at_reboot(&running)?;
    delete_at_reboot(&dir)?;
    println!("{} is removed at the next restart of Windows (this program is running from it)", dir.display());
    Ok(())
}

fn delete_at_reboot(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; a null target means delete.
    if unsafe { MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) } == 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("cannot remove {} at restart", path.display()));
    }
    Ok(())
}

fn os_error(error: &windows_service::Error) -> Option<i32> {
    match error {
        windows_service::Error::Winapi(error) => error.raw_os_error(),
        _ => None,
    }
}

/// Stops the service if it runs and waits until it has.
fn stop(service: &windows_service::service::Service) -> anyhow::Result<()> {
    let status = service.query_status().context("cannot query the service")?;
    if status.current_state == ServiceState::Stopped {
        return Ok(());
    }
    match service.stop() {
        Ok(_) => {}
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_NOT_ACTIVE) => return Ok(()),
        Err(error) => return Err(error).context("cannot stop the service"),
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while service.query_status().context("cannot query the service")?.current_state != ServiceState::Stopped {
        if Instant::now() > deadline {
            bail!("the service did not stop within {} s", STOP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copies `source` over `target`, retrying while the stopped service's
/// process still holds the old file.
fn replace_file(source: &Path, target: &Path) -> anyhow::Result<()> {
    let deadline = Instant::now() + REPLACE_TIMEOUT;
    loop {
        match std::fs::copy(source, target) {
            Ok(_) => return Ok(()),
            Err(error) if Instant::now() < deadline && error.kind() != io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(500))
            }
            Err(error) => return Err(error).with_context(|| format!("cannot copy the client to {}", target.display())),
        }
    }
}

fn write_atomically(path: &Path, text: &str) -> anyhow::Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text).with_context(|| format!("cannot write {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("cannot write {}", path.display()))
}

fn icacls(arguments: &[OsString]) -> anyhow::Result<()> {
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let program = Path::new(&system_root).join("System32").join("icacls.exe");
    let output = Command::new(&program).args(arguments).output().context("cannot run icacls")?;
    if !output.status.success() {
        bail!(
            "icacls {} failed ({}): {}{}",
            arguments.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(arguments: &[OsString]) -> Vec<String> {
        arguments.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn default_directories_follow_the_environment() {
        assert_eq!(
            default_install_dir(Some(r"D:\Programs".into())),
            Path::new(r"D:\Programs\Surface Coordinator")
        );
        assert_eq!(
            default_state_dir(Some(r"D:\Data".into())),
            Path::new(r"D:\Data\Surface Coordinator")
        );
        // Unset or empty: the usual locations.
        assert_eq!(default_install_dir(None), Path::new(r"C:\Program Files\Surface Coordinator"));
        assert_eq!(default_state_dir(Some("".into())), Path::new(r"C:\ProgramData\Surface Coordinator"));
    }

    #[test]
    fn the_service_command_line_quotes_paths_with_spaces() {
        let line = service_command_line(
            Path::new(r"C:\Program Files\Surface Coordinator\client.exe"),
            Path::new(r"C:\ProgramData\Surface Coordinator"),
        )
        .unwrap();
        assert_eq!(
            line,
            r#""C:\Program Files\Surface Coordinator\client.exe" service --state-dir "C:\ProgramData\Surface Coordinator""#
        );
        assert_eq!(
            strings(&service_arguments(Path::new(r"C:\ProgramData\Surface Coordinator"))),
            ["service", "--state-dir", r"C:\ProgramData\Surface Coordinator"]
        );
    }

    #[test]
    fn the_service_command_line_reads_back_unchanged_by_the_system_parser() {
        let exe = Path::new(r"C:\Fake Install\client.exe");
        // A trailing backslash would otherwise escape the closing quote.
        let state_dir = Path::new(r"D:\fake state dir\");
        let parsed = crate::platform::windows::command_line_to_argv(&service_command_line(exe, state_dir).unwrap());
        assert_eq!(parsed, [r"C:\Fake Install\client.exe", "service", "--state-dir", r"D:\fake state dir\"]);
    }

    #[test]
    fn the_state_directory_is_for_system_and_administrators_only() {
        assert_eq!(
            strings(&state_dir_access(Path::new(r"C:\ProgramData\Surface Coordinator"))),
            [
                r"C:\ProgramData\Surface Coordinator",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
            ]
        );
    }

    #[test]
    fn users_may_read_the_scripts_directory_only() {
        assert_eq!(
            strings(&scripts_dir_access(Path::new(r"C:\ProgramData\Surface Coordinator\scripts"))),
            [r"C:\ProgramData\Surface Coordinator\scripts", "/grant:r", "*S-1-5-32-545:(OI)(CI)RX"]
        );
    }

    #[test]
    fn the_refusal_says_what_to_do() {
        assert_eq!(
            not_elevated("install"),
            "client.exe install needs administrator rights: run this from an elevated (Administrator) prompt"
        );
    }

    #[test]
    fn failure_actions_restart_after_5_5_and_60_seconds_and_reset_daily() {
        let actions = failure_actions();
        let delays: Vec<u64> = actions.actions.unwrap().iter().map(|a| a.delay.as_secs()).collect();
        assert_eq!(delays, [5, 5, 60]);
        assert!(matches!(actions.reset_period, ServiceFailureResetPeriod::After(d) if d.as_secs() == 86_400));
    }

    #[test]
    fn elevation_can_be_asked() {
        // Whatever this test runs as; the call itself must work.
        let elevated = is_elevated().unwrap();
        eprintln!("elevated: {elevated}");
    }
}
