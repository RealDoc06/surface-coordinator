//! Everything operating-system specific sits behind [`Platform`].
//!
//! The client core only ever talks to this trait, so it can be tested with a
//! fake and other operating systems can be added as further modules. An
//! operation a platform cannot perform returns [`PlatformError::Unsupported`],
//! which is what every method but the first two does by default.

use std::future::Future;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use protocol::{OutputStream, PowerAction, RunAs, Shell};
use tokio::sync::mpsc;

pub mod executable;
#[cfg(windows)]
pub mod windows;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    pub hostname: String,
    /// Platform identifier reported in `hello`, e.g. `windows`.
    pub os: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRequest {
    pub shell: Shell,
    pub body: String,
    /// The platform must stop the script, and anything it started, when
    /// this elapses.
    pub timeout: Duration,
    pub run_as: RunAs,
}

/// A piece of script output, already decoded to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptOutcome {
    Exited(i32),
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlatformError {
    /// This platform cannot do what was asked.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Nobody is logged in to the device's interactive session, so there is
    /// no user to act as.
    #[error("no user session: {0}")]
    NoUserSession(String),
    /// The operating system refuses to tell. On Windows 11 the list of
    /// Wi-Fi networks and the name of the current one are withheld while
    /// Location services are off.
    #[error("access denied: {0}")]
    AccessDenied(String),
    #[error("{0}")]
    Failed(String),
}

/// The Wi-Fi network the device is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConnection {
    /// `None` when the operating system does not say which network it is
    /// (see [`PlatformError::AccessDenied`]).
    pub ssid: Option<String>,
    /// The device's address on that network.
    pub ip: Option<IpAddr>,
    /// The network's gateway. On a manager's hotspot this is the manager.
    pub gateway: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WifiJoinPolicy {
    /// Explicit operator command: changing networks is its purpose.
    Replace,
    /// Automatic discovery must leave every existing connection alone.
    Disconnected,
    /// A failed explicit move may leave its destination, but not a network
    /// the user selected afterwards. Unknown SSIDs must be preserved.
    RecoverFrom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiJoinRequest {
    pub ssid: String,
    /// `None` for an open network.
    pub passphrase: Option<String>,
    /// Whether the device should come back to this network by itself later.
    pub auto_connect: bool,
    /// Platforms recheck this immediately before initiating the join.
    pub policy: WifiJoinPolicy,
}

fn unsupported<T>(what: &str) -> Result<T, PlatformError> {
    Err(PlatformError::Unsupported(format!("{what} is not supported on this platform")))
}

pub trait Platform: Send + Sync + 'static {
    fn system_info(&self) -> SystemInfo;

    /// Runs a script to completion, sending its output to `output` as it is
    /// produced. All output has been sent by the time this returns.
    fn run_script(
        &self,
        request: ScriptRequest,
        output: mpsc::Sender<OutputChunk>,
    ) -> impl Future<Output = Result<ScriptOutcome, PlatformError>> + Send;

    /// The SSIDs currently in range.
    fn wifi_scan(&self) -> impl Future<Output = Result<Vec<String>, PlatformError>> + Send {
        async { unsupported("scanning for Wi-Fi networks") }
    }

    /// Stores the network's profile and connects to it. Returns once the
    /// device is associated, or with an error if it could not connect.
    fn wifi_join(
        &self,
        request: WifiJoinRequest,
    ) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async move {
            let _ = request;
            unsupported("joining a Wi-Fi network")
        }
    }

    /// The current Wi-Fi connection, or `None` if not connected by Wi-Fi.
    fn wifi_current(
        &self,
    ) -> impl Future<Output = Result<Option<WifiConnection>, PlatformError>> + Send {
        async { unsupported("querying the Wi-Fi connection") }
    }

    /// Starts a program in the logged-in user's interactive session and
    /// returns its process id without waiting for it.
    fn launch_app(
        &self,
        path: String,
        arguments: Vec<String>,
    ) -> impl Future<Output = Result<u32, PlatformError>> + Send {
        async move {
            let _ = (path, arguments);
            unsupported("launching an app")
        }
    }

    /// Applies one entry of [`protocol::settings::catalogue`]. The value has
    /// already been validated against the catalogue.
    fn apply_setting(
        &self,
        setting: String,
        value: serde_json::Value,
    ) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async move {
            let _ = (setting, value);
            unsupported("applying a setting")
        }
    }

    /// Whether [`Platform::power`] can attempt `action`, checked before the
    /// job commits. Runtime failures are still returned by `power`.
    /// Implement it together with `power`.
    fn check_power(&self, action: PowerAction) -> Result<(), PlatformError> {
        let _ = action;
        unsupported("power actions")
    }

    /// Requests a native power action. Returns only once the OS accepts a
    /// delayed restart/shutdown, or the blocking sleep call finishes. An
    /// error is reported as a failed job, never as a prior success.
    fn power(&self, action: PowerAction) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async move {
            let _ = action;
            unsupported("power actions")
        }
    }

    /// The client's executable. `client.update` downloads beside it.
    fn current_exe(&self) -> Result<PathBuf, PlatformError> {
        unsupported("updating the client")
    }

    /// After signature verification, checks that the candidate can start
    /// and identifies as the expected version before replacing the working
    /// service. A bad image cannot run the client's own rollback code.
    fn validate_update(
        &self,
        path: PathBuf,
        expected_version: String,
    ) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async move {
            let _ = (path, expected_version);
            unsupported("validating a client update")
        }
    }

    /// Replaces the executable with `new`: the running one becomes
    /// `client.old.exe` beside it (see [`executable`]).
    fn swap_executable(&self, new: PathBuf) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async move {
            let _ = new;
            unsupported("updating the client")
        }
    }

    /// Undoes [`Platform::swap_executable`]: `client.old.exe` goes back.
    fn restore_executable(&self) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async { unsupported("updating the client") }
    }

    /// Deletes what an update or a rollback left beside the executable.
    fn remove_old_executable(&self) -> impl Future<Output = Result<(), PlatformError>> + Send {
        async { unsupported("updating the client") }
    }

    /// Asks for the client to be stopped and started again, on whatever
    /// executable is in place by then. The client is expected to stop soon
    /// after: this is how an update takes effect.
    fn request_restart(&self) -> Result<(), PlatformError> {
        unsupported("restarting the client")
    }
}

/// Decodes a byte stream as UTF-8 piece by piece, holding back a multi-byte
/// character that is split across two reads. Invalid bytes become U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Utf8Decoder {
        Utf8Decoder::default()
    }

    /// Feeds more bytes and returns the text that is complete so far.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        loop {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                    break;
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&self.pending[consumed..valid_end])
                            .expect("validated by from_utf8"),
                    );
                    consumed = valid_end;
                    match error.error_len() {
                        // Invalid sequence: replace it and carry on.
                        Some(length) => {
                            text.push(char::REPLACEMENT_CHARACTER);
                            consumed += length;
                        }
                        // Incomplete character at the end: wait for more.
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        text
    }

    /// Call at end of stream: returns whatever was held back.
    pub fn finish(&mut self) -> String {
        let rest = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8Decoder;

    #[test]
    fn passes_complete_text_through() {
        let mut decoder = Utf8Decoder::new();
        assert_eq!(decoder.push(b"hello\n"), "hello\n");
        assert_eq!(decoder.push("grüße ✓".as_bytes()), "grüße ✓");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn holds_back_a_character_split_across_reads() {
        let bytes = "a€b😀c".as_bytes();
        for split in 0..=bytes.len() {
            let mut decoder = Utf8Decoder::new();
            let mut text = decoder.push(&bytes[..split]);
            text.push_str(&decoder.push(&bytes[split..]));
            text.push_str(&decoder.finish());
            assert_eq!(text, "a€b😀c", "split at {split}");
        }
        // Byte by byte.
        let mut decoder = Utf8Decoder::new();
        let text: String = bytes.iter().map(|b| decoder.push(&[*b])).collect();
        assert_eq!(text, "a€b😀c");
    }

    #[test]
    fn replaces_invalid_bytes() {
        let mut decoder = Utf8Decoder::new();
        assert_eq!(decoder.push(b"ok\xffok\xc3\x28"), "ok\u{fffd}ok\u{fffd}(");
        assert_eq!(decoder.finish(), "");
    }

    #[test]
    fn flushes_a_truncated_character_at_end_of_stream() {
        let mut decoder = Utf8Decoder::new();
        assert_eq!(decoder.push(&"€".as_bytes()[..2]), "");
        assert_eq!(decoder.finish(), "\u{fffd}");
    }
}
