//! Runs the built `release-tool` binary the way CI and the owner do. Every key
//! here is generated at run time into a temporary directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_release-tool");
const KEY_ENV: &str = "SC_UPDATE_SIGNING_KEY";
const ARTIFACT: &[u8] = b"pretend this is client.exe";

fn run(args: &[&str], signing_key: Option<&str>) -> Output {
    let mut command = Command::new(BIN);
    command.args(args);
    // Never inherit a real key from the developer's environment.
    command.env_remove(KEY_ENV);
    if let Some(key) = signing_key {
        command.env(KEY_ENV, key);
    }
    command.output().expect("release-tool starts")
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

struct Keys {
    private: String,
    public: String,
    public_file: PathBuf,
}

/// Runs `keygen` and reads the keys back from its output, the way an owner would.
fn keygen(dir: &Path, name: &str) -> Keys {
    let public_file = dir.join(name);
    let output = run(&["keygen", "--public-out", path(&public_file)], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    let public = std::fs::read_to_string(&public_file).unwrap();
    assert!(public.ends_with('\n') && !public[..public.len() - 1].contains('\n'));
    let public = public.trim_end().to_owned();
    // The private key is the only other base64 line of the output.
    let private = out
        .lines()
        .map(str::trim)
        .find(|line| line.len() == 44 && line.ends_with('=') && *line != public)
        .expect("keygen prints the private key on a line of its own")
        .to_owned();
    Keys { private, public, public_file }
}

fn write_artifact(dir: &Path) -> PathBuf {
    let file = dir.join("client.exe");
    std::fs::write(&file, ARTIFACT).unwrap();
    file
}

fn sign(file: &Path, version: &str, target: &str, key: &str) -> Output {
    run(
        &["sign", "--file", path(file), "--version", version, "--target", target],
        Some(key),
    )
}

fn verify(file: &Path, version: &str, target: &str, sig: &Path, public: &Path) -> Output {
    run(
        &[
            "verify",
            "--file",
            path(file),
            "--version",
            version,
            "--target",
            target,
            "--signature",
            path(sig),
            "--public-key",
            path(public),
        ],
        None,
    )
}

fn signed_fixture() -> (TempDir, Keys, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let keys = keygen(dir.path(), "client-update.pub");
    let file = write_artifact(dir.path());
    let output = sign(&file, "1.2.3", "windows-x86_64", &keys.private);
    assert!(output.status.success(), "{}", stderr(&output));
    let sig = dir.path().join("client.exe.sig");
    assert!(sig.exists(), "the signature goes next to the artifact by default");
    (dir, keys, file, sig)
}

#[test]
fn keygen_sign_verify_succeeds() {
    let (_dir, keys, file, sig) = signed_fixture();
    let output = verify(&file, "1.2.3", "windows-x86_64", &sig, &keys.public_file);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("signature OK"));
}

#[test]
fn keygen_warns_about_where_the_private_key_belongs() {
    let dir = TempDir::new().unwrap();
    let output = run(&["keygen"], None);
    assert!(output.status.success());
    let out = stdout(&output);
    assert!(out.contains("SC_UPDATE_SIGNING_KEY"));
    assert!(out.to_lowercase().contains("repository"));
    // Without --public-out nothing is written anywhere.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn keygen_makes_a_new_pair_every_time() {
    let dir = TempDir::new().unwrap();
    let a = keygen(dir.path(), "a.pub");
    let b = keygen(dir.path(), "b.pub");
    assert_ne!(a.private, b.private);
    assert_ne!(a.public, b.public);
}

#[test]
fn sign_prints_size_hash_and_signature_path() {
    let dir = TempDir::new().unwrap();
    let keys = keygen(dir.path(), "k.pub");
    let file = write_artifact(dir.path());
    let output = sign(&file, "1.2.3", "windows-x86_64", &keys.private);
    let out = stdout(&output);
    assert!(out.contains(&ARTIFACT.len().to_string()));
    assert!(out.contains(&protocol::update::sha256_hex(ARTIFACT)));
    assert!(out.contains("client.exe.sig"));
}

#[test]
fn sign_writes_to_the_requested_output_file() {
    let dir = TempDir::new().unwrap();
    let keys = keygen(dir.path(), "k.pub");
    let file = write_artifact(dir.path());
    let out_file = dir.path().join("elsewhere.sig");
    let output = run(
        &[
            "sign", "--file", path(&file), "--version", "1.2.3", "--target", "windows-x86_64",
            "--out", path(&out_file),
        ],
        Some(&keys.private),
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let written = std::fs::read_to_string(&out_file).unwrap();
    assert!(written.ends_with('\n'));
    let verified = verify(&file, "1.2.3", "windows-x86_64", &out_file, &keys.public_file);
    assert!(verified.status.success(), "{}", stderr(&verified));
}

#[test]
fn verify_fails_after_one_byte_of_the_artifact_changes() {
    let (_dir, keys, file, sig) = signed_fixture();
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[3] ^= 1;
    std::fs::write(&file, bytes).unwrap();
    let output = verify(&file, "1.2.3", "windows-x86_64", &sig, &keys.public_file);
    assert!(!output.status.success());
    assert!(!stdout(&output).contains("signature OK"));
    assert!(stderr(&output).contains("signature"));
}

#[test]
fn verify_fails_for_another_version() {
    let (_dir, keys, file, sig) = signed_fixture();
    let output = verify(&file, "1.2.4", "windows-x86_64", &sig, &keys.public_file);
    assert!(!output.status.success());
}

#[test]
fn verify_fails_for_another_target() {
    let (_dir, keys, file, sig) = signed_fixture();
    let output = verify(&file, "1.2.3", "linux-x86_64", &sig, &keys.public_file);
    assert!(!output.status.success());
}

#[test]
fn verify_fails_for_another_key() {
    let (dir, _keys, file, sig) = signed_fixture();
    let other = keygen(dir.path(), "other.pub");
    let output = verify(&file, "1.2.3", "windows-x86_64", &sig, &other.public_file);
    assert!(!output.status.success());
}

#[test]
fn verify_fails_for_a_missing_signature_file() {
    let (dir, keys, file, _sig) = signed_fixture();
    let missing = dir.path().join("missing.sig");
    let output = verify(&file, "1.2.3", "windows-x86_64", &missing, &keys.public_file);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("missing.sig"));
}

#[test]
fn sign_without_the_environment_variable_explains_what_to_set() {
    let dir = TempDir::new().unwrap();
    let file = write_artifact(dir.path());
    let output = run(
        &["sign", "--file", path(&file), "--version", "1.2.3", "--target", "windows-x86_64"],
        None,
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("SC_UPDATE_SIGNING_KEY"));
    assert!(!dir.path().join("client.exe.sig").exists());
}

#[test]
fn sign_rejects_a_malformed_signing_key() {
    let dir = TempDir::new().unwrap();
    let file = write_artifact(dir.path());
    let output = sign(&file, "1.2.3", "windows-x86_64", "definitely not a key");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("signing key"));
    assert!(!dir.path().join("client.exe.sig").exists());
}

#[test]
fn sign_and_verify_reject_versions_that_are_not_major_minor_patch() {
    let (dir, keys, file, sig) = signed_fixture();
    for bad in ["v1.2.3", "1.2", "1.2.3-beta"] {
        let output = sign(&file, bad, "windows-x86_64", &keys.private);
        assert!(!output.status.success(), "{bad}");
        let output = verify(&file, bad, "windows-x86_64", &sig, &keys.public_file);
        assert!(!output.status.success(), "{bad}");
    }
    drop(dir);
}

#[test]
fn public_key_command_matches_the_keygen_public_key() {
    let dir = TempDir::new().unwrap();
    let keys = keygen(dir.path(), "k.pub");
    let output = run(&["public-key"], Some(&keys.private));
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), keys.public);
}

#[test]
fn public_key_command_without_the_variable_fails_helpfully() {
    let output = run(&["public-key"], None);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("SC_UPDATE_SIGNING_KEY"));
}

#[test]
fn the_private_key_is_never_written_to_a_file() {
    let (dir, keys, _file, _sig) = signed_fixture();
    let mut checked = 0;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let content = std::fs::read(entry.unwrap().path()).unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(!text.contains(&keys.private));
        checked += 1;
    }
    // Public key, artifact and signature.
    assert_eq!(checked, 3);
}

#[test]
fn the_private_key_is_not_echoed_by_sign_or_public_key() {
    let (dir, keys, file, _sig) = signed_fixture();
    let signed = sign(&file, "1.2.3", "windows-x86_64", &keys.private);
    let public = run(&["public-key"], Some(&keys.private));
    for output in [signed, public] {
        assert!(!stdout(&output).contains(&keys.private));
        assert!(!stderr(&output).contains(&keys.private));
    }
    drop(dir);
}
