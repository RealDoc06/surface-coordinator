//! Compiles the updater public key into the manager, if there is one.
//!
//! The owner commits `updater.pub` (the one-line public key written by
//! `tauri signer generate`). Without it the build still works and simply
//! cannot update itself: `SC_UPDATER_PUBKEY` is empty and the updater plugin
//! is never registered. `SC_UPDATER_PUB_FILE` names another file, for
//! testing a build against a throwaway key without putting it in the
//! repository.

use std::{env, fs, io};

const KEY_FILE: &str = "updater.pub";

fn main() {
    println!("cargo:rerun-if-env-changed=SC_UPDATER_PUB_FILE");
    let key = match env::var("SC_UPDATER_PUB_FILE") {
        // Asked for explicitly, so it must be there.
        Ok(path) => {
            println!("cargo:rerun-if-changed={path}");
            fs::read_to_string(&path).unwrap_or_else(|error| panic!("could not read {path}: {error}"))
        }
        Err(_) => match fs::read_to_string(KEY_FILE) {
            Ok(text) => {
                println!("cargo:rerun-if-changed={KEY_FILE}");
                text
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Cargo treats a missing watched file as always changed and
                // would rebuild the manager on every build. Watching the
                // crate directory notices the file once it is added; the
                // build itself rewrites nothing there (tauri-build only
                // writes `gen/` when its content changes).
                println!("cargo:rerun-if-changed=.");
                String::new()
            }
            Err(error) => panic!("could not read {KEY_FILE}: {error}"),
        },
    };

    let key = key.trim();
    // The plugin expects the key exactly as `tauri signer generate` writes
    // it: one base64 line. A decoded key (two lines, "untrusted comment:")
    // would only be noticed at the first update, as a failed verification.
    if key.contains(['\r', '\n']) || key.starts_with("untrusted comment") {
        panic!("the updater public key must be the one line written by `tauri signer generate`");
    }
    println!("cargo:rustc-env=SC_UPDATER_PUBKEY={key}");

    tauri_build::build()
}
