// SPDX-License-Identifier: AGPL-3.0-or-later
//! Passkey creation, batch assertions and the upload request ([docs/06 §3-§4](../../../docs/06-privacy-and-payload.md)).

use gloo_net::http::Request;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use serde::Deserialize;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use looklab_wasm::AssertionFields;

use crate::base::api_url;
use crate::request_digest::{RequestDomain, challenge, request_digest};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialId(Vec<u8>);

impl core::ops::Deref for CredentialId {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// The WebAuthn relying-party ID the server signs challenges for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RpId(String);

impl RpId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RpId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid = !value.is_empty()
            && value.len() <= 253
            && !value.starts_with('.')
            && !value.ends_with('.')
            && !value.contains("..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
        if valid {
            Ok(Self(value))
        } else {
            Err("invalid relying-party id".to_owned())
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NonceReply {
    pub nonce: [u8; 32],
    pub expires_in_seconds: u64,
    pub rp_id: RpId,
    pub pow: Option<PowParameters>,
}

/// The assertion fields the intake API expects alongside a request payload.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SignedFields {
    pub nonce: String,
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignedRequest {
    pub nonce: [u8; 32],
    pub pow: Option<PowParameters>,
    pub fields: SignedFields,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PowParameters {
    pub memory_kib: u32,
    pub iterations: u32,
    pub difficulty_bits_per_image: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadReply {
    pub status_token: Option<String>,
    pub contribution_id: String,
}

#[derive(Deserialize)]
struct NonceWire {
    nonce: String,
    expires_in_seconds: u64,
    rp_id: String,
    #[serde(default)]
    pow: Option<PowWire>,
}

#[derive(Deserialize)]
struct PowWire {
    memory_kib: u32,
    iterations: u32,
    difficulty_bits_per_image: f64,
}

/// # Errors
/// Reports a server or browser failure while issuing the nonce.
pub async fn fetch_nonce() -> Result<NonceReply, String> {
    let response = Request::post(&api_url("/api/nonce"))
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("nonce: HTTP {}", response.status()));
    }
    let wire: NonceWire = response.json().await.map_err(|error| error.to_string())?;
    let bytes = decode_base64url(&wire.nonce).ok_or_else(|| "nonce: not base64url".to_owned())?;
    let nonce: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "nonce: not 32 bytes".to_owned())?;
    Ok(NonceReply {
        nonce,
        expires_in_seconds: wire.expires_in_seconds,
        rp_id: RpId::try_from(wire.rp_id).map_err(|_| "nonce: invalid relying-party id")?,
        pow: wire.pow.map(|pow| PowParameters {
            memory_kib: pow.memory_kib,
            iterations: pow.iterations,
            difficulty_bits_per_image: pow.difficulty_bits_per_image,
        }),
    })
}

pub async fn create_profile(
    display_name: Option<DisplayHandle>,
) -> Result<CredentialId, SignInError> {
    let response = Request::post(&api_url("/api/passkey/register/start"))
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str(
            &serde_json::json!({ "display_handle": display_name }).to_string(),
        )))
        .map_err(|_| SignInError::Registration)?
        .send()
        .await
        .map_err(|_| SignInError::Network)?;
    if !response.ok() {
        return Err(SignInError::Registration);
    }
    let start: Value = response
        .json()
        .await
        .map_err(|_| SignInError::InvalidResponse)?;
    let handle = start
        .get("handle")
        .and_then(Value::as_str)
        .ok_or(SignInError::InvalidResponse)?
        .to_owned();
    let options = start.get("options").ok_or(SignInError::InvalidResponse)?;
    let options =
        js_sys::JSON::parse(&options.to_string()).map_err(|_| SignInError::InvalidResponse)?;
    patch_creation_options(&options).map_err(|_| SignInError::InvalidResponse)?;

    let credential = call_credentials(CredentialMethod::Create, &options)
        .await
        .map_err(|_| SignInError::Prompt)?;
    let credential_json =
        credential_to_json(&credential).map_err(|_| SignInError::InvalidResponse)?;
    let body = serde_json::json!({ "handle": handle, "credential": credential_json });
    let response = Request::post(&api_url("/api/passkey/register/finish"))
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str(&body.to_string())))
        .map_err(|_| SignInError::Registration)?
        .send()
        .await
        .map_err(|_| SignInError::Network)?;
    if !response.ok() {
        return Err(SignInError::Registration);
    }
    let raw_id = credential_json
        .get("rawId")
        .and_then(Value::as_str)
        .ok_or(SignInError::InvalidResponse)?;
    let id = decode_base64url(raw_id).ok_or(SignInError::InvalidResponse)?;
    crate::idb::put(crate::idb::StorageKey::PENDING_PROFILE, raw_id)
        .await
        .map_err(|_| SignInError::Storage)?;
    Ok(CredentialId(id))
}

/// Signs one batch challenge with the contributing passkey.
///
/// # Errors
/// Reports a WebAuthn failure or a malformed assertion.
pub async fn sign_batch(
    challenge: &[u8],
    credential_id: Option<&[u8]>,
    rp_id: &RpId,
) -> Result<AssertionFields, String> {
    let public_key = Object::new();
    set(&public_key, "challenge", &Uint8Array::from(challenge))?;
    set(&public_key, "rpId", &JsValue::from_str(rp_id.as_str()))?;
    set(
        &public_key,
        "userVerification",
        &JsValue::from_str("required"),
    )?;
    set(&public_key, "timeout", &JsValue::from_f64(60_000.0))?;
    if let Some(credential_id) = credential_id {
        let allow = Array::new();
        let descriptor = Object::new();
        set(&descriptor, "type", &JsValue::from_str("public-key"))?;
        set(&descriptor, "id", &Uint8Array::from(credential_id))?;
        allow.push(&descriptor);
        set(&public_key, "allowCredentials", &allow)?;
    }
    let options = Object::new();
    set(&options, "publicKey", &public_key)?;

    let credential = call_credentials(CredentialMethod::Get, &options.into()).await?;
    let json = credential_to_json(&credential)?;
    assertion_from_json(&json)
}

/// # Errors
/// Reports a server failure while issuing the nonce, or a WebAuthn failure.
pub async fn sign_request(
    domain: RequestDomain,
    payload: &[u8],
    credential_id: Option<&[u8]>,
) -> Result<SignedRequest, String> {
    sign_request_classified(domain, payload, credential_id)
        .await
        .map_err(|error| match error {
            SigningFailure::Nonce(message) | SigningFailure::Prompt(message) => message,
        })
}

enum SigningFailure {
    Nonce(String),
    Prompt(String),
}

async fn sign_request_classified(
    domain: RequestDomain,
    payload: &[u8],
    credential_id: Option<&[u8]>,
) -> Result<SignedRequest, SigningFailure> {
    let nonce = fetch_nonce().await.map_err(SigningFailure::Nonce)?;
    let digest = request_digest(domain, payload);
    let challenge = challenge(&nonce.nonce, &digest);
    let assertion = sign_batch(&challenge, credential_id, &nonce.rp_id)
        .await
        .map_err(SigningFailure::Prompt)?;
    Ok(SignedRequest {
        nonce: nonce.nonce,
        pow: nonce.pow,
        fields: SignedFields {
            nonce: encode_base64url(&nonce.nonce),
            credential_id: encode_base64url(&assertion.credential_id),
            authenticator_data: encode_base64url(&assertion.authenticator_data),
            client_data_json: encode_base64url(&assertion.client_data_json),
            signature: encode_base64url(&assertion.signature),
        },
    })
}

pub async fn stored_credential() -> Result<CredentialId, SignInError> {
    let text = crate::idb::get(crate::idb::StorageKey::CREDENTIAL)
        .await
        .map_err(|_| SignInError::Storage)?
        .ok_or(SignInError::MissingCredential)?;
    decode_base64url(&text)
        .filter(|bytes| !bytes.is_empty())
        .map(CredentialId)
        .ok_or(SignInError::MissingCredential)
}

pub async fn profile_credential(
    session: crate::session::SessionContext,
) -> Result<CredentialId, String> {
    session
        .require_profile()
        .await
        .ok_or_else(|| "Sign-in was cancelled.".to_owned())?;
    stored_credential().await.map_err(|error| error.to_string())
}

#[derive(Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct StatusToken(String);

impl StatusToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StatusToken {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match decode_base64url(&value) {
            Some(bytes) if bytes.len() == 32 => Ok(Self(value)),
            _ => Err("invalid status token".to_owned()),
        }
    }
}

impl core::fmt::Debug for StatusToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("StatusToken(..)")
    }
}

impl<'de> Deserialize<'de> for StatusToken {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct DisplayHandle(String);

impl DisplayHandle {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for DisplayHandle {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.chars().count() > 64 || value.chars().any(char::is_control) {
            return Err(
                "Choose a display name of up to 64 characters, without control characters."
                    .to_owned(),
            );
        }
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for DisplayHandle {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRole {
    Moderator,
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTier {
    New,
    Endorsed,
    Seeder,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Session {
    pub status_token: StatusToken,
    pub roles: Vec<SessionRole>,
    pub tier: SessionTier,
    pub display_handle: Option<DisplayHandle>,
}

#[derive(Clone, Copy, Debug)]
pub enum SignInError {
    Prompt,
    Nonce,
    UnknownPasskey,
    Network,
    Registration,
    Work,
    Rejected,
    InvalidResponse,
    MissingCredential,
    Expired,
    Storage,
}

impl core::fmt::Display for SignInError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Prompt => "The passkey prompt did not finish. Try again and choose the device where you saved your passkey.",
            Self::Nonce => "Couldn't prepare sign-in. Check your connection and try again.",
            Self::UnknownPasskey => "We don't recognise that passkey. Create a profile instead?",
            Self::Network => "Couldn't reach looklab. Check your connection and try again.",
            Self::Registration => "Couldn't create your profile. Try again, or use a different passkey store.",
            Self::Work => "Couldn't prepare your new profile. Try again on this device.",
            Self::Rejected => "Sign-in was not accepted. Try again with your profile's passkey.",
            Self::InvalidResponse => "looklab returned an unreadable response. Try again later.",
            Self::MissingCredential => "Sign out here, then sign in with your profile's passkey before continuing.",
            Self::Expired => "This sign-in link no longer works. Sign in with your passkey instead.",
            Self::Storage => "This browser couldn't remember your sign-in. Allow site storage and try again.",
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum SessionErrorCode {
    CredentialUnknown,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SessionErrorReply {
    error: SessionErrorCode,
}

/// # Errors
/// Reports a server rejection or a WebAuthn failure.
pub async fn sign_in(selected: Option<&CredentialId>) -> Result<Session, SignInError> {
    let pending = crate::idb::get(crate::idb::StorageKey::PENDING_PROFILE)
        .await
        .map_err(|_| SignInError::Storage)?;
    let signed = sign_request_classified(RequestDomain::Session, &[], selected.map(|id| &**id))
        .await
        .map_err(|error| match error {
            SigningFailure::Nonce(_) => SignInError::Nonce,
            SigningFailure::Prompt(_) => SignInError::Prompt,
        })?;
    #[derive(serde::Serialize)]
    struct SessionRequest<'a> {
        #[serde(flatten)]
        signed: &'a SignedFields,
        #[serde(skip_serializing_if = "Option::is_none")]
        pow: Option<Vec<u64>>,
    }
    let pending_profile = pending.as_deref() == Some(signed.fields.credential_id.as_str());
    let pow = if pending_profile {
        let digest = request_digest(RequestDomain::Session, &[]);
        Some(
            crate::data::solve_pow(
                challenge(&signed.nonce, &digest),
                signed.pow.unwrap_or_else(crate::data::default_parameters),
            )
            .await
            .map_err(|_| SignInError::Work)?,
        )
    } else {
        None
    };
    let response = Request::post(&api_url("/api/session"))
        .json(&SessionRequest {
            signed: &signed.fields,
            pow,
        })
        .map_err(|_| SignInError::InvalidResponse)?
        .send()
        .await
        .map_err(|_| SignInError::Network)?;
    if !response.ok() {
        return Err(match response.json::<SessionErrorReply>().await {
            Ok(reply) if matches!(reply.error, SessionErrorCode::CredentialUnknown) => {
                SignInError::UnknownPasskey
            }
            _ => SignInError::Rejected,
        });
    }
    let session = response
        .json()
        .await
        .map_err(|_| SignInError::InvalidResponse)?;
    crate::idb::put(
        crate::idb::StorageKey::CREDENTIAL,
        &signed.fields.credential_id,
    )
    .await
    .map_err(|_| SignInError::Storage)?;
    if pending_profile {
        crate::idb::remove(crate::idb::StorageKey::PENDING_PROFILE)
            .await
            .map_err(|_| SignInError::Storage)?;
    }
    Ok(session)
}

pub async fn restore_session(token: &StatusToken) -> Result<Session, SignInError> {
    let response = Request::get(&api_url("/api/session"))
        .header("Authorization", &format!("Bearer {}", token.as_str()))
        .send()
        .await
        .map_err(|_| SignInError::Network)?;
    if !response.ok() {
        return Err(if matches!(response.status(), 401 | 403 | 404) {
            SignInError::Expired
        } else {
            SignInError::Rejected
        });
    }
    response
        .json()
        .await
        .map_err(|_| SignInError::InvalidResponse)
}

/// # Errors
/// Reports a server rejection or a WebAuthn failure.
pub async fn sign_out_everywhere() -> Result<(), SignInError> {
    let credential = stored_credential().await?;
    let signed = sign_request_classified(RequestDomain::SessionRevoke, &[], Some(&credential))
        .await
        .map_err(|error| match error {
            SigningFailure::Nonce(_) => SignInError::Nonce,
            SigningFailure::Prompt(_) => SignInError::Prompt,
        })?;
    let response = Request::post(&api_url("/api/session/revoke"))
        .json(&signed.fields)
        .map_err(|_| SignInError::InvalidResponse)?
        .send()
        .await
        .map_err(|_| SignInError::Network)?;
    if !response.ok() {
        return Err(SignInError::Rejected);
    }
    Ok(())
}

/// # Errors
/// Reports a server rejection of the envelope.
pub async fn upload_envelope(envelope: Vec<u8>) -> Result<UploadReply, String> {
    let response = Request::post(&api_url("/api/upload"))
        .header("Content-Type", "application/octet-stream")
        .body(Some(JsValue::from(Uint8Array::from(envelope.as_slice()))))
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("upload: HTTP {}", response.status()));
    }
    #[derive(Deserialize)]
    struct Wire {
        status_token: Option<String>,
        contribution_id: String,
    }
    let wire: Wire = response.json().await.map_err(|error| error.to_string())?;
    Ok(UploadReply {
        status_token: wire.status_token,
        contribution_id: wire.contribution_id,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CredentialMethod {
    Create,
    Get,
}

impl CredentialMethod {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Get => "get",
        }
    }
}

async fn call_credentials(method: CredentialMethod, options: &JsValue) -> Result<JsValue, String> {
    let name = method.as_str();
    let window = web_sys::window().ok_or_else(|| "no window".to_owned())?;
    let navigator = window.navigator();
    let container = Reflect::get(&navigator, &JsValue::from_str("credentials"))
        .map_err(|error| format!("{error:?}"))?;
    let function = Reflect::get(&container, &JsValue::from_str(name))
        .map_err(|error| format!("{error:?}"))?
        .dyn_into::<Function>()
        .map_err(|_| format!("credentials.{name} is unavailable"))?;
    let promise = function
        .call1(&container, options)
        .map_err(|error| format!("credentials.{name}: {error:?}"))?
        .dyn_into::<Promise>()
        .map_err(|_| format!("credentials.{name} did not return a promise"))?;
    JsFuture::from(promise)
        .await
        .map_err(|error| format!("credentials.{name}: {error:?}"))
}

fn patch_creation_options(options: &JsValue) -> Result<(), String> {
    let public_key = Reflect::get(options, &JsValue::from_str("publicKey"))
        .map_err(|error| format!("{error:?}"))?;
    replace_challenge(&public_key)?;
    let user = Reflect::get(&public_key, &JsValue::from_str("user"))
        .map_err(|error| format!("{error:?}"))?;
    replace_string_with_bytes(&user, "id")?;
    patch_descriptors(&public_key, "excludeCredentials")?;
    Ok(())
}

fn replace_challenge(public_key: &JsValue) -> Result<(), String> {
    replace_string_with_bytes(public_key, "challenge")
}

fn patch_descriptors(public_key: &JsValue, key: &str) -> Result<(), String> {
    let Ok(value) = Reflect::get(public_key, &JsValue::from_str(key)) else {
        return Ok(());
    };
    if !value.is_array() {
        return Ok(());
    }
    let descriptors = value
        .dyn_into::<Array>()
        .map_err(|_| "descriptors".to_owned())?;
    for descriptor in descriptors.iter() {
        replace_string_with_bytes(&descriptor, "id")?;
    }
    Ok(())
}

fn replace_string_with_bytes(object: &JsValue, key: &str) -> Result<(), String> {
    let value =
        Reflect::get(object, &JsValue::from_str(key)).map_err(|error| format!("{error:?}"))?;
    let Some(text) = value.as_string() else {
        return Ok(());
    };
    let bytes = decode_base64url(&text).ok_or_else(|| format!("{key} is not base64url"))?;
    set(object, key, &Uint8Array::from(bytes.as_slice()))
}

fn set(object: &JsValue, key: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(key), value)
        .map(|_| ())
        .map_err(|error| format!("{error:?}"))
}

fn credential_to_json(credential: &JsValue) -> Result<Value, String> {
    if let Ok(function) = Reflect::get(credential, &JsValue::from_str("toJSON"))
        && let Ok(function) = function.dyn_into::<Function>()
    {
        let value = function
            .call0(credential)
            .map_err(|error| format!("toJSON: {error:?}"))?;
        let text = js_sys::JSON::stringify(&value)
            .map_err(|error| format!("{error:?}"))?
            .as_string()
            .ok_or_else(|| "credential JSON is not a string".to_owned())?;
        return serde_json::from_str(&text).map_err(|error| error.to_string());
    }
    let mut out = serde_json::Map::new();
    out.insert(
        "id".to_owned(),
        Value::String(string_field(credential, "id")?),
    );
    out.insert(
        "rawId".to_owned(),
        Value::String(encode_base64url(&bytes_field(credential, "rawId")?)),
    );
    out.insert(
        "type".to_owned(),
        Value::String(string_field(credential, "type")?),
    );
    let response = Reflect::get(credential, &JsValue::from_str("response"))
        .map_err(|error| format!("{error:?}"))?;
    let mut response_json = serde_json::Map::new();
    for key in [
        "clientDataJSON",
        "attestationObject",
        "authenticatorData",
        "signature",
    ] {
        if let Ok(bytes) = bytes_field(&response, key) {
            response_json.insert(key.to_owned(), Value::String(encode_base64url(&bytes)));
        }
    }
    out.insert("response".to_owned(), Value::Object(response_json));
    out.insert(
        "clientExtensionResults".to_owned(),
        Value::Object(serde_json::Map::new()),
    );
    Ok(Value::Object(out))
}

fn string_field(object: &JsValue, key: &str) -> Result<String, String> {
    Reflect::get(object, &JsValue::from_str(key))
        .map_err(|error| format!("{error:?}"))?
        .as_string()
        .ok_or_else(|| format!("{key} is not a string"))
}

fn bytes_field(object: &JsValue, key: &str) -> Result<Vec<u8>, String> {
    let value =
        Reflect::get(object, &JsValue::from_str(key)).map_err(|error| format!("{error:?}"))?;
    if value.is_undefined() || value.is_null() {
        return Err(format!("{key} is absent"));
    }
    Ok(js_bytes(&value))
}

fn js_bytes(value: &JsValue) -> Vec<u8> {
    if let Ok(array) = value.clone().dyn_into::<Uint8Array>() {
        return array.to_vec();
    }
    Uint8Array::new(value).to_vec()
}

fn assertion_from_json(json: &Value) -> Result<AssertionFields, String> {
    let credential_id = field_bytes(json, "rawId")?;
    let response = json
        .get("response")
        .ok_or_else(|| "assertion has no response".to_owned())?;
    Ok(AssertionFields {
        credential_id,
        authenticator_data: field_bytes(response, "authenticatorData")?,
        client_data_json: field_bytes(response, "clientDataJSON")?,
        signature: field_bytes(response, "signature")?,
    })
}

fn field_bytes(json: &Value, key: &str) -> Result<Vec<u8>, String> {
    decode_base64url(
        json.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("missing {key}"))?,
    )
    .ok_or_else(|| format!("{key} is not base64url"))
}

pub fn encode_base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        out.push(ALPHABET[(b0 >> 2) as usize] as char);
        out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
        }
    }
    out
}

pub fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    let mut values = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        values.push(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        });
    }
    let mut out = Vec::with_capacity(values.len() * 3 / 4);
    for chunk in values.chunks(4) {
        match chunk.len() {
            4 => {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
                out.push((chunk[1] << 4) | (chunk[2] >> 2));
                out.push((chunk[2] << 6) | chunk[3]);
            }
            3 => {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
                out.push((chunk[1] << 4) | (chunk[2] >> 2));
            }
            2 => {
                out.push((chunk[0] << 2) | (chunk[1] >> 4));
            }
            _ => return None,
        }
    }
    Some(out)
}
