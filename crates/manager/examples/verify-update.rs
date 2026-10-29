//! Checks exactly what the installed updater trusts before publishing a bundle.
//! This utility reads only the artifact, signature and public key.

use std::error::Error;
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use clap::Parser;
use minisign_verify::{PublicKey, Signature};

#[derive(Parser)]
struct Args {
    #[arg(long)]
    file: PathBuf,
    #[arg(long)]
    signature: PathBuf,
    #[arg(long)]
    public_key: PathBuf,
    #[arg(long)]
    version: String,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let public_text = std::fs::read_to_string(args.public_key)?;
    let public_text = String::from_utf8(STANDARD.decode(public_text.trim())?)?;
    let signature_text = std::fs::read_to_string(args.signature)?;
    let signature_text = String::from_utf8(STANDARD.decode(signature_text.trim())?)?;
    let public_key = PublicKey::decode(&public_text)?;
    let signature = Signature::decode(&signature_text)?;
    public_key.verify(&std::fs::read(&args.file)?, &signature, true)?;

    // Read the version only after verifying the global signature, which
    // authenticates this comment along with the artifact's signature.
    let version = signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"));
    if version != Some(args.version.as_str()) {
        return Err(format!("the signature is not bound to expected version {}", args.version).into());
    }
    println!("Verified updater signature and version {}.", args.version);
    Ok(())
}
