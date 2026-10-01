//! The script library: the HTTP API and the built-in scripts.

mod common;

use common::TestServer;
use serde_json::{json, Value};

const BUILT_IN_COUNT: usize = 5;

async fn scripts(server: &TestServer) -> Vec<Value> {
    let (status, body) = server.get("/api/v1/scripts").await;
    assert_eq!(status, 200, "{body}");
    body["scripts"].as_array().unwrap().clone()
}

fn user_scripts(all: &[Value]) -> Vec<&Value> {
    all.iter().filter(|s| s["built_in"] == false).collect()
}

fn built_ins(all: &[Value]) -> Vec<&Value> {
    all.iter().filter(|s| s["built_in"] == true).collect()
}

async fn create(server: &TestServer, name: &str, shell: &str, body: &str) -> Value {
    let (status, response) =
        server.post("/api/v1/scripts", json!({"name": name, "shell": shell, "body": body})).await;
    assert_eq!(status, 201, "{response}");
    response["script"].clone()
}

/// Opens the database of a stopped server.
fn database(server: &TestServer) -> rusqlite::Connection {
    rusqlite::Connection::open(server.data_dir.path().join("coordinator.db")).unwrap()
}

#[tokio::test]
async fn script_routes_need_authentication() {
    let server = TestServer::start().await;
    let script = create(&server, "Mine", "cmd", "dir").await;
    let id = script["id"].as_str().unwrap();
    let anonymous = TestServer::client();

    let requests = [
        (reqwest::Method::GET, "/api/v1/scripts".to_owned()),
        (reqwest::Method::POST, "/api/v1/scripts".to_owned()),
        (reqwest::Method::PUT, format!("/api/v1/scripts/{id}")),
        (reqwest::Method::DELETE, format!("/api/v1/scripts/{id}")),
    ];
    for (method, path) in requests {
        let response = anonymous
            .request(method.clone(), server.url(&path))
            .json(&json!({"name": "Hacked", "shell": "cmd", "body": "del *"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{method} {path}");
    }
    assert_eq!(user_scripts(&scripts(&server).await).len(), 1);
    assert_eq!(scripts(&server).await.iter().find(|s| s["id"] == id).unwrap()["body"], "dir");
}

#[tokio::test]
async fn built_in_scripts_are_there_from_the_start() {
    let server = TestServer::start().await;
    let all = scripts(&server).await;
    assert_eq!(all.len(), BUILT_IN_COUNT);
    for script in &all {
        assert_eq!(script["built_in"], true);
        assert!(script["id"].as_str().unwrap().starts_with("builtin-"));
        assert_eq!(script["shell"], "powershell");
        assert!(!script["name"].as_str().unwrap().trim().is_empty());
        assert!(!script["body"].as_str().unwrap().trim().is_empty());
        assert!(script["created_at"].as_i64().unwrap() > 0);
        assert!(script["updated_at"].as_i64().unwrap() >= script["created_at"].as_i64().unwrap());
    }
    let names: Vec<String> = all.iter().map(|s| s["name"].as_str().unwrap().to_lowercase()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "ordered by name");
    // They are what the operator expects to find.
    let text = all.iter().map(|s| s["name"].as_str().unwrap().to_lowercase()).collect::<Vec<_>>().join("|");
    for wanted in ["system summary", "disk space", "network", "updates", "battery"] {
        assert!(text.contains(wanted), "no built-in script about {wanted:?} in {text}");
    }
}

#[tokio::test]
async fn built_in_scripts_survive_restarts_without_duplicates() {
    let mut server = TestServer::start().await;
    let before = scripts(&server).await;
    server.restart().await;
    server.restart().await;
    // Unchanged, including the timestamps: a restart is not an edit.
    assert_eq!(scripts(&server).await, before);
}

#[tokio::test]
async fn a_newer_version_refreshes_the_built_in_scripts_but_not_the_users_own() {
    let mut server = TestServer::start().await;
    let mine = create(&server, "Mine", "cmd", "dir").await;
    server.stop().await;
    let connection = database(&server);
    connection
        .execute(
            "UPDATE scripts SET name = 'Old disk name', body = 'old body', updated = 1
             WHERE id = 'builtin-disk-space'",
            [],
        )
        .unwrap();
    drop(connection);
    server.restart().await;

    let all = scripts(&server).await;
    assert_eq!(built_ins(&all).len(), BUILT_IN_COUNT);
    let disk = all.iter().find(|s| s["id"] == "builtin-disk-space").unwrap();
    assert_eq!(disk["name"], "Disk space");
    assert_ne!(disk["body"], "old body");
    assert!(disk["updated_at"].as_i64().unwrap() > 1);
    assert_eq!(all.iter().find(|s| s["id"] == mine["id"]).unwrap()["body"], "dir");
}

#[tokio::test]
async fn a_user_script_with_a_built_in_name_does_not_stop_the_server_from_starting() {
    let mut server = TestServer::start().await;
    server.stop().await;
    let connection = database(&server);
    // A user script took the name an older built-in had, and the built-in was
    // renamed in this version.
    connection
        .execute("UPDATE scripts SET name = 'Previous disk name' WHERE id = 'builtin-disk-space'", [])
        .unwrap();
    connection
        .execute(
            "INSERT INTO scripts (id, name, shell, body, built_in, created, updated)
             VALUES ('user-1', 'disk SPACE', 'cmd', 'wmic logicaldisk', 0, 1, 1)",
            [],
        )
        .unwrap();
    drop(connection);
    server.restart().await;

    let all = scripts(&server).await;
    let user = all.iter().find(|s| s["id"] == "user-1").unwrap();
    assert_eq!((user["name"].as_str(), user["body"].as_str()), (Some("disk SPACE"), Some("wmic logicaldisk")));
    assert_eq!(built_ins(&all).len(), BUILT_IN_COUNT, "the other built-ins are there, and this one is kept");
}

#[tokio::test]
async fn a_script_is_created_and_listed_by_name() {
    let server = TestServer::start().await;
    let (status, body) = server
        .post("/api/v1/scripts", json!({"name": "  Zip logs ", "shell": "powershell", "body": "Compress-Archive x y"}))
        .await;
    assert_eq!(status, 201, "{body}");
    let script = &body["script"];
    assert_eq!(script["name"], "Zip logs");
    assert_eq!(script["shell"], "powershell");
    assert_eq!(script["body"], "Compress-Archive x y");
    assert_eq!(script["built_in"], false);
    assert!(script["id"].as_str().is_some_and(|id| !id.is_empty() && !id.starts_with("builtin-")));
    assert!(script["created_at"].as_i64().unwrap() > 0);
    assert_eq!(script["updated_at"], script["created_at"]);

    create(&server, "alpha cleanup", "cmd", "del /q %TEMP%\\*").await;
    let all = scripts(&server).await;
    let names: Vec<String> = all.iter().map(|s| s["name"].as_str().unwrap().to_lowercase()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    assert_eq!(user_scripts(&all).len(), 2);
    assert_eq!(all.len(), BUILT_IN_COUNT + 2);
}

#[tokio::test]
async fn script_input_is_validated() {
    let server = TestServer::start().await;
    let long_name = "n".repeat(101);
    let too_big = "x".repeat(256 * 1024 + 1);
    let cases = [
        json!({"name": "", "shell": "cmd", "body": "dir"}),
        json!({"name": "   ", "shell": "cmd", "body": "dir"}),
        json!({"name": long_name, "shell": "cmd", "body": "dir"}),
        json!({"name": "ok", "shell": "cmd", "body": ""}),
        json!({"name": "ok", "shell": "cmd", "body": " \r\n\t"}),
        json!({"name": "ok", "shell": "cmd", "body": too_big}),
        json!({"name": "ok", "shell": "bash", "body": "ls"}),
        json!({"name": "ok", "shell": "PowerShell", "body": "ls"}),
        json!({"name": "ok", "shell": "", "body": "ls"}),
        json!({"name": "ok", "body": "ls"}),
        json!({"shell": "cmd", "body": "dir"}),
        json!({"name": "ok", "shell": "cmd"}),
        json!({"name": 5, "shell": "cmd", "body": "dir"}),
        json!({"name": "ok", "shell": "cmd", "body": ["dir"]}),
        json!({}),
    ];
    for case in cases {
        let (status, body) = server.post("/api/v1/scripts", case.clone()).await;
        let shown: String = case.to_string().chars().take(120).collect();
        assert_eq!(status, 400, "{shown} -> {body}");
        assert_eq!(body["error"]["code"], "invalid_script", "{shown}");
        assert!(body["error"]["message"].is_string());
    }
    assert!(user_scripts(&scripts(&server).await).is_empty());

    // The limits themselves are allowed; the name counts characters.
    create(&server, &"\u{e9}".repeat(100), "cmd", &"x".repeat(256 * 1024)).await;
}

#[tokio::test]
async fn script_names_are_unique_ignoring_case_and_cannot_take_a_built_in_name() {
    let server = TestServer::start().await;
    let first = create(&server, "Cleanup", "cmd", "dir").await;
    for name in ["cleanup", " CLEANUP ", "Disk space"] {
        let (status, body) =
            server.post("/api/v1/scripts", json!({"name": name, "shell": "cmd", "body": "dir"})).await;
        assert_eq!(status, 409, "{name} -> {body}");
        assert_eq!(body["error"]["code"], "name_taken");
    }
    let second = create(&server, "Other", "cmd", "dir").await;

    let id = second["id"].as_str().unwrap();
    let (status, body) = server
        .put(&format!("/api/v1/scripts/{id}"), json!({"name": "cleanup", "shell": "cmd", "body": "dir"}))
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["code"], "name_taken");

    // A script may change its own name's capitalisation.
    let id = first["id"].as_str().unwrap();
    let (status, body) = server
        .put(&format!("/api/v1/scripts/{id}"), json!({"name": "CLEANUP", "shell": "cmd", "body": "dir"}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["script"]["name"], "CLEANUP");
}

#[tokio::test]
async fn a_script_is_replaced_as_a_whole() {
    let server = TestServer::start().await;
    let script = create(&server, "Cleanup", "cmd", "dir").await;
    let id = script["id"].as_str().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    let (status, body) = server
        .put(&format!("/api/v1/scripts/{id}"), json!({"name": " Tidy up ", "shell": "powershell", "body": "Get-Date"}))
        .await;
    assert_eq!(status, 200, "{body}");
    let updated = &body["script"];
    assert_eq!(updated["id"], id);
    assert_eq!(updated["name"], "Tidy up");
    assert_eq!(updated["shell"], "powershell");
    assert_eq!(updated["body"], "Get-Date");
    assert_eq!(updated["built_in"], false);
    assert_eq!(updated["created_at"], script["created_at"]);
    assert!(updated["updated_at"].as_i64().unwrap() > script["created_at"].as_i64().unwrap());
    let listed = scripts(&server).await;
    assert_eq!(listed.iter().find(|s| s["id"] == id).unwrap(), updated);

    let (status, body) = server
        .put(&format!("/api/v1/scripts/{id}"), json!({"name": "Tidy up", "shell": "cmd", "body": ""}))
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_script");
    assert_eq!(scripts(&server).await.iter().find(|s| s["id"] == id).unwrap()["body"], "Get-Date");

    let (status, body) = server
        .put("/api/v1/scripts/no-such-script", json!({"name": "x", "shell": "cmd", "body": "dir"}))
        .await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn a_script_can_be_deleted() {
    let server = TestServer::start().await;
    let script = create(&server, "Cleanup", "cmd", "dir").await;
    let id = script["id"].as_str().unwrap();

    let (status, _) = server.delete(&format!("/api/v1/scripts/{id}")).await;
    assert_eq!(status, 204);
    assert!(user_scripts(&scripts(&server).await).is_empty());
    let (status, body) = server.delete(&format!("/api/v1/scripts/{id}")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
    create(&server, "Cleanup", "cmd", "dir").await;
}

#[tokio::test]
async fn built_in_scripts_can_be_neither_changed_nor_deleted() {
    let server = TestServer::start().await;
    let before = scripts(&server).await;
    for script in &before {
        let id = script["id"].as_str().unwrap();
        let (status, body) = server
            .put(&format!("/api/v1/scripts/{id}"), json!({"name": "Mine now", "shell": "cmd", "body": "dir"}))
            .await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "built_in");
        let (status, body) = server.delete(&format!("/api/v1/scripts/{id}")).await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(body["error"]["code"], "built_in");
    }
    assert_eq!(scripts(&server).await, before);
}
