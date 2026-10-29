//! Surface Coordinator client: in a console (`run`, the default), as a
//! Windows service (`service`), and its own installer (`install`,
//! `uninstall`).

#[cfg(windows)]
mod cli {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Context;
    use clap::{Args, Parser, Subcommand};
    use client::config::{self, Overrides};
    use client::install::{self, InstallOptions};
    use client::platform::windows::WindowsPlatform;
    use client::{Client, ClientConfig, RESTART_EXIT_CODE};
    use tokio_util::sync::CancellationToken;

    #[derive(Parser)]
    #[command(name = "client", version, about = "Surface Coordinator device agent")]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Run the client in this console (the default when no subcommand is given)
        Run(RunArgs),
        /// Run under the Windows service manager; started by the service that `install` registers
        Service {
            /// Directory with config.json, the device's state and the log
            #[arg(long)]
            state_dir: PathBuf,
            /// Log debug detail
            #[arg(long)]
            verbose: bool,
        },
        /// Install the client as a Windows service and start it (elevated prompt).
        /// The enrollment secret is read from SC_KEY_SECRET
        Install(InstallArgs),
        /// Stop and remove the Windows service and the installed client (elevated prompt)
        Uninstall(UninstallArgs),
    }

    #[derive(Args)]
    struct RunArgs {
        /// Coordinator base URL, e.g. ws://127.0.0.1:8080 or wss://coordinator.example.
        /// Without it (here or in config.json) the client looks for the
        /// manager's hotspot. A URL given later by a wifi.join job takes
        /// precedence.
        #[arg(long, env = "SC_COORDINATOR_URL")]
        coordinator_url: Option<String>,
        /// Id of the enrollment key; overrides config.json.
        #[arg(long, env = "SC_KEY_ID")]
        key_id: Option<String>,
        /// The enrollment secret; overrides config.json. Prefer the
        /// environment variable, which keeps it out of the process list.
        #[arg(long, env = "SC_KEY_SECRET", hide_env_values = true)]
        key_secret: Option<String>,
        /// Directory for config.json (optional here), the device id, the job
        /// ledger, the network state and running scripts.
        #[arg(long, env = "SC_STATE_DIR")]
        state_dir: PathBuf,
        /// Disable automatic hotspot scanning and joining for this console
        /// run. Explicit wifi.join jobs remain available.
        #[arg(long)]
        no_hotspot_discovery: bool,
        /// Log debug detail.
        #[arg(long)]
        verbose: bool,
    }

    #[derive(Args)]
    struct InstallArgs {
        /// Id of the enrollment key.
        #[arg(long)]
        key_id: String,
        /// Coordinator base URL; without one the client looks for the manager's hotspot.
        #[arg(long)]
        coordinator_url: Option<String>,
        /// Persistently disable automatic hotspot scanning and joining.
        /// Explicit wifi.join jobs remain available.
        #[arg(long)]
        no_hotspot_discovery: bool,
        /// Where config.json and the device's state go [default: %ProgramData%\Surface Coordinator]
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// Where the executable is copied to [default: %ProgramFiles%\Surface Coordinator]
        #[arg(long)]
        install_dir: Option<PathBuf>,
    }

    #[derive(Args)]
    struct UninstallArgs {
        /// Also remove the state directory: configuration, device id, job record and log.
        #[arg(long)]
        purge: bool,
        /// The state directory given to install [default: %ProgramData%\Surface Coordinator]
        #[arg(long)]
        state_dir: Option<PathBuf>,
        /// The install directory given to install [default: %ProgramFiles%\Surface Coordinator]
        #[arg(long)]
        install_dir: Option<PathBuf>,
    }

    const SUBCOMMANDS: &[&str] = &["run", "service", "install", "uninstall", "help"];

    /// Inserts `run` when no subcommand is given, so the command lines from
    /// before there were subcommands (`client --state-dir ...`) still work.
    pub fn with_default_subcommand(mut args: Vec<OsString>) -> Vec<OsString> {
        let first = args.get(1).and_then(|arg| arg.to_str());
        let explicit = first.is_some_and(|arg| {
            SUBCOMMANDS.contains(&arg) || ["-h", "--help", "-V", "--version"].contains(&arg)
        });
        if !explicit {
            args.insert(1.min(args.len()), "run".into());
        }
        args
    }

    pub fn main() -> anyhow::Result<()> {
        let cli = Cli::parse_from(with_default_subcommand(std::env::args_os().collect()));
        let code = match cli.command {
            Command::Run(args) => run(args)?,
            Command::Service { state_dir, verbose } => client::service::run(state_dir, verbose)?,
            Command::Install(args) => {
                // Before anything else, the secret included: nothing is
                // touched unless elevated.
                install::require_elevation("install")?;
                let key_secret = std::env::var("SC_KEY_SECRET")
                    .ok()
                    .filter(|secret| !secret.is_empty())
                    .context("set the enrollment secret in the environment variable SC_KEY_SECRET")?;
                install::install(&InstallOptions {
                    key_id: args.key_id,
                    key_secret,
                    coordinator_url: args.coordinator_url,
                    hotspot_discovery: !args.no_hotspot_discovery,
                    install_dir: args.install_dir.unwrap_or_else(default_install_dir),
                    state_dir: args.state_dir.unwrap_or_else(default_state_dir),
                })?;
                0
            }
            Command::Uninstall(args) => {
                let install_dir = args.install_dir.unwrap_or_else(default_install_dir);
                let state_dir = args.state_dir.unwrap_or_else(default_state_dir);
                install::uninstall(&install_dir, &state_dir, args.purge)?;
                0
            }
        };
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }

    fn default_install_dir() -> PathBuf {
        install::default_install_dir(std::env::var_os("ProgramFiles"))
    }

    fn default_state_dir() -> PathBuf {
        install::default_state_dir(std::env::var_os("ProgramData"))
    }

    /// The client in this console; returns the exit code.
    #[tokio::main]
    async fn run(args: RunArgs) -> anyhow::Result<i32> {
        let level = if args.verbose { tracing::Level::DEBUG } else { tracing::Level::INFO };
        tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).init();

        let overrides = Overrides {
            coordinator_url: args.coordinator_url,
            key_id: args.key_id,
            key_secret: args.key_secret,
        };
        let settings = config::load(&args.state_dir, overrides)?;
        let mut config = ClientConfig::from_settings(settings, &args.state_dir);
        config.hotspot_discovery &= !args.no_hotspot_discovery;
        let device_id = config.device_id.clone();
        let shutdown = CancellationToken::new();
        let platform = Arc::new(
            WindowsPlatform::new(args.state_dir.join(install::SCRIPTS_DIR)).with_restart(shutdown.clone()),
        );
        let client = Client::new(config, platform.clone()).context("cannot open the client state")?;

        tracing::info!(%device_id, "client starting");
        let on_ctrl_c = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            on_ctrl_c.cancel();
        });
        client.run(shutdown).await;
        if platform.restart_requested() {
            // Nothing restarts a console process: the operator has to.
            tracing::warn!("the client has to be started again to complete the update");
            return Ok(RESTART_EXIT_CODE);
        }
        tracing::info!("client stopped");
        Ok(0)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn args(list: &[&str]) -> Vec<OsString> {
            list.iter().map(OsString::from).collect()
        }

        #[test]
        fn run_is_the_default_subcommand() {
            assert_eq!(
                with_default_subcommand(args(&["client", "--state-dir", "x"])),
                args(&["client", "run", "--state-dir", "x"])
            );
            assert_eq!(with_default_subcommand(args(&["client"])), args(&["client", "run"]));
            for explicit in [&["client", "install", "--key-id", "k"][..], &["client", "run"], &["client", "--help"], &["client", "-V"]] {
                assert_eq!(with_default_subcommand(args(explicit)), args(explicit));
            }
        }

        #[test]
        fn the_old_console_invocation_still_parses() {
            let cli = Cli::try_parse_from(with_default_subcommand(args(&[
                "client",
                "--coordinator-url",
                "ws://127.0.0.1:18080",
                "--key-id",
                "fake-key",
                "--state-dir",
                r"demo-data\client",
            ])))
            .unwrap();
            let Command::Run(run) = cli.command else { panic!("not run") };
            assert_eq!(run.coordinator_url.as_deref(), Some("ws://127.0.0.1:18080"));
            assert_eq!(run.key_id.as_deref(), Some("fake-key"));
            assert_eq!(run.state_dir, PathBuf::from(r"demo-data\client"));
        }

        #[test]
        fn install_takes_its_own_flags() {
            let cli = Cli::try_parse_from(args(&["client", "install", "--key-id", "fake-key", "--install-dir", r"D:\sc"]))
                .unwrap();
            let Command::Install(install) = cli.command else { panic!("not install") };
            assert_eq!(install.key_id, "fake-key");
            assert_eq!(install.install_dir, Some(PathBuf::from(r"D:\sc")));
            assert_eq!(install.state_dir, None);
        }

        #[test]
        fn the_command_line_definition_is_consistent() {
            use clap::CommandFactory;
            Cli::command().debug_assert();
        }
    }
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    cli::main()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("the client has no platform implementation for this operating system yet");
    std::process::exit(1);
}
