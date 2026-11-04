// SPDX-License-Identifier: AGPL-3.0-or-later

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_upload::{
    Challenge, ExpectedAssertion, ManifestHash, Nonce, SignatureAlgorithm, StructuralError,
    StructuralVerdict, WebAuthnAssertion, check_structure,
};
use sha2::{Digest, Sha256};

const ORIGIN: &str = "https://looklab.example";
const RP_ID: &str = "looklab.example";
const OTHER_KEYS: &str = "Do not compare, do not measure. No other keys can be added here.";

fn challenge() -> Challenge {
    Challenge::new(
        &Nonce::from_bytes([9; 32]),
        &ManifestHash::from_bytes([8; 32]),
    )
}

fn client_data_json(challenge: &Challenge, extra: &str) -> Vec<u8> {
    format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"{ORIGIN}"{extra}}}"#,
        URL_SAFE_NO_PAD.encode(challenge.as_bytes())
    )
    .into_bytes()
}

fn authenticator_data(flags: u8, extensions: &[u8]) -> Vec<u8> {
    let mut data = Sha256::digest(RP_ID.as_bytes()).to_vec();
    data.push(flags);
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(extensions);
    data
}

fn assertion(
    challenge: &Challenge,
    extra: &str,
    flags: u8,
    extensions: &[u8],
    signature: Vec<u8>,
) -> WebAuthnAssertion {
    WebAuthnAssertion {
        credential_id: vec![1, 2, 3],
        authenticator_data: authenticator_data(flags, extensions),
        client_data_json: client_data_json(challenge, extra),
        signature,
    }
}

fn expected(algorithm: SignatureAlgorithm) -> ExpectedAssertion<'static> {
    ExpectedAssertion {
        challenge: challenge(),
        origin: ORIGIN,
        rp_id: RP_ID,
        algorithm,
    }
}

fn valid() -> WebAuthnAssertion {
    assertion(&challenge(), "", 0x01, &[], vec![0xab; 64])
}

fn failure(assertion: &WebAuthnAssertion, algorithm: SignatureAlgorithm) -> StructuralError {
    match check_structure(assertion, &expected(algorithm)) {
        StructuralVerdict::Fail(error) => error,
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn valid_assertion_passes() {
    assert!(matches!(
        check_structure(&valid(), &expected(SignatureAlgorithm::Ed25519)),
        StructuralVerdict::Pass
    ));
}

#[test]
fn wrong_type_fails() {
    let mut assertion = valid();
    assertion.client_data_json =
        br#"{"type":"webauthn.create","challenge":"x","origin":"x"}"#.to_vec();
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::WrongType { .. }
    ));
}

#[test]
fn wrong_challenge_fails() {
    let other = Challenge::new(
        &Nonce::from_bytes([1; 32]),
        &ManifestHash::from_bytes([1; 32]),
    );
    let assertion = assertion(&other, "", 0x01, &[], vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::WrongChallenge
    ));
}

#[test]
fn wrong_origin_fails() {
    let mut assertion = valid();
    assertion.client_data_json = format!(
        r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://evil.example"}}"#,
        URL_SAFE_NO_PAD.encode(challenge().as_bytes())
    )
    .into_bytes();
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::WrongOrigin
    ));
}

#[test]
fn cross_origin_fails() {
    let assertion = assertion(
        &challenge(),
        r#","crossOrigin":true"#,
        0x01,
        &[],
        vec![0xab; 64],
    );
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::CrossOriginSet
    ));
}

#[test]
fn cross_origin_false_is_accepted() {
    let assertion = assertion(
        &challenge(),
        r#","crossOrigin":false"#,
        0x01,
        &[],
        vec![0xab; 64],
    );
    assert!(matches!(
        check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)),
        StructuralVerdict::Pass
    ));
}

#[test]
fn unknown_client_data_keys_need_review_sorted() {
    let assertion = assertion(
        &challenge(),
        r#","zz":1,"aa":"two""#,
        0x01,
        &[],
        vec![0xab; 64],
    );
    match check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)) {
        StructuralVerdict::ReviewNeeded {
            unknown_client_data_keys,
            extensions,
        } => {
            assert!(extensions.is_none());
            let keys: Vec<_> = unknown_client_data_keys
                .iter()
                .map(|extra| extra.key.as_str())
                .collect();
            assert_eq!(keys, ["aa", "zz"]);
            assert_eq!(unknown_client_data_keys[0].raw_json, "\"two\"");
        }
        other => panic!("expected review, got {other:?}"),
    }
}

#[test]
fn top_origin_is_accepted() {
    let assertion = assertion(
        &challenge(),
        r#","topOrigin":"https://looklab.example""#,
        0x01,
        &[],
        vec![0xab; 64],
    );
    assert!(matches!(
        check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)),
        StructuralVerdict::Pass
    ));
}

#[test]
fn chrome_extra_key_is_accepted_with_its_fixed_text() {
    let assertion = assertion(
        &challenge(),
        &format!(r#","other_keys_can_be_added_here":"{OTHER_KEYS}""#),
        0x01,
        &[],
        vec![0xab; 64],
    );
    assert!(matches!(
        check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)),
        StructuralVerdict::Pass
    ));
}

#[test]
fn chrome_extra_key_with_other_text_needs_review() {
    let assertion = assertion(
        &challenge(),
        r#","other_keys_can_be_added_here":"something else""#,
        0x01,
        &[],
        vec![0xab; 64],
    );
    assert!(matches!(
        check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)),
        StructuralVerdict::ReviewNeeded { .. }
    ));
}

#[test]
fn malformed_client_data_fails() {
    let mut assertion = valid();
    assertion.client_data_json = b"not json".to_vec();
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::ClientDataJson(_)
    ));
}

#[test]
fn wrong_rp_id_hash_fails() {
    let mut assertion = valid();
    assertion.authenticator_data[0] ^= 0xff;
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::RpIdHashMismatch
    ));
}

#[test]
fn missing_user_presence_fails() {
    let assertion = assertion(&challenge(), "", 0x00, &[], vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::UserPresenceMissing
    ));
}

#[test]
fn attested_credential_data_fails() {
    let assertion = assertion(&challenge(), "", 0x41, &[], vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::AttestedCredentialDataPresent
    ));
}

#[test]
fn trailing_authenticator_data_fails() {
    let assertion = assertion(&challenge(), "", 0x01, &[0x01], vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::TrailingAuthenticatorData { len: 1 }
    ));
}

#[test]
fn extensions_need_review() {
    let extensions = [0xa1, 0x61, b'x', 0x01];
    let assertion = assertion(&challenge(), "", 0x81, &extensions, vec![0xab; 64]);
    match check_structure(&assertion, &expected(SignatureAlgorithm::Ed25519)) {
        StructuralVerdict::ReviewNeeded {
            unknown_client_data_keys,
            extensions,
        } => {
            assert!(unknown_client_data_keys.is_empty());
            let extensions = extensions.expect("extensions are decoded");
            assert_eq!(extensions.entries.len(), 1);
            assert_eq!(extensions.entries[0].key, "x");
        }
        other => panic!("expected review, got {other:?}"),
    }
}

#[test]
fn trailing_extension_bytes_fail() {
    let extensions = [0xa1, 0x61, b'x', 0x01, 0x00];
    let assertion = assertion(&challenge(), "", 0x81, &extensions, vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::TrailingExtensions { len: 1 }
    ));
}

#[test]
fn malformed_extensions_fail() {
    let assertion = assertion(&challenge(), "", 0x81, &[0xff], vec![0xab; 64]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::InvalidExtensions(_)
    ));
}

#[test]
fn ed25519_signature_length_is_checked() {
    let assertion = assertion(&challenge(), "", 0x01, &[], vec![0xab; 63]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::SignatureLength {
            expected: 64,
            found: 63,
            ..
        }
    ));
}

#[test]
fn ecdsa_signature_is_der() {
    let valid_der = vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02];
    let signed = assertion(&challenge(), "", 0x01, &[], valid_der);
    assert!(matches!(
        check_structure(&signed, &expected(SignatureAlgorithm::Ecdsa)),
        StructuralVerdict::Pass
    ));

    let zero_scalar = vec![0x30, 0x06, 0x02, 0x01, 0x00, 0x02, 0x01, 0x02];
    let signed = assertion(&challenge(), "", 0x01, &[], zero_scalar);
    assert!(matches!(
        failure(&signed, SignatureAlgorithm::Ecdsa),
        StructuralError::InvalidDerSignature
    ));

    let trailing = vec![0x30, 0x06, 0x02, 0x01, 0x01, 0x02, 0x01, 0x02, 0x00];
    let signed = assertion(&challenge(), "", 0x01, &[], trailing);
    assert!(matches!(
        failure(&signed, SignatureAlgorithm::Ecdsa),
        StructuralError::InvalidDerSignature
    ));
}

#[test]
fn rsa_signature_lengths_are_checked() {
    for len in [256, 384, 512] {
        let assertion = assertion(&challenge(), "", 0x01, &[], vec![0xab; len]);
        assert!(matches!(
            check_structure(&assertion, &expected(SignatureAlgorithm::Rsa)),
            StructuralVerdict::Pass
        ));
    }
    let assertion = assertion(&challenge(), "", 0x01, &[], vec![0xab; 257]);
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Rsa),
        StructuralError::RsaSignatureLength { found: 257 }
    ));
}

#[test]
fn field_limits_are_checked() {
    let mut assertion = valid();
    assertion.credential_id = Vec::new();
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::CredentialIdEmpty
    ));

    let mut assertion = valid();
    assertion.authenticator_data = vec![0; 36];
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::AuthenticatorDataTooShort { len: 36 }
    ));

    let mut assertion = valid();
    assertion.client_data_json = vec![0; 1025];
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::ClientDataJsonTooLong { len: 1025, .. }
    ));

    let mut assertion = valid();
    assertion.signature = vec![0; 513];
    assert!(matches!(
        failure(&assertion, SignatureAlgorithm::Ed25519),
        StructuralError::SignatureTooLong { len: 513, .. }
    ));
}
