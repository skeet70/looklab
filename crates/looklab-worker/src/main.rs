// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use looklab_worker::jobs::JobPayload;
use looklab_worker::{Worker, WorkerConfig, clock, jobs};

#[derive(Debug, Parser)]
#[command(
    name = "looklab-worker",
    about = "Job runner and static-site publisher."
)]
struct Args {
    /// Server SQLite database.
    #[arg(long)]
    db: PathBuf,
    /// Sample blob directory.
    #[arg(long)]
    blobs: PathBuf,
    /// Body and control-schema snapshots.
    #[arg(long, default_value = "data")]
    data: PathBuf,
    /// Publication root; receives data/ and site-root.txt.
    #[arg(long, default_value = "site")]
    site: PathBuf,
    /// Fixed camera-to-XYZ matrices, one YAML per body.
    #[arg(long, default_value = "data/transforms")]
    transforms: PathBuf,
    #[arg(long, default_value_t = 0x5eed_1a5e)]
    seed: u64,
    /// Job lease; a restarted process reclaims an interrupted job after at most this long.
    #[arg(long, default_value_t = 3_600)]
    lease_seconds: i64,
    /// Drain ready jobs once instead of polling.
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .init();
    let args = Args::parse();
    let mut config = WorkerConfig::new(args.db, args.blobs);
    config.data = args.data;
    config.site = args.site;
    config.transforms = args.transforms;
    config.seed = args.seed;
    config.lease_seconds = args.lease_seconds;
    let worker = Worker::open(config).await?;
    schedule_startup(&worker).await?;
    if args.once {
        let completed = worker.run_until_idle().await?;
        println!("completed {completed} job(s)");
        Ok(())
    } else {
        worker
            .run_until(shutdown_signal())
            .await
            .context("worker loop")
    }
}

/// Publishes a missing catalog, re-queues a requested publication, sweeps staged
/// blob leftovers, and keeps pending jobs in the table.
async fn schedule_startup(worker: &Worker) -> anyhow::Result<()> {
    let now = clock::now_unix();
    jobs::enqueue_pending_publication(worker.pool(), now).await?;
    jobs::enqueue_if_idle(
        worker.pool(),
        &JobPayload::BlobGc {
            contribution_id: None,
        },
        now,
    )
    .await?;
    if !worker.config().site.join("data/index.json").is_file() {
        jobs::request_publish(worker.pool(), now).await?;
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown requested; finishing the current job");
}
