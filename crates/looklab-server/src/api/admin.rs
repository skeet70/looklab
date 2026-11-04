// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use looklab_trust::{Actor, Grant, Scope};
use serde::Deserialize;

use crate::admin::{self, RootAdmissionRecord, RootRevocation, trust_id};
use crate::api::auth;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{ContributionId, CredentialId, RootAdmissionId};
use crate::rate_limit::Action;
use crate::state::AppState;
use crate::verify::{RequestDomain, SignedRequestPayload};

pub(crate) fn bind(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(&(part.len() as u64).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn scope_bytes(scope: &Option<Scope>) -> Result<Vec<u8>, ApiError> {
    match scope {
        Some(scope) => {
            serde_json::to_vec(scope).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))
        }
        None => Ok(Vec::new()),
    }
}

fn parse_credential(value: &str) -> ApiResult<CredentialId> {
    CredentialId::try_from(value).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))
}

#[derive(Deserialize)]
pub struct GrantRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    subject: String,
    grant: Grant,
    scope: Option<Scope>,
    reason: String,
}

pub async fn grant(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<GrantRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let subject = parse_credential(&payload.subject)?;
    let scope_bytes = scope_bytes(&payload.scope)?;
    let digest = bind(&[
        subject.as_str().as_bytes(),
        admin::grant_str(payload.grant).as_bytes(),
        &scope_bytes,
        payload.reason.as_bytes(),
    ]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::Grant,
        &digest,
        Action::Admin,
    )
    .await?;
    auth::require_admin(&state, &actor).await?;
    let record = admin::grant_credential(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &subject,
        payload.grant,
        payload.scope,
        &payload.reason,
    )
    .await?;
    Ok(Json(record).into_response())
}

#[derive(Deserialize)]
pub struct RevokeGrantRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    subject: String,
    grant: Grant,
    reason: String,
}

pub async fn revoke_grant(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<RevokeGrantRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let subject = parse_credential(&payload.subject)?;
    let digest = bind(&[
        subject.as_str().as_bytes(),
        admin::grant_str(payload.grant).as_bytes(),
        payload.reason.as_bytes(),
    ]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::RevokeGrant,
        &digest,
        Action::Admin,
    )
    .await?;
    auth::require_admin(&state, &actor).await?;
    let record = admin::revoke_grant(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &subject,
        payload.grant,
        &payload.reason,
    )
    .await?;
    Ok(Json(record).into_response())
}

#[derive(Deserialize)]
pub struct EndorseRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    subject: String,
    scope: Scope,
    reason: String,
    #[serde(default)]
    evidence: Vec<String>,
}

pub async fn endorse(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<EndorseRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let subject = parse_credential(&payload.subject)?;
    let scope_bytes = serde_json::to_vec(&payload.scope)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let evidence_bytes = serde_json::to_vec(&payload.evidence)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let digest = bind(&[
        subject.as_str().as_bytes(),
        &scope_bytes,
        payload.reason.as_bytes(),
        &evidence_bytes,
    ]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::Endorse,
        &digest,
        Action::Endorse,
    )
    .await?;
    auth::require_seeder(&state, &actor).await?;
    let record = admin::endorse(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &subject,
        payload.scope,
        &payload.reason,
        payload.evidence,
    )
    .await?;
    Ok(Json(record).into_response())
}

#[derive(Deserialize)]
pub struct RevokeEndorsementRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    subject: String,
    reason: String,
}

pub async fn revoke_endorsement(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<RevokeEndorsementRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let subject = parse_credential(&payload.subject)?;
    let digest = bind(&[subject.as_str().as_bytes(), payload.reason.as_bytes()]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::RevokeEndorsement,
        &digest,
        Action::Endorse,
    )
    .await?;
    auth::require_seeder_or_admin(&state, &actor).await?;
    let revoked = admin::revoke_endorsement(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &subject,
        &payload.reason,
    )
    .await?;
    Ok(Json(revoked).into_response())
}

#[derive(Deserialize)]
pub struct AdmitRootRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    contribution_id: String,
    scope: Scope,
    reason: String,
}

pub async fn admit_root(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<AdmitRootRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let contribution = ContributionId::try_from(payload.contribution_id)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let scope_bytes = serde_json::to_vec(&payload.scope)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let digest = bind(&[
        contribution.as_str().as_bytes(),
        &scope_bytes,
        payload.reason.as_bytes(),
    ]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::AdmitRoot,
        &digest,
        Action::Root,
    )
    .await?;
    auth::require_seeder(&state, &actor).await?;
    let record: RootAdmissionRecord = admin::admit_root(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &contribution,
        payload.scope,
        &payload.reason,
    )
    .await?;
    Ok(Json(record).into_response())
}

#[derive(Deserialize)]
pub struct RevokeRootRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    root_admission_id: String,
    reason: String,
}

pub async fn revoke_root(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<RevokeRootRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let root = RootAdmissionId::try_from(payload.root_admission_id)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let digest = bind(&[root.as_str().as_bytes(), payload.reason.as_bytes()]);
    let actor = auth::authenticate(
        &state,
        &payload.signed,
        RequestDomain::RevokeRoot,
        &digest,
        Action::Root,
    )
    .await?;
    auth::require_seeder_or_admin(&state, &actor).await?;
    let revoked: RootRevocation = admin::revoke_root(
        &state.db,
        Actor::Credential(trust_id(&actor)?),
        &root,
        &payload.reason,
    )
    .await?;
    Ok(Json(revoked).into_response())
}
