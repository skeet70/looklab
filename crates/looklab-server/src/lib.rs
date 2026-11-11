// SPDX-License-Identifier: AGPL-3.0-or-later
//! Upload intake and moderation API.

pub mod admin;
mod api;
mod audit;
mod blob;
mod body_context;
pub mod body_schema;
pub mod client_errors;
mod config;
mod db;
mod error;
mod ids;
pub mod operator;
mod preparse;
mod proposals;
mod rate_limit;
mod serve;
mod state;
mod store;
mod trust;
mod verify;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, MatchedPath};
use axum::routing::{get, post};
use axum::{Router, middleware};
use tokio::net::TcpListener;
use tracing::Instrument as _;

pub use blob::{BlobError, BlobHash, BlobStore, FsBlobStore, write_guarded};
pub use config::{Config, CorsOrigin, PowConfig, SiteConfig};
pub use db::{DbError, open as open_database};
pub use ids::{
    ContributionId, CredentialId, EndorsementId, GrantId, JobId, LookVersionId,
    ModerationDecisionId, ProposalId, RootAdmissionId, StatusToken,
};
pub use proposals::expire_stale_lut_uploads;
pub use state::{AppState, ModSessions};
pub use store::active_snapshot_identity;
pub use store::request_publication;
pub use trust::{CredentialStanding, DefaultTrustPolicy, Tier, TrustPolicy};

/// Reads the live confirmation state used by publication.
///
/// # Errors
/// A stored confirmation row is unreadable.
pub async fn manual_summary(
    db: &sqlx::SqlitePool,
    camera: &looklab_schema::ManualCamera,
) -> Result<looklab_schema::ManualConfirmationSummary, sqlx::Error> {
    manual_confirm::summary(db, camera).await
}
pub use verify::{RequestDomain, request_digest};

mod manual_confirm;
mod manuals;
mod model_events;
pub mod model_store;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/nonce", post(api::passkey::nonce))
        .route(
            "/api/passkey/register/start",
            post(api::passkey::register_start),
        )
        .route(
            "/api/passkey/register/finish",
            post(api::passkey::register_finish),
        )
        .route("/api/upload", post(api::upload::upload))
        .route("/api/client-errors", post(api::client_errors::report))
        .route("/api/body-context", get(api::body_context::registered))
        .route("/api/manuals/{make}/{model}", get(api::manual::page))
        .route(
            "/api/manual-confirmations/{make}/{model}",
            get(api::manual::summary),
        )
        .route(
            "/api/manual-confirmations",
            post(api::manual::confirm).layer(DefaultBodyLimit::max(
                looklab_schema::MAX_CONFIRMATION_BYTES + 128 * 1024,
            )),
        )
        .route(
            "/api/manual-confirmations/withdraw",
            post(api::manual::withdraw_confirmation),
        )
        .route(
            "/api/manual-confirmations/{make}/{model}/{key}/moderate",
            post(api::manual::moderate),
        )
        .route(
            "/api/session",
            post(api::status::session).get(api::status::restore_session),
        )
        .route("/api/session/revoke", post(api::status::revoke_session))
        .route("/api/withdraw", post(api::withdraw::withdraw))
        .route("/api/admin/grant", post(api::admin::grant))
        .route("/api/admin/revoke-grant", post(api::admin::revoke_grant))
        .route("/api/seeder/endorse", post(api::admin::endorse))
        .route(
            "/api/seeder/revoke-endorsement",
            post(api::admin::revoke_endorsement),
        )
        .route("/api/seeder/admit-root", post(api::admin::admit_root))
        .route("/api/seeder/revoke-root", post(api::admin::revoke_root))
        .route("/api/mod/session", post(api::moderation::session))
        .route("/api/mod/queue", get(api::moderation::queue))
        .route("/api/mod/decision", post(api::moderation::decision))
        .route(
            "/api/mod/look/{action}",
            post(api::moderation::look_listing),
        )
        .route(
            "/api/mod/credential/block",
            post(api::moderation::block_intake),
        )
        .route("/api/proposals", post(api::proposals::submit))
        .route("/api/proposals/confirm", post(api::proposals::confirm))
        .route("/api/proposals/thread", post(api::proposals::thread))
        .route(
            "/api/proposals/message",
            post(api::proposals::proposer_message),
        )
        .route("/api/proposals/{id}", get(api::proposals::proposal))
        .route("/api/proposals/{id}/detail", get(api::proposals::detail))
        .route("/api/mod/proposal/{id}", get(api::proposals::mod_proposal))
        .route(
            "/api/mod/proposal-message",
            post(api::proposals::mod_message),
        )
        .route(
            "/api/fixtures/donate",
            post(api::proposals::donate_fixture).layer(DefaultBodyLimit::max(
                usize::try_from(looklab_schema::MAX_FIXTURE_BYTES).unwrap_or(usize::MAX)
                    + looklab_schema::FIXTURE_UPLOAD_HEADER_MAX
                    + 32,
            )),
        )
        .route(
            "/api/fixtures/manifest.toml",
            get(api::proposals::fixture_manifest),
        )
        .route(
            "/api/proposals/lut",
            post(api::proposals::submit_lut).layer(DefaultBodyLimit::max(
                usize::try_from(looklab_schema::MAX_LUT_BYTES).unwrap_or(usize::MAX)
                    + looklab_schema::FIXTURE_UPLOAD_HEADER_MAX
                    + 32,
            )),
        )
        .route("/api/recipes", get(api::bounties::recipes))
        .route("/api/cards/verification", get(api::cards::verifications))
        .route("/bounties.json", get(api::bounties::bounties))
        .route("/log.json", get(api::public::audit_log))
        .route("/lists/grants.json", get(api::public::grants))
        .route("/lists/endorsements.json", get(api::public::endorsements))
        .route("/manifests/{look}/{version}", get(api::public::manifest))
        .route("/status/{token}", get(api::status::status))
        .with_state(state)
        .layer(middleware::from_fn(request_span))
}

async fn request_span(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_else(|| "unmatched".to_owned());
    let span = tracing::info_span!("request", %method, %route);
    let response = next.run(request).instrument(span).await;
    tracing::info!(status = response.status().as_u16(), "request");
    response
}

pub async fn run(config_path: Option<PathBuf>) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .init();
    let config = Config::load(config_path.as_deref())?;
    let bind = config.bind;
    let state = Arc::new(AppState::new(config).await?);
    if let Err(error) = proposals::publish_pending(&state).await {
        tracing::warn!(%error, "a pending snapshot publication did not complete");
    }
    let app = serve::production_router(state.clone())?;
    let listener = TcpListener::bind(bind).await?;
    tracing::info!("listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    state.db.close().await;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "SIGINT handler failed");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "SIGTERM handler failed"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("server shutting down");
}
