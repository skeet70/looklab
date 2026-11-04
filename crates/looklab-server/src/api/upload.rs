// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use looklab_upload::{Challenge, Envelope, parse_envelope_strict};
use serde::Serialize;
use tokio::task;

use crate::blob::{BlobHash, BlobStore};
use crate::body_schema::validate_settings;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{ContributionId, CredentialId, RawCredentialId, StatusToken};
use crate::preparse::parse_header;
use crate::rate_limit::{Action, CREDENTIAL_UPLOAD_QUOTA, GLOBAL_UPLOAD_QUOTA, RateKey};
use crate::state::AppState;
use crate::store::{self, ConsumeNonce, ContributionKind, IntentKind, NewContribution, NewImage};
use crate::trust::CredentialStanding;
use crate::verify::verify_assertion;

#[derive(Serialize)]
pub struct UploadResponse {
    status_token: String,
    contribution_id: ContributionId,
}

pub async fn upload(State(state): State<Arc<AppState>>, request: Request) -> ApiResult<Response> {
    if !state
        .rate_limiter
        .check(&RateKey::GlobalUpload, GLOBAL_UPLOAD_QUOTA)
    {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if content_type != Some("application/octet-stream") {
        return Err(ApiError::new(ErrorCode::UnsupportedMediaType));
    }
    let body = axum::body::to_bytes(request.into_body(), state.config.limits.max_body_bytes)
        .await
        .map_err(|_| ApiError::new(ErrorCode::PayloadTooLarge))?;

    let header = parse_header(&body, &state.config.limits)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let raw_id = RawCredentialId::new(header.assertion.credential_id.clone());
    if !state.rate_limiter.check(
        &RateKey::Assertion {
            action: Action::Upload,
            id: raw_id.clone(),
        },
        CREDENTIAL_UPLOAD_QUOTA,
    ) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    consume_nonce(&state, &header.nonce).await?;

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
        (None, Some((passkey, display_handle))) => (
            passkey,
            CredentialStanding {
                tier: crate::trust::Tier::New,
                reputation: 0.5,
            },
            None,
            display_handle,
        ),
        (None, None) => return Err(ApiError::new(ErrorCode::CredentialUnknown)),
    };

    let challenge = Challenge::new(&header.nonce, &header.manifest_hash);
    verify_assertion(
        &passkey,
        &header.assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;

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

    let mut envelope = parse_envelope_strict(&body, &state.config.limits)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    if envelope.nonce != header.nonce || envelope.challenge() != challenge {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }

    let body_record = state
        .bodies
        .get(&envelope.manifest.body)
        .ok_or_else(|| ApiError::new(ErrorCode::BodyUnknown))?;
    validate_settings(
        body_record,
        envelope.manifest.firmware_version.as_str(),
        &envelope.manifest.sim_key,
        &envelope.manifest.settings,
    )
    .map_err(|_| ApiError::new(ErrorCode::SchemaInvalid))?;

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
        blobs,
        ..
    } = envelope;
    let counts = store_blobs(&state, &manifest, blobs).await?;

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
            })
        })
        .collect::<ApiResult<Vec<_>>>()?;

    let (intent, target_look) = match &manifest.intent {
        looklab_upload::Intent::Native => (IntentKind::Native, None),
        looklab_upload::Intent::Recipe { target_look } => {
            (IntentKind::Recipe, Some(target_look.as_str().to_owned()))
        }
    };

    let contribution_id = ContributionId::new();
    let token = StatusToken::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let mut tx = state.db.begin().await?;
    let credential_id = match existing_credential {
        Some(id) => id,
        None => {
            let id = CredentialId::new();
            store::insert_credential(
                &mut tx,
                &id,
                raw_id.as_slice(),
                &passkey,
                display_handle.as_deref(),
            )
            .await?;
            id
        }
    };
    store::insert_contribution(
        &mut tx,
        &NewContribution {
            id: contribution_id.clone(),
            credential_id: credential_id.clone(),
            manifest_hash: *header.manifest_hash.as_bytes(),
            signature: assertion.signature,
            pow: pow_json,
            kind: ContributionKind::from_upload(manifest.kind),
            serial_hmac,
            intent,
            target_look,
            body_id: manifest.body.as_str().to_owned(),
            firmware_version: manifest.firmware_version.as_str().to_owned(),
            sim_key: manifest.sim_key.as_str().to_owned(),
            settings_json: serde_json::to_string(&manifest.settings)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            images,
        },
    )
    .await?;
    store::revoke_tokens(&mut tx, &credential_id).await?;
    store::insert_status_token(&mut tx, &credential_id, &token.hash()).await?;
    store::enqueue_consistency_score(&mut tx, &contribution_id).await?;
    tx.commit().await?;

    state.remove_pending_credential(raw_id.as_slice());

    Ok((
        StatusCode::CREATED,
        Json(UploadResponse {
            status_token: token.as_str().to_owned(),
            contribution_id,
        }),
    )
        .into_response())
}

async fn consume_nonce(state: &AppState, nonce: &looklab_upload::Nonce) -> ApiResult<()> {
    match store::consume_nonce(&state.db, nonce).await? {
        ConsumeNonce::Ok => Ok(()),
        ConsumeNonce::Unknown => Err(ApiError::new(ErrorCode::NonceUnknown)),
        ConsumeNonce::Expired => Err(ApiError::new(ErrorCode::NonceExpired)),
        ConsumeNonce::Replayed => Err(ApiError::new(ErrorCode::NonceReplayed)),
    }
}

async fn store_blobs(
    state: &AppState,
    manifest: &looklab_upload::BatchManifest,
    blobs: Vec<Vec<u8>>,
) -> ApiResult<Vec<i64>> {
    let store: Arc<dyn BlobStore> = state.blobs.clone();
    let images = manifest.images.clone();
    task::spawn_blocking(move || -> Result<Vec<i64>, ()> {
        let mut counts = Vec::with_capacity(blobs.len());
        for (blob, image) in blobs.iter().zip(images.iter()) {
            let hash = BlobHash::of(blob);
            if hash.as_bytes() != &image.blob_sha256 {
                return Err(());
            }
            store.put(hash, blob).map_err(|_| ())?;
            let n = match image.blob_kind {
                looklab_upload::BlobKind::Samples => {
                    looklab_samples::decode(blob).map_err(|_| ())?.n()
                }
                looklab_upload::BlobKind::PatchTable => looklab_samples::decode_patch_table(blob)
                    .map_err(|_| ())?
                    .rows()
                    .len(),
            };
            counts.push(i64::try_from(n).unwrap_or(i64::MAX));
        }
        Ok(counts)
    })
    .await
    .map_err(|_| ApiError::new(ErrorCode::Internal))?
    .map_err(|_| ApiError::new(ErrorCode::Internal))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
