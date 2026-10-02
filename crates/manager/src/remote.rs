//! Reaching a remote coordinator: checking what the operator typed and that
//! there is a coordinator behind it.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RemoteError {
    #[error("enter the address of the coordinator, for example https://coordinator.example.org")]
    Empty,
    #[error("that is not a valid address")]
    Invalid,
    #[error("only http and https addresses can be used")]
    Scheme,
    #[error("the address must not contain a username or password")]
    Credentials,
    #[error("could not reach a coordinator at that address: {0}")]
    Unreachable(String),
    #[error("there is a server at that address, but it is not a Surface Coordinator")]
    NotACoordinator,
}

/// Turns what the operator typed into the origin of a coordinator, e.g.
/// `coordinator.example.org/` → `https://coordinator.example.org`.
pub fn normalise(input: &str) -> Result<String, RemoteError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(RemoteError::Empty);
    }
    let with_scheme =
        if input.contains("://") { input.to_owned() } else { format!("https://{input}") };
    let url = Url::parse(&with_scheme).map_err(|_| RemoteError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(RemoteError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(RemoteError::Credentials);
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(RemoteError::Invalid);
    }
    // Scheme, host and port only: the UI lives at the root.
    Ok(url.origin().ascii_serialization())
}

/// What `/api/v1/health` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    pub protocol_version: u32,
}

/// Asks the server at `origin` whether it is a coordinator.
pub async fn check(origin: &str) -> Result<Health, RemoteError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|error| RemoteError::Unreachable(error.to_string()))?;
    let response = client
        .get(format!("{origin}/api/v1/health"))
        .send()
        .await
        .map_err(|error| RemoteError::Unreachable(describe(&error)))?;
    if !response.status().is_success() {
        return Err(RemoteError::NotACoordinator);
    }
    let health: Health = response.json().await.map_err(|_| RemoteError::NotACoordinator)?;
    if health.status != "ok" {
        return Err(RemoteError::NotACoordinator);
    }
    Ok(health)
}

/// A transport failure, in words for the operator.
pub(crate) fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the connection timed out".into()
    } else if error.is_connect() {
        "the connection was refused or the name could not be resolved".into()
    } else {
        // The innermost cause is the informative one (certificate errors).
        let mut source: &dyn std::error::Error = error;
        while let Some(next) = source.source() {
            source = next;
        }
        source.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_what_operators_type() {
        let cases = [
            ("coordinator.example.org", "https://coordinator.example.org"),
            ("  https://coordinator.example.org/  ", "https://coordinator.example.org"),
            ("https://coordinator.example.org/devices?x=1#frag", "https://coordinator.example.org"),
            ("http://192.168.1.10:8080", "http://192.168.1.10:8080"),
            ("192.168.1.10:8080", "https://192.168.1.10:8080"),
            ("HTTPS://Coordinator.Example.ORG:443/", "https://coordinator.example.org"),
            ("http://localhost:18080/", "http://localhost:18080"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalise(input).as_deref(), Ok(expected), "{input}");
        }
    }

    #[test]
    fn refuses_unusable_addresses() {
        assert_eq!(normalise("   "), Err(RemoteError::Empty));
        assert_eq!(normalise("ftp://example.org"), Err(RemoteError::Scheme));
        assert_eq!(normalise("file:///C:/Windows"), Err(RemoteError::Scheme));
        assert_eq!(normalise("javascript://alert(1)"), Err(RemoteError::Scheme));
        assert_eq!(normalise("https://user:pw@example.org"), Err(RemoteError::Credentials));
        assert_eq!(normalise("https://"), Err(RemoteError::Invalid));
        assert_eq!(normalise("https://exa mple.org"), Err(RemoteError::Invalid));
    }
}
