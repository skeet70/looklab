// SPDX-License-Identifier: AGPL-3.0-or-later
use looklab_upload::Nonce;
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};
use webauthn_rs::prelude::Passkey;

use crate::db::now_unix;
use crate::ids::{
    ContributionId, ContributionImageId, CredentialId, JobId, TokenHash, WithdrawalId,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeNonce {
    Ok,
    Unknown,
    Expired,
    Replayed,
}

#[derive(Debug)]
pub struct StoredCredential {
    pub id: CredentialId,
    pub passkey: Passkey,
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
    let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>, i64, Option<i64>)> = sqlx::query_as(
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

pub async fn update_credential_passkey_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    credential_id: &CredentialId,
    passkey: &Passkey,
) -> Result<(), sqlx::Error> {
    let passkey_json = serde_json::to_string(passkey)
        .map_err(|_| sqlx::Error::Protocol("passkey does not serialize".to_owned()))?;
    sqlx::query("UPDATE credentials SET webauthn_credential = ? WHERE id = ?")
        .bind(passkey_json)
        .bind(credential_id.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn update_credential_passkey(
    db: &SqlitePool,
    credential_id: &CredentialId,
    passkey: &Passkey,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    update_credential_passkey_tx(&mut tx, credential_id, passkey).await?;
    tx.commit().await
}

pub struct NewImage {
    pub samples_hash: [u8; 32],
    pub n_samples: i64,
    pub meta_json: String,
    pub rerender_group: Option<String>,
    /// `(control key, source)` JSON ([docs/03 §1]).
    pub setting_source: String,
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
          status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
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
              status)
             VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
        )
        .bind(id.as_str())
        .bind(contribution.id.as_str())
        .bind(image.samples_hash.as_slice())
        .bind(image.n_samples)
        .bind(&image.meta_json)
        .bind(image.rerender_group.as_deref())
        .bind(&image.setting_source)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
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
    pub created_at: i64,
    pub images: Vec<ImageFeed>,
}

type FeedRow = (
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
);

pub async fn status_feed(
    db: &SqlitePool,
    credential_id: &CredentialId,
) -> Result<Vec<ContributionFeed>, sqlx::Error> {
    let rows: Vec<FeedRow> = sqlx::query_as(
        "SELECT c.id, c.status, c.created_at, i.id, i.status, i.status_reason, i.rms_de
         FROM contributions c
         LEFT JOIN contribution_images i ON i.contribution_id = c.id
         WHERE c.credential_id = ?
         ORDER BY c.created_at DESC, c.id, i.id",
    )
    .bind(credential_id.as_str())
    .fetch_all(db)
    .await?;

    let mut feed: Vec<ContributionFeed> = Vec::new();
    for (contribution_id, status, created_at, image_id, image_status, reason, rms_de) in rows {
        let id = ContributionId::try_from(contribution_id)
            .map_err(|_| sqlx::Error::Protocol("contribution id is not a UUID".to_owned()))?;
        let status = ContributionStatus::from_db(&status)
            .ok_or_else(|| sqlx::Error::Protocol("unknown contribution status".to_owned()))?;
        let entry = match feed.last_mut() {
            Some(entry) if entry.id == id => entry,
            _ => {
                feed.push(ContributionFeed {
                    id,
                    status,
                    created_at,
                    images: Vec::new(),
                });
                feed.last_mut().expect("just pushed")
            }
        };
        if let (Some(image_id), Some(image_status)) = (image_id, image_status) {
            entry.images.push(ImageFeed {
                id: ContributionImageId::try_from(image_id).map_err(|_| {
                    sqlx::Error::Protocol("contribution image id is not a UUID".to_owned())
                })?,
                status: ImageStatus::from_db(&image_status)
                    .ok_or_else(|| sqlx::Error::Protocol("unknown image status".to_owned()))?,
                reason,
                rms_de,
            });
        }
    }
    Ok(feed)
}

pub async fn withdraw(
    db: &SqlitePool,
    contribution_id: &ContributionId,
    credential_id: &CredentialId,
    delete_after: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let mut tx = db.begin().await?;
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
        return Ok(existing.map(|(delete_after,)| delete_after));
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
    tx.commit().await?;
    Ok(Some(delete_after))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
