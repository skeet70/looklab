// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;

use looklab_server::ContributionId;
use sqlx::SqlitePool;

use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db;
use crate::error::Result;

/// Interrupted writes younger than this may still be owned by a live put.
const TEMP_FILE_GRACE_SECONDS: u64 = 3_600;

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution: Option<&ContributionId>,
) -> Result<()> {
    let now = now_unix();
    for withdrawal in db::withdrawals_due(db_pool, contribution, now).await? {
        for image in db::images(db_pool, &withdrawal.contribution).await? {
            db::release_blob_once(
                db_pool,
                &config.blobs,
                &withdrawal.id,
                &image.id,
                &image.samples_hash,
                now,
            )
            .await?;
        }
        db::mark_withdrawal_deleted(db_pool, &withdrawal.contribution).await?;
    }
    sweep_staged(db_pool, config, now).await
}

/// Removes expired blob reservations and files no other record names.
async fn sweep_staged(db_pool: &SqlitePool, config: &WorkerConfig, now: i64) -> Result<()> {
    let mut tx = db_pool.begin_with("BEGIN IMMEDIATE").await?;
    for hash in db::expired_staging_hashes(&mut *tx, now).await? {
        db::delete_expired_staging(&mut *tx, &hash, now).await?;
        if !db::blob_referenced(&mut *tx, &hash).await? {
            db::remove_blob_file(&config.blobs, &hash)?;
        }
    }
    sweep_temporary_files(&config.blobs)?;
    tx.commit().await?;
    Ok(())
}

fn sweep_temporary_files(root: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with('.') && name.ends_with(".tmp")) {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
            continue;
        };
        if age.as_secs() < TEMP_FILE_GRACE_SECONDS {
            continue;
        }
        std::fs::remove_file(entry.path())?;
    }
    Ok(())
}
