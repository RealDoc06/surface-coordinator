//! `client.exe service`: the client under the Windows service control
//! manager, as `install` registers it.
//!
//! Configuration comes from `config.json` in the state directory only, and
//! the log goes to the rotating `client.log` there, as a service has no
//! console. Stop and Shutdown stop the client. When `client.update` asks
//! for a restart, the service stops with an error code, which the
//! restart-on-failure actions set by `install` turn into a start on the new
//! executable.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;
use tokio_util::sync::CancellationToken;
use windows_service::service::{
    ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::{define_windows_service, service_dispatcher};

use crate::config::{self, Overrides};
use crate::install::{SCRIPTS_DIR, SERVICE_NAME};
use crate::logfile::{RotatingFile, DEFAULT_CAP, LOG_FILE};
use crate::platform::windows::WindowsPlatform;
use crate::{Client, ClientConfig, RESTART_EXIT_CODE};

/// Exit code of a service that could not run (no configuration, say).
const FAILURE_EXIT_CODE: i32 = 1;
/// Error code of `StartServiceCtrlDispatcher` outside the service manager.
const ERROR_FAILED_SERVICE_CONTROLLER_CONNECT: i32 = 1063;

/// What `service_main` needs from the command line; the service manager
/// passes it nothing.
static OPTIONS: OnceLock<(PathBuf, bool)> = OnceLock::new();
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

define_windows_service!(ffi_service_main, service_main);

/// Runs the service until it stops and returns the exit code for the
/// process. Only works when started by the service manager.
pub fn run(state_dir: PathBuf, verbose: bool) -> anyhow::Result<i32> {
    let _ = OPTIONS.set((state_dir, verbose));
    match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
        Ok(()) => Ok(EXIT_CODE.load(Ordering::SeqCst)),
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) =>
        {
            anyhow::bail!(
                "`client.exe service` is started by the service manager (see `client.exe install`); \
                 to run the client in this console use `client.exe run`"
            )
        }
        Err(error) => Err(error).context("cannot connect to the service manager"),
    }
}

fn service_main(_arguments: Vec<OsString>) {
    let Some((state_dir, verbose)) = OPTIONS.get() else { return };
    init_logging(state_dir, *verbose);
    if let Err(error) = serve(state_dir) {
        tracing::error!("the service failed: {error:#}");
    }
}

fn init_logging(state_dir: &Path, verbose: bool) {
    let level = if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO };
    let _ = std::fs::create_dir_all(state_dir);
    // Without a log file the service still runs; there is nowhere to say so.
    if let Ok(file) = RotatingFile::open(state_dir.join(LOG_FILE), DEFAULT_CAP) {
        let _ = tracing_subscriber::fmt().with_max_level(level).with_ansi(false).with_writer(file).try_init();
    }
}

fn serve(state_dir: &Path) -> anyhow::Result<()> {
    let shutdown = CancellationToken::new();
    let on_control = shutdown.clone();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            tracing::info!(?control, "stop requested by the service manager");
            on_control.cancel();
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })
    .context("cannot register with the service manager")?;
    let report = |state, controls_accepted, exit_code, wait_hint| {
        let status_update = ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code,
            checkpoint: 0,
            wait_hint,
            process_id: None,
        };
        if let Err(error) = status.set_service_status(status_update) {
            tracing::error!(%error, ?state, "cannot report the service state");
        }
    };

    report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN, ServiceExitCode::NO_ERROR, Duration::ZERO);
    let outcome = run_client(state_dir, shutdown);
    report(ServiceState::StopPending, ServiceControlAccept::empty(), ServiceExitCode::NO_ERROR, Duration::from_secs(10));
    let code = match &outcome {
        Ok(false) => 0,
        Ok(true) => {
            tracing::info!("stopping so that the service manager starts the client again");
            RESTART_EXIT_CODE
        }
        Err(_) => FAILURE_EXIT_CODE,
    };
    EXIT_CODE.store(code, Ordering::SeqCst);
    // Not zero is a failure to the service manager, which restarts the
    // service (`install` enables that for clean exits too).
    let exit_code = match code {
        0 => ServiceExitCode::NO_ERROR,
        code => ServiceExitCode::ServiceSpecific(code as u32),
    };
    report(ServiceState::Stopped, ServiceControlAccept::empty(), exit_code, Duration::ZERO);
    outcome.map(|_| ())
}

/// Runs the client until it stops; returns whether it asked to be restarted.
fn run_client(state_dir: &Path, shutdown: CancellationToken) -> anyhow::Result<bool> {
    let settings = config::load(state_dir, Overrides::default())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let platform =
        Arc::new(WindowsPlatform::new(state_dir.join(SCRIPTS_DIR)).with_restart(shutdown.clone()));
    let config = ClientConfig::from_settings(settings, state_dir);
    tracing::info!(device_id = %config.device_id, version = %config.client_version, "client service starting");
    runtime.block_on(async {
        let client = Client::new(config, platform.clone()).context("cannot open the client state")?;
        client.run(shutdown).await;
        anyhow::Ok(())
    })?;
    // A script still being stopped must not hold the service up for long.
    runtime.shutdown_timeout(Duration::from_secs(5));
    tracing::info!("client service stopped");
    Ok(platform.restart_requested())
}
