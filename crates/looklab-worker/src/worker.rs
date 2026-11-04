// SPDX-License-Identifier: AGPL-3.0-or-later

use std::time::Duration;

use sqlx::SqlitePool;

use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::error::Result;
use crate::jobs::{self, ClaimedJob, JobId, JobPayload};
use crate::publish::{self, PublishConfig};
use crate::refit::{self, RefitOutcome};
use crate::{consistency, duplicates, gc};

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

    /// A failed job is marked before the error returns.
    pub async fn run_once(&self) -> Result<Option<JobId>> {
        let now = now_unix();
        let Some(job) = jobs::claim(&self.pool, self.config.lease_seconds, now).await? else {
            return Ok(None);
        };
        match self.execute(&job).await {
            Ok(()) => {
                jobs::complete(&self.pool, &job.id).await?;
                Ok(Some(job.id))
            }
            Err(error) => {
                jobs::fail(
                    &self.pool,
                    &job.id,
                    job.attempts,
                    self.config.max_attempts,
                    now,
                    error.permanent(),
                )
                .await?;
                Err(error)
            }
        }
    }

    pub async fn run_until_idle(&self) -> Result<usize> {
        let mut completed = 0;
        while self.run_once().await?.is_some() {
            completed += 1;
        }
        Ok(completed)
    }

    pub async fn run(&self) -> Result<()> {
        loop {
            let now = now_unix();
            if jobs::nightly_due(&self.pool, now, self.config.nightly_interval).await? {
                jobs::enqueue(
                    &self.pool,
                    &JobPayload::NightlyRefit { bodies: Vec::new() },
                    now,
                )
                .await?;
            }
            match self.run_once().await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    tokio::time::sleep(Duration::from_secs(self.config.poll_seconds)).await;
                }
                Err(error) => tracing::warn!(%error, "job failed"),
            }
        }
    }

    async fn execute(&self, job: &ClaimedJob) -> Result<()> {
        match job.payload()? {
            JobPayload::ConsistencyScore { contribution_id } => {
                consistency::run(&self.pool, &self.config, &contribution_id).await
            }
            JobPayload::HonestDuplicateMerge => duplicates::run(&self.pool).await,
            JobPayload::NightlyRefit { bodies } => {
                let outcomes = refit::run(&self.pool, &self.config, &bodies).await?;
                if outcomes
                    .iter()
                    .any(|(_, _, outcome)| matches!(outcome, RefitOutcome::Published(_)))
                {
                    jobs::enqueue_if_idle(&self.pool, &JobPayload::PublishSite, now_unix()).await?;
                }
                Ok(())
            }
            JobPayload::BlobGc { contribution_id } => {
                gc::run(&self.pool, &self.config, contribution_id.as_ref()).await
            }
            JobPayload::PublishSite => {
                let config = PublishConfig::from_worker(&self.config);
                publish::publish_with_pool(&self.pool, &config).await?;
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
