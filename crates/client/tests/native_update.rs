//! Native candidate checks only run `--version`; no service or network changes.
#![cfg(windows)]

use client::platform::{windows::WindowsPlatform, Platform};

#[tokio::test]
async fn a_real_downloaded_client_identifies_its_version_before_replacement() {
    let dir = tempfile::tempdir().unwrap();
    // The downloaded filename does not end in .exe. Windows still loads the
    // PE image when CreateProcess receives the exact executable path.
    let candidate = dir.path().join("client.update.tmp");
    std::fs::copy(env!("CARGO_BIN_EXE_client"), &candidate).unwrap();
    let platform = WindowsPlatform::new(dir.path());
    platform.validate_update(candidate.clone(), env!("CARGO_PKG_VERSION").into()).await.unwrap();
    let wrong_version = platform.validate_update(candidate, "999.0.0".into()).await;
    assert!(wrong_version.unwrap_err().to_string().contains("did not identify itself"));
}

#[tokio::test]
async fn an_unlaunchable_signed_candidate_is_rejected_before_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let candidate = dir.path().join("client.update.tmp");
    std::fs::write(&candidate, b"not a Windows executable").unwrap();
    let platform = WindowsPlatform::new(dir.path());
    let result = platform.validate_update(candidate.clone(), env!("CARGO_PKG_VERSION").into()).await;
    assert!(result.unwrap_err().to_string().contains("cannot start"));
    // Rejection has closed every handle, so the failed download can go.
    std::fs::remove_file(candidate).unwrap();
}
