//! Surface Coordinator client, run as a console process.

#[cfg(windows)]
mod console {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Context;
    use clap::Parser;
    use client::platform::windows::WindowsPlatform;
    use client::{Client, ClientConfig};
    use tokio_util::sync::CancellationToken;

    #[derive(Parser)]
    #[command(name = "client", version, about = "Surface Coordinator device agent")]
    struct Cli {
        /// Coordinator base URL, e.g. ws://127.0.0.1:8080 or wss://coordinator.example.
        #[arg(long, env = "SC_COORDINATOR_URL")]
        coordinator_url: String,
        /// Id of the enrollment key.
        #[arg(long, env = "SC_KEY_ID")]
        key_id: String,
        /// The enrollment secret. Prefer the environment variable, which
        /// keeps it out of the process list.
        #[arg(long, env = "SC_KEY_SECRET", hide_env_values = true)]
        key_secret: String,
        /// Directory for the device id, the job ledger and running scripts.
        #[arg(long, env = "SC_STATE_DIR")]
        state_dir: PathBuf,
        /// Log debug detail.
        #[arg(long)]
        verbose: bool,
    }

    #[tokio::main]
    pub async fn main() -> anyhow::Result<()> {
        let cli = Cli::parse();
        let level = if cli.verbose { tracing::Level::DEBUG } else { tracing::Level::INFO };
        tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

        let device_id = client::load_or_create_device_id(&cli.state_dir)
            .context("cannot read or create the device id")?;
        let mut config =
            ClientConfig::new(cli.coordinator_url, cli.key_id, cli.key_secret, device_id.clone());
        config.state_dir = Some(cli.state_dir.clone());
        let platform = Arc::new(WindowsPlatform::new(cli.state_dir.join("scripts")));
        let client = Client::new(config, platform).context("cannot open the job ledger")?;

        tracing::info!(%device_id, "client starting");
        let shutdown = CancellationToken::new();
        let on_ctrl_c = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            on_ctrl_c.cancel();
        });
        client.run(shutdown).await;
        tracing::info!("client stopped");
        Ok(())
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    console::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the client has no platform implementation for this operating system yet");
    std::process::exit(1);
}
