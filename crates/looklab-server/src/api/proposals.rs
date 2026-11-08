// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal submission, confirmations, discussion and the fixture export.

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::Path;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use looklab_schema::{
    FixtureDonation, FixtureUpload, LabeledTag, ProposalKind, Sha256Hex, split_fixture_upload,
};
use looklab_upload::{Challenge, POW_K, PowSolutions};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::auth;
use crate::blob::BlobHash;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{BlobAttemptId, ProposalId};
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
        &state,
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

#[derive(Deserialize)]
pub struct DonateHeader {
    file: String,
    sha256: Sha256Hex,
    size: u64,
    pow: Vec<u64>,
    #[serde(flatten)]
    signed: SignedRequestPayload,
}

#[derive(Serialize)]
pub struct DonateResponse {
    source: String,
}

pub async fn donate_fixture(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err(ApiError::new(ErrorCode::UnsupportedMediaType));
    }
    let (header_bytes, file_bytes) =
        split_fixture_upload(&body).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let header: DonateHeader = serde_json::from_slice(header_bytes)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let upload = FixtureUpload {
        file: header.file.clone(),
        sha256: header.sha256,
        size: header.size,
    };
    upload
        .validate()
        .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    if file_bytes.len() as u64 != upload.size {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    let signing_bytes =
        serde_json::to_vec(&upload).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let credential = auth::authenticate(
        &state,
        &header.signed,
        RequestDomain::FixtureUpload,
        &signing_bytes,
        Action::FixtureUpload,
    )
    .await?;
    let (_, nonce) = header.signed.decode()?;
    let challenge = Challenge::new(
        &nonce,
        &request_digest(RequestDomain::FixtureUpload, &signing_bytes),
    );
    verify_pow(&state, &credential, challenge, &header.pow).await?;

    let hash = BlobHash::of(file_bytes);
    if hash.as_bytes() != upload.sha256.as_bytes() {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    let offset = body.len() - file_bytes.len();
    let payload = body.slice(offset..);
    let attempt = BlobAttemptId::new();
    let hashes = [*hash.as_bytes()];
    crate::store::stage_blobs(
        &state.db,
        &attempt,
        &hashes,
        crate::db::now_unix() + crate::store::STAGED_TTL_SECONDS,
    )
    .await?;
    let published = async {
        let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
        if !crate::store::reservation_live(&mut tx, &attempt, 1).await? {
            return Err(ApiError::new(ErrorCode::Busy));
        }
        let store = state.blobs.clone();
        let payload = payload.clone();
        let (mut tx, ()) = crate::blob::write_guarded(tx, move || ((), store.put(hash, &payload)))
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        sqlx::query(
            "INSERT INTO fixture_uploads (sha256, credential_id, file, size, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (sha256, credential_id)
             DO UPDATE SET file = excluded.file, size = excluded.size",
        )
        .bind(hash.as_bytes().as_slice())
        .bind(credential.as_str())
        .bind(&upload.file)
        .bind(i64::try_from(upload.size).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?)
        .bind(crate::db::now_unix())
        .execute(&mut *tx)
        .await?;
        crate::store::clear_staged(&mut tx, &attempt).await?;
        tx.commit().await?;
        Ok::<(), ApiError>(())
    }
    .await;
    if let Err(error) = published {
        let _ = crate::store::abandon_staged(&state.db, &attempt).await;
        return Err(error);
    }
    Ok((
        StatusCode::CREATED,
        Json(DonateResponse {
            source: looklab_schema::fixture_source(upload.sha256),
        }),
    )
        .into_response())
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
