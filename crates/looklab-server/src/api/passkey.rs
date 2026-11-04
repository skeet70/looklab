// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_upload::Nonce;
use serde::{Deserialize, Serialize};
use webauthn_rs::prelude::{CreationChallengeResponse, RegisterPublicKeyCredential, Uuid};

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{RegistrationHandle, random_bytes};
use crate::rate_limit::RateKey;
use crate::state::AppState;
use crate::store;

#[derive(Serialize)]
pub struct NonceResponse {
    nonce: String,
    expires_in_seconds: u64,
}

pub async fn nonce(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    if !state.rate_limiter.check(&RateKey::Nonce) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let nonce =
        Nonce::from_bytes(random_bytes::<32>().map_err(|_| ApiError::new(ErrorCode::Internal))?);
    let ttl = state.config.nonce_ttl.as_secs();
    store::issue_nonce(&state.db, &nonce, i64::try_from(ttl).unwrap_or(i64::MAX)).await?;
    Ok((
        StatusCode::CREATED,
        Json(NonceResponse {
            nonce: URL_SAFE_NO_PAD.encode(nonce.as_bytes()),
            expires_in_seconds: ttl,
        }),
    )
        .into_response())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterStartRequest {
    display_handle: Option<String>,
}

#[derive(Serialize)]
pub struct RegisterStartResponse {
    handle: String,
    options: CreationChallengeResponse,
}

pub async fn register_start(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<RegisterStartRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let display_handle = match payload.display_handle {
        Some(handle) if valid_display_handle(&handle) => Some(handle),
        Some(_) => return Err(ApiError::new(ErrorCode::MalformedEnvelope)),
        None => None,
    };
    if !state.rate_limiter.check(&RateKey::Registration) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let (options, registration) = state
        .webauthn
        .start_passkey_registration(Uuid::new_v4(), "contributor", "Contributor", None)
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let handle = RegistrationHandle::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?;
    if !state.insert_challenge(handle.clone(), registration, display_handle) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    Ok((
        StatusCode::OK,
        Json(RegisterStartResponse {
            handle: handle.as_str().to_owned(),
            options,
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterFinishRequest {
    handle: String,
    credential: RegisterPublicKeyCredential,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationState {
    PendingUpload,
}

#[derive(Serialize)]
pub struct RegisterFinishResponse {
    state: RegistrationState,
}

pub async fn register_finish(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<RegisterFinishRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let handle = RegistrationHandle::try_from(payload.handle)
        .map_err(|_| ApiError::new(ErrorCode::RegistrationUnknown))?;
    let (registration, display_handle) = state
        .take_challenge(&handle)
        .ok_or_else(|| ApiError::new(ErrorCode::RegistrationUnknown))?;
    let passkey = state
        .webauthn
        .finish_passkey_registration(&payload.credential, &registration)
        .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    let raw_id = passkey.cred_id().as_slice();
    if store::credential_exists(&state.db, raw_id)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        return Err(ApiError::new(ErrorCode::CredentialConflict));
    }
    if !state.insert_credential(handle, passkey, display_handle) {
        return Err(ApiError::new(ErrorCode::CredentialConflict));
    }
    Ok((
        StatusCode::OK,
        Json(RegisterFinishResponse {
            state: RegistrationState::PendingUpload,
        }),
    )
        .into_response())
}

fn valid_display_handle(handle: &str) -> bool {
    !handle.is_empty() && handle.chars().count() <= 64 && !handle.chars().any(char::is_control)
}
