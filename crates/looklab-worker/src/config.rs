// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub db: PathBuf,
    pub blobs: PathBuf,
    /// Body and control-schema snapshots (`data/bodies`).
    pub data: PathBuf,
    /// Approved community snapshots (`data/community/v<version>`).
    pub snapshot_dir: PathBuf,
    /// Publication root passed to [`looklab_publish::write_site`]; receives `data/`.
    pub site: PathBuf,
    /// Fixed camera-to-XYZ matrices, one `<make>-<model>.yaml` per body.
    pub transforms: PathBuf,
    pub seed: u64,
    pub lease_seconds: i64,
    pub max_attempts: i64,
    pub nightly_interval: i64,
    pub poll_seconds: u64,
    pub max_samples_per_image: usize,
    /// Consecutive non-converged batches before a Look opens a moderation item.
    pub max_consecutive_nonconverged: u32,
    /// Sample-evaluation budget; `None` uses the fitter's sweep limit.
    pub sweep_budget: Option<f64>,
}

impl WorkerConfig {
    pub fn new(db: impl Into<PathBuf>, blobs: impl Into<PathBuf>) -> Self {
        Self {
            db: db.into(),
            blobs: blobs.into(),
            data: PathBuf::from("data"),
            snapshot_dir: PathBuf::from("data/community"),
            site: PathBuf::from("site"),
            transforms: PathBuf::from("data/transforms"),
            seed: 0x5eed_1a5e,
            lease_seconds: 3_600,
            max_attempts: 5,
            nightly_interval: 86_400,
            poll_seconds: 5,
            max_samples_per_image: 10_000,
            max_consecutive_nonconverged: 3,
            sweep_budget: None,
        }
    }
}
