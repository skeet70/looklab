// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use looklab_trust::Actor;
use serde::{Deserialize, Serialize};

use crate::admin::{self, ModDecision, ModQueue};
use crate::api::auth;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{CredentialId, ProposalId};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListingAction {
    Hide,
    Remove,
    Relist,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookAction {
    Listing(ListingAction),
    Revert,
}

impl std::str::FromStr for LookAction {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hide" => Ok(Self::Listing(ListingAction::Hide)),
            "remove" => Ok(Self::Listing(ListingAction::Remove)),
            "relist" => Ok(Self::Listing(ListingAction::Relist)),
            "revert" => Ok(Self::Revert),
            _ => Err(()),
        }
    }
}

#[derive(Deserialize)]
pub struct ListingRequest {
    look: String,
    reason: String,
}

#[derive(Deserialize)]
pub struct RevertRequest {
    look: String,
    target_version: u32,
    #[serde(default)]
    excluded_credentials: Vec<String>,
    reason: String,
}

pub async fn look_listing(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(action): Path<String>,
    payload: Result<Json<serde_json::Value>, JsonRejection>,
) -> ApiResult<Response> {
    let moderator = auth::moderator_session(&state, &headers).await?;
    let action: LookAction = action
        .parse()
        .map_err(|()| ApiError::new(ErrorCode::NotFound))?;
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let malformed = || ApiError::new(ErrorCode::MalformedEnvelope);
    match action {
        LookAction::Revert => {
            let request: RevertRequest =
                serde_json::from_value(payload).map_err(|_| malformed())?;
            let look = looklab_upload::LookSlug::try_from(request.look)
                .map_err(|_| malformed())?;
            let excluded = request
                .excluded_credentials
                .iter()
                .map(|id| CredentialId::try_from(id.as_str()).map_err(|_| malformed()))
                .collect::<Result<Vec<_>, _>>()?;
            let record = admin::revert(
                &state.db,
                &state.config.blob_dir,
                Actor::Credential(admin::trust_id(&moderator)?),
                &look,
                request.target_version,
                &excluded,
                &request.reason,
            )
            .await?;
            Ok(Json(record).into_response())
        }
        LookAction::Listing(listing) => {
            let request: ListingRequest =
                serde_json::from_value(payload).map_err(|_| malformed())?;
            let look = looklab_upload::LookSlug::try_from(request.look)
                .map_err(|_| malformed())?;
            let record = match listing {
                ListingAction::Hide => {
                    admin::hide_look(&state.db, &moderator, &look, &request.reason).await?
                }
                ListingAction::Remove => {
                    admin::remove_look(&state.db, &moderator, &look, &request.reason).await?
                }
                ListingAction::Relist => {
                    admin::relist_look(&state.db, &moderator, &look, &request.reason).await?
                }
            };
            Ok(Json(record).into_response())
        }
    }
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
