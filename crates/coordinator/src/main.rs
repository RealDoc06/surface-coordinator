//! Surface Coordinator server: parses configuration and runs
//! `coordinator-core`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use coordinator_core::{admin, AuthMode, Config};

#[derive(Parser)]
#[command(name = "coordinator", version, about = "Surface Coordinator server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve {
        /// Address to listen on for clients and the HTTP API.
        #[arg(long, env = "SC_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        /// Directory for the database.
        #[arg(long, env = "SC_DATA_DIR")]
        data_dir: PathBuf,
        /// Bearer token for the HTTP API. A random one is generated and
        /// printed for this launch if not given.
        #[arg(long, env = "SC_API_TOKEN", hide_env_values = true)]
        api_token: Option<String>,
        /// Seconds between client heartbeats.
        #[arg(long, default_value_t = 30)]
        heartbeat_interval_secs: u64,
        /// Hours a job stays queued for an offline device before it expires.
        #[arg(long, default_value_t = 24)]
        queued_expiry_hours: u64,
        /// Log debug detail.
        #[arg(long)]
        verbose: bool,
    },
    /// Manage enrollment keys.
    Key {
        #[command(subcommand)]
        command: KeyCommand,
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

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve {
            bind,
            data_dir,
            api_token,
            heartbeat_interval_secs,
            queued_expiry_hours,
            verbose,
        } => {
            let level = if verbose { tracing::Level::DEBUG } else { tracing::Level::INFO };
            tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

            let token = match api_token.filter(|token| !token.is_empty()) {
                Some(token) => token,
                None => {
                    let token = hex::encode(rand::random::<[u8; 32]>());
                    println!("API token for this launch: {token}");
                    token
                }
            };
            let mut config = Config::new(bind, data_dir, AuthMode::Token(token));
            config.heartbeat_interval = Duration::from_secs(heartbeat_interval_secs);
            config.queued_expiry = Duration::from_secs(queued_expiry_hours * 60 * 60);
            serve(config)
        }
        Command::Key { command } => key(command),
    }
}

#[tokio::main]
async fn serve(config: Config) -> anyhow::Result<()> {
    let coordinator = coordinator_core::start(config).await.context("failed to start")?;
    println!("Listening on {}", coordinator.local_addr());
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
