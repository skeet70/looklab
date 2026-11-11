// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::RgbSpace;
use looklab_model::ProbeId;
use looklab_publish::{GateCheck, GateSummary, ProvenanceSummary};
use looklab_trust::{GateReport, GatedUnion, ProbeSupport};
use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::error::Result;
use crate::jobs::JobOwnership;

pub const LOOK_ARTIFACT: &str = "look.looklab";
pub const CRM_ARTIFACT: &str = "crm.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProbe {
    pub id: ProbeId,
    /// Pre-clip `σ_ΔE`.
    pub sigma: f64,
    /// Post-clip `σ_ΔE`.
    #[serde(default)]
    pub sigma_post: f64,
    /// `|b_p|`, ΔE.
    #[serde(default)]
    pub bias_de: f64,
    /// The pre-clip value was identified in the fit that published this version.
    #[serde(default)]
    pub pre_clip_identified: bool,
    /// The four support tests as the fit decided them.
    #[serde(default)]
    pub support: ProbeSupport,
}

/// The frame of [`ReleaseRecord::gauge`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GaugeConvention {
    /// `median(x_i - exposure_hint_ev)`; no exact relative conversion exists.
    #[default]
    LegacyAbsoluteHeader,
    /// `median(x_i)`.
    Relative,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub version: u32,
    pub gauge: f64,
    /// Records without a frame are legacy.
    #[serde(default)]
    pub gauge_convention: GaugeConvention,
    pub anchor: u32,
    /// Nominal canonical rendering clip.
    #[serde(default)]
    pub clip_k: f64,
    /// Output encoding of the published probe predictions.
    #[serde(default = "default_output_space")]
    pub output_space: RgbSpace,
    pub probes: Vec<StoredProbe>,
    /// Probes supported in any version published since [`Self::anchor`].
    #[serde(default)]
    pub gated_since_anchor: GatedUnion,
    /// The native stage reached its objective and step tolerances.
    #[serde(default)]
    pub converged: bool,
    /// Absent for a first candidate, which has no previous or anchor to drift against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateReport>,
    /// Supported-probe p95 ΔE of a LUT fit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approximation_p95: Option<f64>,
    pub provenance: ProvenanceSummary,
    pub created_at: i64,
}

fn default_output_space() -> RgbSpace {
    RgbSpace::Srgb
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefitOutcome {
    Published(u32),
    Held(u32),
    /// A reference release predates the relative exposure gauge; nothing was published.
    MigrationRequired(u32),
    /// The native fit exhausted its budget; nothing was published.
    NotConverged(u32),
    /// The body has no input transform, so its photos cannot enter a colour fit yet.
    Unavailable(String),
    Skipped,
}

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    bodies: &[BodyId],
    ownership: &JobOwnership,
) -> Result<Vec<(BodyId, LookSlug, RefitOutcome)>> {
    crate::model_fit::run(db_pool, config, bodies, ownership).await
}

pub async fn run_forced(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
) -> Result<Vec<(BodyId, LookSlug, RefitOutcome)>> {
    crate::model_fit::run_forced(db_pool, config, ownership).await
}

pub fn gate_summary(
    report: Option<&GateReport>,
    converged: bool,
    approximation_p95: Option<f64>,
) -> GateSummary {
    let mut checks = Vec::with_capacity(6);
    if let Some(report) = report {
        for (name, passed) in [
            ("root_previous", report.root_previous.pass),
            ("root_anchor", report.root_anchor.pass),
            ("drift_previous", report.drift_previous.pass),
            ("drift_anchor", report.drift_anchor.pass),
            ("supported_regions", report.supported_regressions.is_empty()),
        ] {
            checks.push(GateCheck {
                name: name.to_owned(),
                passed,
                detail: None,
            });
        }
    }
    if let Some(p95) = approximation_p95 {
        checks.push(GateCheck {
            name: "approximation".to_owned(),
            passed: p95.is_finite() && p95 <= 1.0,
            detail: Some(format!("p95 {p95:.3} ΔE")),
        });
    }
    checks.push(GateCheck {
        name: "converged".to_owned(),
        passed: converged,
        detail: (!converged).then(|| "the native stage exhausted its sweep budget".to_owned()),
    });
    GateSummary { checks }
}
