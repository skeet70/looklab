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
/// Pending or failed LUT uploads are released after this window.
const LUT_RETENTION_SECONDS: i64 = 30 * 86_400;

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution: Option<&ContributionId>,
) -> Result<()> {
    let now = now_unix();
    release_lut_uploads(db_pool, config, now).await?;
    for withdrawal in db::withdrawals_due(db_pool, contribution, now).await? {
        for image in db::images(db_pool, &withdrawal.contribution).await? {
            db::release_image_blob_once(
                db_pool,
                &config.blobs,
                &image.id,
                &image.samples_hash,
                now,
            )
            .await?;
        }
        db::mark_withdrawal_deleted(db_pool, &withdrawal.contribution).await?;
        db::mark_deferred_expiry_deleted(db_pool, &withdrawal.contribution).await?;
    }
    for expiry in db::deferred_expiries_due(db_pool, contribution, now).await? {
        for image in db::images(db_pool, &expiry.contribution).await? {
            db::release_image_blob_once(
                db_pool,
                &config.blobs,
                &image.id,
                &image.samples_hash,
                now,
            )
            .await?;
        }
        db::expire_deferred_contribution(db_pool, &expiry.contribution, now).await?;
        db::mark_deferred_expiry_deleted(db_pool, &expiry.contribution).await?;
        db::mark_withdrawal_deleted(db_pool, &expiry.contribution).await?;
    }
    sweep_staged(db_pool, config, now).await
}

/// Expires unmoderated LUT proposals.
async fn release_lut_uploads(db_pool: &SqlitePool, config: &WorkerConfig, now: i64) -> Result<()> {
    looklab_server::expire_stale_lut_uploads(db_pool, now - LUT_RETENTION_SECONDS).await?;
    sweep_lut_deletions(db_pool, config).await
}

async fn sweep_lut_deletions(db_pool: &SqlitePool, config: &WorkerConfig) -> Result<()> {
    let mut tx = db_pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<Vec<u8>> = sqlx::query_scalar("SELECT hash FROM lut_blob_deletions")
        .fetch_all(&mut *tx)
        .await?;
    for row in rows {
        let Ok(hash) = <[u8; 32]>::try_from(row.as_slice()) else {
            sqlx::query("DELETE FROM lut_blob_deletions WHERE hash = ?")
                .bind(row.as_slice())
                .execute(&mut *tx)
                .await?;
            continue;
        };
        if db::blob_referenced(&mut tx, &hash).await? {
            sqlx::query("DELETE FROM lut_blob_deletions WHERE hash = ?")
                .bind(hash.as_slice())
                .execute(&mut *tx)
                .await?;
            continue;
        }
        if let Err(error) = db::remove_blob_file(&config.blobs, &hash) {
            tracing::warn!(%error, "LUT blob deletion deferred");
            continue;
        }
        sqlx::query("DELETE FROM lut_blob_deletions WHERE hash = ?")
            .bind(hash.as_slice())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Removes expired upload reservations and files no other record names.
async fn sweep_staged(db_pool: &SqlitePool, config: &WorkerConfig, now: i64) -> Result<()> {
    let mut tx = db_pool.begin_with("BEGIN IMMEDIATE").await?;
    for hash in db::expired_staging_hashes(&mut tx, now).await? {
        db::delete_expired_staging(&mut tx, &hash, now).await?;
        if !db::blob_referenced(&mut tx, &hash).await? {
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
