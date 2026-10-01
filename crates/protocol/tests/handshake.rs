use protocol::handshake::{compute_mac, generate_nonce, is_valid_nonce, verify_mac, Role};

// Obviously fake fixtures; never real enrollment secrets.
const SECRET_A: &str = "test-fixture-secret-not-a-real-key";
const SECRET_B: &str = "another-fake-fixture-secret";

const CLIENT_NONCE_A: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const SERVER_NONCE_A: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
const CLIENT_NONCE_B: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
const SERVER_NONCE_B: &str = "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff808182838485868788898a8b8c8d8e8f";

/// Expected values were computed independently with Node's `crypto` module:
/// HMAC-SHA-256(key = secret as UTF-8,
///   "surface-coordinator/handshake/v1" 0x00 role 0x00 client_nonce 0x00 server_nonce).
#[test]
fn hmac_vectors() {
    let vectors = [
        (SECRET_A, Role::Coordinator, CLIENT_NONCE_A, SERVER_NONCE_A,
         "dcb2b79372458784d3979fca93fd97a35474c26ebafa613822693088141cbca0"),
        (SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A,
         "e5cbdbce337cbd0140c70c4208e193385a26e86d275047ff0c25673cad06f66e"),
        (SECRET_B, Role::Coordinator, CLIENT_NONCE_B, SERVER_NONCE_B,
         "45feb223d29e309f1c584eee761c00061cac10774b99465d16ad5b2ebbcbbb29"),
        (SECRET_B, Role::Client, CLIENT_NONCE_B, SERVER_NONCE_B,
         "8c6d2d9130833723b3ded7912a40f2a42c41b786848f4686c373b876381f304f"),
    ];
    for (secret, role, client_nonce, server_nonce, expected) in vectors {
        assert_eq!(compute_mac(secret, role, client_nonce, server_nonce), expected);
        assert!(verify_mac(secret, role, client_nonce, server_nonce, expected));
    }
}

#[test]
fn verify_rejects_wrong_key() {
    let mac = compute_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A);
    assert!(!verify_mac(SECRET_B, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, &mac));
}

#[test]
fn coordinator_mac_cannot_be_replayed_as_client_mac() {
    let mac = compute_mac(SECRET_A, Role::Coordinator, CLIENT_NONCE_A, SERVER_NONCE_A);
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, &mac));
}

#[test]
fn verify_rejects_changed_or_swapped_nonces() {
    let mac = compute_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A);
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_B, SERVER_NONCE_A, &mac));
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_B, &mac));
    assert!(!verify_mac(SECRET_A, Role::Client, SERVER_NONCE_A, CLIENT_NONCE_A, &mac));
}

#[test]
fn verify_rejects_malformed_mac() {
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, ""));
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, "zz"));
    let mac = compute_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A);
    assert!(!verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, &mac[..62]));
}

#[test]
fn verify_accepts_uppercase_hex() {
    let mac = compute_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A).to_uppercase();
    assert!(verify_mac(SECRET_A, Role::Client, CLIENT_NONCE_A, SERVER_NONCE_A, &mac));
}

#[test]
fn nonces_are_random_and_well_formed() {
    let a = generate_nonce();
    let b = generate_nonce();
    assert_ne!(a, b);
    assert!(is_valid_nonce(&a));
    assert!(is_valid_nonce(&b));
    assert_eq!(a.len(), 64);
}

#[test]
fn nonce_validation() {
    assert!(is_valid_nonce(CLIENT_NONCE_B));
    assert!(!is_valid_nonce(""));
    assert!(!is_valid_nonce("abcd"));
    assert!(!is_valid_nonce(&"g".repeat(64)));
    assert!(!is_valid_nonce(&"a".repeat(66)));
}
