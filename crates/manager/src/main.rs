//! Surface Coordinator Manager: the desktop shell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use manager::embedded::{Embedded, Options};
use manager::settings::Settings;
use manager::sync::{self, SyncError};
use manager::update::{self, DownloadProgress, UpdateStatus};
use manager::{is_launcher_page, navigation_allowed, remote};
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

/// Installers are several megabytes; allow slower connections without
/// letting a stalled download hold the launcher forever.
const UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How long opening the local coordinator waits for the enrollment keys to
/// be copied, so they are usually there when its UI loads. Offline the copy
/// can take longer; it then finishes in the background.
const KEY_SYNC_WAIT: Duration = Duration::from_secs(3);

struct AppState {
    data_dir: PathBuf,
    embedded: tokio::sync::Mutex<Option<Embedded>>,
    /// Origins the window may navigate to besides the launcher.
    allowed_origins: Arc<Mutex<Vec<String>>>,
    /// Address of the bundled launcher page, to return to it. Remembered
    /// when the window first navigates to it: the window's own address is
    /// still the blank page while the app is being set up.
    launcher: Arc<Mutex<Option<Url>>>,
    /// The update found by the last check, installed when the operator
    /// confirms.
    pending_update: tokio::sync::Mutex<Option<Update>>,
    /// Coordinator changes and installation must not overlap, including
    /// commands sent just before the launcher disables its controls.
    operation: tokio::sync::Mutex<()>,
    /// How the last copy of enrollment keys from the online coordinator went.
    key_sync: Mutex<KeySync>,
}

impl AppState {
    fn allow(&self, origin: String) {
        let mut allowed = self
            .allowed_origins
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !allowed.contains(&origin) {
            allowed.push(origin);
        }
    }
}

/// What the launcher page shows.
#[derive(Serialize)]
struct Status {
    version: String,
    local_running: bool,
    /// Port clients connect to while the local coordinator runs.
    local_client_port: Option<u16>,
    remote_url: Option<String>,
    key_sync: KeySync,
}

/// The last copy of enrollment keys from the online coordinator into the
/// local one. `at` stays `None` until there is an online coordinator to copy
/// from.
#[derive(Debug, Clone, Default, Serialize)]
struct KeySync {
    /// Unix milliseconds.
    at: Option<u64>,
    imported: usize,
    error: Option<String>,
}

#[tauri::command]
async fn status(app: AppHandle, state: State<'_, AppState>) -> Result<Status, String> {
    let embedded = state.embedded.lock().await;
    Ok(Status {
        version: app.package_info().version.to_string(),
        local_running: embedded.is_some(),
        local_client_port: embedded.as_ref().map(|e| e.client_addr().port()),
        remote_url: Settings::load(&state.data_dir).remote_url,
        key_sync: state
            .key_sync
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
    })
}

fn navigate(window: &WebviewWindow, address: &str) -> Result<(), String> {
    let url = Url::parse(address).map_err(|error| error.to_string())?;
    window.navigate(url).map_err(|error| error.to_string())
}

/// The `sc_session` cookie the window holds for `origin`, if the operator
/// signed in there. Read through the webview's own cookie store, which
/// includes HttpOnly cookies that page scripts cannot see.
async fn session_cookie(window: &WebviewWindow, origin: &str) -> Option<String> {
    let url = Url::parse(origin).ok()?;
    let window = window.clone();
    // The call blocks until the event loop has asked WebView2, so it must
    // not run on the main thread nor hold up an async worker.
    let cookies = tauri::async_runtime::spawn_blocking(move || window.cookies_for_url(url))
        .await
        .ok()?;
    match cookies {
        Ok(cookies) => cookies
            .into_iter()
            .find(|cookie| cookie.name() == sync::SESSION_COOKIE)
            .map(|cookie| cookie.value().to_owned()),
        Err(error) => {
            tracing::warn!(%error, "could not read the webview's cookies");
            None
        }
    }
}

/// Copies the online coordinator's enrollment keys into the local
/// coordinator at `local_origin` and records how that went for the launcher.
/// Without an online coordinator there is nothing to copy and nothing to say.
async fn sync_keys(window: WebviewWindow, local_origin: String, local_token: String) {
    let state = window.state::<AppState>();
    let Some(remote) = Settings::load(&state.data_dir).remote_url else {
        return;
    };
    let outcome = match session_cookie(&window, &remote).await {
        Some(cookie) => sync::pull_keys(&remote, &cookie, &local_origin, &local_token).await,
        None => Err(SyncError::NotLoggedIn),
    };
    match &outcome {
        Ok(report) => tracing::info!(
            %remote,
            imported = report.imported,
            already_present = report.already_present,
            skipped_revoked = report.skipped_revoked,
            revoked = report.revoked,
            "enrollment keys synced"
        ),
        Err(error) => tracing::info!(%remote, %error, "enrollment keys not synced"),
    }
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    *state.key_sync.lock().unwrap_or_else(|e| e.into_inner()) = KeySync {
        at: Some(at),
        imported: outcome.as_ref().map_or(0, |report| report.imported),
        error: outcome.err().map(|error| error.to_string()),
    };
}

/// Runs [`sync_keys`] into `running` in the background.
fn start_key_sync(
    window: &WebviewWindow,
    running: &Embedded,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(sync_keys(
        window.clone(),
        running.origin(),
        running.api_token().to_owned(),
    ))
}

/// Starts the local coordinator if it is not running, copies the online
/// coordinator's enrollment keys into it and shows its UI.
#[tauri::command]
async fn open_local(window: WebviewWindow, state: State<'_, AppState>) -> Result<(), String> {
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| operation_in_progress())?;
    let mut embedded = state.embedded.lock().await;
    if embedded.is_none() {
        let data_dir = state.data_dir.join("coordinator");
        let online_coordinator = Settings::load(&state.data_dir).remote_url;
        let options = Options {
            online_coordinator,
            ..Options::default()
        };
        let started = Embedded::start(&data_dir, options).await.map_err(|error| {
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
    let entry = running.entry_url();
    let syncing = start_key_sync(&window, running);
    drop(embedded);
    let _ = tokio::time::timeout(KEY_SYNC_WAIT, syncing).await;
    navigate(&window, &entry)
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
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| operation_in_progress())?;
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
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| operation_in_progress())?;
    let origin = remote::normalise(&address).map_err(|error| error.to_string())?;
    let health = remote::check(&origin)
        .await
        .map_err(|error| error.to_string())?;
    tracing::info!(%origin, version = %health.version, "remote coordinator reached");

    let mut settings = Settings::load(&state.data_dir);
    settings.remote_url = Some(origin.clone());
    if let Err(error) = settings.save(&state.data_dir) {
        tracing::warn!(%error, "could not save settings");
    }
    // A running local coordinator sends devices to this one from now on and
    // gets its keys, if the operator is still signed in there.
    if let Some(running) = state.embedded.lock().await.as_ref() {
        running.set_online_coordinator(Some(&origin));
        start_key_sync(&window, running);
    }
    state.allow(origin.clone());
    navigate(&window, &origin)
}

/// Asks the repository's latest release whether there is a newer manager.
///
/// Without internet the answer is "nothing available" with a quiet reason,
/// so the launcher keeps working in hotspot mode. Concurrent update work
/// is refused rather than replacing the release being downloaded.
#[tauri::command]
async fn check_update(app: AppHandle, state: State<'_, AppState>) -> Result<UpdateStatus, String> {
    let current = app.package_info().version.to_string();
    if update::configured_key(UPDATER_PUBKEY).is_none() {
        return Ok(UpdateStatus::not_configured(&current));
    }
    let mut pending = state
        .pending_update
        .try_lock()
        .map_err(|_| update_in_progress())?;
    // The plugin was registered with the key in `main`; the endpoint comes
    // from `plugins.updater` in tauri.conf.json.
    let updater = app.updater_builder().timeout(UPDATE_CHECK_TIMEOUT);
    // Tauri's default hook destroys the webview before ShellExecuteW has
    // confirmed that the installer started. Keep it alive for recovery if
    // launching fails. We close the coordinator ourselves before install;
    // the plugin exits the process immediately after a successful launch.
    #[cfg(windows)]
    let updater = updater.on_before_exit(|| {});
    let checked = match updater.build() {
        Ok(updater) => updater.check().await,
        Err(error) => Err(error),
    };
    Ok(match checked {
        Ok(Some(mut found)) => {
            tracing::info!(version = %found.version, "manager update available");
            found.timeout = Some(UPDATE_DOWNLOAD_TIMEOUT);
            let status = UpdateStatus::available(&current, &found.version, found.body.as_deref());
            *pending = Some(found);
            status
        }
        Ok(None) => {
            *pending = None;
            UpdateStatus::up_to_date(&current)
        }
        Err(error) => {
            // Expected in hotspot mode; not worth more than an info line.
            tracing::info!(%error, "update check failed");
            UpdateStatus::failed(&current, &error)
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
    // Keep both guards until the attempt finishes. Failed attempts retain
    // their release for retry; no concurrent check can overwrite it.
    let pending = state
        .pending_update
        .try_lock()
        .map_err(|_| update_in_progress())?;
    let Some(found) = pending.as_ref() else {
        return Err("There is no update to install. Check for updates first.".to_owned());
    };
    let _operation = state
        .operation
        .try_lock()
        .map_err(|_| operation_in_progress())?;
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
            return Err(update::install_error_message(&error));
        }
    };

    let mut embedded = state.embedded.lock().await;
    let options = Options {
        online_coordinator: Settings::load(&state.data_dir).remote_url,
        ..Options::default()
    };
    let installed = update::install_with_recovery(
        &mut embedded,
        &state.data_dir.join("coordinator"),
        options,
        || {
            tracing::info!(version = %found.version, "installing manager update");
            found.install(bytes)
        },
        report,
    )
    .await;
    if let Some(restored) = embedded.as_ref() {
        state.allow(restored.origin());
    }
    // Restart dispatches Exit, which takes this mutex during shutdown.
    drop(embedded);
    installed?;
    report("Restarting…");
    app.restart()
}

fn operation_in_progress() -> String {
    "Another manager operation is in progress. Wait for it to finish and try again.".to_owned()
}

fn update_in_progress() -> String {
    "An update check or installation is already in progress. Wait for it to finish.".to_owned()
}

fn show_launcher(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    // Keep the page carrying installation progress in place until the
    // operation either succeeds or reports its recovery result.
    let Ok(_operation) = state.operation.try_lock() else {
        return;
    };
    let launcher = state
        .launcher
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let (Some(window), Some(launcher)) = (app.get_webview_window(MAIN_WINDOW), launcher) {
        if let Err(error) = window.navigate(launcher) {
            tracing::error!(%error, "could not return to the launcher");
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

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
            let launcher = Arc::new(Mutex::new(None));
            app.manage(AppState {
                data_dir,
                embedded: tokio::sync::Mutex::new(None),
                allowed_origins: allowed_origins.clone(),
                launcher: launcher.clone(),
                pending_update: tokio::sync::Mutex::new(None),
                operation: tokio::sync::Mutex::new(()),
                key_sync: Mutex::new(KeySync::default()),
            });

            // Keep controls above the taskbar on smaller or scaled displays.
            let maximize = app.primary_monitor().ok().flatten().is_some_and(|monitor| {
                let area = monitor
                    .work_area()
                    .size
                    .to_logical::<f64>(monitor.scale_factor());
                area.width < 1320.0 || area.height < 940.0
            });
            WebviewWindowBuilder::new(app, MAIN_WINDOW, WebviewUrl::App("index.html".into()))
                .title("Surface Coordinator")
                .inner_size(1280.0, 860.0)
                .min_inner_size(1000.0, 640.0)
                .maximized(maximize)
                .on_navigation(move |url| {
                    if is_launcher_page(url) {
                        *launcher.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.clone());
                    }
                    let allowed = allowed_origins.lock().unwrap_or_else(|e| e.into_inner());
                    let permitted = navigation_allowed(url, &allowed);
                    if !permitted {
                        tracing::warn!(%url, "navigation blocked");
                    }
                    permitted
                })
                .build()?;

            let change =
                MenuItemBuilder::with_id("change-coordinator", "Change coordinator…").build(app)?;
            let coordinator = SubmenuBuilder::new(app, "Coordinator")
                .item(&change)
                .separator()
                .quit()
                .build()?;
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
