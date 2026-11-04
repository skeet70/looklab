// SPDX-License-Identifier: AGPL-3.0-or-later

use std::io::ErrorKind;

use looklab_server::ContributionId;
use sqlx::SqlitePool;

use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db;
use crate::error::Result;

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution: Option<&ContributionId>,
) -> Result<()> {
    for withdrawal in db::withdrawals_due(db_pool, contribution, now_unix()).await? {
        for image in db::images(db_pool, &withdrawal.contribution).await? {
            let remaining = db::release_blob(db_pool, &image.samples_hash).await?;
            if remaining > 0 {
                continue;
            }
            let path = db::blob_path(&config.blobs, &image.samples_hash);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        db::mark_withdrawal_deleted(db_pool, &withdrawal.contribution).await?;
    }
    Ok(())
}
