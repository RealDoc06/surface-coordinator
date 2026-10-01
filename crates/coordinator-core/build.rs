//! Embeds the built web UI (`ui/dist`) and the client update public key
//! (`release-keys/client-update.pub`) into the library.
//!
//! Generates `$OUT_DIR/ui_assets.rs` with one `include_bytes!` per file. If
//! the UI has not been built, a placeholder page is embedded instead so the
//! Rust build never depends on Node.
//!
//! The key is exported as `SC_CLIENT_UPDATE_PUBKEY`, empty while the owner
//! has not created the file; such a build disables client updates.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const PLACEHOLDER: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>Surface Coordinator</title></head><body style=\"font-family:sans-serif;margin:3rem\">\
<h1>Surface Coordinator</h1><p>The web UI was not built into this binary. \
Run <code>npm ci &amp;&amp; npm run build</code> in <code>ui/</code> and rebuild, \
or start the coordinator with <code>--ui-dir</code>.</p>\
<p>The API is available under <code>/api/v1</code>.</p></body></html>";

fn collect(dir: &Path, root: &Path, files: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            let name = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((name, path));
        }
    }
}

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
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dist = manifest_dir.join("..").join("..").join("ui").join("dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-changed=build.rs");

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files);
        files.sort();
    }

    let mut code = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    if files.is_empty() {
        let placeholder = out_dir.join("ui_placeholder.html");
        std::fs::write(&placeholder, PLACEHOLDER).unwrap();
        writeln!(code, "    (\"index.html\", include_bytes!({:?})),", placeholder.display().to_string()).unwrap();
        println!("cargo:warning=ui/dist not found; embedding a placeholder page instead of the web UI");
    } else {
        for (name, path) in &files {
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            writeln!(code, "    ({:?}, include_bytes!({:?})),", name, path.display().to_string()).unwrap();
        }
    }
    code.push_str("];\n");
    std::fs::write(out_dir.join("ui_assets.rs"), code).unwrap();
}
