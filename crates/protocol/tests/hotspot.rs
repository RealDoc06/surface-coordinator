use protocol::hotspot::{passphrase, ssid};
use protocol::{ClientMessage, Hello, NetworkInfo, WifiJoinPayload};
use serde_json::json;

#[test]
fn ssid_is_the_prefix_and_the_first_eight_characters_of_the_key_id() {
    assert_eq!(ssid("ad74046f2b8a4228a8bd466e9111e13a"), "SC-ad74046f");
    assert_eq!(ssid("short"), "SC-short");
    assert_eq!(ssid(""), "SC-");
    // Never splits a character.
    assert_eq!(ssid("ääääääääää"), "SC-ääääääää");
}

/// Expected values were computed independently with Node's `crypto`:
/// HKDF-SHA-256(ikm = secret as UTF-8, salt = "surface-coordinator/hotspot/v1",
/// info = "wpa2-passphrase", 15 bytes), base32 (a-z, 2-7) without padding.
#[test]
fn passphrase_vectors() {
    assert_eq!(passphrase("test-fixture-secret-not-a-real-key"), "ezcxuvxxqckspcjopjbpns4e");
    assert_eq!(passphrase("another-fake-fixture-secret"), "nqmdq553mhn2rsriwfdtwdmz");
}

#[test]
fn passphrase_is_a_valid_wpa2_passphrase() {
    for secret in ["a", "test-fixture-secret-not-a-real-key", &"f".repeat(64)] {
        let passphrase = passphrase(secret);
        assert_eq!(passphrase.len(), 24);
        assert!((8..=63).contains(&passphrase.len()));
        assert!(passphrase.bytes().all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)));
    }
    assert_ne!(passphrase("a"), passphrase("b"));
}

#[test]
fn wifi_join_payload_shape() {
    let full: WifiJoinPayload = serde_json::from_value(json!({
        "ssid": "School-WiFi",
        "password": "fixture-wifi-password",
        "coordinator_url": "wss://coordinator.example.org",
    }))
    .unwrap();
    assert_eq!(full.ssid, "School-WiFi");
    assert_eq!(full.password.as_deref(), Some("fixture-wifi-password"));
    assert_eq!(full.coordinator_url.as_deref(), Some("wss://coordinator.example.org"));

    let minimal: WifiJoinPayload = serde_json::from_value(json!({"ssid": "Open-Net"})).unwrap();
    assert_eq!(minimal.password, None);
    assert_eq!(minimal.coordinator_url, None);
    assert_eq!(serde_json::to_value(&minimal).unwrap(), json!({"ssid": "Open-Net"}));
    assert!(serde_json::from_value::<WifiJoinPayload>(json!({"password": "x"})).is_err());
}

#[test]
fn hello_may_carry_the_current_network() {
    let hello = |network| Hello {
        protocol_version: protocol::PROTOCOL_VERSION,
        device_id: "d".into(),
        hostname: "h".into(),
        os: "windows".into(),
        client_version: "0.1.0".into(),
        key_id: "k".into(),
        nonce: "ab".repeat(32),
        network,
    };
    let with = ClientMessage::Hello(hello(Some(NetworkInfo {
        ssid: Some("SC-ad74046f".into()),
        ip: Some("192.168.137.23".into()),
    })));
    let value: serde_json::Value = serde_json::from_str(&with.to_json()).unwrap();
    assert_eq!(value["network"], json!({"ssid": "SC-ad74046f", "ip": "192.168.137.23"}));
    assert_eq!(ClientMessage::from_json(&with.to_json()).unwrap(), with);

    // Absent when unknown, and a hello without it (an older client) decodes.
    let without = ClientMessage::Hello(hello(None));
    let value: serde_json::Value = serde_json::from_str(&without.to_json()).unwrap();
    assert!(value.get("network").is_none());
    assert_eq!(ClientMessage::from_json(&without.to_json()).unwrap(), without);
}
