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
    let now = now_unix();
    for withdrawal in db::withdrawals_due(db_pool, contribution, now).await? {
        for image in db::images(db_pool, &withdrawal.contribution).await? {
            let released =
                db::release_blob_once(db_pool, &withdrawal.id, &image.id, &image.samples_hash, now)
                    .await?;
            let Some(remaining) = released else {
                continue;
            };
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
