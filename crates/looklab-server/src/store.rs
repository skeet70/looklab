// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{BTreeSet, HashMap};

use looklab_upload::{BodyId, LookSlug, MonochromeAuthorization, Nonce, RenderClass, SimKey};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use webauthn_rs::prelude::Passkey;

use crate::blob::{BlobHash, BlobStore};
use crate::db::now_unix;
use crate::ids::{
    BlobAttemptId, ContributionId, ContributionImageId, CredentialId, DeferredExpiryId, JobId,
    TokenHash, WithdrawalId,
};
use crate::trust::{CredentialStanding, Tier};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContributionStatus {
    Pending,
    Accepted,
    Quarantined,
    Withdrawn,
}

impl ContributionStatus {
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "accepted" => Some(Self::Accepted),
            "quarantined" => Some(Self::Quarantined),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageStatus {
    Pending,
    Accepted,
    DownWeighted,
    Quarantined,
}

impl ImageStatus {
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "accepted" => Some(Self::Accepted),
            "down_weighted" => Some(Self::DownWeighted),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }
}

impl core::fmt::Display for ImageStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::DownWeighted => "down-weighted",
            Self::Quarantined => "quarantined",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionKind {
    Library,
    Rerender,
    Pattern,
    Gradient,
    Chart,
}

impl ContributionKind {
    pub fn from_upload(kind: looklab_upload::ContributionKind) -> Self {
        match kind {
            looklab_upload::ContributionKind::Library => Self::Library,
            looklab_upload::ContributionKind::Rerender => Self::Rerender,
            looklab_upload::ContributionKind::Pattern => Self::Pattern,
            looklab_upload::ContributionKind::Gradient => Self::Gradient,
            looklab_upload::ContributionKind::Chart => Self::Chart,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Library => "library",
            Self::Rerender => "rerender",
            Self::Pattern => "pattern",
            Self::Gradient => "gradient",
            Self::Chart => "chart",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    ConsistencyScore,
    HardDeleteBlobs,
    PublishSite,
    RevertRefit,
}

impl JobKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ConsistencyScore => "consistency_score",
            Self::HardDeleteBlobs => "hard_delete_blobs",
            Self::PublishSite => "publish_site",
            Self::RevertRefit => "revert_refit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    Native,
    Recipe,
    Card,
}

impl IntentKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Recipe => "recipe",
            Self::Card => "card",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "native" => Some(Self::Native),
            "recipe" => Some(Self::Recipe),
            "card" => Some(Self::Card),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeNonce {
    Ok,
    Unknown,
    Expired,
    Replayed,
}

#[derive(Debug)]
pub struct CredentialRevision(String);

impl CredentialRevision {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub struct StoredCredential {
    pub id: CredentialId,
    pub passkey: Passkey,
    pub revision: CredentialRevision,
    pub standing: CredentialStanding,
}

pub async fn issue_nonce(
    db: &SqlitePool,
    nonce: &Nonce,
    ttl_seconds: i64,
) -> Result<(), sqlx::Error> {
    let now = now_unix();
    sqlx::query("DELETE FROM nonces WHERE expires_at < ?")
        .bind(now - 3600)
        .execute(db)
        .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM nonces")
        .fetch_one(db)
        .await?;
    if count >= 10_000 {
        sqlx::query(
            "DELETE FROM nonces WHERE nonce IN
             (SELECT nonce FROM nonces ORDER BY issued_at ASC LIMIT ?)",
        )
        .bind(count - 9_000)
        .execute(db)
        .await?;
    }
    sqlx::query("INSERT INTO nonces (nonce, issued_at, expires_at) VALUES (?, ?, ?)")
        .bind(hex(nonce.as_bytes()))
        .bind(now)
        .bind(now + ttl_seconds)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn consume_nonce(db: &SqlitePool, nonce: &Nonce) -> Result<ConsumeNonce, sqlx::Error> {
    let key = hex(nonce.as_bytes());
    let now = now_unix();
    let affected = sqlx::query(
        "UPDATE nonces SET used_at = ? WHERE nonce = ? AND used_at IS NULL AND expires_at > ?",
    )
    .bind(now)
    .bind(&key)
    .bind(now)
    .execute(db)
    .await?
    .rows_affected();
    if affected == 1 {
        return Ok(ConsumeNonce::Ok);
    }
    let row: Option<(Option<i64>, i64)> =
        sqlx::query_as("SELECT used_at, expires_at FROM nonces WHERE nonce = ?")
            .bind(&key)
            .fetch_optional(db)
            .await?;
    Ok(match row {
        None => ConsumeNonce::Unknown,
        Some((Some(_), _)) => ConsumeNonce::Replayed,
        Some((None, expires_at)) if expires_at <= now => ConsumeNonce::Expired,
        Some((None, _)) => ConsumeNonce::Unknown,
    })
}

#[derive(Debug, Clone)]
pub struct StoredSend {
    pub manifest_hash: [u8; 32],
    pub credential_id: Vec<u8>,
    pub assertion_hash: [u8; 32],
    pub expires_at: i64,
    pub completed_at: Option<i64>,
}

pub async fn find_send(db: &SqlitePool, nonce: &Nonce) -> Result<Option<StoredSend>, sqlx::Error> {
    type SendRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, Option<i64>);
    let row: Option<SendRow> = sqlx::query_as(
        "SELECT manifest_hash, credential_id, assertion_hash, expires_at, completed_at
         FROM send_manifests WHERE nonce = ?",
    )
    .bind(hex(nonce.as_bytes()))
    .fetch_optional(db)
    .await?;
    row.map(|row| {
        let manifest_hash = <[u8; 32]>::try_from(row.0)
            .map_err(|_| sqlx::Error::Protocol("send manifest hash is not 32 bytes".to_owned()))?;
        let assertion_hash = <[u8; 32]>::try_from(row.2)
            .map_err(|_| sqlx::Error::Protocol("assertion hash is not 32 bytes".to_owned()))?;
        Ok(StoredSend {
            manifest_hash,
            credential_id: row.1,
            assertion_hash,
            expires_at: row.3,
            completed_at: row.4,
        })
    })
    .transpose()
}

/// `None` when the send instance does not list the batch, otherwise whether it
/// already arrived.
pub async fn find_send_batch(
    db: &SqlitePool,
    nonce: &Nonce,
    batch: &[u8; 32],
) -> Result<Option<Option<i64>>, sqlx::Error> {
    let row: Option<(Option<i64>,)> = sqlx::query_as(
        "SELECT received_at FROM send_manifest_batches
         WHERE send_nonce = ? AND batch_manifest_hash = ?",
    )
    .bind(hex(nonce.as_bytes()))
    .bind(batch.as_slice())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| row.0))
}

pub struct NewSend<'a> {
    pub manifest_hash: &'a [u8; 32],
    pub nonce: &'a Nonce,
    pub credential_id: &'a [u8],
    pub assertion_hash: &'a [u8; 32],
    pub batch_hashes: &'a [[u8; 32]],
}

pub enum EstablishSend {
    Established,
    Existing(StoredSend),
    Unknown,
    Expired,
    Replayed,
}

/// Consumes the nonce and inserts the send binding in one transaction. A
/// competing request sees the winning row as [`EstablishSend::Existing`].
pub async fn establish_send(
    db: &SqlitePool,
    send: &NewSend<'_>,
) -> Result<EstablishSend, sqlx::Error> {
    let key = hex(send.nonce.as_bytes());
    let now = now_unix();
    let mut tx = db.begin().await?;
    let affected = sqlx::query(
        "UPDATE nonces SET used_at = ? WHERE nonce = ? AND used_at IS NULL AND expires_at > ?",
    )
    .bind(now)
    .bind(&key)
    .bind(now)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected == 1 {
        let expires_at: i64 = sqlx::query_scalar("SELECT expires_at FROM nonces WHERE nonce = ?")
            .bind(&key)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO send_manifests
             (nonce, manifest_hash, credential_id, assertion_hash, batch_count, received_count,
              created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&key)
        .bind(send.manifest_hash.as_slice())
        .bind(send.credential_id)
        .bind(send.assertion_hash.as_slice())
        .bind(i64::try_from(send.batch_hashes.len()).unwrap_or(i64::MAX))
        .bind(now)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        for hash in send.batch_hashes {
            sqlx::query(
                "INSERT INTO send_manifest_batches (send_nonce, batch_manifest_hash)
                 VALUES (?, ?)",
            )
            .bind(&key)
            .bind(hash.as_slice())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        return Ok(EstablishSend::Established);
    }
    tx.rollback().await?;
    let row: Option<(Option<i64>, i64)> =
        sqlx::query_as("SELECT used_at, expires_at FROM nonces WHERE nonce = ?")
            .bind(&key)
            .fetch_optional(db)
            .await?;
    Ok(match row {
        None => EstablishSend::Unknown,
        Some((Some(_), _)) => match find_send(db, send.nonce).await? {
            Some(existing) => EstablishSend::Existing(existing),
            None => EstablishSend::Replayed,
        },
        Some((None, expires_at)) if expires_at <= now => EstablishSend::Expired,
        Some((None, _)) => EstablishSend::Unknown,
    })
}

/// Moves one batch from pending to received if the send is still open and
/// unexpired; exactly one caller can win.
pub async fn mark_send_batch_received(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    nonce: &Nonce,
    batch: &[u8; 32],
    contribution: &ContributionId,
) -> Result<bool, sqlx::Error> {
    let key = hex(nonce.as_bytes());
    let now = now_unix();
    let affected = sqlx::query(
        "UPDATE send_manifest_batches SET received_at = ?, contribution_id = ?
         WHERE send_nonce = ? AND batch_manifest_hash = ? AND received_at IS NULL
           AND EXISTS (SELECT 1 FROM send_manifests
                       WHERE nonce = ? AND completed_at IS NULL AND expires_at >= ?)",
    )
    .bind(now)
    .bind(contribution.as_str())
    .bind(&key)
    .bind(batch.as_slice())
    .bind(&key)
    .bind(now)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    Ok(affected == 1)
}

pub async fn increment_send_received(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    nonce: &Nonce,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE send_manifests
         SET received_count = received_count + 1,
             completed_at = CASE WHEN received_count + 1 >= batch_count THEN ? ELSE completed_at END
         WHERE nonce = ?",
    )
    .bind(now_unix())
    .bind(hex(nonce.as_bytes()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn find_credential(
    db: &SqlitePool,
    raw_id: &[u8],
) -> Result<Option<StoredCredential>, CredentialLoadError> {
    let row: Option<(String, String, String, f64)> = sqlx::query_as(
        "SELECT id, webauthn_credential, tier, reputation FROM credentials
         WHERE credential_id = ? AND status = 'active'",
    )
    .bind(raw_id)
    .fetch_optional(db)
    .await
    .map_err(CredentialLoadError::Database)?;
    let Some((id, passkey_json, tier, reputation)) = row else {
        return Ok(None);
    };
    let passkey: Passkey =
        serde_json::from_str(&passkey_json).map_err(|_| CredentialLoadError::Corrupt)?;
    let standing = CredentialStanding {
        tier: Tier::from_db(&tier).ok_or(CredentialLoadError::Corrupt)?,
        reputation,
    };
    Ok(Some(StoredCredential {
        id: CredentialId::try_from(id).map_err(|_| CredentialLoadError::Corrupt)?,
        passkey,
        revision: CredentialRevision(passkey_json),
        standing,
    }))
}

pub async fn credential_exists(db: &SqlitePool, raw_id: &[u8]) -> Result<bool, sqlx::Error> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials WHERE credential_id = ?")
        .bind(raw_id)
        .fetch_one(db)
        .await?;
    Ok(count > 0)
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialLoadError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("stored credential is not readable")]
    Corrupt,
}

pub async fn credential_id_for_token(
    db: &SqlitePool,
    token_hash: &TokenHash,
) -> Result<Option<CredentialId>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT credential_id FROM status_tokens WHERE token_hash = ? AND revoked_at IS NULL",
    )
    .bind(token_hash.as_bytes().as_slice())
    .fetch_optional(db)
    .await?;
    match row {
        Some((id,)) => CredentialId::try_from(id)
            .map(Some)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned())),
        None => Ok(None),
    }
}

pub async fn revoke_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    credential_id: &CredentialId,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE status_tokens SET revoked_at = ? WHERE credential_id = ? AND revoked_at IS NULL",
    )
    .bind(now_unix())
    .bind(credential_id.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn insert_status_token(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    credential_id: &CredentialId,
    token_hash: &TokenHash,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO status_tokens (token_hash, credential_id, issued_at) VALUES (?, ?, ?)",
    )
    .bind(token_hash.as_bytes().as_slice())
    .bind(credential_id.as_str())
    .bind(now_unix())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn enqueue_consistency_score(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    contribution_id: &ContributionId,
) -> Result<JobId, sqlx::Error> {
    #[derive(Serialize)]
    struct Payload<'a> {
        contribution_id: &'a ContributionId,
    }
    let payload = serde_json::to_string(&Payload { contribution_id })
        .map_err(|_| sqlx::Error::Protocol("job payload does not serialize".to_owned()))?;
    enqueue_job(tx, JobKind::ConsistencyScore, &payload, now_unix()).await
}

pub async fn insert_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: &CredentialId,
    raw_id: &[u8],
    passkey: &Passkey,
    display_handle: Option<&str>,
) -> Result<(), sqlx::Error> {
    let passkey_json = serde_json::to_string(passkey)
        .map_err(|_| sqlx::Error::Protocol("passkey does not serialize".to_owned()))?;
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, display_handle, tier, roles, reputation, created_at, status)
         VALUES (?, ?, ?, ?, ?, '[]', ?, ?, 'active')",
    )
    .bind(id.as_str())
    .bind(raw_id)
    .bind(passkey_json)
    .bind(display_handle)
    .bind(crate::trust::NEW_CREDENTIAL_STANDING.tier.as_str())
    .bind(crate::trust::NEW_CREDENTIAL_STANDING.reputation)
    .bind(now_unix())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialUpdate {
    Applied,
    Stale,
}

/// Replaces the state `original` was verified against; `Stale` means another
/// assertion changed it first.
pub async fn update_credential_passkey(
    db: &SqlitePool,
    credential_id: &CredentialId,
    passkey: &Passkey,
    revision: &CredentialRevision,
) -> Result<CredentialUpdate, sqlx::Error> {
    let passkey_json = serde_json::to_string(passkey)
        .map_err(|_| sqlx::Error::Protocol("passkey does not serialize".to_owned()))?;
    let updated = sqlx::query(
        "UPDATE credentials SET webauthn_credential = ?
         WHERE id = ? AND webauthn_credential = ?",
    )
    .bind(passkey_json)
    .bind(credential_id.as_str())
    .bind(revision.as_str())
    .execute(db)
    .await?
    .rows_affected();
    Ok(if updated == 1 {
        CredentialUpdate::Applied
    } else {
        CredentialUpdate::Stale
    })
}

pub struct NewImage {
    pub samples_hash: [u8; 32],
    pub n_samples: i64,
    pub meta_json: String,
    pub rerender_group: Option<String>,
    /// `(control key, source)` JSON ([docs/03 §1]).
    pub setting_source: String,
    pub render_class: RenderClass,
    pub native_sim: Option<looklab_upload::SimKey>,
    pub family: Option<looklab_schema::LookId>,
    pub mono_filter: Option<looklab_schema::MonoFilter>,
}

pub struct NewContribution {
    pub id: ContributionId,
    pub credential_id: CredentialId,
    pub manifest_hash: [u8; 32],
    pub send_nonce: Nonce,
    pub signature: Vec<u8>,
    pub pow: String,
    pub kind: ContributionKind,
    pub serial_hmac: Option<[u8; 32]>,
    pub intent: IntentKind,
    pub target_look: Option<String>,
    pub card: Option<String>,
    pub body_id: String,
    pub firmware_version: String,
    pub sim_key: String,
    pub settings_json: String,
    pub normalized_settings: looklab_upload::ControlSettings,
    pub community_snapshot_version: looklab_upload::CommunitySnapshotVersion,
    pub render_class: RenderClass,
    pub monochrome_authorization: Option<MonochromeAuthorization>,
    pub deferred_until: Option<i64>,
    pub images: Vec<NewImage>,
}

pub async fn insert_contribution(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    contribution: &NewContribution,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, send_nonce, signature, pow, kind,
          serial_hmac, intent, target_look_id, card, body_id, firmware_version, sim_key, settings,
          normalized_settings, community_snapshot_version, render_class,
          monochrome_authorization, deferred_until, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(contribution.id.as_str())
    .bind(contribution.credential_id.as_str())
    .bind(contribution.manifest_hash.as_slice())
    .bind(hex(contribution.send_nonce.as_bytes()))
    .bind(contribution.signature.as_slice())
    .bind(&contribution.pow)
    .bind(contribution.kind.as_str())
    .bind(contribution.serial_hmac.map(|hmac| hmac.to_vec()))
    .bind(contribution.intent.as_str())
    .bind(contribution.target_look.as_deref())
    .bind(contribution.card.as_deref())
    .bind(&contribution.body_id)
    .bind(&contribution.firmware_version)
    .bind(&contribution.sim_key)
    .bind(&contribution.settings_json)
    .bind(
        serde_json::to_string(&contribution.normalized_settings)
            .map_err(|_| sqlx::Error::Protocol("settings do not serialize".to_owned()))?,
    )
    .bind(i64::try_from(contribution.community_snapshot_version.get()).unwrap_or(i64::MAX))
    .bind(render_class_str(contribution.render_class))
    .bind(contribution.monochrome_authorization.map(authorization_str))
    .bind(contribution.deferred_until)
    .bind(now_unix())
    .execute(&mut **tx)
    .await?;

    for image in &contribution.images {
        sqlx::query(
            "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        )
        .bind(image.samples_hash.as_slice())
        .execute(&mut **tx)
        .await?;
        let id = ContributionImageId::new();
        sqlx::query(
            "INSERT INTO contribution_images
             (id, contribution_id, samples_hash, n_samples, meta, rerender_group, setting_source,
              render_class, native_sim, family, mono_filter, status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(id.as_str())
        .bind(contribution.id.as_str())
        .bind(image.samples_hash.as_slice())
        .bind(image.n_samples)
        .bind(&image.meta_json)
        .bind(image.rerender_group.as_deref())
        .bind(&image.setting_source)
        .bind(render_class_str(image.render_class))
        .bind(
            image
                .native_sim
                .as_ref()
                .map(looklab_upload::SimKey::as_str),
        )
        .bind(image.family.as_ref().map(looklab_schema::LookId::as_str))
        .bind(image.mono_filter.map(mono_filter_str))
        .execute(&mut **tx)
        .await?;
    }
    if let Some(due_at) = contribution.deferred_until {
        schedule_deferred_expiry(tx, &contribution.id, due_at).await?;
    }
    Ok(())
}

async fn schedule_deferred_expiry(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    contribution_id: &ContributionId,
    due_at: i64,
) -> Result<(), sqlx::Error> {
    let expiry_id = DeferredExpiryId::new();
    sqlx::query(
        "INSERT INTO deferred_expiries (id, contribution_id, due_at, status)
         VALUES (?, ?, ?, 'pending')",
    )
    .bind(expiry_id.as_str())
    .bind(contribution_id.as_str())
    .bind(due_at)
    .execute(&mut **tx)
    .await?;
    let payload = serde_json::to_string(&BlobDeletionPayload { contribution_id })
        .map_err(|_| sqlx::Error::Protocol("job payload does not serialize".to_owned()))?;
    enqueue_job(tx, JobKind::HardDeleteBlobs, &payload, due_at).await?;
    Ok(())
}

pub(crate) fn render_class_from_db(value: &str) -> Option<RenderClass> {
    match value {
        "colour" => Some(RenderClass::Colour),
        "monochrome" => Some(RenderClass::Monochrome),
        _ => None,
    }
}

fn render_class_str(class: RenderClass) -> &'static str {
    match class {
        RenderClass::Colour => "colour",
        RenderClass::Monochrome => "monochrome",
    }
}

fn authorization_str(authorization: MonochromeAuthorization) -> &'static str {
    match authorization {
        MonochromeAuthorization::ModelDevelopment180Days => "model_development_180_days",
    }
}

fn mono_filter_str(filter: looklab_schema::MonoFilter) -> &'static str {
    match filter {
        looklab_schema::MonoFilter::None => "none",
        looklab_schema::MonoFilter::Yellow => "yellow",
        looklab_schema::MonoFilter::Red => "red",
        looklab_schema::MonoFilter::Green => "green",
    }
}

/// Uploads past this age are rejected and swept instead of published.
pub const STAGED_TTL_SECONDS: i64 = 600;

/// Commits the hashes this attempt will write, before any file is created.
pub async fn stage_blobs(
    db: &SqlitePool,
    attempt: &BlobAttemptId,
    hashes: &[[u8; 32]],
    expires_at: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let now = now_unix();
    for hash in hashes {
        sqlx::query(
            "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at)
             VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(attempt.as_str())
        .bind(hash.as_slice())
        .bind(now)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// True when every hash this attempt staged is still inside its lifetime.
pub async fn reservation_live(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &BlobAttemptId,
    expected: usize,
) -> Result<bool, sqlx::Error> {
    let live: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM blob_staging WHERE attempt_id = ? AND expires_at > ?",
    )
    .bind(attempt.as_str())
    .bind(now_unix())
    .fetch_one(&mut **tx)
    .await?;
    Ok(usize::try_from(live).ok() == Some(expected))
}

/// Deletes the attempt's reservations in the caller's transaction.
pub async fn clear_staged(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    attempt: &BlobAttemptId,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM blob_staging WHERE attempt_id = ?")
        .bind(attempt.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Drops the attempt's reservation and unlinks files no other record names.
pub async fn discard_staged(
    db: &SqlitePool,
    blobs: &dyn BlobStore,
    attempt: &BlobAttemptId,
    hashes: &[[u8; 32]],
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    clear_staged(&mut tx, attempt).await?;
    for hash in hashes.iter().collect::<BTreeSet<_>>() {
        if blob_referenced(&mut tx, hash).await? {
            continue;
        }
        blobs
            .delete(BlobHash::from_bytes(*hash))
            .map_err(|error| sqlx::Error::Io(std::io::Error::other(error)))?;
    }
    tx.commit().await
}

/// Expires the attempt's reservation; the worker sweep removes its files.
pub async fn abandon_staged(db: &SqlitePool, attempt: &BlobAttemptId) -> Result<(), sqlx::Error> {
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE blob_staging SET expires_at = 0 WHERE attempt_id = ?")
        .bind(attempt.as_str())
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

pub async fn blob_referenced(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hash: &[u8; 32],
) -> Result<bool, sqlx::Error> {
    let referenced: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM blob_references WHERE hash = ?)")
            .bind(hex(hash))
            .fetch_one(&mut **tx)
            .await?;
    Ok(referenced != 0)
}

#[derive(Serialize)]
struct BlobDeletionPayload<'a> {
    contribution_id: &'a ContributionId,
}

pub async fn enqueue_job(
    conn: &mut SqliteConnection,
    kind: JobKind,
    payload: &str,
    run_after: i64,
) -> Result<JobId, sqlx::Error> {
    let id = JobId::new();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, ?, ?, 'queued', 0, ?, ?)",
    )
    .bind(id.as_str())
    .bind(kind.as_str())
    .bind(payload)
    .bind(run_after)
    .bind(now_unix())
    .execute(conn)
    .await?;
    Ok(id)
}

/// Persists the request while a publisher is queued or running; a running one
/// enqueues its successor when it completes.
pub async fn request_publication(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let now = now_unix();
    sqlx::query("UPDATE publication SET generation = generation + 1 WHERE id = 1")
        .execute(&mut *conn)
        .await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND status IN ('queued', 'running')",
    )
    .bind(JobKind::PublishSite.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if pending == 0 {
        enqueue_job(conn, JobKind::PublishSite, "{}", now).await?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ImageFeed {
    pub id: ContributionImageId,
    pub status: ImageStatus,
    pub reason: Option<String>,
    pub rms_de: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct ContributionFeed {
    pub id: ContributionId,
    pub status: ContributionStatus,
    pub render_class: RenderClass,
    pub deferred_until: Option<i64>,
    pub created_at: i64,
    pub body_id: BodyId,
    pub intent: IntentKind,
    pub target_look: Option<LookSlug>,
    pub sim_key: SimKey,
    pub images: Vec<ImageFeed>,
    #[serde(skip)]
    send_nonce: Option<Nonce>,
}

/// A send groups by its signed nonce; records received before sends were
/// grouped have none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    Send,
    Activity,
}

#[derive(Debug, Serialize)]
pub struct ActivityFeed {
    pub source: ActivitySource,
    pub created_at: i64,
    pub contributions: Vec<ContributionFeed>,
    #[serde(skip)]
    pub(crate) identity: ActivityIdentity,
}

#[derive(Debug)]
pub(crate) struct ActivityIdentity(sha2::digest::Output<Sha256>);

impl std::fmt::Display for ActivityIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::LowerHex::fmt(&self.0, formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum ActivityKey {
    Send(Nonce),
    Activity { day: i64, body: BodyId },
}

fn stored_send_nonce(value: &str) -> Result<Nonce, sqlx::Error> {
    let invalid = || sqlx::Error::Protocol("stored send nonce is invalid".to_owned());
    if value.len() != 64 {
        return Err(invalid());
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        let pair = value.get(index * 2..index * 2 + 2).ok_or_else(invalid)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| invalid())?;
    }
    Ok(Nonce::from_bytes(bytes))
}

#[derive(sqlx::FromRow)]
struct FeedRow {
    contribution_id: String,
    contribution_status: String,
    render_class: String,
    deferred_until: Option<i64>,
    created_at: i64,
    send_nonce: Option<String>,
    body_id: String,
    intent: String,
    target_look_id: Option<String>,
    sim_key: String,
    image_id: Option<String>,
    image_status: Option<String>,
    reason: Option<String>,
    rms_de: Option<f64>,
}

pub async fn status_feed(
    db: &SqlitePool,
    credential_id: &CredentialId,
) -> Result<Vec<ActivityFeed>, sqlx::Error> {
    let rows: Vec<FeedRow> = sqlx::query_as(
        "SELECT c.id AS contribution_id, c.status AS contribution_status,
                c.render_class, c.deferred_until, c.created_at, c.send_nonce,
                c.body_id, c.intent, c.target_look_id, c.sim_key,
                i.id AS image_id, i.status AS image_status, i.status_reason AS reason,
                i.rms_de
         FROM contributions c
         LEFT JOIN contribution_images i ON i.contribution_id = c.id
         WHERE c.credential_id = ?
         ORDER BY c.created_at DESC, c.id, i.id",
    )
    .bind(credential_id.as_str())
    .fetch_all(db)
    .await?;

    let mut contributions: Vec<ContributionFeed> = Vec::new();
    for row in rows {
        let id = ContributionId::try_from(row.contribution_id)
            .map_err(|_| sqlx::Error::Protocol("contribution id is not a UUID".to_owned()))?;
        let entry = match contributions.last_mut() {
            Some(entry) if entry.id == id => entry,
            _ => {
                contributions.push(ContributionFeed {
                    id,
                    status: ContributionStatus::from_db(&row.contribution_status).ok_or_else(
                        || sqlx::Error::Protocol("unknown contribution status".to_owned()),
                    )?,
                    render_class: render_class_from_db(&row.render_class)
                        .ok_or_else(|| sqlx::Error::Protocol("unknown render class".to_owned()))?,
                    deferred_until: row.deferred_until,
                    created_at: row.created_at,
                    body_id: BodyId::try_from(row.body_id).map_err(|_| {
                        sqlx::Error::Protocol("contribution body id is invalid".to_owned())
                    })?,
                    intent: IntentKind::from_db(&row.intent).ok_or_else(|| {
                        sqlx::Error::Protocol("unknown contribution intent".to_owned())
                    })?,
                    target_look: row
                        .target_look_id
                        .map(LookSlug::try_from)
                        .transpose()
                        .map_err(|_| {
                            sqlx::Error::Protocol("contribution target look is invalid".to_owned())
                        })?,
                    sim_key: SimKey::try_from(row.sim_key).map_err(|_| {
                        sqlx::Error::Protocol("contribution sim key is invalid".to_owned())
                    })?,
                    images: Vec::new(),
                    send_nonce: row
                        .send_nonce
                        .as_deref()
                        .map(stored_send_nonce)
                        .transpose()?,
                });
                contributions.last_mut().expect("just pushed")
            }
        };
        if let (Some(image_id), Some(image_status)) = (row.image_id, row.image_status) {
            entry.images.push(ImageFeed {
                id: ContributionImageId::try_from(image_id).map_err(|_| {
                    sqlx::Error::Protocol("contribution image id is not a UUID".to_owned())
                })?,
                status: ImageStatus::from_db(&image_status)
                    .ok_or_else(|| sqlx::Error::Protocol("unknown image status".to_owned()))?,
                reason: row.reason,
                rms_de: row.rms_de,
            });
        }
    }

    let mut groups: HashMap<ActivityKey, ActivityFeed> = HashMap::new();
    for contribution in contributions {
        let key = match contribution.send_nonce {
            Some(nonce) => ActivityKey::Send(nonce),
            None => ActivityKey::Activity {
                day: contribution.created_at.div_euclid(86_400),
                body: contribution.body_id.clone(),
            },
        };
        let source = match &key {
            ActivityKey::Send(_) => ActivitySource::Send,
            ActivityKey::Activity { .. } => ActivitySource::Activity,
        };
        let created_at = contribution.created_at;
        let activity = groups.entry(key).or_insert_with_key(|key| ActivityFeed {
            source,
            created_at,
            contributions: Vec::new(),
            identity: activity_identity(credential_id, key),
        });
        activity.created_at = activity.created_at.min(created_at);
        activity.contributions.push(contribution);
    }
    let mut activities: Vec<(ActivityKey, ActivityFeed)> = groups.into_iter().collect();
    activities.sort_by(|(left_key, left), (right_key, right)| {
        right
            .created_at
            .cmp(&left.created_at)
            .then_with(|| left_key.cmp(right_key))
    });
    Ok(activities
        .into_iter()
        .map(|(_, activity)| activity)
        .collect())
}

fn activity_identity(credential_id: &CredentialId, key: &ActivityKey) -> ActivityIdentity {
    let mut hasher = Sha256::new();
    hasher.update(b"looklab-activity-v1:");
    hasher.update(credential_id.as_str().as_bytes());
    hasher.update(b":");
    match key {
        ActivityKey::Send(nonce) => {
            hasher.update(b"send:");
            hasher.update(nonce.as_bytes());
        }
        ActivityKey::Activity { day, body } => {
            hasher.update(b"activity:");
            hasher.update(day.to_be_bytes());
            hasher.update(b":");
            hasher.update(body.as_str().as_bytes());
        }
    }
    ActivityIdentity(hasher.finalize())
}

pub async fn withdraw(
    db: &SqlitePool,
    contribution_ids: &[ContributionId],
    credential_id: &CredentialId,
    delete_after: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let mut delete_afters = Vec::with_capacity(contribution_ids.len());
    for contribution_id in contribution_ids {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT credential_id, status FROM contributions WHERE id = ?")
                .bind(contribution_id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((owner, status)) = row else {
            return Ok(None);
        };
        if owner != credential_id.as_str() {
            return Ok(None);
        }
        let status = ContributionStatus::from_db(&status)
            .ok_or_else(|| sqlx::Error::Protocol("unknown contribution status".to_owned()))?;
        if status == ContributionStatus::Withdrawn {
            let existing: Option<(i64,)> =
                sqlx::query_as("SELECT delete_after FROM withdrawals WHERE contribution_id = ?")
                    .bind(contribution_id.as_str())
                    .fetch_optional(&mut *tx)
                    .await?;
            match existing {
                Some((existing,)) => delete_afters.push(existing),
                None => delete_afters.push(now_unix()),
            }
            continue;
        }

        sqlx::query(
            "UPDATE contributions SET status = 'withdrawn' WHERE id = ? AND status != 'withdrawn'",
        )
        .bind(contribution_id.as_str())
        .execute(&mut *tx)
        .await?;
        let withdrawal_id = WithdrawalId::new();
        sqlx::query(
            "INSERT INTO withdrawals (id, contribution_id, credential_id, requested_at, delete_after, status)
             VALUES (?, ?, ?, ?, ?, 'pending')",
        )
        .bind(withdrawal_id.as_str())
        .bind(contribution_id.as_str())
        .bind(credential_id.as_str())
        .bind(now_unix())
        .bind(delete_after)
        .execute(&mut *tx)
        .await?;
        let payload = serde_json::to_string(&BlobDeletionPayload { contribution_id })
            .map_err(|_| sqlx::Error::Protocol("job payload does not serialize".to_owned()))?;
        enqueue_job(&mut tx, JobKind::HardDeleteBlobs, &payload, delete_after).await?;
        delete_afters.push(delete_after);
    }
    tx.commit().await?;
    Ok(delete_afters.into_iter().max())
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use webauthn_rs_core::proto::{
        AttestationFormat, COSEAlgorithm, COSEEC2Key, COSEKey, COSEKeyType, Credential, ECDSACurve,
        ParsedAttestation, RegisteredExtensions, UserVerificationPolicy,
    };

    fn passkey(counter: u32, backup_state: bool) -> Passkey {
        Credential {
            cred_id: vec![1, 2, 3, 4],
            cred: COSEKey {
                type_: COSEAlgorithm::ES256,
                key: COSEKeyType::EC_EC2(COSEEC2Key {
                    curve: ECDSACurve::SECP256R1,
                    x: vec![0; 32],
                    y: vec![0; 32],
                }),
            },
            counter,
            transports: None,
            user_verified: true,
            backup_eligible: false,
            backup_state,
            registration_policy: UserVerificationPolicy::Required,
            extensions: RegisteredExtensions::none(),
            attestation: ParsedAttestation::default(),
            attestation_format: AttestationFormat::None,
        }
        .into()
    }

    const RAW_ID: [u8; 4] = [1, 2, 3, 4];

    async fn database() -> (tempfile::TempDir, SqlitePool, CredentialId) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = crate::db::open(&dir.path().join("looklab.sqlite3"))
            .await
            .expect("database");
        let credential_id = CredentialId::new();
        let mut tx = db.begin().await.expect("transaction");
        insert_credential(&mut tx, &credential_id, &RAW_ID, &passkey(0, false), None)
            .await
            .expect("credential");
        tx.commit().await.expect("commit");
        (dir, db, credential_id)
    }

    async fn revision(db: &SqlitePool) -> CredentialRevision {
        find_credential(db, &RAW_ID)
            .await
            .expect("credential")
            .expect("credential")
            .revision
    }

    async fn stored_state(db: &SqlitePool) -> (u32, bool) {
        let stored = find_credential(db, &RAW_ID)
            .await
            .expect("credential")
            .expect("credential");
        let credential = Credential::from(stored.passkey);
        (credential.counter, credential.backup_state)
    }

    #[tokio::test]
    async fn a_stale_credential_update_is_rejected() {
        let (_dir, db, credential_id) = database().await;
        let original = revision(&db).await;
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(7, false), &original)
                .await
                .expect("newer update"),
            CredentialUpdate::Applied
        );
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(5, true), &original)
                .await
                .expect("stale update"),
            CredentialUpdate::Stale
        );
        assert_eq!(stored_state(&db).await, (7, false));
    }

    #[tokio::test]
    async fn a_duplicate_credential_update_is_rejected() {
        let (_dir, db, credential_id) = database().await;
        let original = revision(&db).await;
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(1, false), &original)
                .await
                .expect("first update"),
            CredentialUpdate::Applied
        );
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(1, true), &original)
                .await
                .expect("duplicate update"),
            CredentialUpdate::Stale
        );
        assert_eq!(stored_state(&db).await, (1, false));
    }

    #[tokio::test]
    async fn a_zero_counter_credential_update_is_persisted() {
        let (_dir, db, credential_id) = database().await;
        let original = revision(&db).await;
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(0, true), &original)
                .await
                .expect("zero counter update"),
            CredentialUpdate::Applied
        );
        assert_eq!(stored_state(&db).await, (0, true));
    }

    #[tokio::test]
    async fn a_zero_counter_assertion_keeps_the_stored_counter() {
        let (_dir, db, credential_id) = database().await;
        let original = revision(&db).await;
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(7, false), &original)
                .await
                .expect("counted update"),
            CredentialUpdate::Applied
        );
        let current = revision(&db).await;
        assert_eq!(
            update_credential_passkey(&db, &credential_id, &passkey(7, true), &current)
                .await
                .expect("zero counter update"),
            CredentialUpdate::Applied
        );
        assert_eq!(stored_state(&db).await, (7, true));
    }
}
