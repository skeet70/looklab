// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tunable trust parameters, one struct with the documented defaults ([docs/10 Q12]).
//!
//! [docs/10 Q12]: ../../../docs/10-open-questions.md

use serde::{Deserialize, Serialize};

/// All trust thresholds. Field comments name the doc that fixes the default.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrustParams {
    /// Tier factor for `new` (docs/05 §3).
    pub tier_factor_new: f64,
    /// Tier factor for `endorsed` and `seeder` within scope (docs/05 §3).
    pub tier_factor_trusted: f64,

    /// Reputation range (docs/05 §4).
    pub reputation_min: f64,
    pub reputation_max: f64,
    /// Starting value for a new credential.
    pub reputation_new_start: f64,
    /// Starting value for a seeder.
    pub reputation_seeder_start: f64,
    /// Value decay drifts toward.
    pub reputation_neutral: f64,
    /// Learning rate `η`.
    pub reputation_eta: f64,
    /// Penalty per quarantine judged deliberate.
    pub penalty_deliberate: f64,
    /// Penalty per automatic quarantine.
    pub penalty_automatic: f64,
    /// Bonus per passing chart session.
    pub chart_session_bonus: f64,
    /// Decay half-life toward `reputation_neutral` (docs/05 §4).
    pub reputation_half_life_days: f64,
    /// New-tier reputation at which a credential enters the endorsement queue. The
    /// docs leave the value to tuning.
    pub nomination_threshold: f64,

    /// Evidence clamp bounds (docs/05 §5).
    pub evidence_min: f64,
    pub evidence_max: f64,

    /// `base(kind)` weights (docs/05 §5).
    pub base_library: f64,
    pub base_rerender: f64,
    pub base_pattern: f64,
    pub base_gradient: f64,
    pub base_chart: f64,

    /// γ, the new-tier group cap (docs/05 §6).
    pub new_group_cap: f64,
    /// Per-contributor cap floor on a Look version's weight.
    pub look_contributor_cap_floor: f64,
    /// Per-contributor cap floor on a body input transform's weight.
    pub body_contributor_cap_floor: f64,
    /// Numerator of the continuous per-contributor cap `max(floor, factor / n)`.
    pub contributor_cap_factor: f64,
    /// `n` scale inside the diminishing-returns factor `1/sqrt(1 + n/scale)`.
    pub diminishing_scale: f64,
    /// Effective contributors needed before the Look's provisional label clears.
    pub min_look_contributors: usize,
    /// Effective contributors needed before the body's provisional label clears.
    pub min_body_contributors: usize,

    /// Root sunset: endorsed contributors and distinct bodies needed to re-derive
    /// the root (docs/05 §7).
    pub root_sunset_contributors: usize,
    pub root_sunset_bodies: usize,

    /// Multiplier on `sqrt(σ_ref² + σ_cand²)` in the drift bound `b_p`.
    pub gate_bound_scale: f64,
    /// Bound floor and ceiling versus the previous version (docs/05 §8).
    pub gate_previous_floor: f64,
    pub gate_previous_ceiling: f64,
    /// Bound floor and ceiling versus the anchor.
    pub gate_anchor_floor: f64,
    pub gate_anchor_ceiling: f64,
    /// Allowed median and p95 rise in root-probe ΔE versus the previous version.
    pub gate_root_median_previous: f64,
    pub gate_root_p95_previous: f64,
    /// Allowed median and p95 root-probe ΔE versus the anchor.
    pub gate_root_median_anchor: f64,
    pub gate_root_p95_anchor: f64,
    /// Fraction of gated probes allowed above their bound.
    pub gate_over_bound_fraction: f64,
    /// Allowed p95 of `|Δ|/b`.
    pub gate_p95_ratio: f64,
}

impl TrustParams {
    pub fn base_weight(&self, kind: looklab_upload::ContributionKind) -> f64 {
        use looklab_upload::ContributionKind as Kind;
        match kind {
            Kind::Library => self.base_library,
            Kind::Rerender => self.base_rerender,
            Kind::Pattern => self.base_pattern,
            Kind::Gradient => self.base_gradient,
            Kind::Chart => self.base_chart,
        }
    }
}

impl Default for TrustParams {
    fn default() -> Self {
        Self {
            tier_factor_new: 1.0,
            tier_factor_trusted: 2.0,
            reputation_min: 0.2,
            reputation_max: 1.5,
            reputation_new_start: 0.5,
            reputation_seeder_start: 1.2,
            reputation_neutral: 0.5,
            reputation_eta: 0.1,
            penalty_deliberate: 0.2,
            penalty_automatic: 0.05,
            chart_session_bonus: 0.05,
            reputation_half_life_days: 365.0,
            nomination_threshold: 0.75,
            evidence_min: 0.1,
            evidence_max: 1.0,
            base_library: 0.25,
            base_rerender: 1.0,
            base_pattern: 1.0,
            base_gradient: 1.0,
            base_chart: 2.0,
            new_group_cap: 0.3,
            look_contributor_cap_floor: 0.05,
            body_contributor_cap_floor: 0.2,
            contributor_cap_factor: 2.0,
            diminishing_scale: 20.0,
            min_look_contributors: 20,
            min_body_contributors: 5,
            root_sunset_contributors: 20,
            root_sunset_bodies: 3,
            gate_bound_scale: 2.0,
            gate_previous_floor: 0.75,
            gate_previous_ceiling: 2.0,
            gate_anchor_floor: 1.0,
            gate_anchor_ceiling: 3.0,
            gate_root_median_previous: 0.3,
            gate_root_p95_previous: 0.5,
            gate_root_median_anchor: 0.5,
            gate_root_p95_anchor: 0.8,
            gate_over_bound_fraction: 0.02,
            gate_p95_ratio: 1.0,
        }
    }
}
