//! Surface Coordinator Manager: the desktop shell.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use manager::embedded::{Embedded, Options};
use manager::settings::Settings;
use manager::{navigation_allowed, remote};
use serde::Serialize;
use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{Manager, RunEvent, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};
use url::Url;

const MAIN_WINDOW: &str = "main";

struct AppState {
    data_dir: PathBuf,
    embedded: tokio::sync::Mutex<Option<Embedded>>,
    /// Origins the window may navigate to besides the launcher.
    allowed_origins: Arc<Mutex<Vec<String>>>,
    /// Address of the bundled launcher page, to return to it.
    launcher: Mutex<Option<Url>>,
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

/// Stops the local coordinator; devices connected to it go offline.
#[tauri::command]
async fn stop_local(state: State<'_, AppState>) -> Result<(), String> {
    if let Some(running) = state.embedded.lock().await.take() {
        running.stop().await;
        tracing::info!("local coordinator stopped");
    }
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

    let app = tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![status, open_local, stop_local, open_remote])
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let allowed_origins = Arc::new(Mutex::new(Vec::new()));
            app.manage(AppState {
                data_dir,
                embedded: tokio::sync::Mutex::new(None),
                allowed_origins: allowed_origins.clone(),
                launcher: Mutex::new(None),
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
            tauri::async_runtime::block_on(async {
                if let Some(running) = state.embedded.lock().await.take() {
                    running.stop().await;
                }
            });
        }
    });
}
