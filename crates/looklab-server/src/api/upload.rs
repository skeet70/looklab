// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt as _;
use looklab_upload::{Challenge, Envelope, parse_envelope_strict, peek_nonce};
use serde::Serialize;
use tokio::task;

use crate::api::consume_nonce;
use crate::blob::{BlobHash, BlobStore};
use crate::body_schema::{
    SettingsProblem, SnapshotVersionError, validate_render_sims, validate_settings,
};
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{ContributionId, CredentialId, RawCredentialId, StatusToken};
use crate::preparse::parse_header;
use crate::rate_limit::{Action, RateKey};
use crate::state::AppState;
use crate::store::{self, ContributionKind, IntentKind, NewContribution, NewImage};
use crate::trust::NEW_CREDENTIAL_STANDING;
use crate::verify::verify_assertion;

const NONCE_END: usize = 40;

#[derive(Serialize)]
pub struct UploadResponse {
    status_token: Option<String>,
    contribution_id: ContributionId,
}

pub async fn upload(State(state): State<Arc<AppState>>, request: Request) -> ApiResult<Response> {
    let _slot = state
        .upload_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::new(ErrorCode::Busy))?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if content_type != Some("application/octet-stream") {
        return Err(ApiError::new(ErrorCode::UnsupportedMediaType));
    }
    declared_length(request.headers(), state.config.limits.max_body_bytes)?;

    let mut body = request.into_body();
    let max_bytes = state.config.limits.max_body_bytes;
    let read = async {
        let mut bytes = Vec::new();
        read_until(&mut body, &mut bytes, NONCE_END, max_bytes).await?;
        if bytes.len() < NONCE_END {
            return Err(ApiError::new(ErrorCode::MalformedEnvelope));
        }
        let nonce = peek_nonce(&bytes).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
        consume_nonce(&state, &nonce).await?;
        read_rest(&mut body, &mut bytes, max_bytes).await?;
        Ok::<Bytes, ApiError>(Bytes::from(bytes))
    };
    let body = tokio::time::timeout(state.config.body_timeout, read)
        .await
        .map_err(|_| ApiError::new(ErrorCode::RequestTimeout))??;

    let header = parse_header(&body, &state.config.limits)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if !state.rate_limiter.check(&RateKey::GlobalUpload) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let raw_id = RawCredentialId::new(header.assertion.credential_id.clone());
    let stored = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let pending = if stored.is_none() {
        state.pending_credential(raw_id.as_slice())
    } else {
        None
    };

    let (passkey, standing, existing_credential, display_handle) = match (stored, pending) {
        (Some(credential), _) => (
            credential.passkey,
            credential.standing,
            Some(credential.id),
            None,
        ),
        (None, Some((passkey, display_handle))) => {
            (passkey, NEW_CREDENTIAL_STANDING, None, display_handle)
        }
        (None, None) => return Err(ApiError::new(ErrorCode::CredentialUnknown)),
    };

    let challenge = Challenge::new(&header.nonce, &header.manifest_hash);
    let verified_passkey = verify_assertion(
        &passkey,
        &header.assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
    if !state.rate_limiter.check(&RateKey::Assertion {
        action: Action::Upload,
        id: raw_id.clone(),
    }) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let params = state.trust.pow_params(standing);
    let n_images = header.n_images();
    let pow_json = serde_json::to_string(header.pow.as_array())
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let pow = header.pow;
    let pow_challenge = challenge;
    task::spawn_blocking(move || looklab_upload::verify(&pow_challenge, n_images, &params, &pow))
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .map_err(|_| ApiError::new(ErrorCode::PowInvalid))?;

    let limits = state.config.limits;
    let store = state.blobs.clone();
    let body_for_parse = body.clone();
    let (mut envelope, counts) =
        task::spawn_blocking(move || -> Result<(Envelope, Vec<i64>), ApiError> {
            let envelope = parse_envelope_strict(&body_for_parse, &limits)
                .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
            let counts = store_blobs(&store, &envelope)?;
            Ok((envelope, counts))
        })
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))??;

    if envelope.nonce != header.nonce || envelope.challenge() != challenge {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }

    let records = state
        .bodies
        .at(envelope.manifest.community_snapshot_version)
        .map_err(|error| match error {
            SnapshotVersionError::Unknown { .. } => ApiError::new(ErrorCode::SnapshotUnknown),
            SnapshotVersionError::TooOld { .. } => ApiError::new(ErrorCode::SnapshotTooOld),
        })?;
    let body_record = records
        .get(envelope.manifest.body.as_str())
        .ok_or_else(|| ApiError::new(ErrorCode::BodyUnknown))?;
    validate_settings(
        body_record,
        envelope.manifest.firmware_version.as_str(),
        &envelope.manifest.sim_key,
        &envelope.manifest.settings,
        matches!(envelope.manifest.intent, looklab_upload::Intent::Card(_)),
    )
    .map_err(settings_error)?;
    validate_render_sims(
        body_record,
        envelope.manifest.firmware_version.as_str(),
        envelope.manifest.images.iter().map(|image| &image.meta),
    )
    .map_err(settings_error)?;

    let serial_hmac = match envelope.serial.take() {
        Some(serial) => {
            let hmac = serial.hmac(
                &state.secret.hmac_key(),
                &body_record.make,
                &body_record.model,
            );
            Some(*hmac.as_bytes())
        }
        None => None,
    };

    let Envelope {
        manifest,
        assertion,
        ..
    } = envelope;

    let setting_source = serde_json::to_string(
        &manifest
            .settings
            .entries()
            .iter()
            .map(|entry| (entry.key.clone(), entry.source))
            .collect::<Vec<_>>(),
    )
    .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let images = manifest
        .images
        .iter()
        .zip(counts)
        .map(|(image, n_samples)| -> ApiResult<NewImage> {
            Ok(NewImage {
                samples_hash: image.blob_sha256,
                n_samples,
                meta_json: serde_json::to_string(&image.meta)
                    .map_err(|_| ApiError::new(ErrorCode::Internal))?,
                rerender_group: image.rerender_group.map(|group| hex(group.as_bytes())),
                setting_source: setting_source.clone(),
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;

    let (intent, target_look, card) = match &manifest.intent {
        looklab_upload::Intent::Native => (IntentKind::Native, None, None),
        looklab_upload::Intent::Recipe { target_look } => (
            IntentKind::Recipe,
            Some(target_look.as_str().to_owned()),
            None,
        ),
        looklab_upload::Intent::Card(declaration) => {
            let card = serde_json::to_string(declaration)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?;
            (IntentKind::Card, None, Some(card))
        }
    };

    let first_contribution = existing_credential.is_none();
    let token = if first_contribution {
        Some(StatusToken::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?)
    } else {
        None
    };
    let contribution_id = ContributionId::new();
    let completion = complete(
        &state,
        existing_credential.clone(),
        raw_id.as_slice(),
        &verified_passkey,
        display_handle.as_deref(),
        NewContribution {
            id: contribution_id.clone(),
            credential_id: existing_credential
                .clone()
                .unwrap_or_else(CredentialId::new),
            manifest_hash: *header.manifest_hash.as_bytes(),
            signature: assertion.signature,
            pow: pow_json,
            kind: ContributionKind::from_upload(manifest.kind),
            serial_hmac,
            intent,
            target_look,
            card,
            body_id: manifest.body.as_str().to_owned(),
            firmware_version: manifest.firmware_version.as_str().to_owned(),
            sim_key: manifest.sim_key.as_str().to_owned(),
            settings_json: serde_json::to_string(&manifest.settings)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            images,
        },
        token.as_ref(),
    )
    .await;
    completion?;

    state.remove_pending_credential(raw_id.as_slice());
    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            status_token: token.map(|token| token.as_str().to_owned()),
            contribution_id,
        }),
    )
        .into_response())
}

#[allow(clippy::too_many_arguments)]
async fn complete(
    state: &AppState,
    existing_credential: Option<CredentialId>,
    raw_id: &[u8],
    passkey: &webauthn_rs::prelude::Passkey,
    display_handle: Option<&str>,
    mut contribution: NewContribution,
    token: Option<&StatusToken>,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    let credential_id = match existing_credential {
        Some(id) => {
            store::update_credential_passkey_tx(&mut tx, &id, passkey).await?;
            id
        }
        None => {
            let id = CredentialId::new();
            store::insert_credential(&mut tx, &id, raw_id, passkey, display_handle)
                .await
                .map_err(database_error)?;
            id
        }
    };
    contribution.credential_id = credential_id.clone();
    store::insert_contribution(&mut tx, &contribution).await?;
    if let Some(token) = token {
        store::insert_status_token(&mut tx, &credential_id, &token.hash()).await?;
    }
    store::enqueue_consistency_score(&mut tx, &contribution.id).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(())
}

fn store_blobs(store: &Arc<dyn BlobStore>, envelope: &Envelope) -> Result<Vec<i64>, ApiError> {
    let mut counts = Vec::with_capacity(envelope.blobs.len());
    for (blob, image) in envelope.blobs.iter().zip(envelope.manifest.images.iter()) {
        let hash = BlobHash::of(blob);
        if hash.as_bytes() != &image.blob_sha256 {
            return Err(ApiError::new(ErrorCode::MalformedEnvelope));
        }
        store
            .put(hash, blob)
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let n = match image.blob_kind {
            looklab_upload::BlobKind::Samples => looklab_samples::decode(blob)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?
                .n(),
            looklab_upload::BlobKind::PatchTable => looklab_samples::decode_patch_table(blob)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?
                .rows()
                .len(),
        };
        counts.push(i64::try_from(n).unwrap_or(i64::MAX));
    }
    Ok(counts)
}

fn settings_error(problem: SettingsProblem) -> ApiError {
    match problem {
        SettingsProblem::MonochromeSim(_) => ApiError::new(ErrorCode::MonochromeUnsupported),
        _ => ApiError::new(ErrorCode::SchemaInvalid),
    }
}

pub(super) fn database_error(error: sqlx::Error) -> ApiError {
    if matches!(&error, sqlx::Error::Database(detail) if detail.is_unique_violation()) {
        return ApiError::new(ErrorCode::CredentialConflict);
    }
    ApiError::new(ErrorCode::Internal)
}

fn declared_length(headers: &HeaderMap, max: usize) -> ApiResult<()> {
    let value = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::new(ErrorCode::LengthRequired))?;
    let length: usize = value
        .parse()
        .map_err(|_| ApiError::new(ErrorCode::LengthRequired))?;
    if length > max {
        return Err(ApiError::new(ErrorCode::PayloadTooLarge));
    }
    if length < NONCE_END {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }
    Ok(())
}

async fn read_until(
    body: &mut Body,
    out: &mut Vec<u8>,
    target: usize,
    max: usize,
) -> ApiResult<()> {
    while out.len() < target {
        let Some(frame) = body.frame().await else {
            return Ok(());
        };
        let frame = frame.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
        if let Ok(data) = frame.into_data() {
            append(out, data.as_ref(), max)?;
        }
    }
    Ok(())
}

async fn read_rest(body: &mut Body, out: &mut Vec<u8>, max: usize) -> ApiResult<()> {
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
        if let Ok(data) = frame.into_data() {
            append(out, data.as_ref(), max)?;
        }
    }
    Ok(())
}

fn append(out: &mut Vec<u8>, data: &[u8], max: usize) -> ApiResult<()> {
    if out.len().saturating_add(data.len()) > max {
        return Err(ApiError::new(ErrorCode::PayloadTooLarge));
    }
    out.extend_from_slice(data);
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
