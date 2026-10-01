//! Everything operating-system specific sits behind [`Platform`].
//!
//! The client core only ever talks to this trait, so it can be tested with a
//! fake and other operating systems can be added as further modules. Later
//! milestones extend the trait (launch app, apply setting, Wi-Fi, power);
//! an operation a platform cannot perform returns
//! [`PlatformError::Unsupported`].

use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use protocol::{OutputStream, RunAs, Shell};
use tokio::sync::mpsc;

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
    #[error("{0}")]
    Failed(String),
}

/// The Wi-Fi network the device is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiConnection {
    pub ssid: String,
    /// The device's address on that network.
    pub ip: Option<IpAddr>,
    /// The network's gateway. On a manager's hotspot this is the manager.
    pub gateway: Option<IpAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiJoinRequest {
    pub ssid: String,
    /// `None` for an open network.
    pub passphrase: Option<String>,
    /// Whether the device should come back to this network by itself later.
    pub auto_connect: bool,
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
