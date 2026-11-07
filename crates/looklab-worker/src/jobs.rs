// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_server::ContributionId;
use looklab_upload::BodyId;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};

use crate::clock::now_unix;
use crate::error::{Result, WorkerError};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct JobId(String);

impl JobId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKind {
    ConsistencyScore,
    HonestDuplicateMerge,
    NightlyRefit,
    BlobGc,
    Bounties,
    CardVerification,
    PublishSite,
    LutRebase,
    RevertRefit,
    Other(String),
}

impl JobKind {
    pub fn parse(value: &str) -> Self {
        match value {
            "consistency_score" => Self::ConsistencyScore,
            "honest_duplicate_merge" => Self::HonestDuplicateMerge,
            "nightly_refit" => Self::NightlyRefit,
            // The server enqueues this name when a withdrawal is recorded.
            "blob_gc" | "hard_delete_blobs" => Self::BlobGc,
            "bounties" => Self::Bounties,
            "card_verification" => Self::CardVerification,
            "publish_site" => Self::PublishSite,
            "lut_rebase" => Self::LutRebase,
            "revert_refit" => Self::RevertRefit,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::ConsistencyScore => "consistency_score",
            Self::HonestDuplicateMerge => "honest_duplicate_merge",
            Self::NightlyRefit => "nightly_refit",
            Self::BlobGc => "blob_gc",
            Self::Bounties => "bounties",
            Self::CardVerification => "card_verification",
            Self::PublishSite => "publish_site",
            Self::LutRebase => "lut_rebase",
            Self::RevertRefit => "revert_refit",
            Self::Other(value) => value,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPayload {
    ConsistencyScore {
        contribution_id: ContributionId,
    },
    HonestDuplicateMerge,
    NightlyRefit {
        bodies: Vec<BodyId>,
    },
    BlobGc {
        contribution_id: Option<ContributionId>,
    },
    Bounties,
    CardVerification,
    PublishSite,
    LutRebase,
    RevertRefit,
}

#[derive(Serialize, Deserialize)]
struct ConsistencyWire {
    contribution_id: String,
}

#[derive(Serialize, Deserialize)]
struct NightlyWire {
    #[serde(default)]
    bodies: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct BlobGcWire {
    #[serde(default)]
    contribution_id: Option<String>,
}

impl JobPayload {
    pub fn kind(&self) -> JobKind {
        match self {
            Self::ConsistencyScore { .. } => JobKind::ConsistencyScore,
            Self::HonestDuplicateMerge => JobKind::HonestDuplicateMerge,
            Self::NightlyRefit { .. } => JobKind::NightlyRefit,
            Self::BlobGc { .. } => JobKind::BlobGc,
            Self::Bounties => JobKind::Bounties,
            Self::CardVerification => JobKind::CardVerification,
            Self::PublishSite => JobKind::PublishSite,
            Self::LutRebase => JobKind::LutRebase,
            Self::RevertRefit => JobKind::RevertRefit,
        }
    }

    /// The `kind` column and the JSON `payload` column.
    pub fn encode(&self) -> Result<(JobKind, String)> {
        let payload = match self {
            Self::ConsistencyScore { contribution_id } => {
                serde_json::to_string(&ConsistencyWire {
                    contribution_id: contribution_id.as_str().to_owned(),
                })?
            }
            Self::HonestDuplicateMerge
            | Self::Bounties
            | Self::CardVerification
            | Self::PublishSite
            | Self::LutRebase
            | Self::RevertRefit => "{}".to_owned(),
            Self::NightlyRefit { bodies } => serde_json::to_string(&NightlyWire {
                bodies: bodies.iter().map(|body| body.as_str().to_owned()).collect(),
            })?,
            Self::BlobGc { contribution_id } => serde_json::to_string(&BlobGcWire {
                contribution_id: contribution_id.as_ref().map(|id| id.as_str().to_owned()),
            })?,
        };
        Ok((self.kind(), payload))
    }

    pub fn decode(kind: &JobKind, payload: &str) -> Result<Self> {
        Ok(match kind {
            JobKind::ConsistencyScore => {
                let wire: ConsistencyWire = serde_json::from_str(payload)?;
                let contribution_id = ContributionId::try_from(wire.contribution_id)
                    .map_err(|_| WorkerError::Job("invalid contribution id".to_owned()))?;
                Self::ConsistencyScore { contribution_id }
            }
            JobKind::HonestDuplicateMerge => Self::HonestDuplicateMerge,
            JobKind::NightlyRefit => {
                let wire: NightlyWire = serde_json::from_str(payload)?;
                let bodies = wire
                    .bodies
                    .iter()
                    .map(|body| {
                        BodyId::new(body)
                            .map_err(|_| WorkerError::Job(format!("invalid body id `{body}`")))
                    })
                    .collect::<Result<Vec<_>>>()?;
                Self::NightlyRefit { bodies }
            }
            JobKind::BlobGc => {
                let wire: BlobGcWire = serde_json::from_str(payload)?;
                let contribution_id = wire
                    .contribution_id
                    .map(|id| {
                        ContributionId::try_from(id)
                            .map_err(|_| WorkerError::Job("invalid contribution id".to_owned()))
                    })
                    .transpose()?;
                Self::BlobGc { contribution_id }
            }
            JobKind::Bounties => Self::Bounties,
            JobKind::CardVerification => Self::CardVerification,
            JobKind::PublishSite => Self::PublishSite,
            JobKind::LutRebase => Self::LutRebase,
            JobKind::RevertRefit => Self::RevertRefit,
            JobKind::Other(kind) => {
                return Err(WorkerError::Job(format!("unknown job kind `{kind}`")));
            }
        })
    }
}

#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: JobId,
    pub kind: JobKind,
    pub payload_text: String,
    /// Already incremented by this claim.
    pub attempts: i64,
}

impl ClaimedJob {
    pub fn payload(&self) -> Result<JobPayload> {
        JobPayload::decode(&self.kind, &self.payload_text)
    }

    pub fn ownership(&self) -> JobOwnership {
        JobOwnership {
            id: self.id.clone(),
            attempts: self.attempts,
        }
    }
}

/// Fences one execution: `claim` increments `attempts`, so every execution has a distinct pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOwnership {
    pub id: JobId,
    pub attempts: i64,
}

impl JobOwnership {
    /// Fails when the lease expired and another attempt now owns the job.
    pub async fn check(&self, pool: &SqlitePool) -> Result<()> {
        if owns(pool, &self.id, self.attempts).await? {
            Ok(())
        } else {
            Err(WorkerError::Job(format!(
                "job {} is no longer owned by attempt {}",
                self.id, self.attempts
            )))
        }
    }
}

/// The publication generation a publisher read before taking its snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublicationGeneration(i64);

/// A job whose lease expired is reclaimable. A live publisher blocks another
/// `publish_site` claim, so output writers never overlap.
pub async fn claim(pool: &SqlitePool, lease_seconds: i64, now: i64) -> Result<Option<ClaimedJob>> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "UPDATE jobs SET status = 'running', locked_until = ?, attempts = attempts + 1
         WHERE id = (
             SELECT candidate.id FROM jobs candidate
             WHERE ((candidate.status = 'queued' AND candidate.run_after <= ?)
                 OR (candidate.status = 'running' AND candidate.locked_until IS NOT NULL
                     AND candidate.locked_until <= ?))
               AND (candidate.kind <> 'publish_site'
                 OR NOT EXISTS (
                     SELECT 1 FROM jobs writer
                     WHERE writer.kind = 'publish_site' AND writer.status = 'running'
                       AND writer.id <> candidate.id
                       AND (writer.locked_until IS NULL OR writer.locked_until > ?)))
             ORDER BY candidate.run_after, candidate.created_at, candidate.id
             LIMIT 1
         )
         RETURNING id, kind, payload, attempts",
    )
    .bind(now + lease_seconds)
    .bind(now)
    .bind(now)
    .bind(now)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, kind, payload, attempts)| ClaimedJob {
        id: JobId(id),
        kind: JobKind::parse(&kind),
        payload_text: payload,
        attempts,
    }))
}

/// Extends the lease of the attempt that still owns the job.
pub async fn renew(
    pool: &SqlitePool,
    id: &JobId,
    attempts: i64,
    lease_seconds: i64,
    now: i64,
) -> Result<bool> {
    let updated = sqlx::query(
        "UPDATE jobs SET locked_until = ?
         WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(now + lease_seconds)
    .bind(id.as_str())
    .bind(attempts)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

pub async fn owns(pool: &SqlitePool, id: &JobId, attempts: i64) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(id.as_str())
    .bind(attempts)
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

pub async fn complete(pool: &SqlitePool, id: &JobId, attempts: i64) -> Result<bool> {
    let updated = sqlx::query(
        "UPDATE jobs SET status = 'done', locked_until = NULL
         WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(id.as_str())
    .bind(attempts)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

pub async fn fail(
    pool: &SqlitePool,
    id: &JobId,
    attempts: i64,
    max_attempts: i64,
    now: i64,
    permanent: bool,
) -> Result<bool> {
    let mut conn = pool.acquire().await?;
    mark_failed(&mut conn, id, attempts, max_attempts, now, permanent).await
}

/// Marks a publisher failed and records a terminal attempt, so only a new request retries it.
pub async fn fail_publish(
    pool: &SqlitePool,
    id: &JobId,
    attempts: i64,
    observed: PublicationGeneration,
    max_attempts: i64,
    now: i64,
    permanent: bool,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let updated = mark_failed(&mut tx, id, attempts, max_attempts, now, permanent).await?;
    if updated && (permanent || attempts >= max_attempts) {
        sqlx::query("UPDATE publication SET attempted = MAX(attempted, ?) WHERE id = 1")
            .bind(observed.0)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(updated)
}

async fn mark_failed(
    conn: &mut SqliteConnection,
    id: &JobId,
    attempts: i64,
    max_attempts: i64,
    now: i64,
    permanent: bool,
) -> Result<bool> {
    let exponent = u32::try_from(attempts.max(1) - 1).unwrap_or(0).min(16);
    let backoff = (30_i64 << exponent).min(3_600);
    let (status, run_after) = if permanent || attempts >= max_attempts {
        ("failed", now)
    } else {
        ("queued", now + backoff)
    };
    let updated = sqlx::query(
        "UPDATE jobs SET status = ?, locked_until = NULL, run_after = ?
         WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(status)
    .bind(run_after)
    .bind(id.as_str())
    .bind(attempts)
    .execute(conn)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

pub async fn enqueue(pool: &SqlitePool, payload: &JobPayload, run_after: i64) -> Result<JobId> {
    let (kind, encoded) = payload.encode()?;
    let mut conn = pool.acquire().await?;
    insert(&mut conn, &kind, &encoded, run_after).await
}

/// Enqueues unless one is already queued or running.
pub async fn enqueue_if_idle(
    pool: &SqlitePool,
    payload: &JobPayload,
    run_after: i64,
) -> Result<Option<JobId>> {
    let (kind, encoded) = payload.encode()?;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND status IN ('queued', 'running')",
    )
    .bind(kind.as_str())
    .fetch_one(pool)
    .await?;
    if count > 0 {
        return Ok(None);
    }
    let mut conn = pool.acquire().await?;
    Ok(Some(insert(&mut conn, &kind, &encoded, run_after).await?))
}

async fn insert(
    conn: &mut SqliteConnection,
    kind: &JobKind,
    payload: &str,
    run_after: i64,
) -> Result<JobId> {
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

pub async fn publication_generation(pool: &SqlitePool) -> Result<PublicationGeneration> {
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM publication WHERE id = 1")
        .fetch_one(pool)
        .await?;
    Ok(PublicationGeneration(generation))
}

/// Persists the request even when a publisher is queued or running, so a
/// listing change committed after that publisher's snapshot is not lost.
pub async fn request_publish(pool: &SqlitePool, now: i64) -> Result<Option<JobId>> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE publication SET generation = generation + 1 WHERE id = 1")
        .execute(&mut *tx)
        .await?;
    let id = enqueue_if_no_publication(&mut tx, now).await?;
    tx.commit().await?;
    Ok(id)
}

/// Enqueues the successor a later request needs in the transaction that releases the job.
pub async fn complete_publish(
    pool: &SqlitePool,
    id: &JobId,
    attempts: i64,
    observed: PublicationGeneration,
    now: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE jobs SET status = 'done', locked_until = NULL
         WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(id.as_str())
    .bind(attempts)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE publication SET published = MAX(published, ?) WHERE id = 1")
        .bind(observed.0)
        .execute(&mut *tx)
        .await?;
    let generation: i64 = sqlx::query_scalar("SELECT generation FROM publication WHERE id = 1")
        .fetch_one(&mut *tx)
        .await?;
    if generation > observed.0 {
        enqueue_if_no_publication(&mut tx, now).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Enqueues a publisher for a request that advanced past every settled attempt.
pub async fn enqueue_pending_publication(pool: &SqlitePool, now: i64) -> Result<Option<JobId>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM publication
         WHERE generation > published AND generation > attempted",
    )
    .fetch_one(&mut *tx)
    .await?;
    if pending == 0 {
        return Ok(None);
    }
    let id = enqueue_if_no_publication(&mut tx, now).await?;
    tx.commit().await?;
    Ok(id)
}

async fn enqueue_if_no_publication(conn: &mut SqliteConnection, now: i64) -> Result<Option<JobId>> {
    let (kind, payload) = JobPayload::PublishSite.encode()?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND status IN ('queued', 'running')",
    )
    .bind(kind.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if pending > 0 {
        return Ok(None);
    }
    Ok(Some(insert(conn, &kind, &payload, now).await?))
}

/// Whether no refit is queued or running and the last one is older than `interval`.
pub async fn nightly_due(pool: &SqlitePool, now: i64, interval: i64) -> Result<bool> {
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(created_at) FROM jobs
         WHERE kind = 'nightly_refit'
           AND status IN ('queued', 'running', 'done', 'failed')",
    )
    .fetch_one(pool)
    .await?;
    Ok(last.is_none_or(|last| now - last >= interval))
}

/// Whether no blob sweep is queued or running and the last one is older than `interval`.
pub async fn blob_gc_due(pool: &SqlitePool, now: i64, interval: i64) -> Result<bool> {
    let last: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(created_at) FROM jobs
         WHERE kind = 'blob_gc'
           AND status IN ('queued', 'running', 'done', 'failed')",
    )
    .fetch_one(pool)
    .await?;
    Ok(last.is_none_or(|last| now - last >= interval))
}
