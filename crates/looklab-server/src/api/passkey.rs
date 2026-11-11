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
use webauthn_rs::prelude::{
    COSEAlgorithm, CreationChallengeResponse, Passkey, RegisterPublicKeyCredential, Uuid,
};
use webauthn_rs_core::proto::{AttestationConveyancePreference, UserVerificationPolicy};

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{RegistrationHandle, random_bytes};
use crate::rate_limit::RateKey;
use crate::state::AppState;
use crate::store;
use crate::trust::NEW_CREDENTIAL_STANDING;

#[derive(Serialize)]
pub struct NonceResponse {
    nonce: String,
    expires_in_seconds: u64,
    rp_id: String,
    pow: crate::config::PowConfig,
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
            rp_id: state.config.rp_id.clone(),
            pow: {
                let params = state.trust.pow_params(NEW_CREDENTIAL_STANDING);
                crate::config::PowConfig {
                    memory_kib: params.memory_kib,
                    iterations: params.iterations,
                    difficulty_bits_per_image: params.difficulty_bits_per_image,
                }
            },
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

    let user_id = Uuid::new_v4();
    let builder = state
        .webauthn
        .new_challenge_register_builder(user_id.as_bytes(), "contributor", "Contributor")
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .attestation(AttestationConveyancePreference::None)
        .credential_algorithms(COSEAlgorithm::secure_algs())
        .require_resident_key(true)
        .user_verification_policy(UserVerificationPolicy::Required)
        .reject_synchronised_authenticators(false);
    let (options, registration) = state
        .webauthn
        .generate_challenge_register(builder)
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
        .register_credential(&payload.credential, &registration, None)
        .map(Passkey::from)
        .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    let raw_id = passkey.cred_id().as_slice();
    // Assertions carry a nonempty credential id of at most `MAX_CREDENTIAL_ID` bytes.
    if raw_id.is_empty() || raw_id.len() > looklab_upload::webauthn::MAX_CREDENTIAL_ID {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
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
