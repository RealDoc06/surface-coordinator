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

// Protected DACLs replace inherited AND explicit grants. Administrators
// own the files, so a previous non-admin owner cannot restore write access.
const PRIVATE_ACCESS: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";
const READABLE_ACCESS: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;GRGX;;;BU)";

/// How long a stopping service may take before install or uninstall gives up.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long to keep trying to replace an executable that the stopped
/// service's process may still hold for a moment.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(15);
/// Windows error codes the service calls are expected to return.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;

/// `%ProgramFiles%\Surface Coordinator`.
pub fn default_install_dir(program_files: Option<OsString>) -> PathBuf {
    let base =
        program_files.filter(|dir| !dir.is_empty()).unwrap_or_else(|| r"C:\Program Files".into());
    Path::new(&base).join("Surface Coordinator")
}

/// `%ProgramData%\Surface Coordinator`.
pub fn default_state_dir(program_data: Option<OsString>) -> PathBuf {
    let base =
        program_data.filter(|dir| !dir.is_empty()).unwrap_or_else(|| r"C:\ProgramData".into());
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

/// The message of `install` or `uninstall` refusing to run unelevated.
pub fn not_elevated(command: &str) -> String {
    format!("client.exe {command} needs administrator rights: {NOT_ELEVATED}")
}

/// Whether this process runs elevated (with the Administrators group
/// enabled in its token).
pub fn is_elevated() -> io::Result<bool> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
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
    pub hotspot_discovery: bool,
    pub install_dir: PathBuf,
    pub state_dir: PathBuf,
}

/// Installs (or reinstalls) and starts the service. Needs elevation.
pub fn install(options: &InstallOptions) -> anyhow::Result<()> {
    require_elevation("install")?;
    let (install_dir, state_dir) = installation_paths(&options.install_dir, &options.state_dir)?;
    validate_service_paths(&install_dir, &state_dir)?;
    // Protecting the application directories alone is insufficient when a
    // non-admin can rename or replace one of their parents.
    let _install_parents = secure_ancestors(&install_dir)?;
    let _state_parents = secure_ancestors(&state_dir)?;
    validate_contents(&install_dir, DirectoryKind::Install)?;
    validate_contents(&state_dir, DirectoryKind::State)?;
    if options.key_id.trim().is_empty() || options.key_secret.is_empty() {
        bail!("the enrollment key id and secret must not be empty");
    }
    let source = std::env::current_exe().context("cannot find this executable")?;
    let exe = install_dir.join(EXE_NAME);
    let command_line = service_command_line(&exe, &state_dir)?;

    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("cannot open the service manager")?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::START
        | ServiceAccess::STOP
        | ServiceAccess::DELETE
        | ServiceAccess::CHANGE_CONFIG;
    let existing = match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => Some(service),
        Err(error) if os_error(&error) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => None,
        Err(error) => return Err(error).context("cannot open the existing service"),
    };
    let previous_info = existing
        .as_ref()
        .map(|service| installed_info(service, &install_dir, &state_dir))
        .transpose()?;
    let was_running = existing
        .as_ref()
        .map(|service| service.query_status())
        .transpose()?
        .is_some_and(|status| status.current_state != ServiceState::Stopped);

    // Restricted before the secret is written into it.
    let _owned_directories = [
        create_owned_directory(&install_dir, READABLE_ACCESS)?,
        create_owned_directory(&state_dir, PRIVATE_ACCESS)?,
        create_owned_directory(&state_dir.join(SCRIPTS_DIR), READABLE_ACCESS)?,
    ];
    validate_contents(&install_dir, DirectoryKind::Install)?;
    validate_contents(&state_dir, DirectoryKind::State)?;
    secure_tree(&install_dir, true, false)?;
    secure_tree(&state_dir, false, true)?;
    let device_id = config::existing_device_id(&state_dir)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let config = ConfigFile {
        device_id: Some(device_id.clone()),
        key_id: Some(options.key_id.clone()),
        key_secret: Some(options.key_secret.clone()),
        coordinator_url: options.coordinator_url.clone(),
        hotspot_discovery: options.hotspot_discovery,
    };
    // Stage all copies before stopping a working installation. A failed copy
    // cannot truncate its executable or strand it offline.
    let config_path = state_dir.join(config::CONFIG_FILE);
    let mut new_config = StagedFile::write(&config_path, config.to_json().as_bytes())?;
    let mut old_config = FileBackup::capture(&config_path)?;
    let mut replacement =
        if is_same_file(&source, &exe) { None } else { Some(StagedFile::copy(&source, &exe)?) };
    let mut old_exe = replacement.as_ref().map(|_| FileBackup::capture(&exe)).transpose()?;

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: SERVICE_DISPLAY_NAME.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe.clone(),
        launch_arguments: service_arguments(&state_dir),
        dependencies: Vec::new(),
        // LocalSystem.
        account_name: None,
        account_password: None,
    };
    if let Some(service) = &existing {
        println!("stopping the installed service");
        stop(service)?;
    }
    let mut created = None;
    let mut exe_changed = false;
    let mut config_changed = false;
    let installed = (|| -> anyhow::Result<()> {
        if let Some(replacement) = &mut replacement {
            replacement.replace(&exe)?;
            exe_changed = true;
        }
        new_config.replace(&config_path)?;
        config_changed = true;
        let service = match &existing {
            Some(service) => {
                service.change_config(&info).context("cannot update the service")?;
                service
            }
            None => created.insert(
                manager.create_service(&info, access).context("cannot create the service")?,
            ),
        };
        service
            .set_description(SERVICE_DESCRIPTION)
            .context("cannot set the service description")?;
        service
            .update_failure_actions(failure_actions())
            .context("cannot set the service's restart on failure")?;
        // Restart clean exits with an error code too, as used by updates.
        service
            .set_failure_actions_on_non_crash_failures(true)
            .context("cannot set the service's restart on failure")?;
        start(service)
    })();
    if let Err(error) = installed {
        let rollback = (|| -> anyhow::Result<()> {
            if let Some(service) = existing.as_ref().or(created.as_ref()) {
                stop(service)?;
            }
            if exe_changed {
                old_exe.as_mut().expect("a replaced executable has a backup").restore()?;
            }
            if config_changed {
                old_config.restore()?;
            }
            if let Some(service) = &created {
                service.delete().context("cannot remove the incomplete service")?;
            }
            if let (Some(service), Some(info)) = (&existing, &previous_info) {
                service
                    .change_config(info)
                    .context("cannot restore the previous service configuration")?;
                if was_running {
                    start(service).context("cannot restart the previous installation")?;
                }
            }
            Ok(())
        })();
        return match rollback {
            Ok(()) => Err(error)
                .context("installation failed; previous executable and configuration restored"),
            Err(rollback) => {
                old_config.preserve();
                if let Some(backup) = &mut old_exe {
                    backup.preserve();
                }
                Err(error).context(format!(
                    "installation failed and recovery needs attention: {rollback:#}"
                ))
            }
        };
    }
    println!("wrote the configuration to {} (device id {device_id})", config_path.display());
    println!("service {SERVICE_NAME} installed and started: {command_line}");
    Ok(())
}

/// Restart after 5 s, 5 s, then 60 s; the count starts over after a day
/// without failures.
fn failure_actions() -> ServiceFailureActions {
    let restart = |seconds| ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(seconds),
    };
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
    let (install_dir, state_dir) = installation_paths(install_dir, state_dir)?;
    validate_contents(&install_dir, DirectoryKind::Install)?;
    validate_contents(&state_dir, DirectoryKind::State)?;
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("cannot open the service manager")?;
    let access = ServiceAccess::QUERY_STATUS
        | ServiceAccess::QUERY_CONFIG
        | ServiceAccess::STOP
        | ServiceAccess::DELETE;
    match manager.open_service(SERVICE_NAME, access) {
        Ok(service) => {
            installed_info(&service, &install_dir, &state_dir)?;
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
        remove_install_dir(&install_dir)?;
    }
    if purge && state_dir.exists() {
        std::fs::remove_dir_all(&state_dir)
            .with_context(|| format!("cannot remove {}", state_dir.display()))?;
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
        std::fs::remove_dir_all(install_dir)
            .with_context(|| format!("cannot remove {}", install_dir.display()))?;
        println!("removed {}", install_dir.display());
        return Ok(());
    };
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == running {
            continue;
        }
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        removed.with_context(|| format!("cannot remove {}", path.display()))?;
    }
    delete_at_reboot(&running)?;
    delete_at_reboot(&dir)?;
    println!(
        "{} is removed at the next restart of Windows (this program is running from it)",
        dir.display()
    );
    Ok(())
}

fn delete_at_reboot(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_DELAY_UNTIL_REBOOT};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is NUL-terminated; a null target means delete.
    if unsafe { MoveFileExW(wide.as_ptr(), std::ptr::null(), MOVEFILE_DELAY_UNTIL_REBOOT) } == 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("cannot remove {} at restart", path.display()));
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
    let deadline = Instant::now() + STOP_TIMEOUT;
    let mut requested = false;
    loop {
        let status = service.query_status().context("cannot query the service")?;
        if status.current_state == ServiceState::Stopped {
            return Ok(());
        }
        if !requested
            && !matches!(
                status.current_state,
                ServiceState::StartPending | ServiceState::StopPending
            )
        {
            match service.stop() {
                Ok(_) => requested = true,
                Err(error) if os_error(&error) == Some(ERROR_SERVICE_NOT_ACTIVE) => return Ok(()),
                Err(error) => return Err(error).context("cannot stop the service"),
            }
        }
        if Instant::now() > deadline {
            bail!("the service did not stop within {} s", STOP_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn start(service: &windows_service::service::Service) -> anyhow::Result<()> {
    service.start::<&OsStr>(&[]).context("cannot start the service")?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        let status = service.query_status().context("cannot query the starting service")?;
        match status.current_state {
            ServiceState::Running => return Ok(()),
            ServiceState::Stopped => bail!("the service stopped during startup ({:?}); check client.log in the state directory", status.exit_code),
            _ => {}
        }
        if Instant::now() > deadline {
            bail!("the service did not start within {} s", START_TIMEOUT.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Resolve existing ancestors before comparing paths, including the spelling
/// returned by Windows for short (8.3) names. Reparse points are not accepted.
fn installation_directory(path: &Path) -> anyhow::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        bail!("installation directories must not be empty");
    }
    let absolute = std::path::absolute(path)?;
    for ancestor in absolute.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => reject_reparse_point(ancestor, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot inspect {}", ancestor.display()))
            }
        }
    }
    let mut suffix = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.try_exists()? {
        suffix.push(existing.file_name().context("the directory has no existing ancestor")?);
        existing = existing.parent().context("the directory has no existing ancestor")?;
    }
    let mut resolved = existing.canonicalize()?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn comparison_path(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// SCM stores the command as an expandable string. Reject percent signs before
/// installation changes anything, even when a variable is currently undefined.
/// Uninstall must still be able to remove an older installation with such paths.
fn validate_service_paths(install_dir: &Path, state_dir: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    for (kind, path) in [("install", install_dir), ("state", state_dir)] {
        if path.as_os_str().encode_wide().any(|unit| unit == u16::from(b'%')) {
            bail!(
                "the {kind} directory {} contains '%', which Windows may expand in service paths; \
                 choose a directory without '%'",
                path.display()
            );
        }
    }
    Ok(())
}

fn installation_paths(install_dir: &Path, state_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let install_dir = installation_directory(install_dir)?;
    let state_dir = installation_directory(state_dir)?;
    let install_key = comparison_path(&install_dir);
    let state_key = comparison_path(&state_dir);
    if install_key.starts_with(&state_key) || state_key.starts_with(&install_key) {
        bail!("install and state directories must be separate, non-overlapping directories");
    }
    for path in [&install_dir, &state_dir] {
        if path.file_name().is_none() {
            bail!("{} is a volume root; choose a dedicated application directory", path.display());
        }
        for variable in [
            "SystemRoot",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramData",
            "USERPROFILE",
            "PUBLIC",
            "TEMP",
        ] {
            let Some(protected) = std::env::var_os(variable).filter(|value| !value.is_empty())
            else {
                continue;
            };
            let protected = comparison_path(&installation_directory(Path::new(&protected))?);
            let candidate = comparison_path(path);
            if protected.starts_with(&candidate)
                || (variable == "SystemRoot" && candidate.starts_with(&protected))
            {
                bail!("{} is not a dedicated application directory", path.display());
            }
        }
    }
    Ok((install_dir, state_dir))
}

fn reject_reparse_point(path: &Path, metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!(
            "{} is a junction or symbolic link; choose a regular application directory",
            path.display()
        );
    }
    if metadata.is_file() {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let file = std::fs::File::open(path)
            .with_context(|| format!("cannot inspect links to {}", path.display()))?;
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `file` owns the live handle and the output structure has
        // the size required by Windows. No file contents or ACLs are changed.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot inspect the installation file's links");
        }
        if information.nNumberOfLinks > 1 {
            bail!(
                "{} has multiple hard links; refusing to change access outside the installation",
                path.display()
            );
        }
    }
    Ok(())
}

/// Only inspect ancestors. The installer never changes permissions outside
/// the two dedicated directories selected by the operator.
fn secure_ancestors(directory: &Path) -> anyhow::Result<Vec<std::fs::File>> {
    let parent = directory.parent().context("the application directory has no parent")?;
    if !parent.is_dir() {
        bail!(
            "parent directory must already exist with safe ownership and access: {}",
            parent.display()
        );
    }
    let mut parents = Vec::new();
    let mut anchors = Vec::new();
    for ancestor in directory.ancestors().skip(1).collect::<Vec<_>>().into_iter().rev() {
        let handle = match parents.last() {
            Some(parent) => open_child_directory(parent, ancestor)?,
            None => open_directory(ancestor)?,
        };
        let safe = if handle_security(&handle, DirectorySecurity::Parent).is_ok() {
            Ok(())
        } else {
            // A junction can only replace an empty directory. A protected,
            // pinned direct child prevents this parent from becoming empty.
            handle_security(&handle, DirectorySecurity::AnchoredParent).and_then(|()| {
                protected_child(&handle, ancestor).map(|anchor| anchors.push(anchor))
            })
        };
        safe.with_context(|| format!(
            "{} is not a safe parent for a SYSTEM service; use Program Files/ProgramData or an administrator-owned location",
            ancestor.display(),
        ))?;
        parents.push(handle);
    }
    parents.extend(anchors);
    Ok(parents)
}

fn protected_child(parent: &std::fs::File, directory: &Path) -> anyhow::Result<std::fs::File> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if let Ok(child) = open_child_directory(parent, &path) {
            if handle_security(&child, DirectorySecurity::Parent).is_ok() {
                return Ok(child);
            }
        }
    }
    bail!("{} permits junction changes and has no protected direct child", directory.display())
}

/// No-follow handles pin each name against deletion/renaming. Reparse-point
/// prevention comes from the ACL/anchor proof, not Windows sharing flags.
fn open_directory(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };
    let file = std::fs::OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .with_context(|| format!("cannot inspect directory {}", path.display()))?;
    let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: the handle stays open and the output has the required layout.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0 {
        return Err(io::Error::last_os_error()).context("cannot inspect directory attributes");
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        bail!("{} is not a regular directory", path.display());
    }
    Ok(file)
}

fn directory_path(file: &std::fs::File) -> anyhow::Result<PathBuf> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 512];
    loop {
        // Flags 0 request normalized names with the same DOS volume naming
        // for both handles, including paths originally reached by a junction.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle().cast(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                0,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error()).context("cannot resolve the directory handle");
        }
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length + 1, 0);
    }
}

fn open_child_directory(parent: &std::fs::File, path: &Path) -> anyhow::Result<std::fs::File> {
    let child = open_directory(path)?;
    let canonical = directory_path(&child)?;
    if canonical.parent().map(comparison_path) != Some(comparison_path(&directory_path(parent)?)) {
        bail!("{} does not belong to the inspected parent directory", path.display());
    }
    Ok(child)
}

#[derive(Clone, Copy)]
enum DirectorySecurity {
    Parent,
    AnchoredParent,
    Owned,
}

fn handle_security(file: &std::fs::File, policy: DirectorySecurity) -> anyhow::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: attributes and security come from the same live no-follow
    // handle. Windows supplies one allocation, freed after inspection.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32))
            .context("cannot inspect directory security");
    }
    let safe = unsafe { validate_directory_security(descriptor, policy) };
    unsafe {
        LocalFree(descriptor);
    }
    safe
}

fn trusted_owner(sid: &str) -> bool {
    matches!(
        sid,
        "S-1-5-18"
            | "S-1-5-32-544"
            | "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    )
}

/// `descriptor` must be a live Windows security descriptor. Its ACL and SID
/// pointers are borrowed and never freed independently.
unsafe fn validate_directory_security(
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    policy: DirectorySecurity,
) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::{LocalFree, GENERIC_ALL, GENERIC_WRITE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{
        GetAce, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, ACCESS_ALLOWED_ACE,
        ACE_HEADER, INHERIT_ONLY_ACE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA,
        WRITE_DAC, WRITE_OWNER,
    };
    let sid_string = |sid: windows_sys::Win32::Security::PSID| -> anyhow::Result<String> {
        if sid.is_null() {
            bail!("missing security principal");
        }
        let mut text = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err(io::Error::last_os_error()).context("cannot inspect a security principal");
        }
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(result)
    };
    let mut owner = std::ptr::null_mut();
    let mut defaulted = 0;
    if GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted) == 0 {
        return Err(io::Error::last_os_error()).context("cannot inspect the directory owner");
    }
    if !trusted_owner(&sid_string(owner)?) {
        bail!("the parent is owned by an account other than SYSTEM, Administrators, or TrustedInstaller");
    }
    let mut present = 0;
    let mut acl = std::ptr::null_mut();
    if GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) == 0 {
        return Err(io::Error::last_os_error())
            .context("cannot inspect the directory access rules");
    }
    if present == 0 || acl.is_null() {
        bail!("the parent has unrestricted access");
    }
    for index in 0..(*acl).AceCount {
        let mut ace = std::ptr::null_mut();
        if GetAce(acl, index as u32, &mut ace) == 0 {
            return Err(io::Error::last_os_error())
                .context("cannot inspect a directory access rule");
        }
        let header = &*ace.cast::<ACE_HEADER>();
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        // Standard deny ACEs never grant permission. Unexpected effective
        // ACE types require review rather than assuming they are harmless.
        match header.AceType {
            1 => continue, // ACCESS_DENIED_ACE_TYPE
            0 => {}        // ACCESS_ALLOWED_ACE_TYPE
            _ => bail!("the parent has an unsupported effective access rule"),
        }
        let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        let mut replacement_rights =
            DELETE | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;
        if !matches!(policy, DirectorySecurity::AnchoredParent) {
            replacement_rights |= FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES | GENERIC_WRITE;
        }
        if matches!(policy, DirectorySecurity::Owned) {
            // Changing the ACL does not revoke preexisting handles. An
            // application leaf must not admit user-created subdirectories.
            replacement_rights |= FILE_APPEND_DATA;
        }
        if allowed.Mask & replacement_rights != 0 {
            let principal = sid_string(std::ptr::addr_of!(allowed.SidStart).cast_mut().cast())?;
            if !trusted_owner(&principal) && principal != "S-1-3-4" {
                bail!(
                    "the parent grants replacement, reparse-point, ownership, or permission changes to {principal}"
                );
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum DirectoryKind {
    Install,
    State,
    Scripts,
}

/// Do not recursively change permissions or delete a caller's unrelated data.
fn validate_contents(path: &Path, kind: DirectoryKind) -> anyhow::Result<()> {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot inspect {}", path.display()))
        }
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        reject_reparse_point(&path, &metadata)?;
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if matches!(kind, DirectoryKind::State) && name == SCRIPTS_DIR && metadata.is_dir() {
            validate_contents(&path, DirectoryKind::Scripts)?;
            continue;
        }
        let owned = match kind {
            DirectoryKind::Install => matches!(
                name.as_str(),
                "client.exe" | "client.old.exe" | "client.failed.exe" | "client.update.tmp"
            ),
            DirectoryKind::State => matches!(
                name.as_str(),
                "config.json"
                    | "config.json.tmp"
                    | "device_id"
                    | "jobs.json"
                    | "jobs.json.tmp"
                    | "jobs.json.corrupt"
                    | "network.json"
                    | "network.json.tmp"
                    | "network.json.corrupt"
                    | "update-pending.json"
                    | "update-pending.json.tmp"
                    | "update-pending.json.corrupt"
                    | "client.log"
                    | "client.log.1"
            ),
            DirectoryKind::Scripts => {
                name.starts_with("job-") && (name.ends_with(".ps1") || name.ends_with(".cmd"))
            }
        } || (name.starts_with(".sc-install-") && name.ends_with(".tmp"));
        if !owned || !metadata.is_file() {
            bail!(
                "{} contains unrelated data; use a dedicated install/state directory",
                path.display()
            );
        }
    }
    Ok(())
}

fn command_line_arguments(line: &OsStr) -> anyhow::Result<Vec<OsString>> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;
    let wide: Vec<u16> = line.encode_wide().chain([0]).collect();
    let mut count = 0;
    // SAFETY: the input is terminated; Windows returns `count` terminated
    // arguments in one allocation, which is copied and freed before return.
    unsafe {
        let arguments = CommandLineToArgvW(wide.as_ptr(), &mut count);
        if arguments.is_null() {
            return Err(io::Error::last_os_error())
                .context("cannot parse the installed service command");
        }
        let result = std::slice::from_raw_parts(arguments, count as usize)
            .iter()
            .map(|argument| {
                let mut length = 0;
                while *argument.add(length) != 0 {
                    length += 1;
                }
                OsString::from_wide(std::slice::from_raw_parts(*argument, length))
            })
            .collect();
        LocalFree(arguments.cast());
        Ok(result)
    }
}

/// Refuse to stop a service belonging to different caller-supplied paths.
fn installed_info(
    service: &windows_service::service::Service,
    install_dir: &Path,
    state_dir: &Path,
) -> anyhow::Result<ServiceInfo> {
    let config = service.query_config().context("cannot inspect the installed service")?;
    let arguments = command_line_arguments(config.executable_path.as_os_str())?;
    if arguments.len() != 4 || arguments[1] != "service" || arguments[2] != "--state-dir" {
        bail!("the installed service has an unexpected command; inspect it before reinstalling or uninstalling");
    }
    let exe = PathBuf::from(&arguments[0]);
    let registered_install = installation_directory(
        exe.parent().context("the service executable has no parent directory")?,
    )?;
    let registered_state = installation_directory(Path::new(&arguments[3]))?;
    if comparison_path(&registered_install) != comparison_path(install_dir)
        || comparison_path(&registered_state) != comparison_path(state_dir)
        || !exe
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(EXE_NAME))
    {
        bail!("the installed service uses different directories; pass its original --install-dir and --state-dir");
    }
    Ok(ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: config.display_name,
        service_type: config.service_type,
        start_type: config.start_type,
        error_control: config.error_control,
        executable_path: exe,
        launch_arguments: arguments[1..].to_vec(),
        dependencies: config.dependencies,
        account_name: config.account_name,
        account_password: None,
    })
}

/// Create a directory with its final owner and protected ACL atomically. An
/// empty directory must never inherit permissions allowing a junction swap.
fn create_owned_directory(path: &Path, sddl: &str) -> anyhow::Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let descriptor_text: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the two strings are terminated and the descriptor remains live
    // throughout CreateDirectoryW. The allocation is freed on every outcome.
    let result = unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot build the directory access rules");
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let created = CreateDirectoryW(wide.as_ptr(), &attributes);
        let error = io::Error::last_os_error();
        LocalFree(descriptor);
        if created != 0 {
            Ok(())
        } else {
            Err(error)
        }
    };
    if let Err(error) = result {
        if error.raw_os_error() != Some(183) {
            return Err(error)
                .with_context(|| format!("cannot create protected directory {}", path.display()));
        }
    }
    let handle = open_directory(path)?;
    if comparison_path(&directory_path(&handle)?) != comparison_path(path) {
        bail!("{} was redirected outside its inspected parent", path.display());
    }
    handle_security(&handle, DirectorySecurity::Owned).with_context(|| format!("existing application directory {} is not administrator-controlled; choose a new dedicated directory", path.display()))?;
    // Also remove append/create permissions on a preexisting application
    // leaf before inspecting its contents again.
    set_access(path, sddl)?;
    Ok(handle)
}

fn secure_tree(path: &Path, readable: bool, state_root: bool) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    reject_reparse_point(path, &metadata)?;
    set_access(path, if readable { READABLE_ACCESS } else { PRIVATE_ACCESS })?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let scripts =
                state_root && entry.file_name().to_string_lossy().eq_ignore_ascii_case(SCRIPTS_DIR);
            secure_tree(&entry.path(), readable || scripts, false)?;
        }
    }
    Ok(())
}

fn set_access(path: &Path, sddl: &str) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let descriptor: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut security = std::ptr::null_mut();
    // SAFETY: both inputs are terminated. Conversion allocates a valid
    // descriptor; it remains alive through SetFileSecurityW and is freed.
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot build the installation access rules");
        }
        let result = SetFileSecurityW(
            path_wide.as_ptr(),
            DACL_SECURITY_INFORMATION
                | OWNER_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            security,
        );
        let error = io::Error::last_os_error();
        LocalFree(security);
        if result == 0 {
            return Err(error).with_context(|| format!("cannot secure {}", path.display()));
        }
    }
    Ok(())
}

/// An exclusive sibling file: partial copies never touch the installed file.
struct StagedFile {
    path: PathBuf,
    keep: bool,
}

impl StagedFile {
    fn create(target: &Path) -> anyhow::Result<(Self, std::fs::File)> {
        let path =
            target.with_file_name(format!(".sc-install-{}.tmp", uuid::Uuid::new_v4().simple()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        Ok((Self { path, keep: false }, file))
    }

    fn copy(source: &Path, target: &Path) -> anyhow::Result<Self> {
        let mut source_file = std::fs::File::open(source)
            .with_context(|| format!("cannot read {}", source.display()))?;
        let (staged, mut destination) = Self::create(target)?;
        std::io::copy(&mut source_file, &mut destination)
            .context("cannot stage the installation file")?;
        destination.sync_all().context("cannot flush the installation file")?;
        Ok(staged)
    }

    fn write(target: &Path, bytes: &[u8]) -> anyhow::Result<Self> {
        use std::io::Write;
        let (staged, mut destination) = Self::create(target)?;
        destination.write_all(bytes).context("cannot stage the configuration")?;
        destination.sync_all().context("cannot flush the configuration")?;
        Ok(staged)
    }

    fn replace(&mut self, target: &Path) -> anyhow::Result<()> {
        let deadline = Instant::now() + REPLACE_TIMEOUT;
        loop {
            match std::fs::rename(&self.path, target) {
                Ok(()) => return Ok(()),
                Err(error)
                    if Instant::now() < deadline
                        && matches!(error.raw_os_error(), Some(5 | 32 | 33)) =>
                {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot replace {}", target.display()))
                }
            }
        }
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

struct FileBackup {
    target: PathBuf,
    previous: Option<StagedFile>,
}

impl FileBackup {
    fn capture(target: &Path) -> anyhow::Result<Self> {
        let previous =
            if target.try_exists()? { Some(StagedFile::copy(target, target)?) } else { None };
        Ok(Self { target: target.to_owned(), previous })
    }

    fn restore(&mut self) -> anyhow::Result<()> {
        match &mut self.previous {
            Some(previous) => previous.replace(&self.target),
            None => match std::fs::remove_file(&self.target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("cannot remove {}", self.target.display()))
                }
            },
        }
    }

    fn preserve(&mut self) {
        if let Some(previous) = &mut self.previous {
            previous.keep = true;
            eprintln!("previous {} retained at {}", self.target.display(), previous.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_access(sddl: &str, policy: DirectorySecurity) -> anyhow::Result<()> {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor = std::ptr::null_mut();
        // Parse synthetic descriptors only; these tests never modify ACLs.
        unsafe {
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                ),
                0
            );
            let result = validate_directory_security(descriptor, policy);
            LocalFree(descriptor);
            result
        }
    }

    #[test]
    fn trusted_parent_owners_and_inherit_only_grants_are_allowed() {
        for owner in ["SY", "BA", "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"]
        {
            let descriptor = format!("O:{owner}D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;GRGX;;;BU)(A;OICIIO;GA;;;CO)(A;OICIIO;GA;;;AU)");
            check_access(&descriptor, DirectorySecurity::Parent).unwrap();
        }
        // Users can create new children at normal machine roots without
        // being able to replace a protected existing application directory.
        check_access("O:SYD:P(A;;0x4;;;AU)(A;;GRGX;;;BU)(A;;FA;;;SY)", DirectorySecurity::Parent)
            .unwrap();
    }

    #[test]
    fn user_owned_or_replaceable_parents_are_rejected() {
        assert!(check_access("O:S-1-5-21-1-2-3-1001D:P(A;;FA;;;SY)", DirectorySecurity::Parent)
            .is_err());
        for rights in ["GA", "GW", "0x2", "0x100", "0x40", "0x10000", "0x40000", "0x80000"] {
            assert!(
                check_access(&format!("O:SYD:P(A;;{rights};;;BU)"), DirectorySecurity::Parent)
                    .is_err(),
                "{rights}"
            );
        }
        assert!(check_access("O:SYD:NO_ACCESS_CONTROL", DirectorySecurity::Parent).is_err());
    }

    #[test]
    fn an_anchored_parent_may_grant_file_creation_but_not_child_deletion() {
        // Normal ProgramData permits Users Write (0x116), but a protected
        // child such as Microsoft prevents turning that parent into a junction.
        let program_data = "O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x116;;;BU)(A;;GRGX;;;BU)";
        assert!(check_access(program_data, DirectorySecurity::Parent).is_err());
        check_access(program_data, DirectorySecurity::AnchoredParent).unwrap();
        for rights in ["GA", "0x40", "0x10000", "0x40000", "0x80000"] {
            assert!(
                check_access(
                    &format!("O:SYD:P(A;;{rights};;;BU)"),
                    DirectorySecurity::AnchoredParent
                )
                .is_err(),
                "{rights}"
            );
        }
    }

    #[test]
    fn owned_directories_cannot_admit_user_created_subdirectories() {
        let append_only = "O:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x4;;;BU)";
        check_access(append_only, DirectorySecurity::Parent).unwrap();
        assert!(check_access(append_only, DirectorySecurity::Owned).is_err());
        check_access(PRIVATE_ACCESS, DirectorySecurity::Owned).unwrap();
        check_access(READABLE_ACCESS, DirectorySecurity::Owned).unwrap();
    }

    #[test]
    fn native_default_parent_permissions_support_installation_without_changes() {
        // Read-only native validation includes ProgramData's protected
        // anchor. It neither creates directories nor changes permissions.
        let install =
            installation_directory(&default_install_dir(std::env::var_os("ProgramFiles"))).unwrap();
        let state =
            installation_directory(&default_state_dir(std::env::var_os("ProgramData"))).unwrap();
        let _install_handles = secure_ancestors(&install).unwrap();
        let _state_handles = secure_ancestors(&state).unwrap();
    }

    #[test]
    fn missing_parent_directories_are_not_created_with_unchecked_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("missing");
        let error = secure_ancestors(&parent.join("install")).unwrap_err();
        assert!(error.to_string().contains("parent directory must already exist"));
        assert!(!parent.exists());
    }

    #[test]
    fn hard_links_cannot_redirect_access_changes_outside_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        let install = dir.path().join("install");
        std::fs::create_dir(&install).unwrap();
        std::fs::write(&outside, "untouched").unwrap();
        std::fs::hard_link(&outside, install.join(EXE_NAME)).unwrap();
        let error = validate_contents(&install, DirectoryKind::Install).unwrap_err();
        assert!(error.to_string().contains("multiple hard links"));
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "untouched");
    }

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
        assert_eq!(
            default_state_dir(Some("".into())),
            Path::new(r"C:\ProgramData\Surface Coordinator")
        );
    }

    #[test]
    fn service_paths_accept_spaces_unicode_and_shell_metacharacters() {
        validate_service_paths(
            Path::new(r"C:\Program Files\Surface & (test)!^ café"),
            Path::new(r"C:\ProgramData\Surface & (test)!^ café"),
        )
        .unwrap();
    }

    #[test]
    fn service_paths_reject_percent_before_installation_in_either_directory() {
        let ordinary = Path::new(r"C:\Surface Coordinator");
        for value in [r"C:\literal %PATH%", r"C:\%SC_UNDEFINED_VARIABLE%", r"C:\100%"] {
            let path = Path::new(value);
            for (kind, install, state) in [("install", path, ordinary), ("state", ordinary, path)] {
                let error = validate_service_paths(install, state).unwrap_err().to_string();
                assert!(error.contains(&format!("the {kind} directory {value}")), "{error}");
                assert!(error.contains("choose a directory without '%'"), "{error}");
            }
        }
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
        let parsed = crate::platform::windows::command_line_to_argv(
            &service_command_line(exe, state_dir).unwrap(),
        );
        assert_eq!(
            parsed,
            [r"C:\Fake Install\client.exe", "service", "--state-dir", r"D:\fake state dir\"]
        );
    }

    #[test]
    fn access_descriptors_have_exact_grants_and_administrator_ownership() {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, GetSecurityDescriptorDacl, SE_DACL_PROTECTED,
        };
        for (sddl, entries) in [(PRIVATE_ACCESS, 2), (READABLE_ACCESS, 3)] {
            let wide: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
            let mut descriptor = std::ptr::null_mut();
            // Inspect Windows' parsed ACL without changing machine permissions.
            unsafe {
                assert_ne!(
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        wide.as_ptr(),
                        SDDL_REVISION_1,
                        &mut descriptor,
                        std::ptr::null_mut()
                    ),
                    0
                );
                let mut present = 0;
                let mut defaulted = 0;
                let mut acl = std::ptr::null_mut();
                assert_ne!(
                    GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted),
                    0
                );
                assert_ne!(present, 0);
                assert!(!acl.is_null());
                assert_eq!((*acl).AceCount, entries);
                let mut control = 0;
                let mut revision = 0;
                assert_ne!(
                    GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                    0
                );
                assert_ne!(control & SE_DACL_PROTECTED, 0);
                LocalFree(descriptor);
            }
        }
    }

    #[test]
    fn staging_and_rollback_preserve_the_existing_installation() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, "working version").unwrap();
        assert!(StagedFile::copy(&dir.path().join("missing.exe"), &exe).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "working version");
        {
            let mut backup = FileBackup::capture(&exe).unwrap();
            let mut staged = StagedFile::write(&exe, b"replacement").unwrap();
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), "working version");
            staged.replace(&exe).unwrap();
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), "replacement");
            backup.restore().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "working version");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_fresh_install_removes_only_its_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(EXE_NAME);
        let mut backup = FileBackup::capture(&exe).unwrap();
        StagedFile::write(&exe, b"new installation").unwrap().replace(&exe).unwrap();
        backup.restore().unwrap();
        assert!(!exe.exists());
    }

    #[test]
    fn dangerous_or_shared_directories_are_rejected_before_changes() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let state = dir.path().join("state");
        assert!(installation_paths(&install, &state).is_ok());
        assert!(installation_paths(&install, &install).is_err());
        assert!(installation_paths(&install, &install.join("state")).is_err());
        assert!(installation_paths(Path::new(r"C:\"), &state).is_err());
        std::fs::create_dir(&install).unwrap();
        std::fs::write(install.join("unrelated.txt"), "keep this").unwrap();
        assert!(validate_contents(&install, DirectoryKind::Install).is_err());
        assert_eq!(std::fs::read_to_string(install.join("unrelated.txt")).unwrap(), "keep this");
    }

    #[test]
    fn registered_service_arguments_are_parsed_without_losing_quoted_paths() {
        let line = service_command_line(
            Path::new(r"C:\Fake Install\client.exe"),
            Path::new(r"D:\fake state dir\"),
        )
        .unwrap();
        assert_eq!(
            strings(&command_line_arguments(OsStr::new(&line)).unwrap()),
            [r"C:\Fake Install\client.exe", "service", "--state-dir", r"D:\fake state dir\"]
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
        assert!(
            matches!(actions.reset_period, ServiceFailureResetPeriod::After(d) if d.as_secs() == 86_400)
        );
    }

    #[test]
    fn elevation_can_be_asked() {
        // Whatever this test runs as; the call itself must work.
        let elevated = is_elevated().unwrap();
        eprintln!("elevated: {elevated}");
    }
}
