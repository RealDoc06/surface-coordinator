//! Surface Coordinator Manager: the desktop shell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::time::Duration;

use manager::embedded::{Embedded, Options};
use manager::settings::Settings;
use manager::update::{self, DownloadProgress, UpdateStatus};
use manager::{navigation_allowed, remote};
use serde::Serialize;
use tauri::ipc::Channel;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{AppHandle, Manager, RunEvent, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use tauri_plugin_updater::{Update, UpdaterExt};
use url::Url;

const MAIN_WINDOW: &str = "main";

/// The updater public key from `updater.pub`, empty when the build has none
/// (see build.rs).
const UPDATER_PUBKEY: &str = env!("SC_UPDATER_PUBKEY");

/// Without internet (hotspot mode) the check must give up rather than leave
/// the footer saying "Checking…" for minutes.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(20);

struct AppState {
    data_dir: PathBuf,
    embedded: tokio::sync::Mutex<Option<Embedded>>,
    /// Origins the window may navigate to besides the launcher.
    allowed_origins: Arc<Mutex<Vec<String>>>,
    /// Address of the bundled launcher page, to return to it.
    launcher: Mutex<Option<Url>>,
    /// The update found by the last check, installed when the operator
    /// confirms.
    pending_update: tokio::sync::Mutex<Option<Update>>,
}

impl AppState {
    fn allow(&self, origin: String) {
        let mut allowed = self.allowed_origins.lock().unwrap_or_else(|e| e.into_inner());
        if !allowed.contains(&origin) {
            allowed.push(origin);
        }
    }
}

/// What the launcher page shows.
#[derive(Serialize)]
struct Status {
    version: &'static str,
    local_running: bool,
    /// Port clients connect to while the local coordinator runs.
    local_client_port: Option<u16>,
    remote_url: Option<String>,
}

#[tauri::command]
async fn status(state: State<'_, AppState>) -> Result<Status, String> {
    let embedded = state.embedded.lock().await;
    Ok(Status {
        version: env!("CARGO_PKG_VERSION"),
        local_running: embedded.is_some(),
        local_client_port: embedded.as_ref().map(|e| e.client_addr().port()),
        remote_url: Settings::load(&state.data_dir).remote_url,
    })
}

fn navigate(window: &WebviewWindow, address: &str) -> Result<(), String> {
    let url = Url::parse(address).map_err(|error| error.to_string())?;
    window.navigate(url).map_err(|error| error.to_string())
}

/// Starts the local coordinator if it is not running and shows its UI.
#[tauri::command]
async fn open_local(window: WebviewWindow, state: State<'_, AppState>) -> Result<(), String> {
    let mut embedded = state.embedded.lock().await;
    if embedded.is_none() {
        let data_dir = state.data_dir.join("coordinator");
        let started = Embedded::start(&data_dir, Options::default()).await.map_err(|error| {
            format!(
                "The local coordinator could not be started: {error}. \
                 Is another copy of the manager running?"
            )
        })?;
        tracing::info!(clients = %started.client_addr(), ui = %started.origin(), "local coordinator started");
        *embedded = Some(started);
    }
    let running = embedded.as_ref().expect("just started");
    state.allow(running.origin());
    navigate(&window, &running.entry_url())
}

/// Stops the local coordinator, and with it the hotspot, and closes its
/// database.
async fn stop_embedded(state: &AppState) {
    if let Some(running) = state.embedded.lock().await.take() {
        running.stop().await;
        tracing::info!("local coordinator stopped");
    }
}

/// Stops the local coordinator; devices connected to it go offline.
#[tauri::command]
async fn stop_local(state: State<'_, AppState>) -> Result<(), String> {
    stop_embedded(&state).await;
    Ok(())
}

/// Checks that there is a coordinator at `address` and shows its UI.
#[tauri::command]
async fn open_remote(
    window: WebviewWindow,
    state: State<'_, AppState>,
    address: String,
) -> Result<(), String> {
    let origin = remote::normalise(&address).map_err(|error| error.to_string())?;
    let health = remote::check(&origin).await.map_err(|error| error.to_string())?;
    tracing::info!(%origin, version = %health.version, "remote coordinator reached");

    let mut settings = Settings::load(&state.data_dir);
    settings.remote_url = Some(origin.clone());
    if let Err(error) = settings.save(&state.data_dir) {
        tracing::warn!(%error, "could not save settings");
    }
    state.allow(origin.clone());
    navigate(&window, &origin)
}

/// Asks the repository's latest release whether there is a newer manager.
///
/// Never fails: without internet the answer is "nothing available" with a
/// quiet reason, so the launcher keeps working in hotspot mode.
#[tauri::command]
async fn check_update(app: AppHandle, state: State<'_, AppState>) -> Result<UpdateStatus, String> {
    let current = env!("CARGO_PKG_VERSION");
    if update::configured_key(UPDATER_PUBKEY).is_none() {
        return Ok(UpdateStatus::not_configured(current));
    }
    // The plugin was registered with the key in `main`; the endpoint comes
    // from `plugins.updater` in tauri.conf.json.
    let checked = match app.updater_builder().timeout(UPDATE_CHECK_TIMEOUT).build() {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    let mut pending = state.pending_update.lock().await;
    Ok(match checked {
        Ok(Some(found)) => {
            tracing::info!(version = %found.version, "manager update available");
            let status = UpdateStatus::available(current, &found.version, found.body.as_deref());
            *pending = Some(found);
            status
        }
        Ok(None) => {
            *pending = None;
            UpdateStatus::up_to_date(current)
        }
        Err(error) => {
            // Expected in hotspot mode; not worth more than an info line.
            tracing::info!(%error, "update check failed");
            UpdateStatus::failed(current, &error)
        }
    })
}

/// Downloads the update found by the last check, verifies its signature,
/// stops the local coordinator, installs the update and restarts.
///
/// On Windows the installer takes over: the plugin starts it and ends this
/// process, and the installer starts the new version when it is done.
#[tauri::command]
async fn install_update(
    app: AppHandle,
    state: State<'_, AppState>,
    on_progress: Channel<String>,
) -> Result<(), String> {
    // Taken out so that a second click cannot start a second install.
    let Some(found) = state.pending_update.lock().await.take() else {
        return Err("There is no update to install. Check for updates first.".to_owned());
    };
    let report = |text: &str| {
        let _ = on_progress.send(text.to_owned());
    };

    // Download and verify before stopping anything, so a failed download
    // leaves the local coordinator running.
    report("Downloading the update…");
    let mut progress = DownloadProgress::default();
    let downloaded = found
        .download(
            |chunk, total| {
                if let Some(text) = progress.advance(chunk, total) {
                    report(&text);
                }
            },
            || {},
        )
        .await;
    let bytes = match downloaded {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "update download failed");
            *state.pending_update.lock().await = Some(found);
            return Err(update::install_error_message(&error));
        }
    };

    // Same path as "Stop" and quitting: the hotspot is turned off and the
    // database closed before the installer replaces the files.
    if state.embedded.lock().await.is_some() {
        report("Stopping the local coordinator…");
        stop_embedded(&state).await;
    }
    report("Installing the update…");
    tracing::info!(version = %found.version, "installing manager update");
    if let Err(error) = found.install(bytes) {
        tracing::error!(%error, "update install failed");
        *state.pending_update.lock().await = Some(found);
        return Err(update::install_error_message(&error));
    }
    report("Restarting…");
    app.restart()
}

fn show_launcher(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let launcher = state.launcher.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let (Some(window), Some(launcher)) = (app.get_webview_window(MAIN_WINDOW), launcher) {
        if let Err(error) = window.navigate(launcher) {
            tracing::error!(%error, "could not return to the launcher");
        }
    }
}

fn main() {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let mut builder = tauri::Builder::default();
    // Only builds with a public key get the updater at all. No capability
    // grants its JavaScript commands: the launcher goes through
    // `check_update` and `install_update`, coordinator pages get nothing.
    if let Some(key) = update::configured_key(UPDATER_PUBKEY) {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().pubkey(key).build());
    }

    let app = builder
        .invoke_handler(tauri::generate_handler![
            status,
            open_local,
            stop_local,
            open_remote,
            check_update,
            install_update
        ])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let allowed_origins = Arc::new(Mutex::new(Vec::new()));
            app.manage(AppState {
                data_dir,
                embedded: tokio::sync::Mutex::new(None),
                allowed_origins: allowed_origins.clone(),
                launcher: Mutex::new(None),
                pending_update: tokio::sync::Mutex::new(None),
            });

            let window = WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
                .title("Surface Coordinator")
                .inner_size(1280.0, 860.0)
                .min_inner_size(1000.0, 640.0)
                .on_navigation(move |url| {
                    let allowed = allowed_origins.lock().unwrap_or_else(|e| e.into_inner());
                    let permitted = navigation_allowed(url, &allowed);
                    if !permitted {
                        tracing::warn!(%url, "navigation blocked");
                    }
                    permitted
                })
                .build()?;
            *app.state::<AppState>().launcher.lock().unwrap_or_else(|e| e.into_inner()) =
                window.url().ok();

            let change = MenuItemBuilder::with_id("change-coordinator", "Change coordinator…").build(app)?;
            let coordinator = SubmenuBuilder::new(app, "Coordinator").item(&change).separator().quit().build()?;
            let menu = MenuBuilder::new(app).item(&coordinator).build()?;
            app.set_menu(menu)?;
            app.on_menu_event(|app, event| {
                if event.id().as_ref() == "change-coordinator" {
                    show_launcher(app);
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("the manager failed to start");

    app.run(|app, event| {
        if let RunEvent::Exit = event {
            // Stop the local coordinator cleanly so devices are marked offline
            // and the database is closed.
            let state = app.state::<AppState>();
            tauri::async_runtime::block_on(stop_embedded(&state));
        }
    });
}
