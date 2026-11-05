// SPDX-License-Identifier: AGPL-3.0-or-later
//! A local intake stub for the browser end-to-end flow: static site plus real
//! WebAuthn verification, tiny proof of work and an in-memory status feed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Request, State};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_server::{AppState, Config, PowConfig};
use looklab_upload::{
    ExpectedAssertion, Limits, PowParams, SignatureAlgorithm, StructuralVerdict, check_structure,
    parse_envelope_strict, verify as verify_pow,
};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tower::ServiceExt as _;
use webauthn_rs::prelude::{Passkey, Url};

const POW_MEMORY_KIB: u32 = 8;
const POW_ITERATIONS: u32 = 1;
const POW_DIFFICULTY: f64 = 0.0;

struct Stub {
    dist: PathBuf,
    origin: String,
    rp_id: String,
    nonces: HashMap<[u8; 32], Instant>,
    tokens: HashMap<String, (String, String)>,
    next_contribution: u64,
    server: Arc<AppState>,
    server_routes: Router,
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
    let mut state_dir = std::env::temp_dir().join("looklab-stub-state");
    let mut public_origin = None;
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
            "--state" => {
                state_dir =
                    PathBuf::from(args.next().ok_or_else(|| anyhow::anyhow!("--state path"))?);
            }
            "--origin" => {
                public_origin = Some(Url::parse(
                    &args.next().ok_or_else(|| anyhow::anyhow!("--origin URL"))?,
                )?);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    let origin = public_origin
        .map(|url| url.origin().ascii_serialization())
        .unwrap_or_else(|| format!("http://localhost:{port}"));
    let rp_id = Url::parse(&origin)?
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("origin has no host"))?
        .to_owned();
    std::fs::create_dir_all(&state_dir)?;
    let secret_path = state_dir.join("server-secret");
    if !secret_path.exists() {
        std::fs::write(&secret_path, random_bytes::<32>())?;
    }
    let snapshot_dir = state_dir.join("community");
    std::fs::create_dir_all(&snapshot_dir)?;
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let config = Config {
        bind: address,
        db_path: state_dir.join("looklab.sqlite3"),
        blob_dir: state_dir.join("blobs"),
        rp_id: rp_id.clone(),
        origin: origin.clone(),
        server_secret_path: secret_path,
        bodies_dir: PathBuf::from("data/bodies"),
        snapshot_dir,
        nonce_ttl: Duration::from_secs(3600),
        registration_ttl: Duration::from_secs(3600),
        body_timeout: Duration::from_secs(30),
        blob_retention_days: 30,
        pow: PowConfig {
            memory_kib: POW_MEMORY_KIB,
            iterations: POW_ITERATIONS,
            difficulty_bits_per_image: POW_DIFFICULTY,
        },
        limits: Limits::default(),
    };
    let server = Arc::new(AppState::new(config).await?);
    let server_routes = looklab_server::router(server.clone());
    println!("stub listening on {origin}");
    let stub = Arc::new(Mutex::new(Stub {
        dist,
        origin,
        rp_id,
        nonces: HashMap::new(),
        tokens: HashMap::new(),
        next_contribution: 1,
        server,
        server_routes,
    }));

    let app = Router::new()
        .route("/api/nonce", post(nonce))
        .route("/api/session/revoke", post(revoke_session))
        .route("/api/upload", post(upload))
        .route("/api/__stub/moderator/{credential}", post(grant_moderator))
        .route("/api/__stub/access/{credential}", post(grant_access))
        .route("/status/{token}", get(status))
        .route("/api/{*rest}", any(forward_request))
        .route("/log.json", get(forward_request))
        .route("/lists/grants.json", get(forward_request))
        .route("/lists/endorsements.json", get(forward_request))
        .route("/bounties.json", get(forward_request))
        .route("/manifests/{look}/{version}", get(forward_request))
        .fallback(serve)
        .layer(DefaultBodyLimit::max(65 * 1024 * 1024))
        .with_state(stub);

    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn nonce(State(state): State<Arc<Mutex<Stub>>>) -> Response {
    let (bytes, db, issued_at) = {
        let mut stub = state.lock().expect("stub lock");
        let now = Instant::now();
        stub.nonces.retain(|_, expires| *expires > now);
        let mut bytes = [0u8; 32];
        fill_random(&mut bytes);
        stub.nonces.insert(bytes, now + Duration::from_secs(300));
        (bytes, stub.server.db.clone(), unix_now())
    };
    let issued = sqlx::query("INSERT INTO nonces (nonce, issued_at, expires_at) VALUES (?, ?, ?)")
        .bind(hex(&bytes))
        .bind(issued_at)
        .bind(issued_at + 300)
        .execute(&db)
        .await;
    if let Err(error) = issued {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }
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

async fn upload(State(state): State<Arc<Mutex<Stub>>>, body: Bytes) -> Response {
    let envelope = match parse_envelope_strict(&body, &Limits::default()) {
        Ok(envelope) => envelope,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, &error.to_string()),
    };
    let server = state.lock().expect("stub lock").server.clone();
    #[derive(sqlx::FromRow)]
    struct StoredCredential {
        id: String,
        webauthn_credential: String,
    }
    let stored = match sqlx::query_as::<_, StoredCredential>("SELECT id, webauthn_credential FROM credentials WHERE credential_id = ? AND status = 'active'")
        .bind(envelope.assertion.credential_id.as_slice()).fetch_optional(&server.db).await
    {
        Ok(Some(stored)) => stored,
        Ok(None) => return error_response(StatusCode::UNAUTHORIZED, "credential unknown"),
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let passkey = match serde_json::from_str::<Passkey>(&stored.webauthn_credential) {
        Ok(passkey) => passkey,
        Err(error) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    let reply = {
        let mut stub = state.lock().expect("stub lock");
        if let Err(rejection) = validate_upload(&mut stub, &envelope, &passkey) {
            return error_response(rejection.status, &rejection.message);
        }
        let contribution_id = format!("contribution-{}", stub.next_contribution);
        stub.next_contribution += 1;
        let status_token = URL_SAFE_NO_PAD.encode(random_bytes::<32>());
        let credential = URL_SAFE_NO_PAD.encode(&envelope.assertion.credential_id);
        stub.tokens
            .insert(status_token.clone(), (credential, contribution_id.clone()));
        UploadResponse {
            status_token,
            contribution_id,
        }
    };
    if let Err(error) = sqlx::query(
        "INSERT INTO status_tokens (token_hash, credential_id, issued_at) VALUES (?, ?, ?)",
    )
    .bind(Sha256::digest(reply.status_token.as_bytes()).as_slice())
    .bind(stored.id)
    .bind(unix_now())
    .execute(&server.db)
    .await
    {
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
    }
    Json(reply).into_response()
}

struct UploadRejection {
    status: StatusCode,
    message: String,
}

fn validate_upload(
    stub: &mut Stub,
    envelope: &looklab_upload::Envelope,
    passkey: &Passkey,
) -> Result<(), UploadRejection> {
    let reject = |status, message: &str| UploadRejection {
        status,
        message: message.to_owned(),
    };
    let now = Instant::now();
    match stub.nonces.remove(envelope.nonce.as_bytes()) {
        Some(expires) if expires > now => {}
        _ => {
            return Err(reject(StatusCode::BAD_REQUEST, "nonce unknown or expired"));
        }
    }
    let algorithm = match passkey.cred_algorithm() {
        webauthn_rs::prelude::COSEAlgorithm::ES256 | webauthn_rs::prelude::COSEAlgorithm::ES384 => {
            SignatureAlgorithm::Ecdsa
        }
        webauthn_rs::prelude::COSEAlgorithm::RS256 => SignatureAlgorithm::Rsa,
        _ => {
            return Err(reject(StatusCode::BAD_REQUEST, "algorithm unsupported"));
        }
    };
    let expected = ExpectedAssertion {
        challenge: envelope.challenge(),
        origin: &stub.origin,
        rp_id: &stub.rp_id,
        algorithm,
        top_origin: None,
    };
    match check_structure(&envelope.assertion, &expected) {
        StructuralVerdict::Pass | StructuralVerdict::ReviewNeeded { .. } => {}
        StructuralVerdict::Fail(error) => {
            return Err(reject(StatusCode::BAD_REQUEST, &error.to_string()));
        }
    }
    let mut signed = envelope.assertion.authenticator_data.clone();
    signed.extend_from_slice(&Sha256::digest(&envelope.assertion.client_data_json));
    match passkey
        .get_public_key()
        .verify_signature(&envelope.assertion.signature, &signed)
    {
        Ok(true) => {}
        _ => {
            return Err(reject(StatusCode::UNAUTHORIZED, "signature invalid"));
        }
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
        return Err(reject(StatusCode::BAD_REQUEST, "proof of work invalid"));
    }
    Ok(())
}

async fn status(
    State(state): State<Arc<Mutex<Stub>>>,
    AxumPath(token): AxumPath<String>,
    headers: axum::http::HeaderMap,
    uri: Uri,
) -> Response {
    if !token.ends_with(".rss")
        && headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| accept.split(',').any(|item| item.trim() == "text/html"))
    {
        return serve(State(state), uri).await;
    }
    let known = state.lock().expect("stub lock").tokens.get(&token).cloned();
    let Some((credential, contribution)) = known else {
        let routes = state.lock().expect("stub lock").server_routes.clone();
        let request = Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("status request");
        return match routes.oneshot(request).await {
            Ok(response) => response,
            Err(error) => match error {},
        };
    };
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    Json(StatusResponse {
        credential,
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

#[derive(Deserialize)]
#[serde(transparent)]
struct StubCredentialId(String);

#[derive(Deserialize)]
struct SessionIdentity {
    credential_id: StubCredentialId,
}

async fn revoke_session(State(state): State<Arc<Mutex<Stub>>>, body: Bytes) -> Response {
    let identity: SessionIdentity = match serde_json::from_slice(&body) {
        Ok(identity) => identity,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "invalid session request"),
    };
    let request = Request::builder()
        .method("POST")
        .uri("/api/session/revoke")
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("session request");
    let response = forward_request(State(state.clone()), request).await;
    if response.status().is_success() {
        state
            .lock()
            .expect("stub lock")
            .tokens
            .retain(|_, (credential, _)| credential != &identity.credential_id.0);
    }
    response
}

async fn grant_moderator(
    State(state): State<Arc<Mutex<Stub>>>,
    AxumPath(credential): AxumPath<StubCredentialId>,
) -> Response {
    set_access(state, credential, StubAccess::Moderator).await
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StubAccess {
    Moderator,
    Admin,
    Seeder,
}

async fn grant_access(
    State(state): State<Arc<Mutex<Stub>>>,
    AxumPath(credential): AxumPath<StubCredentialId>,
    Json(access): Json<StubAccess>,
) -> Response {
    set_access(state, credential, access).await
}

async fn set_access(
    state: Arc<Mutex<Stub>>,
    credential: StubCredentialId,
    access: StubAccess,
) -> Response {
    let Ok(raw_id) = URL_SAFE_NO_PAD.decode(&credential.0) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid credential id");
    };
    let server = state.lock().expect("stub lock").server.clone();
    let query = match access {
        StubAccess::Moderator => {
            "UPDATE credentials SET roles = '[\"moderator\"]' WHERE credential_id = ?"
        }
        StubAccess::Admin => "UPDATE credentials SET roles = '[\"admin\"]' WHERE credential_id = ?",
        StubAccess::Seeder => "UPDATE credentials SET tier = 'seeder' WHERE credential_id = ?",
    };
    match sqlx::query(query).bind(raw_id).execute(&server.db).await {
        Ok(result) if result.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => error_response(StatusCode::NOT_FOUND, "unknown credential"),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn forward_request(State(state): State<Arc<Mutex<Stub>>>, request: Request) -> Response {
    let routes = state.lock().expect("stub lock").server_routes.clone();
    let (mut parts, body) = request.into_parts();
    parts.extensions.clear();
    match routes.oneshot(Request::from_parts(parts, body)).await {
        Ok(response) => response,
        Err(error) => match error {},
    }
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
        Ok(bytes) => {
            let bytes = if file
                .extension()
                .is_some_and(|extension| extension == "html")
            {
                let stub = state.lock().expect("stub lock");
                let authority = stub
                    .origin
                    .split_once("://")
                    .map_or(stub.origin.as_str(), |(_, authority)| authority);
                String::from_utf8_lossy(&bytes)
                    .replace("{{__TRUNK_ADDRESS__}}", authority)
                    .replace("{{__TRUNK_WS_BASE__}}", "/")
                    .into_bytes()
            } else {
                bytes
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type(&file))],
                bytes,
            )
                .into_response()
        }
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

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn fill_random(bytes: &mut [u8]) {
    getrandom::fill(bytes).expect("the stub runs on a host with OS randomness");
}
