//! Surface Coordinator Manager: everything that does not need a window.
//!
//! The manager shows the coordinator's own web UI, either from a remote
//! coordinator or from the one embedded here. It owns the choice between
//! the two and never talks to clients itself.

pub mod embedded;
pub mod hotspot;
pub mod remote;
pub mod settings;
pub mod sync;
pub mod update;
pub mod wifi;

use url::Url;

/// Whether the window may navigate to `url`.
///
/// The window shows the bundled launcher and the UI of the coordinator in
/// use; it is not a general browser. `allowed_origins` holds the origins of
/// the embedded coordinator and of the chosen remote coordinator.
pub fn navigation_allowed(url: &Url, allowed_origins: &[String]) -> bool {
    if is_launcher(url) {
        return true;
    }
    matches!(url.scheme(), "http" | "https")
        && allowed_origins.iter().any(|origin| *origin == url.origin().ascii_serialization())
}

/// The bundled launcher page: `tauri://localhost` on macOS and Linux,
/// `http://tauri.localhost` on Windows.
pub fn is_launcher(url: &Url) -> bool {
    match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        "about" => url.path() == "blank",
        _ => false,
    }
}

/// The launcher page proper, as opposed to the blank page a window shows
/// before its first navigation finishes. This is the address "Change
/// coordinator" has to return to, so it must be a page that can be shown.
pub fn is_launcher_page(url: &Url) -> bool {
    is_launcher(url) && url.scheme() != "about"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn the_blank_page_is_not_a_page_to_return_to() {
        assert!(is_launcher_page(&url("http://tauri.localhost/index.html")));
        assert!(is_launcher_page(&url("tauri://localhost/index.html")));
        assert!(!is_launcher_page(&url("about:blank")));
        assert!(!is_launcher_page(&url("http://127.0.0.1:51234/")));
    }

    #[test]
    fn the_window_only_goes_to_the_launcher_and_the_chosen_coordinators() {
        let allowed = vec!["http://127.0.0.1:51234".to_owned(), "https://coordinator.example.org".to_owned()];
        for ok in [
            "tauri://localhost/index.html",
            "http://tauri.localhost/",
            "http://127.0.0.1:51234/#token=abc",
            "http://127.0.0.1:51234/devices",
            "https://coordinator.example.org/jobs/1",
        ] {
            assert!(navigation_allowed(&url(ok), &allowed), "{ok}");
        }
        for refused in [
            "https://example.com/",
            "http://127.0.0.1:9999/",
            "http://coordinator.example.org/",
            "https://coordinator.example.org.evil.test/",
            "https://tauri.localhost.evil.test/",
            "file:///C:/Windows/win.ini",
            "javascript:alert(1)",
        ] {
            assert!(!navigation_allowed(&url(refused), &allowed), "{refused}");
        }
        assert!(!navigation_allowed(&url("https://coordinator.example.org/"), &[]));
    }
}
