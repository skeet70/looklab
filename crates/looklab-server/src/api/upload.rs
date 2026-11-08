// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt as _;
use looklab_upload::{
    Challenge, Envelope, Nonce, WebAuthnAssertion, parse_envelope_strict, peek_nonce,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::task;
use webauthn_rs::prelude::Passkey;

use crate::blob::{BlobHash, BlobStore};
use crate::body_schema::{
    RenderClassProblem, SettingsProblem, SnapshotVersionError, classify_renders,
    mono_filter_setting, validate_settings,
};
use crate::db::now_unix;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{BlobAttemptId, ContributionId, CredentialId, RawCredentialId, StatusToken};
use crate::preparse::{EnvelopeHeader, parse_header};
use crate::rate_limit::{Action, RateKey};
use crate::state::{AppState, BatchAdmissionError, PendingPromotion, PromotionError};
use crate::store::{
    self, ContributionKind, EstablishSend, IntentKind, NewContribution, NewImage, NewSend,
    StoredSend,
};
use crate::trust::{CredentialStanding, NEW_CREDENTIAL_STANDING};
use crate::verify::verify_assertion;

const NONCE_END: usize = 40;

const DEFERRED_RETENTION_DAYS: i64 = 180;

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
        peek_nonce(&bytes).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
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
    let resolved = match (stored, pending) {
        (Some(credential), _) => ResolvedCredential {
            passkey: credential.passkey,
            revision: Some(credential.revision),
            standing: credential.standing,
            existing: Some(credential.id),
        },
        (None, Some((passkey, _))) => ResolvedCredential {
            passkey,
            revision: None,
            standing: NEW_CREDENTIAL_STANDING,
            existing: None,
        },
        (None, None) => return Err(ApiError::new(ErrorCode::CredentialUnknown)),
    };

    let assertion_hash = assertion_digest(&header.assertion);
    let send = store::find_send(&state.db, &header.nonce)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let mut send_expires_at = send.as_ref().map(|send| send.expires_at);
    let passkey = resolved.passkey;
    let mut promotion: Option<PendingPromotion> = None;
    match send {
        Some(send) => {
            check_send_binding(&send, &header, &assertion_hash)?;
            check_send_open(&send)?;
            check_batch_pending(&state, &header).await?;
            if resolved.existing.is_none() {
                promotion = Some(
                    state
                        .begin_promotion(&passkey, &passkey)
                        .map_err(promotion_error)?,
                );
            }
        }
        None => {
            let challenge = Challenge::for_send(&header.nonce, &header.send_manifest_hash);
            let verified = verify_assertion(
                &passkey,
                &header.assertion,
                challenge,
                &state.config.origin,
                &state.config.rp_id,
            )
            .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;
            if let Some(id) = &resolved.existing {
                let revision = resolved
                    .revision
                    .as_ref()
                    .ok_or_else(|| ApiError::new(ErrorCode::Internal))?;
                match store::update_credential_passkey(&state.db, id, &verified, revision).await {
                    Ok(store::CredentialUpdate::Applied) => {}
                    Ok(store::CredentialUpdate::Stale) => {
                        return Err(ApiError::new(ErrorCode::SignatureInvalid));
                    }
                    Err(_) => return Err(ApiError::new(ErrorCode::Internal)),
                }
            } else {
                promotion = Some(
                    state
                        .begin_promotion(&passkey, &verified)
                        .map_err(promotion_error)?,
                );
            }
            let batch_hashes = header
                .send_manifest
                .batches
                .iter()
                .map(|hash| *hash.as_bytes())
                .collect::<Vec<_>>();
            let new_send = NewSend {
                manifest_hash: header.send_manifest_hash.as_bytes(),
                nonce: &header.nonce,
                credential_id: &header.assertion.credential_id,
                assertion_hash: &assertion_hash,
                batch_hashes: &batch_hashes,
            };
            match store::establish_send(&state.db, &new_send)
                .await
                .map_err(|_| ApiError::new(ErrorCode::Internal))?
            {
                EstablishSend::Established => {
                    send_expires_at = Some(
                        store::find_send(&state.db, &header.nonce)
                            .await
                            .map_err(|_| ApiError::new(ErrorCode::Internal))?
                            .ok_or_else(|| ApiError::new(ErrorCode::Internal))?
                            .expires_at,
                    );
                }
                EstablishSend::Existing(send) => {
                    send_expires_at = Some(send.expires_at);
                    check_send_binding(&send, &header, &assertion_hash)?;
                    check_send_open(&send)?;
                    check_batch_pending(&state, &header).await?;
                }
                EstablishSend::Unknown => return Err(ApiError::new(ErrorCode::NonceUnknown)),
                EstablishSend::Expired => return Err(ApiError::new(ErrorCode::NonceExpired)),
                EstablishSend::Replayed => return Err(ApiError::new(ErrorCode::NonceReplayed)),
            }
        }
    };
    if !state.rate_limiter.check(&RateKey::Assertion {
        action: Action::Upload,
        id: raw_id.clone(),
    }) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }

    let send_expires_at = send_expires_at.ok_or_else(|| ApiError::new(ErrorCode::Internal))?;
    let attempt = state
        .batch_admission
        .try_begin(
            header.nonce.as_bytes(),
            header.manifest_hash.as_bytes(),
            send_expires_at,
        )
        .map_err(|error| match error {
            BatchAdmissionError::InFlight => ApiError::new(ErrorCode::Busy),
            BatchAdmissionError::Exhausted => ApiError::new(ErrorCode::RateLimited),
            BatchAdmissionError::Capacity => ApiError::new(ErrorCode::Busy),
        })?;

    let params = state.trust.pow_params(resolved.standing);
    let n_images = header.n_images();
    let pow_json = serde_json::to_string(header.pow.as_array())
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let pow_challenge = header.pow_challenge();
    let pow = header.pow;
    task::spawn_blocking(move || looklab_upload::verify(&pow_challenge, n_images, &params, &pow))
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .map_err(|_| ApiError::new(ErrorCode::PowInvalid))?;

    let limits = state.config.limits;
    let body_for_parse = body.clone();
    let (mut envelope, counts) =
        task::spawn_blocking(move || -> Result<(Envelope, Vec<i64>), ApiError> {
            let envelope = parse_envelope_strict(&body_for_parse, &limits)
                .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
            let counts = blob_counts(&envelope)?;
            Ok((envelope, counts))
        })
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))??;

    if envelope.nonce != header.nonce
        || envelope.send_manifest_hash != header.send_manifest_hash
        || envelope.manifest_hash() != header.manifest_hash
    {
        return Err(ApiError::new(ErrorCode::MalformedEnvelope));
    }

    let bodies = state.bodies.read().await;
    let records = bodies
        .at(envelope.manifest.community_snapshot_version)
        .map_err(|error| match error {
            SnapshotVersionError::Unknown { .. } => ApiError::new(ErrorCode::SnapshotUnknown),
            SnapshotVersionError::TooOld { .. } => ApiError::new(ErrorCode::SnapshotTooOld),
        })?;
    let body_record = records
        .get(envelope.manifest.body.as_str())
        .ok_or_else(|| ApiError::new(ErrorCode::BodyUnknown))?;
    let card_family = match &envelope.manifest.intent {
        looklab_upload::Intent::Card(looklab_upload::CardDeclaration::Tagged(tag)) => {
            looklab_schema::LookId::new(tag.look_slug.as_str()).ok()
        }
        _ => None,
    };
    let classified = classify_renders(
        body_record,
        envelope.manifest.firmware_version.as_str(),
        &envelope.manifest.sim_key,
        envelope.manifest.render_class,
        matches!(envelope.manifest.intent, looklab_upload::Intent::Card(_)),
        card_family.as_ref(),
        mono_filter_setting(
            body_record,
            envelope.manifest.firmware_version.as_str(),
            &envelope.manifest.settings,
        ),
        envelope.manifest.images.iter().map(|image| &image.meta),
    )
    .map_err(render_class_error)?;
    validate_settings(
        body_record,
        envelope.manifest.firmware_version.as_str(),
        &envelope.manifest.sim_key,
        card_family.as_ref(),
        &envelope.manifest.settings,
        matches!(envelope.manifest.intent, looklab_upload::Intent::Card(_)),
        envelope.manifest.render_class == looklab_upload::RenderClass::Monochrome,
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
    drop(bodies);

    let Envelope {
        manifest,
        assertion,
        blobs,
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
        .zip(classified)
        .map(|((image, n_samples), classified)| -> ApiResult<NewImage> {
            Ok(NewImage {
                samples_hash: image.blob_sha256,
                n_samples,
                meta_json: serde_json::to_string(&image.meta)
                    .map_err(|_| ApiError::new(ErrorCode::Internal))?,
                rerender_group: image.rerender_group.map(|group| hex(group.as_bytes())),
                setting_source: setting_source.clone(),
                render_class: classified.render_class,
                native_sim: Some(classified.native_sim),
                family: Some(classified.family),
                mono_filter: classified.mono_filter,
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

    let deferred_until = match manifest.render_class {
        looklab_upload::RenderClass::Colour => None,
        looklab_upload::RenderClass::Monochrome => {
            Some(now_unix() + DEFERRED_RETENTION_DAYS * 86_400)
        }
    };
    let first_contribution = resolved.existing.is_none();
    let token = if first_contribution {
        Some(StatusToken::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?)
    } else {
        None
    };
    let contribution_id = ContributionId::new();
    let staged = StagedUpload::new(
        images.iter().map(|image| image.samples_hash).collect(),
        blobs,
        state.blobs.clone(),
    );
    store::stage_blobs(
        &state.db,
        &staged.attempt,
        &staged.hashes,
        staged.expires_at,
    )
    .await?;
    let completion = complete(
        &state,
        match resolved.existing.clone() {
            Some(id) => CredentialCompletion::Existing(id),
            None => CredentialCompletion::Pending(
                promotion
                    .take()
                    .ok_or_else(|| ApiError::new(ErrorCode::Internal))?,
            ),
        },
        NewContribution {
            id: contribution_id.clone(),
            credential_id: resolved.existing.clone().unwrap_or_else(CredentialId::new),
            manifest_hash: *header.manifest_hash.as_bytes(),
            send_nonce: header.nonce,
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
            normalized_settings: manifest.settings.clone(),
            community_snapshot_version: manifest.community_snapshot_version,
            render_class: manifest.render_class,
            monochrome_authorization: manifest.monochrome_authorization,
            deferred_until,
            images,
        },
        token.as_ref(),
        &header.nonce,
        header.manifest_hash.as_bytes(),
        &staged,
    )
    .await;
    if let Err(error) = completion {
        let _ = store::discard_staged(
            &state.db,
            state.blobs.as_ref(),
            &staged.attempt,
            &staged.hashes,
        )
        .await;
        return Err(error);
    }
    attempt.succeed();

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

struct ResolvedCredential {
    passkey: Passkey,
    revision: Option<store::CredentialRevision>,
    standing: CredentialStanding,
    existing: Option<CredentialId>,
}

enum CredentialCompletion {
    Existing(CredentialId),
    Pending(PendingPromotion),
}

fn check_send_binding(
    send: &StoredSend,
    header: &EnvelopeHeader,
    assertion_hash: &[u8; 32],
) -> ApiResult<()> {
    if send.manifest_hash != *header.send_manifest_hash.as_bytes() {
        return Err(ApiError::new(ErrorCode::SendManifestInvalid));
    }
    if send.credential_id != header.assertion.credential_id
        || send.assertion_hash != *assertion_hash
    {
        return Err(ApiError::new(ErrorCode::SendCredentialMismatch));
    }
    Ok(())
}

fn check_send_open(send: &StoredSend) -> ApiResult<()> {
    if send.completed_at.is_some() {
        return Err(ApiError::new(ErrorCode::SendReplayed));
    }
    if now_unix() > send.expires_at {
        return Err(ApiError::new(ErrorCode::SendExpired));
    }
    Ok(())
}

async fn check_batch_pending(state: &AppState, header: &EnvelopeHeader) -> ApiResult<()> {
    match store::find_send_batch(&state.db, &header.nonce, header.manifest_hash.as_bytes())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        None => Err(ApiError::new(ErrorCode::SendManifestInvalid)),
        Some(Some(_)) => Err(ApiError::new(ErrorCode::BatchReplayed)),
        Some(None) => Ok(()),
    }
}

fn assertion_digest(assertion: &WebAuthnAssertion) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for field in [
        &assertion.credential_id,
        &assertion.authenticator_data,
        &assertion.client_data_json,
        &assertion.signature,
    ] {
        hasher.update(u32::try_from(field.len()).unwrap_or(u32::MAX).to_le_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&digest);
    bytes
}

async fn complete(
    state: &AppState,
    credential: CredentialCompletion,
    mut contribution: NewContribution,
    token: Option<&StatusToken>,
    nonce: &Nonce,
    batch_hash: &[u8; 32],
    staged: &StagedUpload,
) -> ApiResult<()> {
    let tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    let mut tx = staged.acquire(tx).await?;
    let (credential_id, promotion) = match credential {
        CredentialCompletion::Existing(id) => (id, None),
        CredentialCompletion::Pending(promotion) => {
            let id = CredentialId::new();
            store::insert_credential(
                &mut tx,
                &id,
                promotion.passkey().cred_id().as_slice(),
                promotion.passkey(),
                promotion.display_handle(),
            )
            .await
            .map_err(database_error)?;
            (id, Some(promotion))
        }
    };
    contribution.credential_id = credential_id.clone();
    store::insert_contribution(&mut tx, &contribution).await?;
    if !store::mark_send_batch_received(&mut tx, nonce, batch_hash, &contribution.id).await? {
        return Err(ApiError::new(ErrorCode::BatchReplayed));
    }
    store::increment_send_received(&mut tx, nonce).await?;
    if let Some(token) = token {
        store::insert_status_token(&mut tx, &credential_id, &token.hash()).await?;
    }
    if contribution.render_class == looklab_upload::RenderClass::Colour {
        store::enqueue_consistency_score(&mut tx, &contribution.id).await?;
    }
    store::clear_staged(&mut tx, &staged.attempt).await?;
    match promotion {
        Some(promotion) => {
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = done_tx.send(promotion.commit(tx).await);
            });
            done_rx
                .await
                .map_err(|_| ApiError::new(ErrorCode::Internal))?
                .map_err(database_error)?;
        }
        None => tx.commit().await.map_err(database_error)?,
    }
    Ok(())
}

pub(super) fn promotion_error(error: PromotionError) -> ApiError {
    match error {
        PromotionError::Stale => ApiError::new(ErrorCode::SignatureInvalid),
        PromotionError::Busy => ApiError::new(ErrorCode::Busy),
    }
}

fn blob_counts(envelope: &Envelope) -> Result<Vec<i64>, ApiError> {
    let mut counts = Vec::with_capacity(envelope.blobs.len());
    for (blob, image) in envelope.blobs.iter().zip(envelope.manifest.images.iter()) {
        let hash = BlobHash::of(blob);
        if hash.as_bytes() != &image.blob_sha256 {
            return Err(ApiError::new(ErrorCode::MalformedEnvelope));
        }
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

/// One upload's blob reservation and bytes.
struct StagedUpload {
    attempt: BlobAttemptId,
    hashes: Vec<[u8; 32]>,
    blobs: Arc<Vec<Vec<u8>>>,
    store: Arc<dyn BlobStore>,
    expires_at: i64,
}

impl StagedUpload {
    fn new(hashes: Vec<[u8; 32]>, blobs: Vec<Vec<u8>>, store: Arc<dyn BlobStore>) -> Self {
        Self {
            attempt: BlobAttemptId::new(),
            hashes,
            blobs: Arc::new(blobs),
            store,
            expires_at: now_unix() + store::STAGED_TTL_SECONDS,
        }
    }

    /// Verifies the reservation and writes under a transaction the blocking task owns.
    async fn acquire(
        &self,
        mut tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    ) -> ApiResult<sqlx::Transaction<'static, sqlx::Sqlite>> {
        if !store::reservation_live(&mut tx, &self.attempt, self.distinct_hashes()).await? {
            return Err(ApiError::new(ErrorCode::Busy));
        }
        let store = self.store.clone();
        let hashes = self.hashes.clone();
        let blobs = self.blobs.clone();
        let (tx, ()) = crate::blob::write_guarded(tx, move || {
            for (hash, blob) in hashes.iter().zip(blobs.iter()) {
                if let Err(error) = store.put(BlobHash::from_bytes(*hash), blob) {
                    return ((), Err(error));
                }
            }
            ((), Ok(()))
        })
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        Ok(tx)
    }

    fn distinct_hashes(&self) -> usize {
        self.hashes.iter().collect::<BTreeSet<_>>().len()
    }
}

fn settings_error(_problem: SettingsProblem) -> ApiError {
    ApiError::new(ErrorCode::SchemaInvalid)
}

fn render_class_error(problem: RenderClassProblem) -> ApiError {
    match problem {
        RenderClassProblem::ClassMismatch { .. }
        | RenderClassProblem::SimMismatch { .. }
        | RenderClassProblem::AmbiguousSim
        | RenderClassProblem::CardFamilyUnknown
        | RenderClassProblem::MonochromeUnsupported { .. } => {
            ApiError::new(ErrorCode::RenderClassMismatch)
        }
        RenderClassProblem::Mapping(looklab_schema::MappingError::RenderClassConflict {
            ..
        }) => ApiError::new(ErrorCode::RenderClassMismatch),
        RenderClassProblem::NoEpoch
        | RenderClassProblem::UnknownSim(_)
        | RenderClassProblem::Mapping(_) => ApiError::new(ErrorCode::SchemaInvalid),
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
