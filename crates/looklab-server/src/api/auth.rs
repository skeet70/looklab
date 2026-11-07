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
    authenticate_with_pow(state, signed, domain, payload, action, None).await
}

pub(crate) async fn authenticate_with_pow(
    state: &AppState,
    signed: &SignedRequestPayload,
    domain: RequestDomain,
    payload: &[u8],
    action: Action,
    pow: Option<looklab_upload::PowSolutions>,
) -> ApiResult<CredentialId> {
    let (assertion, nonce) = signed.decode()?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    consume_nonce(state, &nonce).await?;
    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let stored = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let (original, existing, revision) = match stored {
        Some(credential) => (
            credential.passkey,
            Some(credential.id),
            Some(credential.revision),
        ),
        None if domain == RequestDomain::Session => {
            if store::credential_exists(&state.db, raw_id.as_slice()).await? {
                return Err(ApiError::new(ErrorCode::CredentialUnknown));
            }
            let (passkey, _) = state
                .pending_credential(raw_id.as_slice())
                .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
            (passkey, None, None)
        }
        None => return Err(ApiError::new(ErrorCode::CredentialUnknown)),
    };
    let challenge = Challenge::new(&nonce, &request_digest(domain, payload));
    let verified = verify_assertion(
        &original,
        &assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    if !state.rate_limiter.check(&RateKey::Assertion {
        action,
        id: raw_id.clone(),
    }) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    if let Some(credential) = existing {
        let revision = revision.ok_or_else(|| ApiError::new(ErrorCode::Internal))?;
        match store::update_credential_passkey(&state.db, &credential, &verified, &revision).await {
            Ok(store::CredentialUpdate::Applied) => {}
            Ok(store::CredentialUpdate::Stale) => {
                return Err(ApiError::new(ErrorCode::SignatureInvalid));
            }
            Err(_) => return Err(ApiError::new(ErrorCode::Internal)),
        }
        Ok(credential)
    } else {
        let solutions = pow.ok_or_else(|| ApiError::new(ErrorCode::PowInvalid))?;
        let promotion = state
            .begin_promotion(&original, &verified)
            .map_err(super::upload::promotion_error)?;
        let params = state
            .trust
            .pow_params(crate::trust::NEW_CREDENTIAL_STANDING);
        let slot = state
            .upload_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::new(ErrorCode::Busy))?;
        tokio::task::spawn_blocking(move || {
            let _slot = slot;
            looklab_upload::verify(&challenge, 1, &params, &solutions)
        })
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .map_err(|_| ApiError::new(ErrorCode::PowInvalid))?;
        let credential = CredentialId::new();
        let mut tx = state.db.begin().await?;
        store::insert_credential(
            &mut tx,
            &credential,
            promotion.passkey().cred_id().as_slice(),
            promotion.passkey(),
            promotion.display_handle(),
        )
        .await
        .map_err(super::upload::database_error)?;
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = done_tx.send(promotion.commit(tx).await);
        });
        done_rx
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?
            .map_err(super::upload::database_error)?;
        Ok(credential)
    }
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
