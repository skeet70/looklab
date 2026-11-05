// SPDX-License-Identifier: AGPL-3.0-or-later
//! Upload intake and moderation API.

pub mod admin;
mod api;
mod audit;
mod blob;
pub mod body_schema;
mod config;
mod db;
mod error;
mod ids;
mod preparse;
mod proposals;
mod rate_limit;
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

pub use config::{Config, PowConfig};
pub use db::{DbError, open as open_database};
pub use ids::{
    ContributionId, CredentialId, EndorsementId, GrantId, LookVersionId, ModerationDecisionId,
    ProposalId, RootAdmissionId, StatusToken,
};
pub use state::{AppState, ModSessions};
pub use trust::{CredentialStanding, DefaultTrustPolicy, Tier, TrustPolicy};
pub use verify::{RequestDomain, request_digest};

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
        .route("/api/proposals", post(api::proposals::submit))
        .route("/api/proposals/confirm", post(api::proposals::confirm))
        .route("/api/proposals/thread", post(api::proposals::thread))
        .route(
            "/api/proposals/message",
            post(api::proposals::proposer_message),
        )
        .route("/api/proposals/{id}", get(api::proposals::proposal))
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
    let app = router(state);
    let listener = TcpListener::bind(bind).await?;
    tracing::info!("listening");
    axum::serve(listener, app).await?;
    Ok(())
}
