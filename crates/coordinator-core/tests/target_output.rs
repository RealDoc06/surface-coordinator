//! Job metadata can be loaded without copying every device's output.

mod common;

use common::{FakeClient, TestServer};
use serde_json::json;

#[tokio::test]
async fn metadata_omits_output_while_default_details_and_target_output_preserve_it() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut first = FakeClient::connect(&server, &key, "device-a").await;
    let mut second = FakeClient::connect(&server, &key, "device-b").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let id = job["id"].as_str().unwrap();
    let (first_target, ..) = first.expect_job().await;
    let (second_target, ..) = second.expect_job().await;
    for (client, target, output) in [
        (&mut first, &first_target, "device a: café 🧪\n"),
        (&mut second, &second_target, "device b: separate output\n"),
    ] {
        client.started(target).await;
        client.output(target, 1, output).await;
        client.result(target, 0).await;
        client.expect_result_ack(target).await;
        let (status, body) = server.get(&format!("/api/v1/jobs/{id}/targets/{target}/output")).await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({"output": output, "output_truncated": false}));
    }

    let (status, full) = server.get(&format!("/api/v1/jobs/{id}")).await;
    assert_eq!(status, 200);
    let (status, explicit) = server.get(&format!("/api/v1/jobs/{id}?include_output=true")).await;
    assert_eq!(status, 200);
    assert_eq!(explicit, full);
    let (status, metadata) = server.get(&format!("/api/v1/jobs/{id}?include_output=false")).await;
    assert_eq!(status, 200);
    let mut expected = full;
    for target in expected["job"]["targets"].as_array_mut().unwrap() {
        assert!(target.as_object_mut().unwrap().remove("output").is_some());
    }
    assert_eq!(metadata, expected, "metadata retains all fields except output");
    assert_eq!(server.get("/api/v1/jobs/missing?include_output=false").await.0, 404);
}

#[tokio::test]
async fn target_output_requires_authentication_and_the_matching_job() {
    let server = TestServer::start().await;
    let key = server.new_key();
    FakeClient::connect(&server, &key, "device-a").await.disconnect().await;
    let first = server.create_script_job(json!({"type": "all"})).await;
    let second = server.create_script_job(json!({"type": "all"})).await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    let target = first["targets"][0]["id"].as_str().unwrap();
    let path = format!("/api/v1/jobs/{first_id}/targets/{target}/output");
    let (status, body) = server.get(&path).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"output": "", "output_truncated": false}));

    let anonymous = TestServer::client();
    for path in [&path, &format!("/api/v1/jobs/{first_id}?include_output=false")] {
        assert_eq!(anonymous.get(server.url(path)).send().await.unwrap().status(), 401);
    }
    for path in [
        format!("/api/v1/jobs/{second_id}/targets/{target}/output"),
        format!("/api/v1/jobs/missing/targets/{target}/output"),
        format!("/api/v1/jobs/{first_id}/targets/missing/output"),
    ] {
        let (status, body) = server.get(&path).await;
        assert_eq!(status, 404, "{path}: {body}");
        assert_eq!(body["error"]["code"], "not_found");
    }
}

#[tokio::test]
async fn target_output_reports_the_stored_cap_and_truncation_flag() {
    let server = TestServer::start().await;
    let key = server.new_key();
    let mut client = FakeClient::connect(&server, &key, "device-a").await;
    let job = server.create_script_job(json!({"type": "all"})).await;
    let id = job["id"].as_str().unwrap();
    let (target, ..) = client.expect_job().await;
    client.started(&target).await;
    let chunk = "x".repeat(64 * 1024);
    for seq in 1..=17 {
        client.output(&target, seq, &chunk).await;
    }
    client.result(&target, 0).await;
    client.expect_result_ack(&target).await;
    let (status, body) = server.get(&format!("/api/v1/jobs/{id}/targets/{target}/output")).await;
    assert_eq!(status, 200);
    assert_eq!(body["output_truncated"], true);
    let output = body["output"].as_str().unwrap();
    let marker = "\n[output truncated]\n";
    assert_eq!(output.len(), protocol::MAX_OUTPUT_BYTES + marker.len());
    assert!(output.ends_with(marker));
}
