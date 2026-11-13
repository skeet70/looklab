// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_schema::{
    DependencyGeneration, DomainId, ModelSubject, ModelVersionId, PendingGeneration,
    QualificationCapability, RuleRevision, Sha256Hex, SourceRef,
};
use looklab_server::{ContributionId, ProposalId};
use looklab_upload::BodyId;
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqliteConnection, SqlitePool};

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

/// The subset of job kinds one worker process claims.
///
/// A publication must not wait for a fit, so a host can run one process per
/// lane. `All` keeps the single-process behaviour and also claims kinds this
/// build does not know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Lane {
    #[default]
    All,
    Publish,
    Fit,
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
    LutImport,
    RevertRefit,
    ModelAssessment,
    Other(String),
}

/// Every stored `kind` string and the kind it parses to.
///
/// One table feeds [`JobKind::parse`] and [`Lane::kinds`], so the decoder and
/// the SQL filter cannot drift. `hard_delete_blobs` is the legacy name the
/// server writes when a withdrawal is recorded.
const JOB_KINDS: &[(&str, JobKind)] = &[
    ("consistency_score", JobKind::ConsistencyScore),
    ("honest_duplicate_merge", JobKind::HonestDuplicateMerge),
    ("nightly_refit", JobKind::NightlyRefit),
    ("blob_gc", JobKind::BlobGc),
    ("hard_delete_blobs", JobKind::BlobGc),
    ("bounties", JobKind::Bounties),
    ("card_verification", JobKind::CardVerification),
    ("publish_site", JobKind::PublishSite),
    ("lut_rebase", JobKind::LutRebase),
    ("lut_import", JobKind::LutImport),
    ("revert_refit", JobKind::RevertRefit),
    ("model_assessment", JobKind::ModelAssessment),
];

impl Lane {
    /// The `kind` values this lane claims, or `None` for every value.
    fn kinds(self) -> Option<impl Iterator<Item = &'static str>> {
        match self {
            Self::All => None,
            lane => Some(
                JOB_KINDS
                    .iter()
                    .filter(move |(_, kind)| kind.lane() == Some(lane))
                    .map(|(name, _)| *name),
            ),
        }
    }

    /// Whether this lane claims `kind`; `All` claims every kind.
    pub fn includes(self, kind: &JobKind) -> bool {
        match self {
            Self::All => true,
            lane => kind.lane() == Some(lane),
        }
    }
}

impl JobKind {
    /// The lane that owns this kind, or `None` for an unknown stored string.
    ///
    /// Every variant is listed so a new kind must be assigned before it
    /// compiles. Publication lives in `Publish`; fits, long numerical work and
    /// jobs that must stay serial with their enqueuers live in `Fit`.
    pub fn lane(&self) -> Option<Lane> {
        match self {
            // Writes or validates the site tree; no colour fit.
            Self::PublishSite => Some(Lane::Publish),
            // Deletes blobs; no fit.
            Self::BlobGc => Some(Lane::Publish),
            // Re-scores accepted card images without fitting, but shares the
            // fit lane with the code that enqueues it so a run cannot miss a
            // card accepted while it is queued.
            Self::CardVerification => Some(Lane::Fit),
            // Merges duplicate images by capture tuple; no fit.
            Self::HonestDuplicateMerge => Some(Lane::Publish),
            // Runs the consistency fitter.
            Self::ConsistencyScore => Some(Lane::Fit),
            // Runs hierarchical refits.
            Self::NightlyRefit | Self::RevertRefit => Some(Lane::Fit),
            // Scores every sample pool for a target; long numerical work.
            Self::Bounties => Some(Lane::Fit),
            // Evaluates fitted probes and can queue a refit.
            Self::ModelAssessment => Some(Lane::Fit),
            // Rebuilds a LUT approximation with `import_lut_look`.
            Self::LutRebase | Self::LutImport => Some(Lane::Fit),
            Self::Other(_) => None,
        }
    }

    pub fn parse(value: &str) -> Self {
        JOB_KINDS
            .iter()
            .find_map(|(name, kind)| (*name == value).then(|| kind.clone()))
            .unwrap_or_else(|| Self::Other(value.to_owned()))
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
            Self::LutImport => "lut_import",
            Self::RevertRefit => "revert_refit",
            Self::ModelAssessment => "model_assessment",
            Self::Other(value) => value,
        }
    }
}

/// The fitter, evaluator and model identities an assessment job was queued under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentSources {
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
}

/// A versioned qualification assessment over exact candidate and dependency pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssessmentJob {
    Camera {
        subject: ModelSubject,
        candidate: ModelVersionId,
        capability: QualificationCapability,
        domain: DomainId,
        dependencies: Vec<DependencyGeneration>,
        pending: PendingGeneration,
        seeds: Vec<u64>,
        #[serde(default)]
        reassess_selected: bool,
        sources: AssessmentSources,
    },
    Engine {
        engine: SourceRef,
        rule_revision: RuleRevision,
        reference: Option<Sha256Hex>,
        dependencies: Vec<DependencyGeneration>,
        sources: AssessmentSources,
    },
}

impl AssessmentJob {
    pub fn sources(&self) -> &AssessmentSources {
        match self {
            Self::Camera { sources, .. } | Self::Engine { sources, .. } => sources,
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
    LutImport {
        proposal_id: ProposalId,
    },
    RevertRefit,
    ModelAssessment {
        request: Box<AssessmentJob>,
    },
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

#[derive(Serialize, Deserialize)]
struct LutImportWire {
    proposal_id: String,
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
            Self::LutImport { .. } => JobKind::LutImport,
            Self::RevertRefit => JobKind::RevertRefit,
            Self::ModelAssessment { .. } => JobKind::ModelAssessment,
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
            Self::LutImport { proposal_id } => serde_json::to_string(&LutImportWire {
                proposal_id: proposal_id.as_str().to_owned(),
            })?,
            Self::ModelAssessment { request } => serde_json::to_string(request)?,
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
            JobKind::LutImport => {
                let wire: LutImportWire = serde_json::from_str(payload)?;
                let proposal_id = ProposalId::try_from(wire.proposal_id)
                    .map_err(|_| WorkerError::Job("invalid proposal id".to_owned()))?;
                Self::LutImport { proposal_id }
            }
            JobKind::RevertRefit => Self::RevertRefit,
            JobKind::ModelAssessment => Self::ModelAssessment {
                request: Box::new(serde_json::from_str(payload)?),
            },
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
    pub fn server_id(&self) -> looklab_server::JobId {
        looklab_server::JobId::try_from(self.id.as_str()).expect("worker job ids are UUIDs")
    }

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
/// `publish_site` claim, so output writers never overlap. A lane claims only
/// its own kinds, so it never reclaims another lane's expired lease.
pub async fn claim(
    pool: &SqlitePool,
    lease_seconds: i64,
    now: i64,
    lane: Lane,
) -> Result<Option<ClaimedJob>> {
    let mut query =
        sqlx::QueryBuilder::<Sqlite>::new("UPDATE jobs SET status = 'running', locked_until = ");
    query.push_bind(now + lease_seconds);
    query.push(
        ", attempts = attempts + 1
         WHERE id = (
             SELECT candidate.id FROM jobs candidate
             WHERE ((candidate.status = 'queued' AND candidate.run_after <= ",
    );
    query.push_bind(now);
    query.push(
        ") OR (candidate.status = 'running' AND candidate.locked_until IS NOT NULL
                     AND candidate.locked_until <= ",
    );
    query.push_bind(now);
    query.push(
        "))
               AND (candidate.kind <> 'publish_site'
                 OR NOT EXISTS (
                     SELECT 1 FROM jobs writer
                     WHERE writer.kind = 'publish_site' AND writer.status = 'running'
                       AND writer.id <> candidate.id
                       AND (writer.locked_until IS NULL OR writer.locked_until > ",
    );
    query.push_bind(now);
    query.push(")))");
    if let Some(kinds) = lane.kinds() {
        query.push(" AND candidate.kind IN (");
        let mut separated = query.separated(", ");
        for kind in kinds {
            separated.push_bind(kind);
        }
        separated.push_unseparated(")");
    }
    query.push(
        "
             ORDER BY candidate.run_after, candidate.created_at, candidate.id
             LIMIT 1
         )
         RETURNING id, kind, payload, attempts",
    );
    let row: Option<(String, String, String, i64)> =
        query.build_query_as().fetch_optional(pool).await?;
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

pub(crate) async fn insert(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lut_import_job_round_trips_its_proposal() {
        let payload = JobPayload::LutImport {
            proposal_id: ProposalId::new(),
        };
        let (kind, encoded) = payload.encode().expect("encode");
        assert_eq!(kind, JobKind::LutImport);
        assert_eq!(
            JobPayload::decode(&kind, &encoded).expect("decode"),
            payload
        );
    }

    #[test]
    fn every_known_kind_has_one_lane() {
        let publish: Vec<_> = Lane::Publish.kinds().expect("publish kinds").collect();
        let fit: Vec<_> = Lane::Fit.kinds().expect("fit kinds").collect();
        assert!(publish.contains(&"hard_delete_blobs"));
        assert!(fit.contains(&"bounties"));
        assert!(fit.contains(&"card_verification"));
        assert!(!publish.contains(&"card_verification"));
        assert!(
            publish
                .iter()
                .all(|name| !fit.contains(name)
                    && JobKind::parse(name).lane() == Some(Lane::Publish))
        );
        assert!(
            fit.iter()
                .all(|name| JobKind::parse(name).lane() == Some(Lane::Fit))
        );
        assert_eq!(JobKind::Other("future_kind".to_owned()).lane(), None);
        assert_eq!(
            JobKind::parse("future_kind").lane(),
            None,
            "an unknown stored kind belongs to no lane"
        );
        assert!(Lane::All.kinds().is_none());
    }

    #[test]
    fn lanes_include_only_the_kinds_they_schedule() {
        assert!(Lane::All.includes(&JobKind::NightlyRefit));
        assert!(Lane::All.includes(&JobKind::BlobGc));
        assert!(Lane::All.includes(&JobKind::PublishSite));
        assert!(Lane::Fit.includes(&JobKind::NightlyRefit));
        assert!(!Lane::Publish.includes(&JobKind::NightlyRefit));
        assert!(Lane::Publish.includes(&JobKind::BlobGc));
        assert!(!Lane::Fit.includes(&JobKind::BlobGc));
        assert!(Lane::Publish.includes(&JobKind::PublishSite));
        assert!(!Lane::Fit.includes(&JobKind::PublishSite));
    }

    async fn claim_fixture() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().expect("temp dir");
        let pool = looklab_server::open_database(&dir.path().join("db.sqlite"))
            .await
            .expect("database");
        (dir, pool)
    }

    #[tokio::test]
    async fn a_publish_lane_claim_skips_an_earlier_queued_fit_job() {
        let (_dir, pool) = claim_fixture().await;
        let fit = enqueue(
            &pool,
            &JobPayload::ConsistencyScore {
                contribution_id: ContributionId::new(),
            },
            0,
        )
        .await
        .expect("enqueue fit");
        let publish = enqueue(&pool, &JobPayload::PublishSite, 1)
            .await
            .expect("enqueue publish");

        let claimed = claim(&pool, 60, 100, Lane::Publish)
            .await
            .expect("claim")
            .expect("queued publisher");
        assert_eq!(claimed.id, publish);
        assert_eq!(claimed.kind, JobKind::PublishSite);
        let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
            .bind(fit.as_str())
            .fetch_one(&pool)
            .await
            .expect("fit status");
        assert_eq!(status, "queued");

        let next = claim(&pool, 60, 100, Lane::All)
            .await
            .expect("claim")
            .expect("queued fit job");
        assert_eq!(next.id, fit);
        assert_eq!(next.kind, JobKind::ConsistencyScore);
    }

    #[tokio::test]
    async fn a_fit_lane_claim_skips_publish_site() {
        let (_dir, pool) = claim_fixture().await;
        let publish = enqueue(&pool, &JobPayload::PublishSite, 0)
            .await
            .expect("enqueue publish");
        let fit = enqueue(&pool, &JobPayload::NightlyRefit { bodies: vec![] }, 1)
            .await
            .expect("enqueue fit");

        let claimed = claim(&pool, 60, 100, Lane::Fit)
            .await
            .expect("claim")
            .expect("queued fit job");
        assert_eq!(claimed.id, fit);
        assert_eq!(claimed.kind, JobKind::NightlyRefit);
        let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
            .bind(publish.as_str())
            .fetch_one(&pool)
            .await
            .expect("publish status");
        assert_eq!(status, "queued");

        let next = claim(&pool, 60, 100, Lane::All)
            .await
            .expect("claim")
            .expect("queued publisher");
        assert_eq!(next.id, publish);
        assert_eq!(next.kind, JobKind::PublishSite);
    }

    #[tokio::test]
    async fn the_all_lane_claims_both_kinds_in_queue_order() {
        let (_dir, pool) = claim_fixture().await;
        let fit = enqueue(
            &pool,
            &JobPayload::ConsistencyScore {
                contribution_id: ContributionId::new(),
            },
            0,
        )
        .await
        .expect("enqueue fit");
        let publish = enqueue(&pool, &JobPayload::PublishSite, 1)
            .await
            .expect("enqueue publish");

        let first = claim(&pool, 60, 100, Lane::All)
            .await
            .expect("claim")
            .expect("queued fit job");
        assert_eq!(first.id, fit);
        assert_eq!(first.kind, JobKind::ConsistencyScore);

        let second = claim(&pool, 60, 100, Lane::All)
            .await
            .expect("claim")
            .expect("queued publisher");
        assert_eq!(second.id, publish);
        assert_eq!(second.kind, JobKind::PublishSite);
    }

    #[tokio::test]
    async fn the_publish_lane_claims_the_legacy_hard_delete_name() {
        let (_dir, pool) = claim_fixture().await;
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES ('legacy', 'hard_delete_blobs', '{}', 'queued', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("insert legacy kind");

        let claimed = claim(&pool, 60, 100, Lane::Publish)
            .await
            .expect("claim")
            .expect("queued blob sweep");
        assert_eq!(claimed.id.as_str(), "legacy");
        assert_eq!(claimed.kind, JobKind::BlobGc);
        assert_eq!(claimed.kind.as_str(), "blob_gc");
    }

    #[tokio::test]
    async fn a_lane_never_reclaims_another_lanes_expired_lease() {
        let (_dir, pool) = claim_fixture().await;
        let fit = enqueue(
            &pool,
            &JobPayload::ConsistencyScore {
                contribution_id: ContributionId::new(),
            },
            0,
        )
        .await
        .expect("enqueue fit");
        let first = claim(&pool, 10, 100, Lane::Fit)
            .await
            .expect("claim")
            .expect("fit job");
        assert_eq!(first.id, fit);
        let publish = enqueue(&pool, &JobPayload::PublishSite, 1)
            .await
            .expect("enqueue publish");

        let claimed = claim(&pool, 60, 200, Lane::Publish)
            .await
            .expect("claim")
            .expect("queued publisher");
        assert_eq!(claimed.id, publish);
        let attempts: i64 = sqlx::query_scalar("SELECT attempts FROM jobs WHERE id = ?")
            .bind(fit.as_str())
            .fetch_one(&pool)
            .await
            .expect("fit attempts");
        assert_eq!(attempts, 1, "the publish lane left the expired fit lease");

        let reclaimed = claim(&pool, 60, 200, Lane::All)
            .await
            .expect("claim")
            .expect("expired fit lease");
        assert_eq!(reclaimed.id, fit);
        assert_eq!(reclaimed.attempts, 2);
    }

    #[tokio::test]
    async fn only_the_all_lane_claims_an_unknown_kind() {
        let (_dir, pool) = claim_fixture().await;
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES ('future', 'future_kind', '{}', 'queued', 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .expect("insert unknown kind");

        assert!(
            claim(&pool, 60, 100, Lane::Publish)
                .await
                .expect("claim")
                .is_none()
        );
        assert!(
            claim(&pool, 60, 100, Lane::Fit)
                .await
                .expect("claim")
                .is_none()
        );
        let claimed = claim(&pool, 60, 100, Lane::All)
            .await
            .expect("claim")
            .expect("unknown kind");
        assert_eq!(claimed.kind, JobKind::Other("future_kind".to_owned()));
    }
}
