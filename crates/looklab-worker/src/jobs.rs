// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_server::ContributionId;
use looklab_upload::BodyId;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

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
    PublishSite,
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
            "publish_site" => Self::PublishSite,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::ConsistencyScore => "consistency_score",
            Self::HonestDuplicateMerge => "honest_duplicate_merge",
            Self::NightlyRefit => "nightly_refit",
            Self::BlobGc => "blob_gc",
            Self::PublishSite => "publish_site",
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
    PublishSite,
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
            Self::PublishSite => JobKind::PublishSite,
        }
    }

    /// The `kind` column and the JSON `payload` column.
    pub fn encode(&self) -> Result<(String, String)> {
        let payload = match self {
            Self::ConsistencyScore { contribution_id } => {
                serde_json::to_string(&ConsistencyWire {
                    contribution_id: contribution_id.as_str().to_owned(),
                })?
            }
            Self::HonestDuplicateMerge | Self::PublishSite => "{}".to_owned(),
            Self::NightlyRefit { bodies } => serde_json::to_string(&NightlyWire {
                bodies: bodies.iter().map(|body| body.as_str().to_owned()).collect(),
            })?,
            Self::BlobGc { contribution_id } => serde_json::to_string(&BlobGcWire {
                contribution_id: contribution_id.as_ref().map(|id| id.as_str().to_owned()),
            })?,
        };
        Ok((self.kind().as_str().to_owned(), payload))
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
            JobKind::PublishSite => Self::PublishSite,
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
}

/// A job whose lease expired is reclaimable.
pub async fn claim(pool: &SqlitePool, lease_seconds: i64, now: i64) -> Result<Option<ClaimedJob>> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "UPDATE jobs SET status = 'running', locked_until = ?, attempts = attempts + 1
         WHERE id = (
             SELECT id FROM jobs
             WHERE (status = 'queued' AND run_after <= ?)
                OR (status = 'running' AND locked_until IS NOT NULL AND locked_until <= ?)
             ORDER BY run_after, created_at, id
             LIMIT 1
         )
         RETURNING id, kind, payload, attempts",
    )
    .bind(now + lease_seconds)
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

pub async fn complete(pool: &SqlitePool, id: &JobId) -> Result<()> {
    sqlx::query("UPDATE jobs SET status = 'done', locked_until = NULL WHERE id = ?")
        .bind(id.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn fail(
    pool: &SqlitePool,
    id: &JobId,
    attempts: i64,
    max_attempts: i64,
    now: i64,
    permanent: bool,
) -> Result<()> {
    if permanent || attempts >= max_attempts {
        sqlx::query("UPDATE jobs SET status = 'failed', locked_until = NULL WHERE id = ?")
            .bind(id.as_str())
            .execute(pool)
            .await?;
        return Ok(());
    }
    let exponent = u32::try_from(attempts.max(1) - 1).unwrap_or(0).min(16);
    let backoff = (30_i64 << exponent).min(3_600);
    sqlx::query(
        "UPDATE jobs SET status = 'queued', locked_until = NULL, run_after = ? WHERE id = ?",
    )
    .bind(now + backoff)
    .bind(id.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn enqueue(pool: &SqlitePool, payload: &JobPayload, run_after: i64) -> Result<JobId> {
    let (kind, encoded) = payload.encode()?;
    insert(pool, &kind, &encoded, run_after).await
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
    .bind(&kind)
    .fetch_one(pool)
    .await?;
    if count > 0 {
        return Ok(None);
    }
    Ok(Some(insert(pool, &kind, &encoded, run_after).await?))
}

async fn insert(pool: &SqlitePool, kind: &str, payload: &str, run_after: i64) -> Result<JobId> {
    let id = JobId::new();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, ?, ?, 'queued', 0, ?, ?)",
    )
    .bind(id.as_str())
    .bind(kind)
    .bind(payload)
    .bind(run_after)
    .bind(now_unix())
    .execute(pool)
    .await?;
    Ok(id)
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
