// SPDX-License-Identifier: AGPL-3.0-or-later
//! A local intake stub for the browser end-to-end flow: static site plus real
//! WebAuthn verification, tiny proof of work and an in-memory status feed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_upload::{
    ExpectedAssertion, PowParams, SignatureAlgorithm, StructuralVerdict, check_structure,
    parse_envelope_strict, verify as verify_pow,
};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyRegistration, RegisterPublicKeyCredential,
    Url, Uuid, Webauthn, WebauthnBuilder,
};

const POW_MEMORY_KIB: u32 = 8;
const POW_ITERATIONS: u32 = 1;
const POW_DIFFICULTY: f64 = 0.0;

struct Stub {
    dist: PathBuf,
    origin: String,
    rp_id: String,
    webauthn: Webauthn,
    nonces: HashMap<[u8; 32], Instant>,
    registrations: HashMap<String, PasskeyRegistration>,
    passkeys: Vec<Passkey>,
    tokens: HashMap<String, (String, String)>,
    next_contribution: u64,
}

#[derive(Serialize)]
struct NonceResponse {
    nonce: String,
    expires_in_seconds: u64,
    pow: PowView,
}

#[derive(Serialize)]
struct PowView {
    memory_kib: u32,
    iterations: u32,
    difficulty_bits_per_image: f64,
}

#[derive(Serialize)]
struct RegisterStartResponse {
    handle: String,
    options: CreationChallengeResponse,
}

#[derive(Deserialize)]
struct RegisterFinishRequest {
    handle: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Serialize)]
struct UploadResponse {
    status_token: String,
    contribution_id: String,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum FeedStatus {
    Accepted,
}

#[derive(Serialize)]
struct StatusResponse {
    credential: String,
    contributions: Vec<ContributionView>,
}

#[derive(Serialize)]
struct ContributionView {
    id: String,
    status: FeedStatus,
    created_at: i64,
    images: Vec<ImageView>,
}

#[derive(Serialize)]
struct ImageView {
    id: String,
    status: FeedStatus,
    reason: Option<String>,
    rms_de: Option<f64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut dist = PathBuf::from("site/dist");
    let mut port = 8099u16;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dist" => {
                dist = PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("--dist path"))?);
            }
            "--port" => {
                port = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--port"))?
                    .parse()?;
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let origin = format!("http://localhost:{port}");
    let rp_id = "localhost".to_owned();
    let webauthn = WebauthnBuilder::new(&rp_id, &Url::parse(&origin)?)?
        .rp_name("looklab stub")
        .build()?;
    println!("stub listening on {origin}");
    let stub = Arc::new(Mutex::new(Stub {
        dist,
        origin,
        rp_id,
        webauthn,
        nonces: HashMap::new(),
        registrations: HashMap::new(),
        passkeys: Vec::new(),
        tokens: HashMap::new(),
        next_contribution: 1,
    }));

    let app = Router::new()
        .route("/api/nonce", post(nonce))
        .route("/api/passkey/register/start", post(register_start))
        .route("/api/passkey/register/finish", post(register_finish))
        .route("/api/upload", post(upload))
        .route("/status/{token}", get(status))
        .fallback(serve)
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(stub);

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn nonce(State(state): State<Arc<Mutex<Stub>>>) -> Response {
    let mut stub = state.lock().expect("stub lock");
    let now = Instant::now();
    stub.nonces.retain(|_, expires| *expires > now);
    let mut bytes = [0u8; 32];
    fill_random(&mut bytes);
    stub.nonces
        .insert(bytes, now + Duration::from_secs(300));
    Json(NonceResponse {
        nonce: URL_SAFE_NO_PAD.encode(bytes),
        expires_in_seconds: 300,
        pow: PowView {
            memory_kib: POW_MEMORY_KIB,
            iterations: POW_ITERATIONS,
            difficulty_bits_per_image: POW_DIFFICULTY,
        },
    })
    .into_response()
}

async fn register_start(State(state): State<Arc<Mutex<Stub>>>) -> Response {
    let mut stub = state.lock().expect("stub lock");
    let (options, registration) = match stub.webauthn.start_passkey_registration(
        Uuid::new_v4(),
        "contributor",
        "Contributor",
        None,
    ) {
        Ok(result) => result,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let handle = URL_SAFE_NO_PAD.encode(random_bytes::<16>());
    stub.registrations.insert(handle.clone(), registration);
    Json(RegisterStartResponse { handle, options }).into_response()
}

async fn register_finish(
    State(state): State<Arc<Mutex<Stub>>>,
    Json(payload): Json<RegisterFinishRequest>,
) -> Response {
    let mut stub = state.lock().expect("stub lock");
    let Some(registration) = stub.registrations.remove(&payload.handle) else {
        return error_response(StatusCode::NOT_FOUND, "unknown registration");
    };
    match stub
        .webauthn
        .finish_passkey_registration(&payload.credential, &registration)
    {
        Ok(passkey) => {
            stub.passkeys.push(passkey);
            Json(serde_json::json!({ "state": "pending_upload" })).into_response()
        }
        Err(error) => error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    }
}

async fn upload(State(state): State<Arc<Mutex<Stub>>>, body: Bytes) -> Response {
    let mut stub = state.lock().expect("stub lock");
    let envelope = match parse_envelope_strict(&body, &looklab_upload::Limits::default()) {
        Ok(envelope) => envelope,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let now = Instant::now();
    match stub.nonces.remove(envelope.nonce.as_bytes()) {
        Some(expires) if expires > now => {}
        _ => return error_response(StatusCode::BAD_REQUEST, "nonce unknown or expired"),
    }
    let Some(passkey) = stub
        .passkeys
        .iter()
        .find(|passkey| passkey.cred_id().as_slice() == envelope.assertion.credential_id.as_slice())
    else {
        return error_response(StatusCode::UNAUTHORIZED, "credential unknown");
    };
    let algorithm = match passkey.cred_algorithm() {
        webauthn_rs::prelude::COSEAlgorithm::ES256
        | webauthn_rs::prelude::COSEAlgorithm::ES384 => SignatureAlgorithm::Ecdsa,
        webauthn_rs::prelude::COSEAlgorithm::RS256 => SignatureAlgorithm::Rsa,
        _ => return error_response(StatusCode::BAD_REQUEST, "algorithm unsupported"),
    };
    let expected = ExpectedAssertion {
        challenge: envelope.challenge(),
        origin: &stub.origin,
        rp_id: &stub.rp_id,
        algorithm,
    };
    match check_structure(&envelope.assertion, &expected) {
        StructuralVerdict::Pass | StructuralVerdict::ReviewNeeded { .. } => {}
        StructuralVerdict::Fail(error) => {
            return error_response(StatusCode::BAD_REQUEST, &error.to_string());
        }
    }
    let mut signed = envelope.assertion.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&envelope.assertion.client_data_json));
    match passkey
        .get_public_key()
        .verify_signature(&envelope.assertion.signature, &signed)
    {
        Ok(true) => {}
        _ => return error_response(StatusCode::UNAUTHORIZED, "signature invalid"),
    }
    let n_images = u32::try_from(envelope.manifest.images.len()).unwrap_or(u32::MAX);
    if verify_pow(
        &envelope.challenge(),
        n_images,
        &PowParams {
            memory_kib: POW_MEMORY_KIB,
            iterations: POW_ITERATIONS,
            difficulty_bits_per_image: POW_DIFFICULTY,
        },
        &envelope.pow,
    )
    .is_err()
    {
        return error_response(StatusCode::BAD_REQUEST, "proof of work invalid");
    }

    let contribution = format!("contribution-{}", stub.next_contribution);
    stub.next_contribution += 1;
    let token = URL_SAFE_NO_PAD.encode(random_bytes::<16>());
    let credential = URL_SAFE_NO_PAD.encode(envelope.assertion.credential_id.clone());
    stub.tokens
        .insert(token.clone(), (credential, contribution.clone()));
    Json(UploadResponse {
        status_token: token,
        contribution_id: contribution,
    })
    .into_response()
}

async fn status(State(state): State<Arc<Mutex<Stub>>>, AxumPath(token): AxumPath<String>) -> Response {
    let stub = state.lock().expect("stub lock");
    let Some((credential, contribution)) = stub.tokens.get(&token) else {
        return error_response(StatusCode::NOT_FOUND, "unknown token");
    };
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    Json(StatusResponse {
        credential: credential.clone(),
        contributions: vec![ContributionView {
            id: contribution.clone(),
            status: FeedStatus::Accepted,
            created_at,
            images: (0..1)
                .map(|index| ImageView {
                    id: format!("{contribution}-image-{index}"),
                    status: FeedStatus::Accepted,
                    reason: None,
                    rms_de: None,
                })
                .collect(),
        }],
    })
    .into_response()
}

async fn serve(State(state): State<Arc<Mutex<Stub>>>, uri: Uri) -> Response {
    let dist = state.lock().expect("stub lock").dist.clone();
    let requested = uri.path().trim_start_matches('/');
    if requested.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let candidate = if requested.is_empty() {
        dist.join("index.html")
    } else {
        dist.join(requested)
    };
    let file = if candidate.is_file() {
        candidate
    } else {
        dist.join("index.html")
    };
    match std::fs::read(&file) {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type(&file))],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    fill_random(&mut bytes);
    bytes
}

fn fill_random(bytes: &mut [u8]) {
    getrandom::fill(bytes).expect("the stub runs on a host with OS randomness");
}
