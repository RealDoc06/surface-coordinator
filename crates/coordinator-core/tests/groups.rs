//! Device groups: the HTTP API, membership, and what forgetting a device does.

mod common;

use common::{FakeClient, TestServer};
use serde_json::{json, Value};

fn ids(group: &Value) -> Vec<&str> {
    group["device_ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap()).collect()
}

async fn groups(server: &TestServer) -> Vec<Value> {
    let (status, body) = server.get("/api/v1/groups").await;
    assert_eq!(status, 200, "{body}");
    body["groups"].as_array().unwrap().clone()
}

async fn group_names(server: &TestServer) -> Vec<String> {
    groups(server).await.iter().map(|g| g["name"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn group_routes_need_authentication() {
    let server = TestServer::start().await;
    let group = server.create_group("Classroom").await;
    let id = group["id"].as_str().unwrap();
    let anonymous = TestServer::client();

    let requests = [
        (reqwest::Method::GET, "/api/v1/groups".to_owned()),
        (reqwest::Method::POST, "/api/v1/groups".to_owned()),
        (reqwest::Method::PATCH, format!("/api/v1/groups/{id}")),
        (reqwest::Method::DELETE, format!("/api/v1/groups/{id}")),
        (reqwest::Method::PUT, format!("/api/v1/groups/{id}/members")),
    ];
    for (method, path) in requests {
        let response = anonymous
            .request(method.clone(), server.url(&path))
            .json(&json!({"name": "Hacked", "device_ids": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{method} {path}");
    }
    assert_eq!(group_names(&server).await, ["Classroom"]);
}

#[tokio::test]
async fn groups_are_created_listed_by_name_and_start_empty() {
    let server = TestServer::start().await;
    assert!(groups(&server).await.is_empty());

    let (status, body) = server.post("/api/v1/groups", json!({"name": "Room 2"})).await;
    assert_eq!(status, 201, "{body}");
    let group = &body["group"];
    assert_eq!(group["name"], "Room 2");
    assert!(group["id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(group["created_at"].as_i64().is_some_and(|at| at > 0));
    assert_eq!(group["device_ids"], json!([]));

    server.create_group("room 10").await;
    server.create_group("Cart").await;
    // Alphabetical, ignoring case.
    assert_eq!(group_names(&server).await, ["Cart", "room 10", "Room 2"]);
}

#[tokio::test]
async fn group_names_are_trimmed_limited_and_unique_ignoring_case() {
    let server = TestServer::start().await;

    let group = server.create_group("  Front desk \t").await;
    assert_eq!(group["name"], "Front desk");

    for name in [json!(""), json!("   "), json!("x".repeat(65)), json!(5), json!(null)] {
        let (status, body) = server.post("/api/v1/groups", json!({"name": name})).await;
        assert_eq!(status, 400, "{name} -> {body}");
        assert_eq!(body["error"]["code"], "invalid_name");
    }
    let (status, body) = server.post("/api/v1/groups", json!({})).await;
    assert_eq!((status, body["error"]["code"].as_str()), (400, Some("invalid_name")));

    // 64 characters is fine, counted as characters rather than bytes.
    server.create_group(&"\u{e9}".repeat(64)).await;

    for name in ["front desk", "FRONT DESK", " Front Desk "] {
        let (status, body) = server.post("/api/v1/groups", json!({"name": name})).await;
        assert_eq!(status, 409, "{name} -> {body}");
        assert_eq!(body["error"]["code"], "name_taken");
    }
    assert_eq!(groups(&server).await.len(), 2);
}

#[tokio::test]
async fn a_group_can_be_renamed() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    let group = server.create_group("Old name").await;
    let id = group["id"].as_str().unwrap();
    server.set_members(id, &["device-a"]).await;
    server.create_group("Taken").await;

    let (status, body) = server.patch(&format!("/api/v1/groups/{id}"), json!({"name": "  New name "})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["group"]["id"], id);
    assert_eq!(body["group"]["name"], "New name");
    assert_eq!(body["group"]["created_at"], group["created_at"]);
    assert_eq!(ids(&body["group"]), ["device-a"], "members are kept");

    // Only the capitalisation may change, since it is the group's own name.
    let (status, body) = server.patch(&format!("/api/v1/groups/{id}"), json!({"name": "NEW NAME"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["group"]["name"], "NEW NAME");

    let (status, body) = server.patch(&format!("/api/v1/groups/{id}"), json!({"name": "taken"})).await;
    assert_eq!(status, 409);
    assert_eq!(body["error"]["code"], "name_taken");
    let (status, body) = server.patch(&format!("/api/v1/groups/{id}"), json!({"name": " "})).await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_name");
    let (status, body) = server.patch("/api/v1/groups/no-such-group", json!({"name": "Whatever"})).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");

    assert_eq!(group_names(&server).await, ["NEW NAME", "Taken"]);
}

#[tokio::test]
async fn membership_is_replaced_not_added_to_and_duplicates_are_ignored() {
    let server = TestServer::start().await;
    let key = server.new_key();
    for id in ["device-c", "device-a", "device-b"] {
        FakeClient::connect(&server, &key, id).await;
    }
    let group = server.create_group("Lab").await;
    let id = group["id"].as_str().unwrap();

    let group = server.set_members(id, &["device-c", "device-a", "device-c"]).await;
    assert_eq!(ids(&group), ["device-a", "device-c"], "sorted, without duplicates");

    let group = server.set_members(id, &["device-b"]).await;
    assert_eq!(ids(&group), ["device-b"]);
    assert_eq!(ids(&groups(&server).await[0]), ["device-b"]);

    let group = server.set_members(id, &[]).await;
    assert!(ids(&group).is_empty());
}

#[tokio::test]
async fn an_unknown_device_changes_nothing_and_an_unknown_group_is_404() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    let group = server.create_group("Lab").await;
    let id = group["id"].as_str().unwrap();
    server.set_members(id, &["device-a"]).await;

    let (status, body) = server
        .put(&format!("/api/v1/groups/{id}/members"), json!({"device_ids": ["device-a", "no-such-device"]}))
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "unknown_device");
    assert!(body["error"]["message"].as_str().unwrap().contains("no-such-device"));
    assert_eq!(ids(&groups(&server).await[0]), ["device-a"]);

    let (status, body) = server.put("/api/v1/groups/no-such-group/members", json!({"device_ids": []})).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn devices_list_the_groups_they_belong_to() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    let _b = FakeClient::connect(&server, &key, "device-b").await;
    let first = server.create_group("First").await;
    let second = server.create_group("Second").await;
    let (first_id, second_id) = (first["id"].as_str().unwrap(), second["id"].as_str().unwrap());
    server.set_members(first_id, &["device-a"]).await;
    server.set_members(second_id, &["device-a", "device-b"]).await;

    let mut expected = vec![first_id, second_id];
    expected.sort_unstable();
    let device = server.device("device-a").await.unwrap();
    assert_eq!(device["group_ids"], json!(expected), "sorted");
    assert_eq!(server.device("device-b").await.unwrap()["group_ids"], json!([second_id]));

    let (status, body) = server.get("/api/v1/devices/device-a").await;
    assert_eq!(status, 200);
    assert_eq!(body["device"]["group_ids"], json!(expected));

    server.set_members(first_id, &[]).await;
    assert_eq!(server.device("device-a").await.unwrap()["group_ids"], json!([second_id]));
}

#[tokio::test]
async fn a_device_without_groups_has_an_empty_list() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(server.device("device-a").await.unwrap()["group_ids"], json!([]));
}

#[tokio::test]
async fn deleting_a_group_keeps_its_devices() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    let group = server.create_group("Lab").await;
    let id = group["id"].as_str().unwrap();
    server.set_members(id, &["device-a"]).await;

    let (status, _) = server.delete(&format!("/api/v1/groups/{id}")).await;
    assert_eq!(status, 204);
    assert!(groups(&server).await.is_empty());
    assert_eq!(server.device("device-a").await.unwrap()["group_ids"], json!([]));

    let (status, body) = server.delete(&format!("/api/v1/groups/{id}")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"]["code"], "not_found");

    // The name is free again.
    server.create_group("Lab").await;
}

#[tokio::test]
async fn forgetting_a_device_removes_it_from_its_groups() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let _a = FakeClient::connect(&server, &key, "device-a").await;
    let _b = FakeClient::connect(&server, &key, "device-b").await;
    let group = server.create_group("Lab").await;
    let id = group["id"].as_str().unwrap();
    server.set_members(id, &["device-a", "device-b"]).await;

    let (status, _) = server.delete("/api/v1/devices/device-a").await;
    assert_eq!(status, 204);
    assert_eq!(ids(&groups(&server).await[0]), ["device-b"]);

    // A device that enrolls again under the same id starts without groups.
    let _again = FakeClient::connect(&server, &key, "device-a").await;
    assert_eq!(server.device("device-a").await.unwrap()["group_ids"], json!([]));
    assert_eq!(ids(&groups(&server).await[0]), ["device-b"]);
}

#[tokio::test]
async fn groups_and_memberships_survive_a_restart() {
    let mut server = TestServer::start().await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    let group = server.create_group("Lab").await;
    let id = group["id"].as_str().unwrap().to_owned();
    server.set_members(&id, &["device-a"]).await;

    server.restart().await;
    let listed = groups(&server).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(ids(&listed[0]), ["device-a"]);
    assert_eq!(server.device("device-a").await.unwrap()["group_ids"], json!([id]));
}
