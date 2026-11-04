// SPDX-License-Identifier: AGPL-3.0-or-later
//! Passkey-signed action authentication and staff-session authorization.

use axum::http::{HeaderMap, header};
use looklab_trust::Role;
use looklab_upload::Challenge;

use crate::api::consume_nonce;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{CredentialId, RawCredentialId, StatusToken};
use crate::rate_limit::{Action, RateKey};
use crate::state::AppState;
use crate::store;
use crate::trust::Tier;
use crate::verify::{RequestDomain, SignedRequestPayload, request_digest, verify_assertion};

#[derive(Debug, Clone)]
pub(crate) struct Roles {
    pub tier: Tier,
    pub roles: Vec<Role>,
}

impl Roles {
    fn is_admin(&self) -> bool {
        self.roles.contains(&Role::Admin)
    }

    fn is_moderator(&self) -> bool {
        self.roles.contains(&Role::Moderator)
    }

    fn is_seeder(&self) -> bool {
        self.tier == Tier::Seeder
    }
}

pub(crate) async fn authenticate(
    state: &AppState,
    signed: &SignedRequestPayload,
    domain: RequestDomain,
    payload: &[u8],
    action: Action,
) -> ApiResult<CredentialId> {
    let (assertion, nonce) = signed.decode()?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    consume_nonce(state, &nonce).await?;
    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let challenge = Challenge::new(&nonce, &request_digest(domain, payload));
    let passkey = verify_assertion(
        &credential.passkey,
        &assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    if !state
        .rate_limiter
        .check(&RateKey::Assertion { action, id: raw_id })
    {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    store::update_credential_passkey(&state.db, &credential.id, &passkey)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(credential.id)
}

pub(crate) async fn roles(state: &AppState, credential: &CredentialId) -> ApiResult<Roles> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT tier, roles FROM credentials WHERE id = ? AND status = 'active'")
            .bind(credential.as_str())
            .fetch_optional(&state.db)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let Some((tier, roles)) = row else {
        return Err(ApiError::new(ErrorCode::Forbidden));
    };
    Ok(Roles {
        tier: Tier::from_db(&tier).ok_or_else(|| ApiError::new(ErrorCode::Internal))?,
        roles: serde_json::from_str(&roles).map_err(|_| ApiError::new(ErrorCode::Internal))?,
    })
}

pub(crate) async fn require_admin(state: &AppState, credential: &CredentialId) -> ApiResult<()> {
    if !roles(state, credential).await?.is_admin() {
        return Err(ApiError::new(ErrorCode::Forbidden));
    }
    Ok(())
}

pub(crate) async fn require_seeder(state: &AppState, credential: &CredentialId) -> ApiResult<()> {
    if !roles(state, credential).await?.is_seeder() {
        return Err(ApiError::new(ErrorCode::Forbidden));
    }
    Ok(())
}

pub(crate) async fn require_seeder_or_admin(
    state: &AppState,
    credential: &CredentialId,
) -> ApiResult<()> {
    let roles = roles(state, credential).await?;
    if !roles.is_seeder() && !roles.is_admin() {
        return Err(ApiError::new(ErrorCode::Forbidden));
    }
    Ok(())
}

pub(crate) async fn require_moderator(
    state: &AppState,
    credential: &CredentialId,
) -> ApiResult<()> {
    let roles = roles(state, credential).await?;
    if !roles.is_moderator() && !roles.is_admin() {
        return Err(ApiError::new(ErrorCode::Forbidden));
    }
    Ok(())
}

pub(crate) fn bearer_token(headers: &HeaderMap) -> ApiResult<StatusToken> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    StatusToken::from_base64url(value).ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))
}

pub(crate) async fn moderator_session(
    state: &AppState,
    headers: &HeaderMap,
) -> ApiResult<CredentialId> {
    let token = bearer_token(headers)?;
    let credential = state
        .mod_sessions
        .authenticate(&token)
        .ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    require_moderator(state, &credential).await?;
    Ok(credential)
}
