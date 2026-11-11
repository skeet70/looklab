// SPDX-License-Identifier: AGPL-3.0-or-later

use std::str::FromStr;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use looklab_schema::{
    CapabilityKey, ManualConfirmationSubmission, ManualSubmissionReceipt, ManualUrl,
    ModeratorEvidence, ModeratorEvidenceKind, ModeratorFirmwareScope, ModeratorRecord, Sha256Hex,
};
use looklab_upload::{
    AttestationHash, Challenge, ManualConfirmationRequest, ManualModeration,
    ManualWithdrawalRequest, SendManifest, canonical_send_manifest_bytes, send_manifest_hash,
};
use serde::Deserialize;

use crate::api::{auth, upload};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::RawCredentialId;
use crate::manual_confirm;
use crate::rate_limit::{Action, RateKey};
use crate::state::AppState;
use crate::store::{self, NewSend};
use crate::verify::{RequestDomain, SignedRequestPayload, request_digest, verify_assertion};

const MAX_MODERATOR_DESCRIPTION: usize = 2000;

fn signed_payload(request: &ManualConfirmationRequest) -> SignedRequestPayload {
    SignedRequestPayload {
        nonce: request.nonce.clone(),
        credential_id: request.credential_id.clone(),
        authenticator_data: request.authenticator_data.clone(),
        client_data_json: request.client_data_json.clone(),
        signature: request.signature.clone(),
    }
}

fn camera_id_from_path(make: &str, model: &str) -> ApiResult<looklab_schema::BodyId> {
    looklab_schema::BodyId::try_from(format!("{make}/{model}"))
        .map_err(|_| ApiError::new(ErrorCode::NotFound))
}

pub async fn page(
    State(state): State<Arc<AppState>>,
    Path((make, model)): Path<(String, String)>,
) -> ApiResult<Response> {
    if !state.rate_limiter.check(&RateKey::PublicRead) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let camera_id = camera_id_from_path(&make, &model)?;
    let Some(camera) = state.manuals.camera(&camera_id) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let summary = manual_confirm::summary(&state.db, camera)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let page = looklab_publish::ManualPage::from_camera(
        camera,
        state.manuals.catalog().catalog_version,
        state.manuals.digest(),
        looklab_publish::PreparationPage::default(),
        summary,
    );
    Ok(Json(page).into_response())
}

pub async fn summary(
    State(state): State<Arc<AppState>>,
    Path((make, model)): Path<(String, String)>,
) -> ApiResult<Response> {
    if !state.rate_limiter.check(&RateKey::PublicRead) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let camera_id = camera_id_from_path(&make, &model)?;
    let Some(camera) = state.manuals.camera(&camera_id) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let summary = manual_confirm::summary(&state.db, camera)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(Json(summary).into_response())
}

pub async fn confirm(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let request: ManualConfirmationRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if request.canonical_bytes().as_slice() != body.as_ref() {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    request
        .confirmation
        .validate_structure()
        .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    request
        .confirmation
        .validate(state.manuals.catalog())
        .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    let Some(camera) = state.manuals.camera(&request.confirmation.camera) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let (assertion, nonce) = signed_payload(&request).decode()?;
    let assertion_hash = upload::assertion_digest(&assertion);

    let manifest_bytes = URL_SAFE_NO_PAD
        .decode(&request.send_manifest)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if manifest_bytes.len() > state.config.limits.max_manifest_bytes {
        return Err(ApiError::new(ErrorCode::SendManifestInvalid));
    }
    let (send_manifest, rest) = postcard::take_from_bytes::<SendManifest>(&manifest_bytes)
        .map_err(|_| ApiError::new(ErrorCode::SendManifestInvalid))?;
    if !rest.is_empty() || canonical_send_manifest_bytes(&send_manifest) != manifest_bytes {
        return Err(ApiError::new(ErrorCode::SendManifestInvalid));
    }
    send_manifest
        .validate(
            state.config.limits.max_batches,
            state.config.limits.max_attestations,
        )
        .map_err(|_| ApiError::new(ErrorCode::SendManifestInvalid))?;
    let manifest_hash = send_manifest_hash(&send_manifest);

    let digest = request.confirmation.digest();
    let attestation = AttestationHash::from_bytes(*digest.as_bytes());
    if !send_manifest.contains_attestation(&attestation) {
        return Err(ApiError::new(ErrorCode::SendManifestInvalid));
    }

    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    if !state.rate_limiter.check(&RateKey::Assertion {
        action: Action::Confirmation,
        id: RawCredentialId::new(assertion.credential_id.clone()),
    }) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let _guard = state.lock_assertion().await;
    let credential = store::find_credential(&state.db, &assertion.credential_id)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;

    match store::find_send(&state.db, &nonce)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        Some(send) => {
            upload::check_send_binding(
                &send,
                manifest_hash.as_bytes(),
                &assertion.credential_id,
                &assertion_hash,
            )?;
            upload::check_send_open(&send)?;
        }
        None => {
            let verified = verify_assertion(
                &credential.passkey,
                &assertion,
                Challenge::for_send(&nonce, &manifest_hash),
                &state.config.origin,
                &state.config.rp_id,
            )
            .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
            match store::update_credential_passkey(
                &state.db,
                &credential.id,
                &verified,
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
            let batch_hashes = send_manifest
                .batches
                .iter()
                .map(|hash| *hash.as_bytes())
                .collect::<Vec<_>>();
            let attestation_hashes = send_manifest
                .attestations
                .iter()
                .map(|hash| *hash.as_bytes())
                .collect::<Vec<_>>();
            let new_send = NewSend {
                manifest_hash: manifest_hash.as_bytes(),
                nonce: &nonce,
                credential_id: &assertion.credential_id,
                assertion_hash: &assertion_hash,
                batch_hashes: &batch_hashes,
                attestation_hashes: &attestation_hashes,
            };
            match store::establish_send(&state.db, &new_send)
                .await
                .map_err(|_| ApiError::new(ErrorCode::Internal))?
            {
                store::EstablishSend::Established => {}
                store::EstablishSend::Existing(send) => {
                    upload::check_send_binding(
                        &send,
                        manifest_hash.as_bytes(),
                        &assertion.credential_id,
                        &assertion_hash,
                    )?;
                    upload::check_send_open(&send)?;
                }
                store::EstablishSend::Unknown => {
                    return Err(ApiError::new(ErrorCode::NonceUnknown));
                }
                store::EstablishSend::Expired => {
                    return Err(ApiError::new(ErrorCode::NonceExpired));
                }
                store::EstablishSend::Replayed => {
                    return Err(ApiError::new(ErrorCode::NonceReplayed));
                }
            }
        }
    }
    drop(_guard);

    match store::find_send_attestation(&state.db, &nonce, digest.as_bytes())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        None => return Err(ApiError::new(ErrorCode::SendManifestInvalid)),
        Some(Some(_)) => return Err(ApiError::new(ErrorCode::Conflict)),
        Some(None) => {}
    }

    match manual_confirm::record(
        &state.db,
        &credential.id,
        camera,
        &request.confirmation,
        digest,
        &assertion_hash,
        &nonce,
    )
    .await
    {
        Ok(_) => {}
        Err(manual_confirm::RecordError::AttestationNotPending)
        | Err(manual_confirm::RecordError::Blocked) => {
            return Err(ApiError::new(ErrorCode::Conflict));
        }
        Err(manual_confirm::RecordError::Database(_)) => {
            return Err(ApiError::new(ErrorCode::Internal));
        }
    }
    let summary = manual_confirm::summary(&state.db, camera)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let receipt = ManualSubmissionReceipt {
        camera: camera.id.clone(),
        owner_linkage: manual_confirm::owner_linkage(&state.db, &camera.id, &credential.id)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?,
        recorded_checks: u32::try_from(request.confirmation.observations.len()).unwrap_or(u32::MAX),
    };
    Ok(Json(ManualConfirmationSubmission { summary, receipt }).into_response())
}

fn withdrawal_signed(request: &ManualWithdrawalRequest) -> SignedRequestPayload {
    SignedRequestPayload {
        nonce: request.nonce.clone(),
        credential_id: request.credential_id.clone(),
        authenticator_data: request.authenticator_data.clone(),
        client_data_json: request.client_data_json.clone(),
        signature: request.signature.clone(),
    }
}

pub async fn withdraw_confirmation(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Response> {
    let request: ManualWithdrawalRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if request.canonical_bytes().as_slice() != body.as_ref() {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    let Some(camera) = state.manuals.camera(&request.camera) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let (assertion, nonce) = withdrawal_signed(&request).decode()?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    crate::api::consume_nonce(&state, &nonce).await?;
    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let guard = state.lock_assertion().await;
    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let digest = request_digest(
        RequestDomain::WithdrawManualConfirmation,
        request.camera.as_str().as_bytes(),
    );
    let passkey = verify_assertion(
        &credential.passkey,
        &assertion,
        Challenge::new(&nonce, &digest),
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
    drop(guard);
    manual_confirm::withdraw(&state.db, &credential.id, camera)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModerateRequest {
    pub nonce: String,
    pub credential_id: String,
    pub authenticator_data: String,
    pub client_data_json: String,
    pub signature: String,
    pub definition_digest: Sha256Hex,
    pub confirmed: bool,
    pub evidence: ModeratorEvidenceRequest,
}

impl ModerateRequest {
    fn signed(&self) -> SignedRequestPayload {
        SignedRequestPayload {
            nonce: self.nonce.clone(),
            credential_id: self.credential_id.clone(),
            authenticator_data: self.authenticator_data.clone(),
            client_data_json: self.client_data_json.clone(),
            signature: self.signature.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeratorEvidenceRequest {
    pub kind: ModeratorEvidenceKind,
    #[serde(default)]
    pub url: Option<String>,
    pub description: String,
    pub firmware_scope: ModeratorFirmwareScope,
}

pub async fn moderate(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((make, model, key)): Path<(String, String, String)>,
    payload: Result<Json<ModerateRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Response> {
    let Json(request) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let (assertion, nonce) = request.signed().decode()?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    crate::api::consume_nonce(&state, &nonce).await?;
    if !state.rate_limiter.check(&RateKey::GlobalSigned) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let session_credential = auth::moderator_session(&state, &headers).await?;
    if session_credential != credential.id {
        return Err(ApiError::new(ErrorCode::Forbidden));
    }
    auth::require_moderator(&state, &credential.id).await?;
    let camera_id = camera_id_from_path(&make, &model)?;
    let Some(camera) = state.manuals.camera(&camera_id) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let Ok(key) = CapabilityKey::from_str(&key) else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let Some(capability) = camera
        .capabilities
        .iter()
        .find(|capability| capability.key == key)
    else {
        return Err(ApiError::new(ErrorCode::NotFound));
    };
    let definition_digest = capability.definition_digest(camera);
    if request.definition_digest != definition_digest {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    let description = request.evidence.description.trim();
    if description.is_empty()
        || description.chars().count() > MAX_MODERATOR_DESCRIPTION
        || description.chars().any(char::is_control)
    {
        return Err(ApiError::new(ErrorCode::SchemaInvalid));
    }
    let url = request
        .evidence
        .url
        .as_deref()
        .map(ManualUrl::new)
        .transpose()
        .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;
    if request.evidence.kind == ModeratorEvidenceKind::Video && url.is_none() {
        return Err(ApiError::new(ErrorCode::SchemaInvalid));
    }
    let evidence = ModeratorEvidence {
        kind: request.evidence.kind,
        url,
        description: description.to_owned(),
        firmware_scope: request.evidence.firmware_scope,
    };
    let moderation = ManualModeration {
        camera: camera.id.clone(),
        key,
        definition_digest,
        confirmed: request.confirmed,
        evidence: evidence.clone(),
    };
    let digest = request_digest(
        RequestDomain::ModerateManualConfirmation,
        &moderation.canonical_bytes(),
    );
    let guard = state.lock_assertion().await;
    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let passkey = verify_assertion(
        &credential.passkey,
        &assertion,
        Challenge::new(&nonce, &digest),
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    if !state.rate_limiter.check(&RateKey::Assertion {
        action: Action::Confirmation,
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
    drop(guard);
    let record = ModeratorRecord {
        confirmed: request.confirmed,
        evidence,
    };
    manual_confirm::record_moderator(
        &state.db,
        camera,
        key,
        &definition_digest,
        &credential.id,
        &record,
    )
    .await
    .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let summary = manual_confirm::summary(&state.db, camera)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(Json(summary).into_response())
}
