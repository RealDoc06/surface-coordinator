//! The hotspot a manager opens when there is no network.
//!
//! Both the SSID and the passphrase follow from the enrollment key, so a
//! client knows them without any exchange.

use hkdf::Hkdf;
use sha2::Sha256;

const SSID_PREFIX: &str = "SC-";
const SSID_KEY_ID_CHARS: usize = 8;
const SALT: &[u8] = b"surface-coordinator/hotspot/v1";
const INFO: &[u8] = b"wpa2-passphrase";
/// 15 bytes are exactly 24 base32 characters.
const PASSPHRASE_BYTES: usize = 15;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// `SC-` followed by the first eight characters of the enrollment key id.
pub fn ssid(key_id: &str) -> String {
    let prefix: String = key_id.chars().take(SSID_KEY_ID_CHARS).collect();
    format!("{SSID_PREFIX}{prefix}")
}

/// The WPA2 passphrase of the hotspot for an enrollment secret: 24
/// characters of `a-z2-7` derived with HKDF-SHA-256.
pub fn passphrase(secret: &str) -> String {
    let mut bytes = [0u8; PASSPHRASE_BYTES];
    Hkdf::<Sha256>::new(Some(SALT), secret.as_bytes())
        .expand(INFO, &mut bytes)
        .expect("15 bytes is a valid HKDF-SHA-256 output length");
    base32(&bytes)
}

fn base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}
