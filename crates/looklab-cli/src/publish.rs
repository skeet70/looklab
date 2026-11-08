// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args;
use looklab_worker::{PublishConfig, publish_site};

#[derive(Debug, Args)]
pub struct PublishArgs {
    /// Server SQLite database.
    #[arg(long)]
    pub db: PathBuf,
    /// Sample and artifact blob directory.
    #[arg(long)]
    pub blobs: PathBuf,
    /// Output directory for `write_site` (for example `site/data`).
    #[arg(long)]
    pub out: PathBuf,
    /// Body and control-schema snapshots.
    #[arg(long, default_value = "data")]
    pub data: PathBuf,
    /// Approved community snapshots; the newest `v<version>` is published.
    #[arg(long, default_value = "data/community")]
    pub snapshot_dir: PathBuf,
    /// Body input transforms, one `<make>-<model>.yaml` per body.
    #[arg(long, default_value = "data/transforms")]
    pub transforms: PathBuf,
}

pub fn run(args: &PublishArgs) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;
    let summary = runtime.block_on(publish_site(&PublishConfig {
        db: args.db.clone(),
        blobs: args.blobs.clone(),
        data: args.data.clone(),
        snapshot_dir: args.snapshot_dir.clone(),
        transforms: args.transforms.clone(),
        out: args.out.clone(),
    }))?;
    println!(
        "published {} file(s), site root {}",
        summary.files, summary.root
    );
    Ok(())
}
