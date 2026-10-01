use protocol::update::{
    current_target, parse_sha256_hex, public_key_for, sha256_hex, sign, signing_message, verify,
    SignError, Version, VerifyError, TARGET_WINDOWS_X86_64,
};

// RFC 8032 section 7.1 test 1: a published test key, not a secret.
const SEED_B64: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
const PUBLIC_B64: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";

const VERSION: &str = "1.2.3";
const TARGET: &str = "windows-x86_64";
const SHA_HEX: &str = "3d161da2e50b08a345254cf76752177f2a05ff4644c1b3a2dff825eeba61216f";

/// Produced independently with Node 24: the seed above as a JWK
/// (`{kty:'OKP',crv:'Ed25519',d,x}`) through `crypto.createPrivateKey`, then
/// `crypto.sign(null, message, key)` over the message documented on
/// `signing_message`, base64 encoded. Ed25519 is deterministic, so this is stable.
const SIGNATURE_B64: &str =
    "Yl2zfdK9vblZgGXlpeLmvw6toMsLvUCppp3du8z4K4iwafVJ2SxhDTXx/gwvV2HtXeBMcZthTdhPV8l/HCu8DA==";

fn sha() -> [u8; 32] {
    parse_sha256_hex(SHA_HEX).unwrap()
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn base64_decode(text: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).unwrap()
}

#[test]
fn signing_message_has_the_documented_exact_bytes() {
    // Same bytes Node signed to produce the fixture signature.
    let expected = concat!(
        "737572666163652d636f6f7264696e61746f722f636c69656e742d7570646174652f7631",
        "00",
        "312e322e33",
        "00",
        "77696e646f77732d7838365f3634",
        "00",
        "3d161da2e50b08a345254cf76752177f2a05ff4644c1b3a2dff825eeba61216f",
    );
    assert_eq!(hex::encode(signing_message(VERSION, TARGET, &sha())), expected);
}

#[test]
fn public_key_is_derived_from_the_seed() {
    assert_eq!(public_key_for(SEED_B64).unwrap(), PUBLIC_B64);
}

#[test]
fn signing_reproduces_the_independently_computed_signature() {
    assert_eq!(sign(SEED_B64, VERSION, TARGET, &sha()).unwrap(), SIGNATURE_B64);
}

#[test]
fn verify_accepts_the_independently_computed_signature() {
    verify(PUBLIC_B64, VERSION, TARGET, &sha(), SIGNATURE_B64).unwrap();
}

#[test]
fn verify_ignores_whitespace_around_key_and_signature() {
    let key = format!("  {PUBLIC_B64}\r\n");
    let sig = format!("\n{SIGNATURE_B64}\n");
    verify(&key, VERSION, TARGET, &sha(), &sig).unwrap();
}

#[test]
fn verify_fails_for_another_version() {
    assert_eq!(
        verify(PUBLIC_B64, "1.2.4", TARGET, &sha(), SIGNATURE_B64),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn verify_fails_for_another_target() {
    assert_eq!(
        verify(PUBLIC_B64, VERSION, "linux-x86_64", &sha(), SIGNATURE_B64),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn verify_fails_for_another_hash() {
    let mut hash = sha();
    hash[31] ^= 1;
    assert_eq!(
        verify(PUBLIC_B64, VERSION, TARGET, &hash, SIGNATURE_B64),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn verify_fails_for_a_changed_signature() {
    let mut bytes = base64_decode(SIGNATURE_B64);
    bytes[10] ^= 1;
    assert_eq!(
        verify(PUBLIC_B64, VERSION, TARGET, &sha(), &base64_encode(&bytes)),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn verify_fails_with_a_different_key() {
    let other_public = public_key_for(&base64_encode(&[7u8; 32])).unwrap();
    assert_ne!(other_public, PUBLIC_B64);
    assert_eq!(
        verify(&other_public, VERSION, TARGET, &sha(), SIGNATURE_B64),
        Err(VerifyError::BadSignature)
    );
}

#[test]
fn verify_rejects_malformed_base64() {
    assert_eq!(
        verify("not base64!!", VERSION, TARGET, &sha(), SIGNATURE_B64),
        Err(VerifyError::BadPublicKey)
    );
    assert_eq!(
        verify(PUBLIC_B64, VERSION, TARGET, &sha(), "%%%not base64%%%"),
        Err(VerifyError::BadSignatureEncoding)
    );
}

#[test]
fn verify_rejects_unpadded_base64() {
    // The wire format is standard base64 with padding; be strict about it.
    assert_eq!(
        verify(PUBLIC_B64.trim_end_matches('='), VERSION, TARGET, &sha(), SIGNATURE_B64),
        Err(VerifyError::BadPublicKey)
    );
}

#[test]
fn verify_rejects_a_key_of_the_wrong_length() {
    for len in [31usize, 33] {
        assert_eq!(
            verify(&base64_encode(&vec![1u8; len]), VERSION, TARGET, &sha(), SIGNATURE_B64),
            Err(VerifyError::BadPublicKey)
        );
    }
}

#[test]
fn verify_rejects_a_signature_of_the_wrong_length() {
    for len in [63usize, 65] {
        assert_eq!(
            verify(PUBLIC_B64, VERSION, TARGET, &sha(), &base64_encode(&vec![1u8; len])),
            Err(VerifyError::BadSignatureEncoding)
        );
    }
}

#[test]
fn sign_rejects_a_bad_signing_key() {
    assert_eq!(sign("nope!", VERSION, TARGET, &sha()), Err(SignError::BadSigningKey));
    assert_eq!(
        sign(&base64_encode(&[1u8; 31]), VERSION, TARGET, &sha()),
        Err(SignError::BadSigningKey)
    );
    assert_eq!(public_key_for(""), Err(SignError::BadSigningKey));
}

#[test]
fn sign_ignores_whitespace_around_the_key() {
    let key = format!("{SEED_B64}\n");
    assert_eq!(sign(&key, VERSION, TARGET, &sha()).unwrap(), SIGNATURE_B64);
}

#[test]
fn errors_have_readable_messages() {
    assert!(VerifyError::BadSignature.to_string().contains("signature"));
    assert!(VerifyError::BadPublicKey.to_string().contains("public key"));
    assert!(SignError::BadSigningKey.to_string().contains("signing key"));
}

#[test]
fn current_target_is_known_on_windows_x86_64_only() {
    if cfg!(all(windows, target_arch = "x86_64")) {
        assert_eq!(current_target(), Some(TARGET_WINDOWS_X86_64));
    } else {
        assert_eq!(current_target(), None);
    }
}

#[test]
fn versions_parse_and_display() {
    let v: Version = "1.20.300".parse().unwrap();
    assert_eq!(v, Version { major: 1, minor: 20, patch: 300 });
    assert_eq!(v.to_string(), "1.20.300");
}

#[test]
fn versions_order_numerically_not_textually() {
    let v = |s: &str| s.parse::<Version>().unwrap();
    assert!(v("1.2.10") > v("1.2.9"));
    assert!(v("2.0.0") > v("1.99.99"));
    assert!(v("1.10.0") > v("1.9.99"));
    assert!(v("0.1.0") == v("0.1.0"));
}

#[test]
fn versions_reject_anything_but_three_plain_numbers() {
    for bad in [
        "1.2",
        "1.2.3.4",
        "v1.2.3",
        "1.2.x",
        "",
        "-1.2.3",
        "1.-2.3",
        "1. 2.3",
        " 1.2.3",
        "1.2.3 ",
        "1.2.3-beta",
        "1..3",
        "+1.2.3",
        "1.2.99999999999999999999999",
    ] {
        assert!(bad.parse::<Version>().is_err(), "{bad:?} must be rejected");
    }
}

#[test]
fn sha256_hex_matches_the_known_digest_of_empty_input() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_hex_round_trips_through_parse() {
    let hex = sha256_hex(b"surface-coordinator test artifact");
    assert_eq!(hex, SHA_HEX);
    let raw = parse_sha256_hex(&hex).unwrap();
    assert_eq!(hex::encode(raw), hex);
}

#[test]
fn parse_sha256_hex_rejects_bad_input() {
    assert!(parse_sha256_hex("").is_none());
    assert!(parse_sha256_hex(&SHA_HEX[..62]).is_none());
    assert!(parse_sha256_hex(&format!("{SHA_HEX}00")).is_none());
    assert!(parse_sha256_hex(&SHA_HEX.replace('3', "g")).is_none());
    assert!(parse_sha256_hex(&format!(" {}", &SHA_HEX[..63])).is_none());
}

#[test]
fn parse_sha256_hex_accepts_uppercase() {
    assert_eq!(parse_sha256_hex(&SHA_HEX.to_uppercase()), parse_sha256_hex(SHA_HEX));
}
