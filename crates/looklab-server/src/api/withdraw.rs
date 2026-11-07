// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use looklab_upload::Challenge;
use serde::{Deserialize, Serialize};

use crate::api::consume_nonce;
use crate::db::now_unix;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{ContributionId, RawCredentialId};
use crate::rate_limit::{Action, RateKey};
use crate::state::AppState;
use crate::store;
use crate::verify::{RequestDomain, SignedRequestPayload, request_digest, verify_assertion};

#[derive(Deserialize)]
pub struct WithdrawRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    contribution_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WithdrawalState {
    Withdrawn,
}

#[derive(Serialize)]
pub struct WithdrawResponse {
    status: WithdrawalState,
    delete_after: i64,
}

pub async fn withdraw(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<WithdrawRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let (assertion, nonce) = payload.signed.decode()?;
    let contribution_id = ContributionId::try_from(payload.contribution_id)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    consume_nonce(&state, &nonce).await?;
    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let digest = request_digest(RequestDomain::Withdraw, contribution_id.as_str().as_bytes());
    let challenge = Challenge::new(&nonce, &digest);
    let passkey = verify_assertion(
        &credential.passkey,
        &assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    if !state.rate_limiter.check(&RateKey::Assertion {
        action: Action::Withdraw,
        id: raw_id,
    }) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    match store::update_credential_passkey(
        &state.db,
        &credential.id,
        &passkey,
        &credential.revision,
    )
    .await
    {
        Ok(store::CredentialUpdate::Applied) => {}
        Ok(store::CredentialUpdate::Stale) => {
            return Err(ApiError::new(ErrorCode::SignatureInvalid));
        }
        Err(_) => return Err(ApiError::new(ErrorCode::Internal)),
    }

    let delete_after = now_unix() + i64::from(state.config.blob_retention_days) * 86_400;
    let delete_after = store::withdraw(&state.db, &contribution_id, &credential.id, delete_after)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::ContributionUnknown))?;

    Ok(Json(WithdrawResponse {
        status: WithdrawalState::Withdrawn,
        delete_after,
    })
    .into_response())
}
