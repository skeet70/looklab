// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::Duration;

use sqlx::SqlitePool;

use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db;
use crate::error::{Result, WorkerError};
use crate::jobs::{self, ClaimedJob, JobId, JobKind, JobPayload};
use crate::publish::{self, PublishConfig, PublishSummary};
use crate::refit::{self, RefitOutcome};
use crate::{bounties, cards, consistency, duplicates, gc, lut_rebase};

const BLOB_GC_INTERVAL_SECONDS: i64 = 3_600;

pub struct Worker {
    pool: SqlitePool,
    config: WorkerConfig,
}

impl Worker {
    pub async fn open(config: WorkerConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.blobs)?;
        let pool = looklab_server::open_database(&config.db).await?;
        Ok(Self { pool, config })
    }

    pub fn config(&self) -> &WorkerConfig {
        &self.config
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn enqueue(&self, payload: &JobPayload, run_after: i64) -> Result<JobId> {
        jobs::enqueue(&self.pool, payload, run_after).await
    }

    /// Fails without touching the site when `ownership` has moved on.
    pub async fn publish_owned(&self, ownership: &jobs::JobOwnership) -> Result<PublishSummary> {
        let config = PublishConfig::from_worker(&self.config);
        let lock = config.lock_output_async().await?;
        // The lock precedes the check so a claim that waited behind a newer publisher writes nothing.
        ownership.check(&self.pool).await?;
        publish::publish_with_pool_locked(&self.pool, &config, &lock).await
    }

    /// A failed job is marked before the error returns.
    pub async fn run_once(&self) -> Result<Option<JobId>> {
        let now = now_unix();
        let _ = jobs::enqueue_pending_publication(&self.pool, now).await?;
        let Some(job) = jobs::claim(&self.pool, self.config.lease_seconds, now).await? else {
            return Ok(None);
        };
        // Read before the snapshot: a request committed during publication then
        // makes the completion enqueue a successor.
        let observed = if job.kind == JobKind::PublishSite {
            Some(jobs::publication_generation(&self.pool).await?)
        } else {
            None
        };
        let renewal = self.spawn_renewal(&job);
        let executed = self.execute(&job).await;
        renewal.abort();
        match executed {
            Ok(()) => {
                if let Some(observed) = observed {
                    jobs::complete_publish(&self.pool, &job.id, job.attempts, observed, now)
                        .await?;
                } else {
                    jobs::complete(&self.pool, &job.id, job.attempts).await?;
                }
                Ok(Some(job.id))
            }
            Err(error) => {
                match observed {
                    Some(observed) => {
                        jobs::fail_publish(
                            &self.pool,
                            &job.id,
                            job.attempts,
                            observed,
                            self.config.max_attempts,
                            now,
                            error.permanent(),
                        )
                        .await?;
                    }
                    None => {
                        jobs::fail(
                            &self.pool,
                            &job.id,
                            job.attempts,
                            self.config.max_attempts,
                            now,
                            error.permanent(),
                        )
                        .await?;
                    }
                }
                Err(error)
            }
        }
    }

    fn spawn_renewal(&self, job: &ClaimedJob) -> tokio::task::JoinHandle<()> {
        let pool = self.pool.clone();
        let id = job.id.clone();
        let attempts = job.attempts;
        let lease_seconds = self.config.lease_seconds;
        tokio::spawn(async move {
            let period = (lease_seconds / 3).max(1);
            loop {
                tokio::time::sleep(Duration::from_secs(u64::try_from(period).unwrap_or(1))).await;
                match jobs::renew(&pool, &id, attempts, lease_seconds, now_unix()).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!(job = %id, "lease lost; this attempt must not commit");
                        break;
                    }
                    Err(error) => tracing::warn!(%error, job = %id, "lease renewal failed"),
                }
            }
        })
    }

    pub async fn run_until_idle(&self) -> Result<usize> {
        self.run_until_idle_bounded(usize::MAX, Duration::MAX).await
    }

    /// Fails once `max_jobs` have run or `deadline` passes; the clock is read between jobs.
    pub async fn run_until_idle_bounded(
        &self,
        max_jobs: usize,
        deadline: Duration,
    ) -> Result<usize> {
        let started = std::time::Instant::now();
        let mut completed = 0;
        while self.run_once().await?.is_some() {
            completed += 1;
            if completed >= max_jobs {
                return Err(WorkerError::Job(format!(
                    "drained {completed} jobs without reaching idle; a job is re-enqueuing itself"
                )));
            }
            if started.elapsed() > deadline {
                return Err(WorkerError::Job(format!(
                    "drained {completed} jobs in {:?} without reaching idle",
                    started.elapsed()
                )));
            }
        }
        Ok(completed)
    }

    pub async fn run_until(&self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => return Ok(()),
                () = std::future::ready(()) => {}
            }
            let now = now_unix();
            if jobs::nightly_due(&self.pool, now, self.config.nightly_interval).await? {
                jobs::enqueue(
                    &self.pool,
                    &JobPayload::NightlyRefit { bodies: Vec::new() },
                    now,
                )
                .await?;
            }
            if jobs::blob_gc_due(&self.pool, now, BLOB_GC_INTERVAL_SECONDS).await? {
                jobs::enqueue_if_idle(
                    &self.pool,
                    &JobPayload::BlobGc {
                        contribution_id: None,
                    },
                    now,
                )
                .await?;
            }
            match self.run_once().await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    tokio::select! {
                        () = &mut shutdown => return Ok(()),
                        () = tokio::time::sleep(Duration::from_secs(self.config.poll_seconds)) => {}
                    }
                }
                Err(error) => tracing::warn!(%error, "job failed"),
            }
        }
    }

    pub async fn run(&self) -> Result<()> {
        self.run_until(std::future::pending()).await
    }

    async fn refit_bodies(
        &self,
        bodies: &[looklab_upload::BodyId],
        ownership: &jobs::JobOwnership,
    ) -> Result<()> {
        let outcomes = refit::run(&self.pool, &self.config, bodies, ownership).await?;
        self.finish_refit(&outcomes).await
    }

    async fn finish_refit(
        &self,
        outcomes: &[(
            looklab_upload::BodyId,
            looklab_upload::LookSlug,
            RefitOutcome,
        )],
    ) -> Result<()> {
        if !outcomes.is_empty() {
            jobs::enqueue_if_idle(&self.pool, &JobPayload::Bounties, now_unix()).await?;
            jobs::enqueue_if_idle(&self.pool, &JobPayload::CardVerification, now_unix()).await?;
        }
        if outcomes
            .iter()
            .any(|(_, _, outcome)| matches!(outcome, RefitOutcome::Published(_)))
        {
            let _ = jobs::request_publish(&self.pool, now_unix()).await?;
        }
        Ok(())
    }

    async fn execute(&self, job: &ClaimedJob) -> Result<()> {
        match job.payload()? {
            JobPayload::ConsistencyScore { contribution_id } => {
                let contribution = db::contribution(&self.pool, &contribution_id).await?;
                if contribution.intent == db::ContributionIntent::Card {
                    cards::validate(&self.pool, &self.config, &contribution_id).await
                } else {
                    consistency::run(&self.pool, &self.config, &contribution_id).await
                }
            }
            JobPayload::HonestDuplicateMerge => duplicates::run(&self.pool).await,
            JobPayload::NightlyRefit { bodies } => {
                self.refit_bodies(&bodies, &job.ownership()).await
            }
            JobPayload::RevertRefit => {
                let outcomes =
                    refit::run_forced(&self.pool, &self.config, &job.ownership()).await?;
                self.finish_refit(&outcomes).await
            }
            JobPayload::BlobGc { contribution_id } => {
                gc::run(&self.pool, &self.config, contribution_id.as_ref()).await
            }
            JobPayload::Bounties => bounties::run(&self.pool, &self.config).await,
            JobPayload::CardVerification => cards::run(&self.pool, &self.config).await,
            JobPayload::LutRebase => {
                lut_rebase::run(&self.pool, &self.config, &job.ownership()).await
            }
            JobPayload::PublishSite => {
                self.publish_owned(&job.ownership()).await?;
                Ok(())
            }
        }
    }
}

impl core::fmt::Debug for Worker {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Worker")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
