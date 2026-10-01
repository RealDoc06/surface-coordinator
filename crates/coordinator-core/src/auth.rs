//! Who is calling the API: passwords, sessions and the request guards.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::api::ApiError;
use crate::state::AppState;
use crate::store::{self, Account};
use crate::AuthMode;

pub const SESSION_COOKIE: &str = "sc_session";
pub const SESSION_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub const MIN_PASSWORD_LEN: usize = 10;
const MAX_PASSWORD_LEN: usize = 1024;
const MAX_FAILED_LOGINS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(30);

/// The authenticated caller, available to handlers as a request extension.
#[derive(Debug, Clone)]
pub struct Identity {
    /// `None` when authenticated by the embedded-mode token.
    pub account: Option<Account>,
}

impl Identity {
    pub fn username(&self) -> Option<&str> {
        self.account.as_ref().map(|account| account.username.as_str())
    }
}

// ---------------------------------------------------------------- passwords

/// Hashes a password with Argon2id. Slow by design; call on the blocking pool.
pub fn hash_password(password: &str) -> String {
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>())
        .expect("16 bytes is a valid salt length");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("hashing with default parameters cannot fail")
        .to_string()
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
        .unwrap_or(false)
}

pub fn valid_username(username: &str) -> bool {
    (1..=64).contains(&username.len())
        && username.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub fn valid_password(password: &str) -> bool {
    password.chars().count() >= MIN_PASSWORD_LEN && password.len() <= MAX_PASSWORD_LEN
}

// ---------------------------------------------------------------- sessions

/// A fresh session token and the hash stored for it.
pub fn new_session_token() -> (String, String) {
    let token = hex::encode(rand::random::<[u8; 32]>());
    let hash = hash_token(&token);
    (token, hash)
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        SESSION_LIFETIME.as_secs(),
        if secure { "; Secure" } else { "" }
    )
}

pub fn clear_session_cookie(secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

pub fn session_token_from(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

pub fn token_matches(expected: &str, presented: &str) -> bool {
    bool::from(presented.as_bytes().ct_eq(expected.as_bytes()))
}

/// Works out who sent these headers, if anyone we know.
pub async fn identify(state: &AppState, headers: &HeaderMap) -> Result<Option<Identity>, crate::Error> {
    if let Some(token) = session_token_from(headers) {
        let hash = hash_token(token);
        if let Some(account) = state.store.call(move |conn| store::get_session(conn, &hash)).await? {
            return Ok(Some(Identity { account }));
        }
    }
    if let AuthMode::Token(expected) = &state.config.auth {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if bearer.is_some_and(|presented| token_matches(expected, presented)) {
            return Ok(Some(Identity { account: None }));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- guards

/// Rejects requests from callers that are not logged in.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match identify(&state, request.headers()).await {
        Ok(Some(identity)) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Ok(None) => ApiError::unauthorized().into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

/// True if the request's `Origin`, when it has one, is this server.
pub fn is_same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok()) else {
        // Not sent by a browser cross-origin request.
        return true;
    };
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok());
    let origin_host = origin.split_once("://").map(|(_, rest)| rest);
    match (origin_host, host) {
        (Some(origin_host), Some(host)) => origin_host.eq_ignore_ascii_case(host),
        _ => false,
    }
}

/// Refuses state-changing requests that a page on another site caused.
pub async fn same_origin(request: Request, next: Next) -> Response {
    let safe = matches!(*request.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && !is_same_origin(request.headers()) {
        return ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "cross_origin",
            "requests from other origins are not allowed",
        )
        .into_response();
    }
    next.run(request).await
}

// ---------------------------------------------------------------- throttle

/// Slows down password guessing: after a few consecutive failures for a
/// username, further attempts are refused for a while.
#[derive(Default)]
pub struct LoginThrottle {
    entries: Mutex<HashMap<String, Attempts>>,
}

struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last: Instant,
}

impl LoginThrottle {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a login for this username may be attempted now.
    pub fn allowed(&self, username: &str) -> bool {
        let now = Instant::now();
        let mut entries = self.entries();
        // Forget stale entries so the table cannot grow without bound.
        entries.retain(|_, attempts| now.duration_since(attempts.last) < LOCKOUT * 20);
        match entries.get(&username.to_ascii_lowercase()) {
            Some(Attempts { locked_until: Some(until), .. }) => now >= *until,
            _ => true,
        }
    }

    pub fn record_failure(&self, username: &str) {
        let now = Instant::now();
        let mut entries = self.entries();
        let attempts = entries
            .entry(username.to_ascii_lowercase())
            .or_insert(Attempts { failures: 0, locked_until: None, last: now });
        attempts.last = now;
        attempts.failures += 1;
        if attempts.failures >= MAX_FAILED_LOGINS {
            attempts.failures = 0;
            attempts.locked_until = Some(now + LOCKOUT);
        }
    }

    pub fn record_success(&self, username: &str) {
        self.entries().remove(&username.to_ascii_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_verifies_only_the_right_password() {
        let hash = hash_password("correct horse battery");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &hash));
        assert!(!verify_password("correct horse batterz", &hash));
        assert!(!verify_password("correct horse battery", "not a hash"));
        assert_ne!(hash, hash_password("correct horse battery"), "salted");
    }

    #[test]
    fn username_and_password_rules() {
        assert!(valid_username("alice"));
        assert!(valid_username("a.b_c-9"));
        assert!(!valid_username(""));
        assert!(!valid_username("has space"));
        assert!(!valid_username(&"x".repeat(65)));
        assert!(valid_password("0123456789"));
        assert!(!valid_password("012345678"));
    }

    #[test]
    fn cookie_parsing_finds_the_session_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "theme=dark; sc_session=abc123; other=1".parse().unwrap());
        assert_eq!(session_token_from(&headers), Some("abc123"));
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "xsc_session=nope".parse().unwrap());
        assert_eq!(session_token_from(&headers), None);
    }

    #[test]
    fn same_origin_compares_origin_with_host() {
        let headers = |origin: Option<&str>, host: &str| {
            let mut map = HeaderMap::new();
            map.insert(header::HOST, host.parse().unwrap());
            if let Some(origin) = origin {
                map.insert(header::ORIGIN, origin.parse().unwrap());
            }
            map
        };
        assert!(is_same_origin(&headers(None, "c.example")));
        assert!(is_same_origin(&headers(Some("https://c.example"), "c.example")));
        assert!(is_same_origin(&headers(Some("http://127.0.0.1:8080"), "127.0.0.1:8080")));
        assert!(!is_same_origin(&headers(Some("https://evil.example"), "c.example")));
        assert!(!is_same_origin(&headers(Some("http://127.0.0.1:9999"), "127.0.0.1:8080")));
        assert!(!is_same_origin(&headers(Some("null"), "c.example")));
    }

    #[test]
    fn throttle_locks_after_repeated_failures_and_clears_on_success() {
        let throttle = LoginThrottle::default();
        for _ in 0..MAX_FAILED_LOGINS - 1 {
            throttle.record_failure("Alice");
            assert!(throttle.allowed("alice"));
        }
        throttle.record_failure("alice");
        assert!(!throttle.allowed("ALICE"));
        assert!(throttle.allowed("bob"));
        throttle.record_success("alice");
        assert!(throttle.allowed("alice"));
    }
}
