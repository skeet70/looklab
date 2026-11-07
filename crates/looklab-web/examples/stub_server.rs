// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local development server: static site assets in front of the intake router.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path as AxumPath, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_server::{AppState, Config, PowConfig};
use looklab_upload::Limits;
use serde::Deserialize;
use tower::ServiceExt as _;
use webauthn_rs::prelude::Url;

const POW_MEMORY_KIB: u32 = 8;
const POW_ITERATIONS: u32 = 1;
const POW_DIFFICULTY: f64 = 0.0;

struct Stub {
    dist: PathBuf,
    origin: String,
    server: Arc<AppState>,
    server_routes: Router,
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
        cors_origins: Vec::new(),
        site: Default::default(),
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
    let stub = Arc::new(Stub {
        dist,
        origin,
        server,
        server_routes,
    });

    let app = Router::new()
        .route("/api/__stub/moderator/{credential}", post(grant_moderator))
        .route("/api/__stub/access/{credential}", post(grant_access))
        .route("/status/{token}", get(status))
        .route("/data/site-root.json", get(site_root))
        .route("/api/{*rest}", any(forward_request))
        .route("/log.json", get(forward_request))
        .route("/lists/grants.json", get(forward_request))
        .route("/lists/endorsements.json", get(forward_request))
        .route("/bounties.json", get(forward_request))
        .route("/manifests/{look}/{version}", get(forward_request))
        .fallback(serve)
        .layer(DefaultBodyLimit::max(65 * 1024 * 1024))
        .with_state(stub);

    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn status(
    State(stub): State<Arc<Stub>>,
    AxumPath(token): AxumPath<String>,
    request: Request,
) -> Response {
    if token.ends_with(".rss") || !accepts_html(request.headers()) {
        return forward(&stub, request).await;
    }
    serve_asset(&stub, request.uri()).await
}

async fn site_root(State(stub): State<Arc<Stub>>) -> Response {
    match std::fs::read(stub.dist.join("data/index.json")) {
        Ok(bytes) => Json(serde_json::json!({
            "root": looklab_publish::ArtifactHash::of(&bytes).as_str(),
        }))
        .into_response(),
        Err(_) => error_response(StatusCode::SERVICE_UNAVAILABLE, "site data is not published"),
    }
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.split(',').any(|item| item.trim() == "text/html"))
}

#[derive(Deserialize)]
#[serde(transparent)]
struct StubCredentialId(String);

async fn grant_moderator(
    State(stub): State<Arc<Stub>>,
    AxumPath(credential): AxumPath<StubCredentialId>,
) -> Response {
    set_access(&stub, credential, StubAccess::Moderator).await
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum StubAccess {
    Moderator,
    Admin,
    Seeder,
}

async fn grant_access(
    State(stub): State<Arc<Stub>>,
    AxumPath(credential): AxumPath<StubCredentialId>,
    Json(access): Json<StubAccess>,
) -> Response {
    set_access(&stub, credential, access).await
}

async fn set_access(stub: &Stub, credential: StubCredentialId, access: StubAccess) -> Response {
    let Ok(raw_id) = URL_SAFE_NO_PAD.decode(&credential.0) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid credential id");
    };
    let query = match access {
        StubAccess::Moderator => {
            "UPDATE credentials SET roles = '[\"moderator\"]' WHERE credential_id = ?"
        }
        StubAccess::Admin => "UPDATE credentials SET roles = '[\"admin\"]' WHERE credential_id = ?",
        StubAccess::Seeder => "UPDATE credentials SET tier = 'seeder' WHERE credential_id = ?",
    };
    match sqlx::query(query)
        .bind(raw_id)
        .execute(&stub.server.db)
        .await
    {
        Ok(result) if result.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => error_response(StatusCode::NOT_FOUND, "unknown credential"),
        Err(error) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    }
}

async fn forward_request(State(stub): State<Arc<Stub>>, request: Request) -> Response {
    forward(&stub, request).await
}

async fn forward(stub: &Stub, request: Request) -> Response {
    let (mut parts, body) = request.into_parts();
    parts.extensions.clear();
    match stub
        .server_routes
        .clone()
        .oneshot(Request::from_parts(parts, body))
        .await
    {
        Ok(response) => response,
        Err(error) => match error {},
    }
}

async fn serve(State(stub): State<Arc<Stub>>, uri: Uri) -> Response {
    serve_asset(&stub, &uri).await
}

async fn serve_asset(stub: &Stub, uri: &Uri) -> Response {
    let requested = uri.path().trim_start_matches('/');
    if requested.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let candidate = if requested.is_empty() {
        stub.dist.join("index.html")
    } else {
        stub.dist.join(requested)
    };
    let file = if candidate.is_file() {
        candidate
    } else {
        stub.dist.join("index.html")
    };
    match std::fs::read(&file) {
        Ok(bytes) => {
            let bytes = if file
                .extension()
                .is_some_and(|extension| extension == "html")
            {
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
    getrandom::fill(&mut bytes).expect("the stub runs on a host with OS randomness");
    bytes
}
