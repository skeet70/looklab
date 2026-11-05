// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::admin::{self, ModDecision, ModQueue};
use crate::api::auth;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::ProposalId;
use crate::rate_limit::Action;
use crate::state::{AppState, MOD_SESSION_TTL};
use crate::verify::{RequestDomain, SignedRequestPayload};

#[derive(Serialize)]
pub struct SessionResponse {
    session_token: String,
    expires_in_seconds: u64,
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SignedRequestPayload>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let actor = auth::authenticate(
        &state,
        &payload,
        RequestDomain::ModSession,
        &[],
        Action::ModSession,
    )
    .await?;
    auth::require_moderator(&state, &actor).await?;
    let token = state
        .mod_sessions
        .open(actor)
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(Json(SessionResponse {
        session_token: token.as_str().to_owned(),
        expires_in_seconds: MOD_SESSION_TTL.as_secs(),
    })
    .into_response())
}

pub async fn queue(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let actor = auth::moderator_session(&state, &headers).await?;
    let tier = auth::roles(&state, &actor).await?.tier;
    let queue: ModQueue = admin::mod_queue(&state.db, tier).await?;
    Ok(Json(queue).into_response())
}

#[derive(Deserialize)]
pub struct DecisionRequest {
    proposal_id: String,
    decision: ModDecision,
    reason: String,
}

pub async fn decision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Result<Json<DecisionRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let moderator = auth::moderator_session(&state, &headers).await?;
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let proposal = ProposalId::try_from(payload.proposal_id)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    crate::api::proposals::decide(
        &state,
        &moderator,
        &proposal,
        payload.decision,
        &payload.reason,
    )
    .await
}
