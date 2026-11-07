// SPDX-License-Identifier: AGPL-3.0-or-later
//! Production HTTP composition: API, health, published data, SPA and CORS.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use looklab_publish::{ArtifactHash, SITE_FORMAT, SiteIndex};
use serde::Serialize;

use crate::config::{Config, CorsOrigin};
use crate::state::AppState;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("site distribution `{}` is not a directory", .0.display())]
    DistMissing(PathBuf),
    #[error("site distribution `{}` has no index.html", .0.display())]
    DistIndex(PathBuf),
    #[error("site data `{}` is not a directory", .0.display())]
    DataMissing(PathBuf),
    #[error("configured origin `{0}` is not an origin")]
    Origin(String),
    #[error("site data: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum SiteStatus {
    Ok,
    NotConfigured,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadinessStatus {
    Ok,
    NotReady,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum DatabaseStatus {
    Ok,
    Unavailable,
}

#[derive(Debug, Serialize)]
struct Health {
    status: ReadinessStatus,
}

#[derive(Debug, Serialize)]
struct Readiness {
    status: ReadinessStatus,
    database: DatabaseStatus,
    site: SiteStatus,
}

#[derive(Debug, Serialize)]
struct SiteRootReply {
    root: ArtifactHash,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

#[derive(Debug)]
struct Assets {
    app: Arc<AppState>,
    dist: Option<PathBuf>,
    data: Option<PathBuf>,
    origins: Vec<CorsOrigin>,
}

/// The API plus health checks, and the configured published data or SPA.
///
/// # Errors
/// Fails when a configured distribution lacks its directory or `index.html`.
pub fn production_router(state: Arc<AppState>) -> Result<Router, ServeError> {
    let assets = Arc::new(Assets::new(state.clone())?);
    let mut app = crate::router(state);
    app = app.merge(control_router(assets.clone()));
    if assets.data.is_some() {
        app = app.merge(data_router(assets.clone()));
    }
    if assets.dist.is_some() {
        app = app.merge(spa_router(assets.clone()));
    }
    Ok(app
        .layer(middleware::from_fn_with_state(
            assets.clone(),
            spa_navigation,
        ))
        .layer(middleware::from_fn_with_state(assets, cors)))
}

impl Assets {
    fn new(app: Arc<AppState>) -> Result<Self, ServeError> {
        let config: &Config = &app.config;
        let dist = match &config.site.dist_dir {
            Some(dir) => {
                let resolved = dir
                    .canonicalize()
                    .map_err(|_| ServeError::DistMissing(dir.clone()))?;
                if !resolved.is_dir() {
                    return Err(ServeError::DistMissing(dir.clone()));
                }
                if resolve_under(&resolved, Path::new("index.html")).is_none() {
                    return Err(ServeError::DistIndex(dir.clone()));
                }
                Some(dir.clone())
            }
            None => None,
        };
        let data = match &config.site.data_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir)?;
                let resolved = dir
                    .canonicalize()
                    .map_err(|_| ServeError::DataMissing(dir.clone()))?;
                if !resolved.is_dir() {
                    return Err(ServeError::DataMissing(dir.clone()));
                }
                Some(dir.clone())
            }
            None => None,
        };
        let mut origins: Vec<CorsOrigin> = Vec::with_capacity(config.cors_origins.len() + 1);
        origins.push(
            CorsOrigin::parse(&config.origin)
                .map_err(|_| ServeError::Origin(config.origin.clone()))?,
        );
        for origin in &config.cors_origins {
            if !origins.iter().any(|known| known == origin) {
                origins.push(origin.clone());
            }
        }
        Ok(Self {
            app: app.clone(),
            dist,
            data,
            origins,
        })
    }

    fn allows(&self, origin: &[u8]) -> bool {
        self.origins
            .iter()
            .any(|allowed| allowed.as_str().as_bytes() == origin)
    }

    fn index_dir(&self) -> Option<PathBuf> {
        match &self.data {
            Some(data) => Some(data.clone()),
            None => self
                .dist
                .as_ref()
                .map(|dist| dist.join("data"))
                .filter(|data| data.is_dir()),
        }
    }

    async fn site_status(&self) -> SiteStatus {
        let Some(dir) = self.index_dir() else {
            return SiteStatus::NotConfigured;
        };
        match read_index(&dir).await {
            Ok(_) => SiteStatus::Ok,
            Err(IndexError::Missing) => SiteStatus::Missing,
            Err(IndexError::Invalid) => SiteStatus::Invalid,
        }
    }

    async fn serve_dist(&self, requested: &str) -> Response {
        let Some(dist) = &self.dist else {
            return not_found();
        };
        if requested.is_empty() {
            return index_html(dist).await;
        }
        let Some(relative) = safe_relative(requested) else {
            return not_found();
        };
        let Some(file) = resolve_under(dist, &relative) else {
            return index_html(dist).await;
        };
        match read_file(file, false).await {
            Some(response) => response,
            None => index_html(dist).await,
        }
    }
}

fn control_router(assets: Arc<Assets>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/data/site-root.json", get(site_root))
        .with_state(assets)
}

fn data_router(assets: Arc<Assets>) -> Router {
    Router::new()
        .route("/data/{*path}", get(serve_data))
        .with_state(assets)
}

fn spa_router(assets: Arc<Assets>) -> Router {
    Router::new().fallback(serve_spa).with_state(assets)
}

async fn health() -> Json<Health> {
    Json(Health {
        status: ReadinessStatus::Ok,
    })
}

async fn ready(State(assets): State<Arc<Assets>>) -> Response {
    let database = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&assets.app.db)
        .await
        .is_ok();
    let site = assets.site_status().await;
    let healthy = database && !matches!(site, SiteStatus::Missing | SiteStatus::Invalid);
    let payload = Readiness {
        status: if healthy {
            ReadinessStatus::Ok
        } else {
            ReadinessStatus::NotReady
        },
        database: if database {
            DatabaseStatus::Ok
        } else {
            DatabaseStatus::Unavailable
        },
        site,
    };
    (
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(payload),
    )
        .into_response()
}

async fn site_root(State(assets): State<Arc<Assets>>) -> Response {
    let Some(dir) = assets.index_dir() else {
        return error(StatusCode::NOT_FOUND, "publication root is not served");
    };
    match read_index(&dir).await {
        Ok(root) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(SiteRootReply { root }),
        )
            .into_response(),
        Err(IndexError::Missing) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "site data is not published")
        }
        Err(IndexError::Invalid) => {
            error(StatusCode::SERVICE_UNAVAILABLE, "site index is invalid")
        }
    }
}

async fn serve_data(
    State(assets): State<Arc<Assets>>,
    AxumPath(path): AxumPath<String>,
) -> Response {
    let Some(data) = &assets.data else {
        return not_found();
    };
    let Some(relative) = safe_relative(path.trim_start_matches('/')) else {
        return not_found();
    };
    let immutable = is_version_artifact(&relative);
    let Some(file) = resolve_under(data, &relative) else {
        return not_found();
    };
    match read_file(file, immutable).await {
        Some(response) => response,
        None => not_found(),
    }
}

async fn serve_spa(
    State(assets): State<Arc<Assets>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if assets.dist.is_none() || uri.path().starts_with("/api/") {
        return not_found();
    }
    let requested = uri.path().trim_start_matches('/');
    if requested.is_empty() {
        return assets.serve_dist(requested).await;
    }
    let candidate = safe_relative(requested);
    if let Some(relative) = &candidate {
        let dist = assets.dist.as_ref().expect("checked above");
        if resolve_under(dist, relative).is_some() {
            return assets.serve_dist(requested).await;
        }
        if relative.extension().is_some() {
            return not_found();
        }
    }
    if accepts_html(&headers) || candidate.is_some() {
        return assets.serve_dist(requested).await;
    }
    not_found()
}

async fn spa_navigation(
    State(assets): State<Arc<Assets>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    if request.method() == Method::GET
        && path.starts_with("/status/")
        && !path.ends_with(".rss")
        && accepts_html(request.headers())
        && assets.dist.is_some()
    {
        return index_html(assets.dist.as_ref().expect("checked above")).await;
    }
    next.run(request).await
}

async fn cors(State(assets): State<Arc<Assets>>, request: Request, next: Next) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .map(HeaderValue::as_bytes);
    let allowed = origin
        .filter(|origin| assets.allows(origin))
        .map(|origin| origin.to_vec());
    if request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    {
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_cors(&mut response, allowed.as_deref());
        return response;
    }
    let mut response = next.run(request).await;
    apply_cors(&mut response, allowed.as_deref());
    response
}

fn apply_cors(response: &mut Response, origin: Option<&[u8]>) {
    let Some(origin) = origin else {
        return;
    };
    let Ok(origin) = HeaderValue::from_bytes(origin) else {
        return;
    };
    let headers = response.headers_mut();
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
}

async fn index_html(dist: &Path) -> Response {
    let Some(file) = resolve_under(dist, Path::new("index.html")) else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "site index is unavailable");
    };
    match tokio::task::spawn_blocking(move || std::fs::read(file)).await {
        Ok(Ok(bytes)) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        _ => error(StatusCode::SERVICE_UNAVAILABLE, "site index is unavailable"),
    }
}

async fn read_file(file: PathBuf, immutable: bool) -> Option<Response> {
    let content_type = content_type(&file);
    let cache = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(file))
        .await
        .ok()?
        .ok()?;
    Some(([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, cache)], bytes).into_response())
}

fn safe_relative(requested: &str) -> Option<PathBuf> {
    if requested.is_empty() || requested.contains('\\') || requested.contains('%') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in requested.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.starts_with('.') {
            return None;
        }
        out.push(part);
    }
    Some(out)
}

fn resolve_under(root: &Path, relative: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let file = root.join(relative).canonicalize().ok()?;
    file.starts_with(&root).then_some(file)
}

fn is_version_artifact(relative: &Path) -> bool {
    let parts: Vec<_> = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect();
    parts.len() >= 4 && parts[0] == "looks"
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("wasm") => "application/wasm",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        Some("toml" | "yaml" | "yml") => "text/plain; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| {
            accept
                .split(',')
                .any(|item| item.trim().split(';').next() == Some("text/html"))
        })
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(ErrorBody { error: message })).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexError {
    Missing,
    Invalid,
}

/// The digest clients pin: the SHA-256 of the exact `index.json` bytes served.
async fn read_index(data: &Path) -> Result<ArtifactHash, IndexError> {
    let file = data.join("index.json");
    let bytes = tokio::task::spawn_blocking(move || std::fs::read(file))
        .await
        .map_err(|_| IndexError::Invalid)?
        .map_err(|_| IndexError::Missing)?;
    let index: SiteIndex = serde_json::from_slice(&bytes).map_err(|_| IndexError::Invalid)?;
    if index.format != SITE_FORMAT {
        return Err(IndexError::Invalid);
    }
    Ok(ArtifactHash::of(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_and_dot_paths_are_rejected() {
        assert_eq!(safe_relative("../secret"), None);
        assert_eq!(safe_relative("looks/../../x"), None);
        assert_eq!(safe_relative("looks/.env"), None);
        assert_eq!(safe_relative("a//b"), None);
        assert_eq!(safe_relative("/etc/passwd"), None);
        assert_eq!(
            safe_relative("looks/portra/1.0.0/manifest.json"),
            Some(PathBuf::from("looks/portra/1.0.0/manifest.json"))
        );
    }

    #[test]
    fn only_version_paths_are_immutable() {
        assert!(is_version_artifact(Path::new(
            "looks/portra/1.0.0/look.looklab"
        )));
        assert!(!is_version_artifact(Path::new("looks/portra.json")));
        assert!(!is_version_artifact(Path::new("index.json")));
        assert!(!is_version_artifact(Path::new("bodies/fuji/x-t5.json")));
    }

    #[test]
    fn html_accept_matches_media_type() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml"),
        );
        assert!(accepts_html(&headers));
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        assert!(!accepts_html(&headers));
    }
}
