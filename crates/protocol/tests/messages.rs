use protocol::{
    job_kind, ClientMessage, Hello, JobResult, JobStatus, OutputStream, RunAs, ScriptRunPayload,
    ServerMessage, Shell, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn hello() -> Hello {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        device_id: "device-1".into(),
        hostname: "SURFACE-01".into(),
        os: "windows".into(),
        client_version: "0.1.0".into(),
        key_id: "key-1".into(),
        nonce: "ab".repeat(32),
        network: None,
    }
}

fn all_client_messages() -> Vec<ClientMessage> {
    vec![
        ClientMessage::Hello(hello()),
        ClientMessage::Auth { mac: "cd".repeat(32) },
        ClientMessage::Heartbeat,
        ClientMessage::JobStarted { job_target_id: "t1".into() },
        ClientMessage::JobOutput {
            job_target_id: "t1".into(),
            seq: 7,
            stream: OutputStream::Stderr,
            data: "line one\nünïcödé\n".into(),
        },
        ClientMessage::JobResult(JobResult {
            job_target_id: "t1".into(),
            status: JobStatus::Succeeded,
            exit_code: Some(0),
            reason: None,
            message: None,
        }),
        ClientMessage::JobResult(JobResult {
            job_target_id: "t2".into(),
            status: JobStatus::Failed,
            exit_code: None,
            reason: Some("unsupported".into()),
            message: Some("job kind power is not supported".into()),
        }),
    ]
}

fn all_server_messages() -> Vec<ServerMessage> {
    vec![
        ServerMessage::Challenge { nonce: "01".repeat(32), mac: "02".repeat(32) },
        ServerMessage::Welcome { heartbeat_interval_ms: 30_000, restricted: false },
        ServerMessage::Welcome { heartbeat_interval_ms: 30_000, restricted: true },
        ServerMessage::Reject { reason: "bad_mac".into() },
        ServerMessage::HeartbeatAck,
        ServerMessage::Job {
            job_target_id: "t1".into(),
            job_id: "j1".into(),
            kind: job_kind::SCRIPT_RUN.into(),
            payload: json!({"shell": "powershell", "body": "Get-Date", "timeout_secs": 60, "run_as": "system"}),
        },
        ServerMessage::JobResultAck { job_target_id: "t1".into() },
        ServerMessage::JobCancel { job_target_id: "t1".into() },
    ]
}

#[test]
fn client_messages_round_trip() {
    for message in all_client_messages() {
        let json = message.to_json();
        let decoded = ClientMessage::from_json(&json).unwrap();
        assert_eq!(decoded, message, "round trip of {json}");
    }
}

#[test]
fn server_messages_round_trip() {
    for message in all_server_messages() {
        let json = message.to_json();
        let decoded = ServerMessage::from_json(&json).unwrap();
        assert_eq!(decoded, message, "round trip of {json}");
    }
}

#[test]
fn hello_wire_shape() {
    let value: Value = serde_json::from_str(&ClientMessage::Hello(hello()).to_json()).unwrap();
    assert_eq!(
        value,
        json!({
            "type": "hello",
            "protocol_version": 1,
            "device_id": "device-1",
            "hostname": "SURFACE-01",
            "os": "windows",
            "client_version": "0.1.0",
            "key_id": "key-1",
            "nonce": "ab".repeat(32),
        })
    );
}

#[test]
fn type_tags_on_the_wire() {
    let tag = |json: String| -> String {
        let value: Value = serde_json::from_str(&json).unwrap();
        value["type"].as_str().unwrap().to_owned()
    };
    let client: Vec<String> = all_client_messages().iter().map(|m| tag(m.to_json())).collect();
    assert_eq!(
        client,
        ["hello", "auth", "heartbeat", "job.started", "job.output", "job.result", "job.result"]
    );
    let server: Vec<String> = all_server_messages().iter().map(|m| tag(m.to_json())).collect();
    assert_eq!(
        server,
        ["challenge", "welcome", "welcome", "reject", "heartbeat_ack", "job", "job.result_ack", "job.cancel"]
    );
}

#[test]
fn job_output_and_result_wire_shape() {
    let output = ClientMessage::JobOutput {
        job_target_id: "t1".into(),
        seq: 1,
        stream: OutputStream::Stdout,
        data: "hi\n".into(),
    };
    let value: Value = serde_json::from_str(&output.to_json()).unwrap();
    assert_eq!(
        value,
        json!({"type": "job.output", "job_target_id": "t1", "seq": 1, "stream": "stdout", "data": "hi\n"})
    );

    // Absent optional fields are omitted rather than sent as null.
    let result = ClientMessage::JobResult(JobResult {
        job_target_id: "t1".into(),
        status: JobStatus::Failed,
        exit_code: Some(3),
        reason: Some("nonzero_exit".into()),
        message: None,
    });
    let value: Value = serde_json::from_str(&result.to_json()).unwrap();
    assert_eq!(
        value,
        json!({"type": "job.result", "job_target_id": "t1", "status": "failed", "exit_code": 3, "reason": "nonzero_exit"})
    );
}

#[test]
fn welcome_mentions_the_restricted_state_only_when_it_applies() {
    // An unrestricted welcome looks exactly like it did before the field
    // existed, so older clients see no difference.
    let value: Value =
        serde_json::from_str(&ServerMessage::Welcome { heartbeat_interval_ms: 30_000, restricted: false }.to_json())
            .unwrap();
    assert_eq!(value, json!({"type": "welcome", "heartbeat_interval_ms": 30000}));
    let value: Value =
        serde_json::from_str(&ServerMessage::Welcome { heartbeat_interval_ms: 30_000, restricted: true }.to_json())
            .unwrap();
    assert_eq!(value, json!({"type": "welcome", "heartbeat_interval_ms": 30000, "restricted": true}));
    // A welcome from a coordinator that predates the field is unrestricted.
    assert_eq!(
        ServerMessage::from_json(r#"{"type":"welcome","heartbeat_interval_ms":5}"#).unwrap(),
        ServerMessage::Welcome { heartbeat_interval_ms: 5, restricted: false }
    );
}

#[test]
fn the_current_and_previous_protocol_versions_are_supported() {
    assert!(protocol::SUPPORTED_PROTOCOL_VERSIONS.contains(&PROTOCOL_VERSION));
    assert!(protocol::SUPPORTED_PROTOCOL_VERSIONS.len() <= 2);
    if PROTOCOL_VERSION > 1 {
        assert!(protocol::is_supported(PROTOCOL_VERSION - 1));
    }
    assert!(protocol::is_supported(PROTOCOL_VERSION));
    assert!(!protocol::is_supported(PROTOCOL_VERSION + 1));
    assert!(!protocol::is_supported(0));
}

#[test]
fn unknown_message_types_decode_as_unknown() {
    let client = ClientMessage::from_json(r#"{"type":"telemetry.report","cpu":3}"#).unwrap();
    assert_eq!(client, ClientMessage::Unknown);
    let server = ServerMessage::from_json(r#"{"type":"config.push","settings":{"a":1}}"#).unwrap();
    assert_eq!(server, ServerMessage::Unknown);
}

#[test]
fn unknown_fields_are_ignored() {
    let json = r#"{"type":"welcome","heartbeat_interval_ms":30000,"extra":{"x":1}}"#;
    assert_eq!(
        ServerMessage::from_json(json).unwrap(),
        ServerMessage::Welcome { heartbeat_interval_ms: 30_000, restricted: false }
    );
    let json = r#"{"type":"heartbeat","uptime_secs":12}"#;
    assert_eq!(ClientMessage::from_json(json).unwrap(), ClientMessage::Heartbeat);
}

#[test]
fn malformed_messages_are_errors() {
    assert!(ClientMessage::from_json("not json").is_err());
    assert!(ClientMessage::from_json(r#"{"no_type":true}"#).is_err());
    // A known type with missing required fields is an error, not Unknown.
    assert!(ClientMessage::from_json(r#"{"type":"auth"}"#).is_err());
    assert!(ServerMessage::from_json(r#"{"type":"job","job_id":"j"}"#).is_err());
}

#[test]
fn job_with_unknown_kind_decodes() {
    let json = r#"{"type":"job","job_target_id":"t9","job_id":"j9","kind":"power","payload":{"action":"restart"}}"#;
    match ServerMessage::from_json(json).unwrap() {
        ServerMessage::Job { kind, payload, .. } => {
            assert_eq!(kind, "power");
            assert_eq!(payload, json!({"action": "restart"}));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn script_run_payload_defaults_and_shape() {
    let payload: ScriptRunPayload =
        serde_json::from_value(json!({"shell": "powershell", "body": "Get-Date"})).unwrap();
    assert_eq!(
        payload,
        ScriptRunPayload {
            shell: Shell::PowerShell,
            body: "Get-Date".into(),
            timeout_secs: 300,
            run_as: RunAs::System,
        }
    );
    let full = ScriptRunPayload {
        shell: Shell::Cmd,
        body: "echo hi".into(),
        timeout_secs: 5,
        run_as: RunAs::User,
    };
    let value = serde_json::to_value(&full).unwrap();
    assert_eq!(
        value,
        json!({"shell": "cmd", "body": "echo hi", "timeout_secs": 5, "run_as": "user"})
    );
    assert_eq!(serde_json::from_value::<ScriptRunPayload>(value).unwrap(), full);
    // A shell this version does not know is a payload error.
    assert!(serde_json::from_value::<ScriptRunPayload>(json!({"shell": "bash", "body": "ls"})).is_err());
}

#[test]
fn job_cancel_wire_shape() {
    let value: Value =
        serde_json::from_str(&ServerMessage::JobCancel { job_target_id: "t1".into() }.to_json()).unwrap();
    assert_eq!(value, json!({"type": "job.cancel", "job_target_id": "t1"}));
    assert_eq!(
        ServerMessage::from_json(r#"{"type":"job.cancel","job_target_id":"t5","why":"operator"}"#).unwrap(),
        ServerMessage::JobCancel { job_target_id: "t5".into() }
    );
    assert!(ServerMessage::from_json(r#"{"type":"job.cancel"}"#).is_err());
}

#[test]
fn milestone_4_job_kinds_and_reasons_have_their_wire_names() {
    assert_eq!(job_kind::APP_LAUNCH, "app.launch");
    assert_eq!(job_kind::SETTING_APPLY, "setting.apply");
    assert_eq!(job_kind::POWER, "power");
    assert_eq!(protocol::reason::CANCELLED, "cancelled");
    assert_eq!(protocol::reason::NO_USER_SESSION, "no_user_session");
}
