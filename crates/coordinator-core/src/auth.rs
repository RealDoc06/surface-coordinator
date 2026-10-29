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
pub const MAX_PASSWORD_LEN: usize = 1024;
const MAX_FAILED_LOGINS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(30);
const MAX_THROTTLED_USERS: usize = 4096;

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
    password.len() <= MAX_PASSWORD_LEN && password.chars().count() >= MIN_PASSWORD_LEN
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

/// Whether a session opened for `account` (`None`: opened with the token)
/// counts under `mode`. The same data directory can be served in either
/// mode, and a session must not outlive a switch: an account's cookie is
/// meaningless where the token is the only credential, and a token session
/// is not a login.
pub fn session_fits_mode(mode: &AuthMode, account: &Option<Account>) -> bool {
    match mode {
        AuthMode::Accounts => account.is_some(),
        AuthMode::Token(_) => account.is_none(),
    }
}

/// Works out who sent these headers, if anyone we know.
pub async fn identify(state: &AppState, headers: &HeaderMap) -> Result<Option<Identity>, crate::Error> {
    if let Some(token) = session_token_from(headers) {
        let hash = hash_token(token);
        if let Some(account) = state.store.call(move |conn| store::get_session(conn, &hash)).await? {
            if session_fits_mode(&state.config.auth, &account) {
                return Ok(Some(Identity { account }));
            }
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
    state: Mutex<ThrottleState>,
}

#[derive(Default)]
struct ThrottleState {
    entries: HashMap<String, Attempts>,
    last_pruned: Option<Instant>,
}

struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last: Instant,
}

impl LoginThrottle {
    fn state(&self) -> std::sync::MutexGuard<'_, ThrottleState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether a login for this username may be attempted now.
    pub fn allowed(&self, username: &str) -> bool {
        let now = Instant::now();
        let mut state = self.state();
        // Bound both the table and pruning work when callers rotate usernames.
        if state.last_pruned.is_none_or(|last| now.duration_since(last) >= LOCKOUT) {
            state.entries.retain(|_, attempts| now.duration_since(attempts.last) < LOCKOUT * 20);
            state.last_pruned = Some(now);
        }
        match state.entries.get(&username.to_ascii_lowercase()) {
            Some(Attempts { locked_until: Some(until), .. }) => now >= *until,
            Some(_) => true,
            None => state.entries.len() < MAX_THROTTLED_USERS,
        }
    }

    pub fn record_failure(&self, username: &str) {
        let now = Instant::now();
        let mut state = self.state();
        let username = username.to_ascii_lowercase();
        if state.entries.len() >= MAX_THROTTLED_USERS && !state.entries.contains_key(&username) {
            return;
        }
        let attempts = state.entries
            .entry(username)
            .or_insert(Attempts { failures: 0, locked_until: None, last: now });
        attempts.last = now;
        attempts.failures += 1;
        if attempts.failures >= MAX_FAILED_LOGINS {
            attempts.failures = 0;
            attempts.locked_until = Some(now + LOCKOUT);
        }
    }

    pub fn record_success(&self, username: &str) {
        self.state().entries.remove(&username.to_ascii_lowercase());
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
    fn a_session_only_counts_in_the_mode_it_was_opened_in() {
        let account = Some(Account { id: "a".into(), username: "operator".into(), created_at: 0 });
        let token = AuthMode::Token("fixture-token".into());
        assert!(session_fits_mode(&AuthMode::Accounts, &account));
        assert!(!session_fits_mode(&AuthMode::Accounts, &None));
        assert!(session_fits_mode(&token, &None));
        assert!(!session_fits_mode(&token, &account));
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

    #[test]
    fn throttle_bounds_rotating_usernames_and_recovers_stale_capacity() {
        let throttle = LoginThrottle::default();
        for index in 0..MAX_THROTTLED_USERS + 1 {
            throttle.record_failure(&format!("user-{index}"));
        }
        assert_eq!(throttle.state().entries.len(), MAX_THROTTLED_USERS);
        assert!(!throttle.allowed("another-user"));
        assert!(throttle.allowed("user-0"));
        {
            let mut state = throttle.state();
            for attempts in state.entries.values_mut() {
                attempts.last = Instant::now() - LOCKOUT * 21;
            }
            state.last_pruned = None;
        }
        assert!(throttle.allowed("another-user"));
        assert!(throttle.state().entries.is_empty());
    }

    #[test]
    fn a_password_reset_or_account_deletion_wins_over_an_in_flight_login() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::Store::open(dir.path()).unwrap();
        store.call_blocking(|conn| {
            let account = store::create_account(conn, "operator", "old-hash")?.unwrap();
            let expires = store::now_ms() + 60_000;
            assert!(store::create_account_session(conn, "first", &account.id, "old-hash", expires)?);
            store::set_account_password(conn, &account.id, "new-hash")?;
            assert!(store::get_session(conn, "first")?.is_none());
            assert!(!store::create_account_session(conn, "racing-login", &account.id, "old-hash", expires)?);
            assert!(store::create_account_session(conn, "new-login", &account.id, "new-hash", expires)?);
            store::delete_account(conn, &account.id)?;
            assert!(!store::create_account_session(conn, "deleted-login", &account.id, "new-hash", expires)?);
            Ok(())
        }).unwrap();
    }
}
