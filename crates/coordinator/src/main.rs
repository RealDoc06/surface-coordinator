//! Surface Coordinator server: parses configuration and runs
//! `coordinator-core`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use coordinator_core::{admin, AuthMode, Config};

#[derive(Parser)]
#[command(name = "coordinator", version, about = "Surface Coordinator server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Auth {
    /// Username and password accounts (see `coordinator account`).
    Accounts,
    /// A single bearer token, for scripts and local use.
    Token,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Address clients connect to; also serves the API and web UI
        /// unless --ui-bind is given.
        #[arg(long, env = "SC_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        /// Serve the API and web UI on this address instead of --bind.
        #[arg(long, env = "SC_UI_BIND")]
        ui_bind: Option<SocketAddr>,
        /// Directory for the database.
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        /// How the API and web UI authenticate.
        #[arg(long, env = "SC_AUTH", value_enum, default_value = "accounts")]
        auth: Auth,
        /// With --auth token: the token. A random one is generated and
        /// printed for this launch if not given.
        #[arg(long, env = "SC_API_TOKEN", hide_env_values = true)]
        api_token: Option<String>,
        /// Mark the session cookie Secure. Use when the coordinator is
        /// reached through HTTPS (a TLS-terminating proxy).
        #[arg(long, env = "SC_SECURE_COOKIES")]
        secure_cookies: bool,
        /// Serve the web UI from this directory instead of the built-in copy.
        #[arg(long, env = "SC_UI_DIR")]
        ui_dir: Option<PathBuf>,
        /// Seconds between client heartbeats.
        #[arg(long, env = "SC_HEARTBEAT_INTERVAL_SECS", default_value_t = 30)]
        heartbeat_interval_secs: u64,
        /// Hours a job stays queued for an offline device before it expires.
        #[arg(long, default_value_t = 24)]
        queued_expiry_hours: u64,
        /// Do not look for new client releases (neither periodically nor
        /// when asked from the UI). Releases already cached, or uploaded
        /// through the UI, can still be pushed to clients.
        #[arg(long, env = "SC_NO_UPDATE_CHECK")]
        no_update_check: bool,
        /// Roll out the newest cached client release automatically (one
        /// device first, then the rest). Only the initial default: once
        /// stored, the setting is changed from the UI.
        #[arg(long, env = "SC_AUTO_UPDATE")]
        auto_update: bool,
        /// GitHub repository (owner/repo) whose releases provide client updates.
        #[arg(long, env = "SC_UPDATE_SOURCE", default_value = coordinator_core::DEFAULT_UPDATE_SOURCE)]
        update_source: String,
        /// Log debug detail.
        #[arg(long)]
        verbose: bool,
    },
    /// Manage enrollment keys.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    /// Manage the accounts that can log in to the web UI.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    /// Create a key and print its id and secret.
    Create {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        #[arg(long, default_value = "")]
        label: String,
    },
    /// List keys (without secrets).
    List {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Revoke a key so it is refused at the next handshake.
    Revoke {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        id: String,
    },
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Create an account. The password is read from SC_ACCOUNT_PASSWORD or
    /// asked for.
    Create {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        username: String,
    },
    /// List accounts.
    List {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
    },
    /// Set a new password and end the account's sessions.
    SetPassword {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        username: String,
    },
    /// Delete an account.
    Delete {
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        username: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve {
            bind,
            ui_bind,
            data_dir,
            auth,
            api_token,
            secure_cookies,
            ui_dir,
            heartbeat_interval_secs,
            queued_expiry_hours,
            no_update_check,
            auto_update,
            update_source,
            verbose,
        } => {
            let level = if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO };
            tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

            let auth = match auth {
                Auth::Accounts => {
                    if admin::list_accounts(&data_dir)?.is_empty() {
                        println!(
                            "No accounts exist yet. Create one with:\n  coordinator account create --data-dir {} <username>",
                            data_dir.display()
                        );
                    }
                    AuthMode::Accounts
                }
                Auth::Token => AuthMode::Token(match api_token.filter(|token| !token.is_empty()) {
                    Some(token) => token,
                    None => {
                        let token = hex::encode(rand::random::<[u8; 32]>());
                        println!("API token for this launch: {token}");
                        token
                    }
                }),
            };
            let mut config = Config::new(bind, data_dir, auth);
            config.ui_bind = ui_bind;
            config.ui_dir = ui_dir;
            config.secure_cookies = secure_cookies;
            config.heartbeat_interval = Duration::from_secs(heartbeat_interval_secs);
            config.queued_expiry = Duration::from_secs(queued_expiry_hours * 60 * 60);
            config.updates.enabled = !no_update_check;
            config.updates.source = update_source;
            config.auto_update = auto_update;
            serve(config)
        }
        Command::Key { command } => key(command),
        Command::Account { command } => account(command),
    }
}

#[tokio::main]
async fn serve(config: Config) -> anyhow::Result<()> {
    let has_key = config.has_update_key();
    let checking = if config.updates.enabled {
        format!("checking the releases of {}", config.updates.source)
    } else {
        "not checking for releases".to_owned()
    };
    let coordinator = coordinator_core::start(config).await.context("failed to start")?;
    println!("Clients connect to {}", coordinator.local_addr());
    println!("Web UI and API on http://{}", coordinator.ui_addr());
    if has_key {
        let auto = if coordinator.auto_update() { "on" } else { "off" };
        println!("Client updates: update key present, {checking}, automatic rollout {auto}");
    } else {
        println!("Client updates: disabled (no update key is built into this coordinator)");
    }
    shutdown_signal().await;
    coordinator.stop().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

fn key(command: KeyCommand) -> anyhow::Result<()> {
    match command {
        KeyCommand::Create { data_dir, label } => {
            let key = admin::create_enrollment_key(&data_dir, &label)?;
            println!("key id:     {}", key.id);
            println!("key secret: {}", key.secret);
        }
        KeyCommand::List { data_dir } => {
            for key in admin::list_enrollment_keys(&data_dir)? {
                let status = if key.revoked.is_some() { "revoked" } else { "active" };
                println!("{}  {}  {}", key.id, status, key.label);
            }
        }
        KeyCommand::Revoke { data_dir, id } => {
            if admin::revoke_enrollment_key(&data_dir, &id)? {
                println!("revoked {id}");
            } else {
                anyhow::bail!("no active key with id {id}");
            }
        }
    }
    Ok(())
}

/// The password from the environment, or typed twice without echo.
fn read_password() -> anyhow::Result<String> {
    if let Ok(password) = std::env::var("SC_ACCOUNT_PASSWORD") {
        return Ok(password);
    }
    let password = rpassword::prompt_password("Password: ")?;
    let again = rpassword::prompt_password("Repeat password: ")?;
    if password != again {
        anyhow::bail!("the passwords do not match");
    }
    Ok(password)
}

fn account(command: AccountCommand) -> anyhow::Result<()> {
    let list = |data_dir: &Path| -> anyhow::Result<()> {
        for account in admin::list_accounts(data_dir)? {
            println!("{}", account.username);
        }
        Ok(())
    };
    match command {
        AccountCommand::Create { data_dir, username } => {
            let account = admin::create_account(&data_dir, &username, &read_password()?)?;
            println!("created account {}", account.username);
        }
        AccountCommand::List { data_dir } => list(&data_dir)?,
        AccountCommand::SetPassword { data_dir, username } => {
            admin::set_account_password(&data_dir, &username, &read_password()?)?;
            println!("password changed for {username}");
        }
        AccountCommand::Delete { data_dir, username } => {
            admin::delete_account(&data_dir, &username)?;
            println!("deleted account {username}");
        }
    }
    Ok(())
}
