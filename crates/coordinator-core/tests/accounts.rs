//! Accounts, sessions and the request guards.

mod common;

use common::{FakeClient, TestServer, API_TOKEN};
use coordinator_core::admin;
use serde_json::{json, Value};

const USER: &str = "operator";
const PASSWORD: &str = "fixture-password-1";

async fn login(server: &TestServer, username: &str, password: &str) -> reqwest::Response {
    server
        .http
        .post(server.url("/api/v1/auth/login"))
        .json(&json!({"username": username, "password": password}))
        .send()
        .await
        .unwrap()
}

async fn session(server: &TestServer) -> Value {
    server.http.get(server.url("/api/v1/session")).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn login_opens_a_session_and_logout_ends_it() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;

    let before = session(&server).await;
    assert_eq!(before["authenticated"], false);
    assert_eq!(before["auth_mode"], "accounts");
    assert_eq!(before["capabilities"]["accounts"], true);
    assert_eq!(before["capabilities"]["hotspot"], false);
    assert_eq!(server.http.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 401);

    let response = login(&server, USER, PASSWORD).await;
    assert_eq!(response.status(), 200);
    let cookie = response.headers()["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("sc_session="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(!cookie.contains("Secure"), "{cookie}");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["account"]["username"], USER);

    let after = session(&server).await;
    assert_eq!(after["authenticated"], true);
    assert_eq!(after["account"]["username"], USER);
    assert_eq!(server.http.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 200);

    let response = server.http.post(server.url("/api/v1/auth/logout")).send().await.unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(session(&server).await["authenticated"], false);
    assert_eq!(server.http.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn wrong_credentials_are_refused_and_repeated_failures_are_throttled() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;

    let response = login(&server, USER, "not-the-password").await;
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_credentials");
    // Same answer for an account that does not exist.
    let response = login(&server, "nobody", PASSWORD).await;
    assert_eq!(response.status(), 401);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "invalid_credentials");

    for _ in 0..4 {
        assert_eq!(login(&server, USER, "not-the-password").await.status(), 401);
    }
    // Five failures in a row: even the right password has to wait now.
    let response = login(&server, USER, PASSWORD).await;
    assert_eq!(response.status(), 429);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "too_many_attempts");
    assert_eq!(session(&server).await["authenticated"], false);
}

#[tokio::test]
async fn the_bearer_token_means_nothing_in_accounts_mode() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;
    let response =
        server.http.get(server.url("/api/v1/devices")).bearer_auth(API_TOKEN).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let response = server
        .http
        .post(server.url("/api/v1/auth/token"))
        .json(&json!({"token": API_TOKEN}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "wrong_auth_mode");
}

#[tokio::test]
async fn token_mode_exchanges_the_token_for_a_session_cookie() {
    let server = TestServer::start().await;
    let info = session(&server).await;
    assert_eq!(info["auth_mode"], "token");
    assert_eq!(info["authenticated"], false);
    assert_eq!(info["capabilities"]["accounts"], false);

    let response = server
        .http
        .post(server.url("/api/v1/auth/token"))
        .json(&json!({"token": "not-the-token"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "invalid_token");

    let response = server
        .http
        .post(server.url("/api/v1/auth/token"))
        .json(&json!({"token": API_TOKEN}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    // The cookie alone now authenticates, with no account behind it.
    let info = session(&server).await;
    assert_eq!(info["authenticated"], true);
    assert!(info["account"].is_null());
    assert_eq!(server.http.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 200);

    // Password login and account management do not exist in this mode.
    assert_eq!(login(&server, USER, PASSWORD).await.status(), 400);
    assert_eq!(server.http.get(server.url("/api/v1/accounts")).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn token_sessions_do_not_survive_a_restart() {
    let mut server = TestServer::start().await;
    let response = server
        .http
        .post(server.url("/api/v1/auth/token"))
        .json(&json!({"token": API_TOKEN}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(session(&server).await["authenticated"], true);
    server.restart().await;
    assert_eq!(session(&server).await["authenticated"], false);
}

#[tokio::test]
async fn account_sessions_survive_a_restart() {
    let mut server = TestServer::start_accounts(USER, PASSWORD).await;
    assert_eq!(login(&server, USER, PASSWORD).await.status(), 200);
    server.restart().await;
    assert_eq!(session(&server).await["authenticated"], true);
}

#[tokio::test]
async fn cross_origin_requests_that_change_state_are_refused() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;
    assert_eq!(login(&server, USER, PASSWORD).await.status(), 200);
    let job = json!({"kind": "script.run", "payload": {"shell": "cmd", "body": "dir"}, "targets": {"type": "all"}});

    let response = server
        .http
        .post(server.url("/api/v1/jobs"))
        .header("Origin", "https://evil.example")
        .json(&job)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "cross_origin");

    // Login itself is protected the same way.
    let response = server
        .http
        .post(server.url("/api/v1/auth/login"))
        .header("Origin", "https://evil.example")
        .json(&json!({"username": USER, "password": PASSWORD}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);

    // The same request from the coordinator's own origin goes through to
    // validation (there are no devices, hence no targets).
    let response = server
        .http
        .post(server.url("/api/v1/jobs"))
        .header("Origin", format!("http://{}", server.addr))
        .json(&job)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "no_targets");
}

#[tokio::test]
async fn accounts_can_be_managed_through_the_api() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;
    assert_eq!(login(&server, USER, PASSWORD).await.status(), 200);
    let me = session(&server).await["account"]["id"].as_str().unwrap().to_owned();

    let (status, body) = server.post("/api/v1/accounts", json!({"username": "second", "password": "another-fixture-pw"})).await;
    assert_eq!(status, 201, "{body}");
    let second = body["account"]["id"].as_str().unwrap().to_owned();
    assert!(body["account"].get("password_hash").is_none());

    for (request, code, status) in [
        (json!({"username": "second", "password": "another-fixture-pw"}), "username_taken", 409),
        (json!({"username": "SECOND", "password": "another-fixture-pw"}), "username_taken", 409),
        (json!({"username": "bad name", "password": "another-fixture-pw"}), "invalid_username", 400),
        (json!({"username": "third", "password": "short"}), "weak_password", 400),
    ] {
        let (got, body) = server.post("/api/v1/accounts", request.clone()).await;
        assert_eq!(got, status, "{request} -> {body}");
        assert_eq!(body["error"]["code"], code);
    }

    let (_, body) = server.get("/api/v1/accounts").await;
    let names: Vec<&str> =
        body["accounts"].as_array().unwrap().iter().map(|a| a["username"].as_str().unwrap()).collect();
    assert_eq!(names, [USER, "second"]);

    // The new account can log in; changing its password ends that session.
    let other = TestServer::client();
    let url = server.url("/api/v1/auth/login");
    let response = other.post(&url).json(&json!({"username": "second", "password": "another-fixture-pw"})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let (status, _) = server.post(&format!("/api/v1/accounts/{second}/password"), json!({"password": "changed-fixture-pw"})).await;
    assert_eq!(status, 204);
    assert_eq!(other.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 401);
    let response = other.post(&url).json(&json!({"username": "second", "password": "another-fixture-pw"})).send().await.unwrap();
    assert_eq!(response.status(), 401);
    let response = other.post(&url).json(&json!({"username": "second", "password": "changed-fixture-pw"})).send().await.unwrap();
    assert_eq!(response.status(), 200);

    // Nobody deletes their own account; others can be deleted.
    let response = server.http.delete(server.url(&format!("/api/v1/accounts/{me}"))).send().await.unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.json::<Value>().await.unwrap()["error"]["code"], "cannot_delete_self");
    let response = server.http.delete(server.url(&format!("/api/v1/accounts/{second}"))).send().await.unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(other.get(server.url("/api/v1/devices")).send().await.unwrap().status(), 401);
    let response = server.http.delete(server.url(&format!("/api/v1/accounts/{second}"))).send().await.unwrap();
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn jobs_record_who_created_them() {
    let server = TestServer::start_accounts(USER, PASSWORD).await;
    let key = server.new_key();
    let _client = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(login(&server, USER, PASSWORD).await.status(), 200);
    let job = server.create_script_job(json!({"type": "all"})).await;
    assert_eq!(job["created_by"], USER);
}

#[tokio::test]
async fn admin_functions_manage_accounts_on_a_data_directory() {
    let dir = tempfile::tempdir().unwrap();
    admin::create_account(dir.path(), "alice", "fixture-password-1").unwrap();
    assert!(matches!(
        admin::create_account(dir.path(), "alice", "fixture-password-2"),
        Err(admin::AccountError::UsernameTaken)
    ));
    assert!(matches!(
        admin::create_account(dir.path(), "bob", "short"),
        Err(admin::AccountError::WeakPassword)
    ));
    assert!(matches!(
        admin::create_account(dir.path(), "not valid", "fixture-password-1"),
        Err(admin::AccountError::InvalidUsername)
    ));
    admin::set_account_password(dir.path(), "alice", "fixture-password-3").unwrap();
    assert!(matches!(
        admin::set_account_password(dir.path(), "nobody", "fixture-password-3"),
        Err(admin::AccountError::NotFound)
    ));
    assert_eq!(admin::list_accounts(dir.path()).unwrap().len(), 1);
    admin::delete_account(dir.path(), "alice").unwrap();
    assert!(admin::list_accounts(dir.path()).unwrap().is_empty());
}
