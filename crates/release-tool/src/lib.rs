//! Logic behind the `release-tool` commands, kept apart from argument parsing
//! so it can be reused and tested.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use protocol::update::{self, Version};
use sha2::{Digest, Sha256};

/// Environment variable holding the base64 Ed25519 seed. The key lives in the
/// CI secret store, never in a file of the repository.
pub const SIGNING_KEY_ENV: &str = "SC_UPDATE_SIGNING_KEY";

/// A freshly generated key pair, both halves base64.
pub struct KeyPair {
    pub private: String,
    pub public: String,
}

pub fn generate_key_pair() -> KeyPair {
    let private = STANDARD.encode(rand::random::<[u8; 32]>());
    let public = update::public_key_for(&private).expect("a generated seed is always valid");
    KeyPair { private, public }
}

/// Size and SHA-256 of a file, read in chunks so large artifacts stay out of memory.
pub fn hash_file(path: &Path) -> Result<(u64, [u8; 32])> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((size, hasher.finalize().into()))
}

/// Checks a version argument and returns it in its canonical form.
pub fn parse_version(text: &str) -> Result<String> {
    let version: Version = text.parse()?;
    Ok(version.to_string())
}

/// The signing key from the environment, with a hint when it is missing.
pub fn signing_key_from_env() -> Result<String> {
    match std::env::var(SIGNING_KEY_ENV) {
        Ok(key) if !key.trim().is_empty() => Ok(key),
        _ => bail!(
            "{SIGNING_KEY_ENV} is not set; export the base64 signing key (the GitHub Actions \
             secret of that name) before running this command"
        ),
    }
}

/// Result of signing an artifact.
pub struct Signed {
    pub size: u64,
    pub sha256_hex: String,
    pub signature: String,
}

pub fn sign_file(signing_key: &str, file: &Path, version: &str, target: &str) -> Result<Signed> {
    let version = parse_version(version)?;
    let (size, hash) = hash_file(file)?;
    let signature = update::sign(signing_key, &version, target, &hash)?;
    Ok(Signed { size, sha256_hex: hex::encode(hash), signature })
}

pub fn verify_file(
    public_key: &Path,
    file: &Path,
    version: &str,
    target: &str,
    signature: &Path,
) -> Result<()> {
    let version = parse_version(version)?;
    let public_key = std::fs::read_to_string(public_key)
        .with_context(|| format!("cannot read {}", public_key.display()))?;
    let signature = std::fs::read_to_string(signature)
        .with_context(|| format!("cannot read {}", signature.display()))?;
    let (_, hash) = hash_file(file)?;
    update::verify(&public_key, &version, target, &hash, &signature)?;
    Ok(())
}
