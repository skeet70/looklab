// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Parser;
use looklab_worker::{Worker, WorkerConfig};

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
    /// Output directory for `write_site`.
    #[arg(long, default_value = "site/data")]
    site: PathBuf,
    /// Fixed camera-to-XYZ matrices, one YAML per body.
    #[arg(long, default_value = "data/transforms")]
    transforms: PathBuf,
    #[arg(long, default_value_t = 0x5eed_1a5e)]
    seed: u64,
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
    let worker = Worker::open(config).await?;
    if args.once {
        let completed = worker.run_until_idle().await?;
        println!("completed {completed} job(s)");
        Ok(())
    } else {
        worker.run().await.context("worker loop")
    }
}
