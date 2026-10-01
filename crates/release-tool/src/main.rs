//! `release-tool`: update signing keys and artifact signatures.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use release_tool::{
    generate_key_pair, sign_file, signing_key_from_env, verify_file, SIGNING_KEY_ENV,
};

#[derive(Parser)]
#[command(name = "release-tool", about = "Sign and verify Surface Coordinator update artifacts")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate an update signing key pair and print both halves
    Keygen {
        /// Also write the public key (one line) to this file
        #[arg(long)]
        public_out: Option<PathBuf>,
    },
    /// Sign an artifact; the key comes from SC_UPDATE_SIGNING_KEY
    Sign {
        #[arg(long)]
        file: PathBuf,
        /// major.minor.patch, without a leading "v"
        #[arg(long)]
        version: String,
        /// For example windows-x86_64
        #[arg(long)]
        target: String,
        /// Signature file; defaults to the artifact path plus ".sig"
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Check an artifact against its signature and a public key file
    Verify {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        version: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        public_key: PathBuf,
    },
    /// Print the public key for SC_UPDATE_SIGNING_KEY
    PublicKey,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Keygen { public_out } => {
            let keys = generate_key_pair();
            if let Some(path) = public_out {
                std::fs::write(&path, format!("{}\n", keys.public))
                    .with_context(|| format!("cannot write {}", path.display()))?;
                println!("Public key written to {}", path.display());
            }
            println!("Public key:  {}", keys.public);
            println!();
            println!("PRIVATE KEY (shown once; this tool never writes it to a file):");
            println!("{}", keys.private);
            println!();
            println!("Store it as the GitHub Actions secret {SIGNING_KEY_ENV} and nowhere");
            println!("else. Never put it in the repository, a file or a chat message.");
        }
        Command::Sign { file, version, target, out } => {
            let key = signing_key_from_env()?;
            let signed = sign_file(&key, &file, &version, &target)?;
            let out = out.unwrap_or_else(|| {
                let mut name = file.clone().into_os_string();
                name.push(".sig");
                PathBuf::from(name)
            });
            std::fs::write(&out, format!("{}\n", signed.signature))
                .with_context(|| format!("cannot write {}", out.display()))?;
            println!("size:      {} bytes", signed.size);
            println!("sha256:    {}", signed.sha256_hex);
            println!("signature: {}", out.display());
        }
        Command::Verify { file, version, target, signature, public_key } => {
            verify_file(&public_key, &file, &version, &target, &signature)?;
            println!("signature OK");
        }
        Command::PublicKey => {
            let key = signing_key_from_env()?;
            println!("{}", protocol::update::public_key_for(&key)?);
        }
    }
    Ok(())
}
