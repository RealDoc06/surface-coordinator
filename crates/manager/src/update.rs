//! Manager self-update: the decisions that need no window.
//!
//! The Tauri updater downloads `latest.json` from the repository's latest
//! release, verifies the installer's signature against the public key
//! compiled into this build and runs the installer. This module decides what
//! the launcher is told about it. A failed check is never an error the
//! operator has to dismiss: in hotspot mode there is no internet, and the
//! manager keeps working on its current version.

use std::path::Path;

use serde::Serialize;
use tauri_plugin_updater::Error;

use crate::embedded::{Embedded, Options};

/// Release notes longer than this are cut; the full text is on the release
/// page, and the launcher banner is no place for a changelog of every
/// version since the last update.
pub const NOTES_LIMIT: usize = 4000;

/// What the launcher shows when the check failed for want of a network.
pub const UNREACHABLE: &str = "could not reach the update server";

/// The updater public key compiled into this build, if there is one.
///
/// Builds made without `updater.pub` (development builds, forks) get an
/// empty key and have self-update disabled rather than an updater that
/// would reject every download.
pub fn configured_key(compiled: &str) -> Option<&str> {
    let key = compiled.trim();
    (!key.is_empty()).then_some(key)
}

/// The answer to the launcher's `check_update`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// Whether this build can update itself at all.
    pub configured: bool,
    pub available: bool,
    pub current_version: String,
    /// The version offered, when `available`.
    pub version: Option<String>,
    /// Release notes as plain text, clipped to [`NOTES_LIMIT`].
    pub notes: Option<String>,
    /// Why the check did not give an answer; shown quietly, only when the
    /// operator asked.
    pub error: Option<String>,
}

impl UpdateStatus {
    pub fn not_configured(current_version: &str) -> Self {
        Self::base(current_version, false)
    }

    pub fn up_to_date(current_version: &str) -> Self {
        Self::base(current_version, true)
    }

    pub fn available(current_version: &str, version: &str, notes: Option<&str>) -> Self {
        UpdateStatus {
            available: true,
            version: Some(version.to_owned()),
            notes: notes.and_then(|notes| clip_notes(notes, NOTES_LIMIT)),
            ..Self::base(current_version, true)
        }
    }

    /// The check failed; reported as "nothing available" with a reason.
    pub fn failed(current_version: &str, error: &Error) -> Self {
        UpdateStatus {
            error: Some(check_error_message(error).to_owned()),
            ..Self::base(current_version, true)
        }
    }

    fn base(current_version: &str, configured: bool) -> Self {
        UpdateStatus {
            configured,
            available: false,
            current_version: current_version.to_owned(),
            version: None,
            notes: None,
            error: None,
        }
    }
}

/// A short reason for a failed check, fit for the launcher's footer.
///
/// The plugin's own messages talk about JSON and target triples; the
/// operator only needs to know whether to try again later.
pub fn check_error_message(error: &Error) -> &'static str {
    match error {
        Error::Reqwest(_) | Error::Network(_) => UNREACHABLE,
        // The endpoint answered with an error status: no release yet, or a
        // release without the manifest.
        Error::ReleaseNotFound => "the update server has no release information",
        Error::TargetNotFound(_) | Error::TargetsNotFound(_) => {
            "the latest release has no update for this system"
        }
        _ => "the update information could not be read",
    }
}

/// Why installing failed, shown in the update banner.
pub fn install_error_message(error: &Error) -> String {
    match error {
        Error::Reqwest(_) | Error::Network(_) => {
            format!("The update could not be downloaded: {UNREACHABLE}.")
        }
        // A signature that does not verify means the file is not from this
        // project's release workflow; it is never installed.
        Error::Minisign(_) | Error::Base64(_) | Error::SignatureUtf8(_) => {
            "The downloaded update is not signed with this build's key and was not installed."
                .to_owned()
        }
        Error::SignedVersionMismatch { .. } => {
            "The downloaded update's signed version does not match the announced release and was not installed."
                .to_owned()
        }
        Error::MissingSignedVersion => {
            "The downloaded update has no signed version and was not installed. Wait for a corrected release."
                .to_owned()
        }
        other => format!("The update could not be installed: {other}"),
    }
}

/// Closes the local database before running an already verified installer.
/// If the installer fails to start, restores the coordinator with its data
/// and client port. Hotspot activation remains an explicit operator action:
/// its OS state may have changed outside the manager.
pub async fn install_with_recovery(
    embedded: &mut Option<Embedded>,
    data_dir: &Path,
    options: Options,
    install: impl FnOnce() -> Result<(), Error>,
    report: impl Fn(&str),
) -> Result<(), String> {
    let was_running = embedded.is_some();
    if let Some(running) = embedded.take() {
        report("Stopping the local coordinator…");
        running.stop().await;
    }
    report("Installing the update…");
    let Err(error) = install() else { return Ok(()) };
    tracing::error!(%error, "update install failed");
    let message = install_error_message(&error);
    if !was_running {
        return Err(message);
    }

    report("Restoring the local coordinator…");
    match Embedded::start(data_dir, options).await {
        Ok(restored) => {
            tracing::info!(clients = %restored.client_addr(), "local coordinator restored after failed update");
            *embedded = Some(restored);
            Err(format!(
                "{message} The local coordinator is running again. \
                 If you were using its hotspot, open the local coordinator and start the hotspot again."
            ))
        }
        Err(error) => {
            tracing::error!(%error, "could not restore local coordinator after failed update");
            Err(format!(
                "{message} The local coordinator could not be restarted: {error}. \
                 Use Start local coordinator to try again."
            ))
        }
    }
}

/// Trims the notes and cuts them at `limit` characters; `None` when there
/// is nothing to show.
pub fn clip_notes(notes: &str, limit: usize) -> Option<String> {
    let notes = notes.trim();
    if notes.is_empty() {
        return None;
    }
    // Cut on a character boundary, never inside a multi-byte character.
    match notes.char_indices().nth(limit) {
        None => Some(notes.to_owned()),
        Some((end, _)) => Some(format!("{}…", notes[..end].trim_end())),
    }
}

/// Turns the updater's per-chunk callbacks into progress text, and only
/// produces a message when the text changes, so the launcher is not sent a
/// message for every few kilobytes.
#[derive(Debug, Default)]
pub struct DownloadProgress {
    downloaded: u64,
    shown: Option<String>,
}

impl DownloadProgress {
    pub fn advance(&mut self, chunk: usize, total: Option<u64>) -> Option<String> {
        self.downloaded += chunk as u64;
        let text = progress_text(self.downloaded, total);
        if self.shown.as_ref() == Some(&text) {
            return None;
        }
        self.shown = Some(text.clone());
        Some(text)
    }
}

pub fn progress_text(downloaded: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => {
            let percent = downloaded.min(total) * 100 / total;
            format!("Downloading the update… {percent}%")
        }
        // GitHub sends a length, but a proxy may not.
        _ => format!("Downloading the update… {:.1} MB", downloaded as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_without_a_public_key_is_not_configured() {
        assert_eq!(configured_key(""), None);
        assert_eq!(configured_key("  \n\t "), None);
        assert_eq!(configured_key(" dW50cnVzdGVk \n"), Some("dW50cnVzdGVk"));
    }

    #[test]
    fn a_build_without_a_key_reports_that_updates_are_not_configured() {
        let status = UpdateStatus::not_configured("0.1.0");
        assert!(!status.configured);
        assert!(!status.available);
        assert_eq!(status.current_version, "0.1.0");
        assert_eq!(status.error, None);
    }

    #[test]
    fn being_up_to_date_is_configured_and_not_available_without_an_error() {
        let status = UpdateStatus::up_to_date("0.1.0");
        assert!(status.configured);
        assert!(!status.available);
        assert_eq!((status.version, status.notes, status.error), (None, None, None));
    }

    #[test]
    fn an_available_update_carries_its_version_and_trimmed_notes() {
        let status = UpdateStatus::available("0.1.0", "0.2.0", Some("\n  Faster hotspot start.\n"));
        assert!(status.configured && status.available);
        assert_eq!(status.version.as_deref(), Some("0.2.0"));
        assert_eq!(status.notes.as_deref(), Some("Faster hotspot start."));
        assert_eq!(status.error, None);
    }

    #[test]
    fn an_update_without_notes_or_with_blank_notes_has_none() {
        assert_eq!(UpdateStatus::available("0.1.0", "0.2.0", None).notes, None);
        assert_eq!(UpdateStatus::available("0.1.0", "0.2.0", Some(" \n ")).notes, None);
    }

    #[test]
    fn a_network_failure_is_reported_quietly_as_nothing_available() {
        let status = UpdateStatus::failed("0.1.0", &Error::Network("connection refused".into()));
        assert!(status.configured);
        assert!(!status.available);
        assert_eq!(status.error.as_deref(), Some(UNREACHABLE));
    }

    #[test]
    fn a_missing_release_and_a_release_without_this_platform_are_told_apart() {
        assert_eq!(
            check_error_message(&Error::ReleaseNotFound),
            "the update server has no release information"
        );
        assert_eq!(
            check_error_message(&Error::TargetsNotFound(vec!["windows-x86_64".into()])),
            "the latest release has no update for this system"
        );
    }

    #[test]
    fn an_unreadable_manifest_is_reported_without_the_parser_details() {
        let parse_error = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert_eq!(
            check_error_message(&Error::Serialization(parse_error)),
            "the update information could not be read"
        );
    }

    #[test]
    fn a_bad_signature_says_the_update_was_not_installed() {
        let message = install_error_message(&Error::SignatureUtf8("x".into()));
        assert!(message.contains("not signed with this build's key"), "{message}");
        let message = install_error_message(&Error::Network("timed out".into()));
        assert!(message.contains(UNREACHABLE), "{message}");
    }

    #[test]
    fn an_untrusted_release_version_is_refused_without_suggesting_bypassing_verification() {
        let mismatch = install_error_message(&Error::SignedVersionMismatch {
            signed: "0.1.0".to_owned(),
            announced: "9.9.9".to_owned(),
        });
        assert!(mismatch.contains("signed version does not match"), "{mismatch}");
        assert!(mismatch.contains("was not installed"), "{mismatch}");
        let missing = install_error_message(&Error::MissingSignedVersion);
        assert!(missing.contains("no signed version"), "{missing}");
        assert!(missing.contains("Wait for a corrected release"), "{missing}");
    }

    #[test]
    fn the_status_serialises_with_the_field_names_the_launcher_reads() {
        let json = serde_json::to_value(UpdateStatus::available("0.1.0", "0.2.0", Some("n"))).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "configured": true,
                "available": true,
                "current_version": "0.1.0",
                "version": "0.2.0",
                "notes": "n",
                "error": null,
            })
        );
    }

    #[test]
    fn short_notes_are_kept_whole() {
        assert_eq!(clip_notes("Fixes.", 10).as_deref(), Some("Fixes."));
        assert_eq!(clip_notes("0123456789", 10).as_deref(), Some("0123456789"));
    }

    #[test]
    fn long_notes_are_cut_at_the_limit_with_an_ellipsis() {
        assert_eq!(clip_notes("0123456789ab", 10).as_deref(), Some("0123456789…"));
        let long = "x".repeat(NOTES_LIMIT + 500);
        let clipped = UpdateStatus::available("0.1.0", "0.2.0", Some(&long)).notes.unwrap();
        assert_eq!(clipped.chars().count(), NOTES_LIMIT + 1);
    }

    #[test]
    fn notes_are_cut_between_characters_not_inside_one() {
        // Each of these is several bytes in UTF-8; slicing by bytes would panic.
        assert_eq!(clip_notes("ééééé", 3).as_deref(), Some("ééé…"));
        assert_eq!(clip_notes("🙂🙂🙂", 2).as_deref(), Some("🙂🙂…"));
    }

    #[test]
    fn progress_is_a_percentage_when_the_size_is_known() {
        assert_eq!(progress_text(0, Some(200)), "Downloading the update… 0%");
        assert_eq!(progress_text(50, Some(200)), "Downloading the update… 25%");
        // More bytes than announced must not show more than 100%.
        assert_eq!(progress_text(300, Some(200)), "Downloading the update… 100%");
    }

    #[test]
    fn progress_is_in_megabytes_when_the_size_is_unknown() {
        assert_eq!(progress_text(3 * 1_048_576, None), "Downloading the update… 3.0 MB");
        assert_eq!(progress_text(10, Some(0)), "Downloading the update… 0.0 MB");
    }

    #[test]
    fn progress_is_only_reported_when_the_text_changes() {
        let mut progress = DownloadProgress::default();
        assert_eq!(progress.advance(1, Some(1000)).as_deref(), Some("Downloading the update… 0%"));
        assert_eq!(progress.advance(1, Some(1000)), None);
        assert_eq!(progress.advance(8, Some(1000)).as_deref(), Some("Downloading the update… 1%"));
        assert_eq!(progress.advance(990, Some(1000)).as_deref(), Some("Downloading the update… 100%"));
    }
}
