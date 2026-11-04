// SPDX-License-Identifier: AGPL-3.0-or-later
//! Structural WebAuthn assertion checks shared by the server and
//! `looklab-cli verify-payload` ([docs/06 §5](../../../docs/06-privacy-and-payload.md)).
//! Cryptographic signature verification belongs to the server (`webauthn-rs`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ciborium::value::Value as CborValue;
use serde::Deserialize;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::nonce::Challenge;

pub const MAX_CREDENTIAL_ID: usize = 255;
pub const MAX_AUTHENTICATOR_DATA: usize = 512;
pub const MAX_CLIENT_DATA_JSON: usize = 1024;
pub const MAX_SIGNATURE: usize = 512;

const FLAG_USER_PRESENT: u8 = 0x01;
const FLAG_ATTESTED_CREDENTIAL_DATA: u8 = 0x40;
const FLAG_EXTENSION_DATA: u8 = 0x80;

const CLIENT_DATA_TYPE: &str = "webauthn.get";
const OTHER_KEYS_TEXT: &str = "Do not compare, do not measure. No other keys can be added here.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebAuthnAssertion {
    pub credential_id: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    Ed25519,
    Ecdsa,
    Rsa,
}

#[derive(Debug, Clone, Copy)]
pub struct ExpectedAssertion<'a> {
    pub challenge: Challenge,
    pub origin: &'a str,
    pub rp_id: &'a str,
    pub algorithm: SignatureAlgorithm,
    /// The top-level origin expected inside `client_data_json`, when one is.
    pub top_origin: Option<&'a str>,
}

/// A `client_data_json` key the checks do not know, kept for a person to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientDataExtra {
    pub key: String,
    pub raw_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Extensions {
    pub entries: Vec<ExtensionEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionEntry {
    pub key: String,
    pub value: ReviewValue,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReviewValue {
    Null,
    Bool(bool),
    Integer(i128),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Array(Vec<ReviewValue>),
    Map(Vec<(ReviewValue, ReviewValue)>),
    Tag(u64, Box<ReviewValue>),
}

#[derive(Debug)]
pub enum StructuralVerdict {
    Pass,
    ReviewNeeded {
        unknown_client_data_keys: Vec<ClientDataExtra>,
        /// The raw `client_data_json` when it is not the WebAuthn serialization.
        non_canonical_client_data: Option<String>,
        top_origin: Option<TopOriginReview>,
        extensions: Option<Extensions>,
    },
    Fail(StructuralError),
}

/// A `topOrigin` in `client_data_json` that no expected value accounts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopOriginReview {
    Unexpected(String),
    Missing,
}

#[derive(Debug, thiserror::Error)]
pub enum StructuralError {
    #[error("credential id is empty")]
    CredentialIdEmpty,
    #[error("credential id of {len} bytes exceeds the {max}-byte limit")]
    CredentialIdTooLong { len: usize, max: usize },
    #[error("authenticator data of {len} bytes is too short for the fixed layout")]
    AuthenticatorDataTooShort { len: usize },
    #[error("authenticator data of {len} bytes exceeds the {max}-byte limit")]
    AuthenticatorDataTooLong { len: usize, max: usize },
    #[error("client_data_json of {len} bytes exceeds the {max}-byte limit")]
    ClientDataJsonTooLong { len: usize, max: usize },
    #[error("signature of {len} bytes exceeds the {max}-byte limit")]
    SignatureTooLong { len: usize, max: usize },
    #[error("client_data_json is not a valid WebAuthn client data object")]
    ClientDataJson(#[source] serde_json::Error),
    #[error("client_data_json repeats the JSON object key {key:?}")]
    DuplicateClientDataKey { key: String },
    #[error("client_data_json type is {found:?}, expected \"webauthn.get\"")]
    WrongType { found: String },
    #[error("client_data_json challenge does not match H(nonce ‖ manifest hash)")]
    WrongChallenge,
    #[error("client_data_json origin does not match the expected origin")]
    WrongOrigin,
    #[error("client_data_json sets crossOrigin")]
    CrossOriginSet,
    #[error("authenticator data rpIdHash is not SHA-256 of the RP ID")]
    RpIdHashMismatch,
    #[error("authenticator data does not set the user-present flag")]
    UserPresenceMissing,
    #[error("authenticator data sets the attested-credential-data flag")]
    AttestedCredentialDataPresent,
    #[error("{len} unexpected bytes follow the authenticator data")]
    TrailingAuthenticatorData { len: usize },
    #[error("authenticator extensions are not a valid CBOR map")]
    InvalidExtensions(#[source] ciborium::de::Error<std::io::Error>),
    #[error("{len} unexpected bytes follow the CBOR extensions")]
    TrailingExtensions { len: usize },
    #[error("{algorithm:?} signature has {found} bytes, expected {expected}")]
    SignatureLength {
        algorithm: SignatureAlgorithm,
        expected: usize,
        found: usize,
    },
    #[error("RSA signature has {found} bytes, expected 256, 384 or 512")]
    RsaSignatureLength { found: usize },
    #[error("signature is not a DER SEQUENCE of two positive INTEGERs")]
    InvalidDerSignature,
}

#[derive(Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(rename = "crossOrigin", default)]
    cross_origin: Option<bool>,
    #[serde(rename = "topOrigin", default)]
    top_origin: Option<String>,
    #[serde(flatten)]
    extras: BTreeMap<String, JsonValue>,
}

/// Rejects repeated object keys at any depth.
struct DuplicateScan<'a> {
    duplicate: &'a mut Option<String>,
}

impl<'de, 'a> DeserializeSeed<'de> for DuplicateScan<'a> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(ScanVisitor {
            duplicate: self.duplicate,
        })
    }
}

struct ScanVisitor<'a> {
    duplicate: &'a mut Option<String>,
}

impl<'de, 'a> Visitor<'de> for ScanVisitor<'a> {
    type Value = ();

    fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_borrowed_str<E>(self, _: &'de str) -> Result<(), E> {
        Ok(())
    }

    fn visit_string<E>(self, _: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq
            .next_element_seed(DuplicateScan {
                duplicate: &mut *self.duplicate,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                *self.duplicate = Some(key);
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value_seed(DuplicateScan {
                duplicate: &mut *self.duplicate,
            })?;
        }
        Ok(())
    }
}

pub fn check_structure(
    assertion: &WebAuthnAssertion,
    expected: &ExpectedAssertion<'_>,
) -> StructuralVerdict {
    match check_lengths(assertion) {
        Ok(()) => {}
        Err(error) => return StructuralVerdict::Fail(error),
    }
    let client = match check_client_data(assertion, expected) {
        Ok(client) => client,
        Err(error) => return StructuralVerdict::Fail(error),
    };
    let extensions = match check_authenticator_data(assertion, expected) {
        Ok(extensions) => extensions,
        Err(error) => return StructuralVerdict::Fail(error),
    };
    if let Err(error) = check_signature(&assertion.signature, expected.algorithm) {
        return StructuralVerdict::Fail(error);
    }
    if client.unknown.is_empty()
        && client.non_canonical.is_none()
        && client.top_origin.is_none()
        && extensions.is_none()
    {
        StructuralVerdict::Pass
    } else {
        StructuralVerdict::ReviewNeeded {
            unknown_client_data_keys: client.unknown,
            non_canonical_client_data: client.non_canonical,
            top_origin: client.top_origin,
            extensions,
        }
    }
}

fn check_lengths(assertion: &WebAuthnAssertion) -> Result<(), StructuralError> {
    if assertion.credential_id.is_empty() {
        return Err(StructuralError::CredentialIdEmpty);
    }
    if assertion.credential_id.len() > MAX_CREDENTIAL_ID {
        return Err(StructuralError::CredentialIdTooLong {
            len: assertion.credential_id.len(),
            max: MAX_CREDENTIAL_ID,
        });
    }
    if assertion.authenticator_data.len() < 37 {
        return Err(StructuralError::AuthenticatorDataTooShort {
            len: assertion.authenticator_data.len(),
        });
    }
    if assertion.authenticator_data.len() > MAX_AUTHENTICATOR_DATA {
        return Err(StructuralError::AuthenticatorDataTooLong {
            len: assertion.authenticator_data.len(),
            max: MAX_AUTHENTICATOR_DATA,
        });
    }
    if assertion.client_data_json.len() > MAX_CLIENT_DATA_JSON {
        return Err(StructuralError::ClientDataJsonTooLong {
            len: assertion.client_data_json.len(),
            max: MAX_CLIENT_DATA_JSON,
        });
    }
    if assertion.signature.len() > MAX_SIGNATURE {
        return Err(StructuralError::SignatureTooLong {
            len: assertion.signature.len(),
            max: MAX_SIGNATURE,
        });
    }
    Ok(())
}

struct ClientDataCheck {
    unknown: Vec<ClientDataExtra>,
    non_canonical: Option<String>,
    top_origin: Option<TopOriginReview>,
}

fn check_client_data(
    assertion: &WebAuthnAssertion,
    expected: &ExpectedAssertion<'_>,
) -> Result<ClientDataCheck, StructuralError> {
    let mut duplicate = None;
    let mut deserializer = serde_json::Deserializer::from_slice(&assertion.client_data_json);
    let scanned = DuplicateScan {
        duplicate: &mut duplicate,
    }
    .deserialize(&mut deserializer);
    match scanned {
        Ok(()) => deserializer
            .end()
            .map_err(StructuralError::ClientDataJson)?,
        Err(error) => {
            return Err(match duplicate {
                Some(key) => StructuralError::DuplicateClientDataKey { key },
                None => StructuralError::ClientDataJson(error),
            });
        }
    }

    let client: ClientData = serde_json::from_slice(&assertion.client_data_json)
        .map_err(StructuralError::ClientDataJson)?;
    if client.kind != CLIENT_DATA_TYPE {
        return Err(StructuralError::WrongType { found: client.kind });
    }
    if client.challenge != URL_SAFE_NO_PAD.encode(expected.challenge.as_bytes()) {
        return Err(StructuralError::WrongChallenge);
    }
    if client.origin != expected.origin {
        return Err(StructuralError::WrongOrigin);
    }
    if client.cross_origin.unwrap_or(false) {
        return Err(StructuralError::CrossOriginSet);
    }

    let mut unknown = Vec::new();
    for (key, value) in &client.extras {
        if key == "other_keys_can_be_added_here" && value.as_str() == Some(OTHER_KEYS_TEXT) {
            continue;
        }
        unknown.push(ClientDataExtra {
            key: key.clone(),
            raw_json: value.to_string(),
        });
    }

    let canonical = canonical_client_data(&client);
    let non_canonical = (canonical.as_bytes() != assertion.client_data_json.as_slice())
        .then(|| String::from_utf8_lossy(&assertion.client_data_json).into_owned());
    let top_origin = match (client.top_origin.as_deref(), expected.top_origin) {
        (None, None) => None,
        (Some(actual), Some(want)) if actual == want => None,
        (Some(actual), _) => Some(TopOriginReview::Unexpected(actual.to_owned())),
        (None, Some(_)) => Some(TopOriginReview::Missing),
    };
    Ok(ClientDataCheck {
        unknown,
        non_canonical,
        top_origin,
    })
}

/// The serialization WebAuthn specifies for `client_data_json`
/// (WebAuthn L3 §5.8.1.1): fixed key order, no whitespace, JSON string escapes.
fn canonical_client_data(client: &ClientData) -> String {
    let mut out = String::from("{\"type\":");
    push_json_string(&mut out, &client.kind);
    out.push_str(",\"challenge\":");
    push_json_string(&mut out, &client.challenge);
    out.push_str(",\"origin\":");
    push_json_string(&mut out, &client.origin);
    out.push_str(",\"crossOrigin\":");
    out.push_str(if client.cross_origin.unwrap_or(false) {
        "true"
    } else {
        "false"
    });
    if let Some(top_origin) = &client.top_origin {
        out.push_str(",\"topOrigin\":");
        push_json_string(&mut out, top_origin);
    }
    out.push('}');
    out
}

fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

fn check_authenticator_data(
    assertion: &WebAuthnAssertion,
    expected: &ExpectedAssertion<'_>,
) -> Result<Option<Extensions>, StructuralError> {
    let data = &assertion.authenticator_data;
    let rp_id_hash = Sha256::digest(expected.rp_id.as_bytes());
    if data[..32] != rp_id_hash[..] {
        return Err(StructuralError::RpIdHashMismatch);
    }
    let flags = data[32];
    if flags & FLAG_USER_PRESENT == 0 {
        return Err(StructuralError::UserPresenceMissing);
    }
    if flags & FLAG_ATTESTED_CREDENTIAL_DATA != 0 {
        return Err(StructuralError::AttestedCredentialDataPresent);
    }
    let extension_bytes = &data[37..];
    if flags & FLAG_EXTENSION_DATA == 0 {
        if !extension_bytes.is_empty() {
            return Err(StructuralError::TrailingAuthenticatorData {
                len: extension_bytes.len(),
            });
        }
        return Ok(None);
    }
    let mut remaining = extension_bytes;
    let value: CborValue =
        ciborium::de::from_reader(&mut remaining).map_err(StructuralError::InvalidExtensions)?;
    if !remaining.is_empty() {
        return Err(StructuralError::TrailingExtensions {
            len: remaining.len(),
        });
    }
    extensions_from_cbor(value).map(Some)
}

fn extensions_from_cbor(value: CborValue) -> Result<Extensions, StructuralError> {
    let CborValue::Map(map) = value else {
        return Err(StructuralError::InvalidExtensions(
            ciborium::de::Error::semantic(None, "authenticator extensions must be a CBOR map"),
        ));
    };
    let mut entries = Vec::with_capacity(map.len());
    for (key, value) in map {
        let CborValue::Text(key) = key else {
            return Err(StructuralError::InvalidExtensions(
                ciborium::de::Error::semantic(None, "authenticator extension keys must be text"),
            ));
        };
        entries.push(ExtensionEntry {
            key,
            value: ReviewValue::from(value),
        });
    }
    Ok(Extensions { entries })
}

impl From<CborValue> for ReviewValue {
    fn from(value: CborValue) -> Self {
        match value {
            CborValue::Null => Self::Null,
            CborValue::Bool(value) => Self::Bool(value),
            CborValue::Integer(value) => Self::Integer(i128::from(value)),
            CborValue::Float(value) => Self::Float(value),
            CborValue::Text(value) => Self::Text(value),
            CborValue::Bytes(value) => Self::Bytes(value),
            CborValue::Array(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            CborValue::Map(entries) => Self::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| (Self::from(key), Self::from(value)))
                    .collect(),
            ),
            CborValue::Tag(tag, value) => Self::Tag(tag, Box::new(Self::from(*value))),
            _ => Self::Null,
        }
    }
}

fn check_signature(signature: &[u8], algorithm: SignatureAlgorithm) -> Result<(), StructuralError> {
    match algorithm {
        SignatureAlgorithm::Ed25519 => {
            if signature.len() == 64 {
                Ok(())
            } else {
                Err(StructuralError::SignatureLength {
                    algorithm,
                    expected: 64,
                    found: signature.len(),
                })
            }
        }
        SignatureAlgorithm::Ecdsa => {
            if is_der_ecdsa(signature) {
                Ok(())
            } else {
                Err(StructuralError::InvalidDerSignature)
            }
        }
        SignatureAlgorithm::Rsa => {
            if matches!(signature.len(), 256 | 384 | 512) {
                Ok(())
            } else {
                Err(StructuralError::RsaSignatureLength {
                    found: signature.len(),
                })
            }
        }
    }
}

fn is_der_ecdsa(signature: &[u8]) -> bool {
    let Some((body, rest)) = der_tlv(signature, 0x30) else {
        return false;
    };
    if !rest.is_empty() {
        return false;
    }
    let Some((r, rest)) = der_tlv(body, 0x02) else {
        return false;
    };
    let Some((s, rest)) = der_tlv(rest, 0x02) else {
        return false;
    };
    rest.is_empty() && is_positive_der_integer(r) && is_positive_der_integer(s)
}

/// Returns the value of a definite-length TLV whose tag matches and the bytes
/// after it. Lengths must use DER's minimal form.
fn der_tlv(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (actual, rest) = input.split_first()?;
    if *actual != tag {
        return None;
    }
    let (first, rest) = rest.split_first()?;
    let (len, rest) = if *first < 0x80 {
        (usize::from(*first), rest)
    } else {
        let count = usize::from(*first & 0x7f);
        if count == 0 || count > 3 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(count)?;
        if bytes[0] == 0 {
            return None;
        }
        let mut len = 0usize;
        for byte in bytes {
            len = len.checked_mul(256)?.checked_add(usize::from(*byte))?;
        }
        if len < 0x80 {
            return None;
        }
        (len, rest)
    };
    let (value, rest) = rest.split_at_checked(len)?;
    Some((value, rest))
}

fn is_positive_der_integer(bytes: &[u8]) -> bool {
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    if first & 0x80 != 0 {
        return false;
    }
    if *first == 0 {
        return matches!(rest.first(), Some(next) if next & 0x80 != 0);
    }
    true
}
