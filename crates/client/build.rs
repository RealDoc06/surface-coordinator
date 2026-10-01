//! Compiles the client update public key (`release-keys/client-update.pub`)
//! into the client as `SC_CLIENT_UPDATE_PUBKEY`. It is empty while the owner
//! has not created the file; such a build refuses every `client.update`.

use std::path::{Path, PathBuf};

fn update_public_key(manifest_dir: &Path) -> String {
    let keys = manifest_dir.join("..").join("..").join("release-keys");
    // The directory rather than the file: a missing file would make cargo
    // rerun this script, and rebuild the crate, on every build.
    println!("cargo:rerun-if-changed={}", keys.display());
    let Ok(text) = std::fs::read_to_string(keys.join("client-update.pub")) else {
        return String::new();
    };
    // One base64 line; anything after it would break the variable.
    let key = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("").to_owned();
    if key.is_empty() {
        println!("cargo:warning=release-keys/client-update.pub is empty; client updates are disabled");
    }
    key
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let key = update_public_key(&manifest_dir);
    println!("cargo:rustc-env=SC_CLIENT_UPDATE_PUBKEY={key}");
    println!("cargo:rerun-if-changed=build.rs");
}
