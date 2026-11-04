// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal submission, confirmations, discussion and the fixture export.

use std::sync::Arc;

use axum::Json;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use looklab_schema::{FixtureDonation, LabeledTag, ProposalKind};
use looklab_upload::{Challenge, POW_K, PowSolutions};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::auth;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::ProposalId;
use crate::proposals::{
    MessageAuthor, ModProposal, ProposalError, ProposalMessage, SnapshotRecord, Submission,
};
use crate::rate_limit::Action;
use crate::state::AppState;
use crate::trust::CredentialStanding;
use crate::verify::{RequestDomain, SignedRequestPayload, request_digest};

impl From<ProposalError> for ApiError {
    fn from(error: ProposalError) -> Self {
        let code = match error {
            ProposalError::Database(_) | ProposalError::Corrupt => ErrorCode::Internal,
            ProposalError::Admin(admin) => return admin.into(),
            ProposalError::InvalidPayload(_) | ProposalError::InvalidFixture(_) => {
                ErrorCode::SchemaInvalid
            }
            ProposalError::UnknownProposal => ErrorCode::NotFound,
            ProposalError::Closed => ErrorCode::Conflict,
            ProposalError::RoundTripTimeout => ErrorCode::RequestTimeout,
            ProposalError::RoundTrip(_) | ProposalError::Snapshot(_) => ErrorCode::SchemaInvalid,
        };
        Self::new(code)
    }
}

#[derive(Serialize)]
struct SubmitSigningBody<'a> {
    kind: ProposalKind,
    payload: &'a str,
    fixtures: &'a [FixtureDonation],
}

/// Signs the canonical JSON of `{kind, payload, fixtures}` under `proposal`.
#[derive(Deserialize)]
pub struct SubmitRequest {
    kind: String,
    payload: String,
    #[serde(default)]
    fixtures: Vec<FixtureDonation>,
    pow: Vec<u64>,
    #[serde(flatten)]
    signed: SignedRequestPayload,
}

pub async fn submit(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SubmitRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let kind = payload
        .kind
        .parse::<ProposalKind>()
        .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    let bytes = signing_bytes(&SubmitSigningBody {
        kind,
        payload: &payload.payload,
        fixtures: &payload.fixtures,
    })?;
    let credential = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::Proposal,
        &bytes,
        Action::Proposal,
    )
    .await?;
    let (_, nonce) = payload.signed.decode()?;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::Proposal, &bytes));
    verify_pow(&state, &credential, challenge, &payload.pow).await?;
    let submitted = crate::proposals::submit(
        &state.db,
        &credential,
        Submission {
            kind,
            payload: payload.payload,
            fixtures: payload.fixtures,
        },
    )
    .await?;
    Ok((axum::http::StatusCode::CREATED, Json(submitted)).into_response())
}

#[derive(Serialize)]
struct ConfirmationSigningBody<'a> {
    proposal_id: &'a str,
    tags: &'a [LabeledTag],
}

#[derive(Deserialize)]
pub struct ConfirmRequest {
    proposal_id: String,
    tags: Vec<LabeledTag>,
    #[serde(flatten)]
    signed: SignedRequestPayload,
}

pub async fn confirm(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<ConfirmRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let proposal = ProposalId::try_from(payload.proposal_id.clone())
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let bytes = signing_bytes(&ConfirmationSigningBody {
        proposal_id: &payload.proposal_id,
        tags: &payload.tags,
    })?;
    let credential = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::ProposalConfirmation,
        &bytes,
        Action::Confirmation,
    )
    .await?;
    let view = crate::proposals::confirm(&state.db, &credential, &proposal, payload.tags).await?;
    Ok(Json(view).into_response())
}

#[derive(Serialize)]
struct ThreadSigningBody<'a> {
    proposal_id: &'a str,
}

#[derive(Deserialize)]
pub struct ThreadRequest {
    proposal_id: String,
    #[serde(flatten)]
    signed: SignedRequestPayload,
}

#[derive(Serialize)]
pub struct ThreadResponse {
    messages: Vec<ProposalMessage>,
}

pub async fn thread(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<ThreadRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let proposal = ProposalId::try_from(payload.proposal_id.clone())
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let bytes = signing_bytes(&ThreadSigningBody {
        proposal_id: &payload.proposal_id,
    })?;
    let credential = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::ProposalThread,
        &bytes,
        Action::ProposalThread,
    )
    .await?;
    let messages = crate::proposals::proposer_thread(&state.db, &credential, &proposal).await?;
    Ok(Json(ThreadResponse { messages }).into_response())
}

#[derive(Deserialize)]
pub struct MessageRequest {
    proposal_id: String,
    body: String,
}

pub async fn proposal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let proposal = ProposalId::try_from(id).map_err(|_| ApiError::new(ErrorCode::NotFound))?;
    let view = crate::proposals::view(&state.db, &proposal).await?;
    Ok(Json(view).into_response())
}

pub async fn mod_proposal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth::moderator_session(&state, &headers).await?;
    let proposal = ProposalId::try_from(id).map_err(|_| ApiError::new(ErrorCode::NotFound))?;
    let record: ModProposal = crate::proposals::moderator_view(&state.db, &proposal).await?;
    Ok(Json(record).into_response())
}

pub async fn mod_message(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Result<Json<MessageRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let moderator = auth::moderator_session(&state, &headers).await?;
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let proposal = ProposalId::try_from(payload.proposal_id)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    crate::proposals::post_message(
        &state.db,
        &moderator,
        &proposal,
        MessageAuthor::Moderator,
        &payload.body,
    )
    .await?;
    Ok(Json(Value::Null).into_response())
}

#[derive(Serialize)]
struct ProposerMessageSigningBody<'a> {
    proposal_id: &'a str,
    body: &'a str,
}

#[derive(Deserialize)]
pub struct ProposerMessageRequest {
    proposal_id: String,
    body: String,
    #[serde(flatten)]
    signed: SignedRequestPayload,
}

pub async fn proposer_message(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<ProposerMessageRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let proposal = ProposalId::try_from(payload.proposal_id.clone())
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let bytes = signing_bytes(&ProposerMessageSigningBody {
        proposal_id: &payload.proposal_id,
        body: &payload.body,
    })?;
    let credential = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::ProposalThread,
        &bytes,
        Action::ProposalThread,
    )
    .await?;
    crate::proposals::post_message(
        &state.db,
        &credential,
        &proposal,
        MessageAuthor::Proposer,
        &payload.body,
    )
    .await?;
    Ok(Json(Value::Null).into_response())
}

pub async fn fixture_manifest(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let toml = crate::proposals::fixture_manifest(&state.db).await?;
    Ok((
        [(header::CONTENT_TYPE, "application/toml; charset=utf-8")],
        toml,
    )
        .into_response())
}

fn signing_bytes<T: Serialize>(body: &T) -> ApiResult<Vec<u8>> {
    serde_json::to_vec(body).map_err(|_| ApiError::new(ErrorCode::Internal))
}

async fn verify_pow(
    state: &AppState,
    credential: &crate::ids::CredentialId,
    challenge: Challenge,
    pow: &[u64],
) -> ApiResult<()> {
    let solutions: [u64; POW_K] = pow
        .try_into()
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let row: Option<(String, f64)> = sqlx::query_as(
        "SELECT tier, reputation FROM credentials WHERE id = ? AND status = 'active'",
    )
    .bind(credential.as_str())
    .fetch_optional(&state.db)
    .await
    .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let Some((tier, reputation)) = row else {
        return Err(ApiError::new(ErrorCode::CredentialUnknown));
    };
    let standing = CredentialStanding {
        tier: crate::trust::Tier::from_db(&tier)
            .ok_or_else(|| ApiError::new(ErrorCode::Internal))?,
        reputation,
    };
    let params = state.trust.pow_params(standing);
    let solutions = PowSolutions::new(solutions);
    tokio::task::spawn_blocking(move || looklab_upload::verify(&challenge, 1, &params, &solutions))
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .map_err(|_| ApiError::new(ErrorCode::PowInvalid))
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Recorded,
}

#[derive(Serialize)]
pub struct DecisionResponse {
    state: DecisionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot: Option<SnapshotRecord>,
}

pub async fn decide(
    state: &AppState,
    moderator: &crate::ids::CredentialId,
    proposal: &ProposalId,
    decision: crate::admin::ModDecision,
    reason: &str,
) -> ApiResult<Response> {
    let snapshot =
        crate::proposals::record_decision(state, moderator, proposal, decision, reason).await?;
    Ok(Json(DecisionResponse {
        state: DecisionState::Recorded,
        snapshot,
    })
    .into_response())
}
