//! Restart, shut down and sleep.
//!
//! Tests only ever look at what would be run ([`power_command`]); nothing
//! here that acts is called by a test.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr::null_mut;

use protocol::PowerAction;
use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Power::SetSuspendState;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::session::CREATE_NO_WINDOW;
use super::system32;
use crate::platform::PlatformError;

/// Seconds `shutdown.exe` waits. Enough for the WebSocket to close, and it
/// implies `/f`: applications are closed without asking.
const SHUTDOWN_DELAY_SECS: &str = "5";
/// Shown to the logged-in user during the delay and kept in the event log.
const SHUTDOWN_COMMENT: &str = "Surface Coordinator";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PowerCommand {
    Run { program: PathBuf, arguments: Vec<&'static str> },
    /// `SetSuspendState(FALSE, FALSE, FALSE)`: sleep, not hibernate, and
    /// wake events stay enabled. Called directly rather than through
    /// `rundll32 powrprof.dll,SetSuspendState`, which passes its arguments
    /// wrongly and hibernates where hibernation is enabled.
    Suspend,
}

/// What carrying out `action` runs.
pub(crate) fn power_command(action: PowerAction) -> PowerCommand {
    let shutdown = |mode: &'static str| PowerCommand::Run {
        program: system32().join("shutdown.exe"),
        arguments: vec![mode, "/t", SHUTDOWN_DELAY_SECS, "/c", SHUTDOWN_COMMENT],
    };
    match action {
        PowerAction::Restart => shutdown("/r"),
        PowerAction::Shutdown => shutdown("/s"),
        PowerAction::Sleep => PowerCommand::Suspend,
    }
}

/// Whether `action` can be attempted here, before success is reported.
pub(crate) fn check(action: PowerAction) -> Result<(), PlatformError> {
    match power_command(action) {
        PowerCommand::Run { program, .. } if !program.is_file() => {
            Err(PlatformError::Failed(format!("{} is missing", program.display())))
        }
        PowerCommand::Run { .. } | PowerCommand::Suspend => Ok(()),
    }
}

/// Carries out `action`. For sleep this returns once the device has woken
/// up again.
pub(crate) async fn carry_out(action: PowerAction) -> Result<(), PlatformError> {
    match power_command(action) {
        PowerCommand::Run { program, arguments } => {
            let output = tokio::process::Command::new(&program)
                .args(&arguments)
                .creation_flags(CREATE_NO_WINDOW)
                .output()
                .await
                .map_err(|error| PlatformError::Failed(format!("cannot run {}: {error}", program.display())))?;
            if output.status.success() {
                return Ok(());
            }
            let text = String::from_utf8_lossy(if output.stderr.is_empty() { &output.stdout } else { &output.stderr });
            Err(PlatformError::Failed(format!(
                "{} failed ({}): {}",
                program.display(),
                output.status,
                text.trim()
            )))
        }
        PowerCommand::Suspend => tokio::task::spawn_blocking(suspend)
            .await
            .unwrap_or_else(|error| Err(PlatformError::Failed(format!("suspending failed: {error}")))),
    }
}

fn suspend() -> Result<(), PlatformError> {
    enable_shutdown_privilege()?;
    // SAFETY: plain call; blocks until the device resumes.
    if unsafe { SetSuspendState(0, 0, 0) } == 0 {
        return Err(PlatformError::Failed(format!(
            "SetSuspendState failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// `SetSuspendState` needs `SeShutdownPrivilege` enabled, which even
/// LocalSystem only holds disabled. (`shutdown.exe` enables it itself.)
fn enable_shutdown_privilege() -> Result<(), PlatformError> {
    let failed = |what: &str| {
        PlatformError::Failed(format!("{what} failed: {}", std::io::Error::last_os_error()))
    };
    // SAFETY: plain calls with valid out pointers; the token handle is
    // owned by the `OwnedHandle`.
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) == 0 {
            return Err(failed("OpenProcessToken"));
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut luid = LUID { LowPart: 0, HighPart: 0 };
        if LookupPrivilegeValueW(std::ptr::null(), SE_SHUTDOWN_NAME, &mut luid) == 0 {
            return Err(failed("LookupPrivilegeValueW"));
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let handle = token.as_raw_handle() as HANDLE;
        if AdjustTokenPrivileges(handle, 0, &privileges, 0, null_mut(), null_mut()) == 0 {
            return Err(failed("AdjustTokenPrivileges"));
        }
        // Succeeds even when the privilege is not held at all.
        if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
            return Err(PlatformError::Failed("the client does not hold the shutdown privilege".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_and_shutdown_run_shutdown_exe_with_a_delay() {
        let shutdown_exe = system32().join("shutdown.exe");
        assert_eq!(
            power_command(PowerAction::Restart),
            PowerCommand::Run {
                program: shutdown_exe.clone(),
                arguments: vec!["/r", "/t", "5", "/c", "Surface Coordinator"],
            }
        );
        assert_eq!(
            power_command(PowerAction::Shutdown),
            PowerCommand::Run { program: shutdown_exe, arguments: vec!["/s", "/t", "5", "/c", "Surface Coordinator"] }
        );
    }

    #[test]
    fn sleep_suspends_rather_than_hibernates() {
        assert_eq!(power_command(PowerAction::Sleep), PowerCommand::Suspend);
    }

    #[test]
    fn every_action_can_be_attempted_on_this_machine() {
        // Only checks that the programs are there; nothing is run.
        for action in [PowerAction::Restart, PowerAction::Shutdown, PowerAction::Sleep] {
            assert_eq!(check(action), Ok(()), "{action:?}");
        }
    }
}
