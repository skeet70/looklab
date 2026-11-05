// SPDX-License-Identifier: AGPL-3.0-or-later
//! Passkey creation, batch assertions and the upload request ([docs/06 §3-§4](../../../docs/06-privacy-and-payload.md)).

use gloo_net::http::Request;
use js_sys::{Array, Function, Object, Promise, Reflect, Uint8Array};
use serde::Deserialize;
use serde_json::Value;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use looklab_wasm::AssertionFields;

use crate::request_digest::{RequestDomain, challenge, request_digest};

#[derive(Clone, Debug, PartialEq)]
pub struct NonceReply {
    pub nonce: [u8; 32],
    pub expires_in_seconds: u64,
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
    let response = Request::post("/api/nonce")
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
        pow: wire.pow.map(|pow| PowParameters {
            memory_kib: pow.memory_kib,
            iterations: pow.iterations,
            difficulty_bits_per_image: pow.difficulty_bits_per_image,
        }),
    })
}

/// Creates a passkey at the first contribution and returns its credential id.
///
/// # Errors
/// Reports a server rejection or a WebAuthn failure.
pub async fn create_passkey() -> Result<Vec<u8>, String> {
    let response = Request::post("/api/passkey/register/start")
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str("{}")))
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("passkey start: HTTP {}", response.status()));
    }
    let start: Value = response.json().await.map_err(|error| error.to_string())?;
    let handle = start
        .get("handle")
        .and_then(Value::as_str)
        .ok_or_else(|| "passkey start: no handle".to_owned())?
        .to_owned();
    let options = start
        .get("options")
        .ok_or_else(|| "passkey start: no options".to_owned())?;
    let options = js_sys::JSON::parse(&options.to_string())
        .map_err(|error| format!("passkey start options: {error:?}"))?;
    patch_creation_options(&options)?;

    let credential = call_credentials(CredentialMethod::Create, &options).await?;
    let credential_json = credential_to_json(&credential)?;
    let body = serde_json::json!({ "handle": handle, "credential": credential_json });
    let response = Request::post("/api/passkey/register/finish")
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str(&body.to_string())))
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("passkey finish: HTTP {}", response.status()));
    }
    let raw_id = credential_json
        .get("rawId")
        .and_then(Value::as_str)
        .ok_or_else(|| "passkey finish: no rawId".to_owned())?;
    decode_base64url(raw_id).ok_or_else(|| "passkey finish: rawId is not base64url".to_owned())
}

/// Signs one batch challenge with the contributing passkey.
///
/// # Errors
/// Reports a WebAuthn failure or a malformed assertion.
pub async fn sign_batch(
    challenge: &[u8],
    credential_id: Option<&[u8]>,
) -> Result<AssertionFields, String> {
    let public_key = Object::new();
    set(&public_key, "challenge", &Uint8Array::from(challenge))?;
    set(&public_key, "rpId", &JsValue::from_str(&rp_id()))?;
    set(
        &public_key,
        "userVerification",
        &JsValue::from_str("preferred"),
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
    let nonce = fetch_nonce().await?;
    let digest = request_digest(domain, payload);
    let challenge = challenge(&nonce.nonce, &digest);
    let assertion = sign_batch(&challenge, credential_id).await?;
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

/// Returns the stored credential id, registering a passkey on first use.
///
/// # Errors
/// Reports a WebAuthn failure or a storage failure.
pub async fn ensure_credential() -> Result<Vec<u8>, String> {
    if let Some(text) = crate::idb::get(crate::idb::StorageKey::CREDENTIAL).await? {
        return decode_base64url(&text)
            .ok_or_else(|| "the stored credential id is malformed".to_owned());
    }
    let id = create_passkey().await?;
    crate::idb::put(crate::idb::StorageKey::CREDENTIAL, &encode_base64url(&id)).await?;
    Ok(id)
}

/// Re-issues the status link from a passkey assertion ([docs/06 §4](../../../docs/06-privacy-and-payload.md)).
///
/// # Errors
/// Reports a server rejection or a WebAuthn failure.
pub async fn reissue_status_token(credential_id: Option<&[u8]>) -> Result<String, String> {
    let signed = sign_request(RequestDomain::StatusReissue, &[], credential_id).await?;
    let body = serde_json::to_value(&signed.fields).map_err(|error| error.to_string())?;
    let response = Request::post("/api/status/reissue")
        .header("Content-Type", "application/json")
        .body(Some(JsValue::from_str(&body.to_string())))
        .map_err(|error| error.to_string())?
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.ok() {
        return Err(format!("status reissue: HTTP {}", response.status()));
    }
    let reply: Value = response.json().await.map_err(|error| error.to_string())?;
    reply
        .get("status_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "status reissue: no token".to_owned())
}

/// # Errors
/// Reports a server rejection of the envelope.
pub async fn upload_envelope(envelope: Vec<u8>) -> Result<UploadReply, String> {
    let response = Request::post("/api/upload")
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

pub fn rp_id() -> String {
    web_sys::window()
        .and_then(|window| window.location().hostname().ok())
        .unwrap_or_default()
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
