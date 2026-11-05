// SPDX-License-Identifier: AGPL-3.0-or-later
//! Red-team harness: simulated contributor populations, attacker strategies and the
//! release loop that weights, refits and gates each version (docs/05 §10).

mod population;
mod release;

pub use population::{
    SampleAxes, artifact_hash, axes, capture_tuple, evidence_grid, hue_band_look, raised_g_look,
    region_coverage, render,
};
pub use release::{
    Batch, CaptureSpec, HarnessParams, ModerationCause, ModerationItem, ProbeShift, ReleaseOutcome,
    RevertOutcome, Sim, SimImage, Version,
};

use crate::error::TrustError;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("fit failed: {0}")]
    Fit(#[from] looklab_fit::FitError),
    #[error("trust: {0}")]
    Trust(#[from] TrustError),
    #[error("serial is not printable ASCII")]
    InvalidSerial,
    #[error("no image enters the fit")]
    EmptyPool,
    #[error("the fit returned no estimate for probe {0}")]
    MissingProbe(usize),
}

pub type HarnessResult<T> = core::result::Result<T, HarnessError>;
