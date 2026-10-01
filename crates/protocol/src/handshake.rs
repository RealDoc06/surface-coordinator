//! Enrollment-key handshake helpers.
//!
//! Client and coordinator each prove knowledge of the enrollment secret by
//! sending an HMAC-SHA-256 over both nonces. The MAC input carries the role
//! of the sender, so one side's MAC can never be replayed as the other's:
//!
//! ```text
//! "surface-coordinator/handshake/v1" 0x00 role 0x00 client_nonce 0x00 server_nonce
//! ```
//!
//! Nonces are the hex strings exactly as sent on the wire. The HMAC key is
//! the enrollment secret as UTF-8 bytes. MACs travel as lowercase hex.

use hmac::{Hmac, Mac};
use sha2::Sha256;

const CONTEXT: &[u8] = b"surface-coordinator/handshake/v1";
const NONCE_BYTES: usize = 32;

/// Which side of the handshake produced a MAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Coordinator,
    Client,
}

impl Role {
    fn label(self) -> &'static [u8] {
        match self {
            Role::Coordinator => b"coordinator",
            Role::Client => b"client",
        }
    }
}

/// A fresh random nonce: 32 bytes, hex encoded.
pub fn generate_nonce() -> String {
    hex::encode(rand::random::<[u8; NONCE_BYTES]>())
}

/// True if `nonce` has the shape produced by [`generate_nonce`].
pub fn is_valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_BYTES * 2 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

fn mac(secret: &str, role: Role, client_nonce: &str, server_nonce: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(CONTEXT);
    mac.update(&[0]);
    mac.update(role.label());
    mac.update(&[0]);
    mac.update(client_nonce.as_bytes());
    mac.update(&[0]);
    mac.update(server_nonce.as_bytes());
    mac
}

/// The MAC `role` sends for this pair of nonces, as lowercase hex.
pub fn compute_mac(secret: &str, role: Role, client_nonce: &str, server_nonce: &str) -> String {
    hex::encode(mac(secret, role, client_nonce, server_nonce).finalize().into_bytes())
}

/// Checks a MAC received from `role` in constant time.
pub fn verify_mac(
    secret: &str,
    role: Role,
    client_nonce: &str,
    server_nonce: &str,
    mac_hex: &str,
) -> bool {
    let Ok(received) = hex::decode(mac_hex) else {
        return false;
    };
    mac(secret, role, client_nonce, server_nonce)
        .verify_slice(&received)
        .is_ok()
}
