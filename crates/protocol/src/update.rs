//! Signing and verification of client update artifacts.
//!
//! The client runs as LocalSystem and releases come from a public repository,
//! so an update is installed only if an Ed25519 signature verifies against the
//! public key compiled into the build. The signature covers the version and
//! target as well as the file hash, so a signature for one artifact cannot be
//! reused for another version (downgrade) or another platform:
//!
//! ```text
//! "surface-coordinator/client-update/v1" 0x00 version 0x00 target 0x00 sha256
//! ```
//!
//! `sha256` is the 32 raw digest bytes. Public keys, signing keys (the 32-byte
//! Ed25519 seed) and signatures travel as standard base64 with padding.

use std::fmt;
use std::str::FromStr;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};

const CONTEXT: &[u8] = b"surface-coordinator/client-update/v1";

/// Update target of 64-bit Windows, the only client platform today.
pub const TARGET_WINDOWS_X86_64: &str = "windows-x86_64";

/// The update target of the running build, if it has one.
pub fn current_target() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some(TARGET_WINDOWS_X86_64)
    } else {
        None
    }
}

/// The exact bytes that are signed for an update artifact.
pub fn signing_message(version: &str, target: &str, sha256: &[u8; 32]) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(CONTEXT.len() + version.len() + target.len() + sha256.len() + 3);
    message.extend_from_slice(CONTEXT);
    message.push(0);
    message.extend_from_slice(version.as_bytes());
    message.push(0);
    message.extend_from_slice(target.as_bytes());
    message.push(0);
    message.extend_from_slice(sha256);
    message
}

/// Why an update signature was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the update public key is not a valid base64 Ed25519 public key")]
    BadPublicKey,
    #[error("the signature is not a valid base64 Ed25519 signature")]
    BadSignatureEncoding,
    #[error("the signature does not match the artifact, version and target")]
    BadSignature,
}

/// Why a signing key could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    #[error("the signing key is not a base64 encoded 32-byte Ed25519 seed")]
    BadSigningKey,
}

fn decode_array<const N: usize>(text: &str) -> Option<[u8; N]> {
    STANDARD.decode(text.trim()).ok()?.try_into().ok()
}

/// Checks `signature` over the artifact described by `version`, `target` and
/// `sha256` against `public_key`. Surrounding whitespace is ignored in both
/// base64 arguments, so a value read from a file with a trailing newline works.
pub fn verify(
    public_key: &str,
    version: &str,
    target: &str,
    sha256: &[u8; 32],
    signature: &str,
) -> Result<(), VerifyError> {
    let key_bytes = decode_array::<32>(public_key).ok_or(VerifyError::BadPublicKey)?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|_| VerifyError::BadPublicKey)?;
    let signature_bytes =
        decode_array::<64>(signature).ok_or(VerifyError::BadSignatureEncoding)?;
    let signature = Signature::from_bytes(&signature_bytes);
    // Strict verification also refuses malleable signatures and weak keys.
    key.verify_strict(&signing_message(version, target, sha256), &signature)
        .map_err(|_| VerifyError::BadSignature)
}

fn signing_key(seed: &str) -> Result<SigningKey, SignError> {
    let bytes = decode_array::<32>(seed).ok_or(SignError::BadSigningKey)?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Signs an artifact. `signing_key` is the base64 of the 32-byte Ed25519
/// seed; the result is the base64 signature. Used by the release tool and tests.
pub fn sign(
    signing_key_b64: &str,
    version: &str,
    target: &str,
    sha256: &[u8; 32],
) -> Result<String, SignError> {
    let key = signing_key(signing_key_b64)?;
    let signature = key.sign(&signing_message(version, target, sha256));
    Ok(STANDARD.encode(signature.to_bytes()))
}

/// The base64 public key belonging to a base64 seed.
pub fn public_key_for(signing_key_b64: &str) -> Result<String, SignError> {
    let key = signing_key(signing_key_b64)?;
    Ok(STANDARD.encode(key.verifying_key().to_bytes()))
}

/// A release version: strictly `major.minor.patch`, three unsigned integers.
/// No leading `v`, no pre-release or build suffix. Ordering is numeric per
/// component, so `1.2.10` is newer than `1.2.9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

/// The text is not a `major.minor.patch` version.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a version of the form major.minor.patch")]
pub struct ParseVersionError(String);

impl FromStr for Version {
    type Err = ParseVersionError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || ParseVersionError(text.to_owned());
        let mut parts = text.split('.');
        let mut next = || -> Result<u64, ParseVersionError> {
            let part = parts.next().ok_or_else(bad)?;
            // `u64::from_str` would also accept a leading `+`.
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(bad());
            }
            part.parse().map_err(|_| bad())
        };
        let version = Version { major: next()?, minor: next()?, patch: next()? };
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// SHA-256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Parses a 64-character hex digest (either case) into raw bytes.
pub fn parse_sha256_hex(hex_digest: &str) -> Option<[u8; 32]> {
    hex::decode(hex_digest).ok()?.try_into().ok()
}
