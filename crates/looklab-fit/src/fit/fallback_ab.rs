// SPDX-License-Identifier: AGPL-3.0-or-later
//! Frozen diagonal-fallback A/B: one shared linearization, one trust-region solve per arm, the
//! objective trials of each arm's applied step.

use super::*;
use crate::TrustRadius;
use crate::trust_region::{
    CauchyRay, DiagnosticTrustRegionInput, TrustRegionError, TrustRegionScratch,
    TrustRegionTermination, TrustRegionTrace, predicted_decrease, solve_diagnostic,
};

/// Analytic Hessian-vector products shared by both arms, after the eight post-solve products.
const SHARED_INNER_PRODUCTS: usize = 4088;
/// Post-solve products reserved for the two arms.
const RESERVED_POST_PRODUCTS: usize = 8;
/// Sample evaluations over the frozen 3360-row case: one linearization and two trials.
const SAMPLE_CEILING: u64 = 10_080;
const TOTAL_PRODUCT_CEILING: usize = 4096;

/// `artifact://17034`, printed to ten significant digits.
const RECORDED_BASELINE_COST: f64 = -3.402376726e1;
const RECORDED_BASELINE_PREDICTION: f64 = 2.761857971e1;
const RECORDED_BASELINE_DECREASE: f64 = 2.206629919e1;
const RECORDED_BASELINE_GAIN: f64 = 7.989657476e-1;
const RECORDED_BASELINE_TERMINATION: &str = "boundary";
const RECORDED_BASELINE_INNER_PRODUCTS: usize = 2;
const RECORDED_BASELINE_POST_PRODUCTS: usize = 3;
const RECORDED_BASELINE_RADIUS: f64 = 5.930281952e0;
const RECORDED_BASELINE_LARGEST: f64 = 1.744576376e-2;
const RECORDED_BASELINE_CLIPPING: usize = 0;
const RECORDED_BASELINE_TR_SCALE: f64 = 1.0;
const RECORDED_BASELINE_TR_LIMITER: &str = "metric-radius";
const RECORDED_BASELINE_TR_RELATIVE_RESIDUAL: f64 = 1.112060946e0;
const RECORDED_BASELINE_TR_METRIC_NORM: f64 = 5.930281952e0;
const RECORDED_BASELINE_TR_PREDICTION: f64 = 2.761857971e1;
const RECORDED_BASELINE_CAUCHY_ALPHA: f64 = 1.058798871e-6;
const RECORDED_BASELINE_CAUCHY_LIMITER: &str = "ray-minimum";
const RECORDED_BASELINE_CAUCHY_PREDICTION: f64 = 1.515113850e1;
const RECORDED_BASELINE_CAUCHY_METRIC_NORM: f64 = 4.999328188e0;
const RECORDED_BASELINE_CAUCHY_LARGEST: f64 = 2.355364523e-3;
const RECORDED_BASELINE_TRIAL_COST: f64 = -5.609006645e1;
const RECORDED_BASELINE_GRADIENT_NORM: f64 = 5.349718093e3;
const RECORDED_BASELINE_TOLERANCE: f64 = 2.0e-2;
const RECORDED_BASELINE_IMAGES: usize = 28;
const RECORDED_BASELINE_ROWS: usize = 3360;
const RECORDED_BASELINE_COORDINATES: usize = 19790;
const RECORDED_BASELINE_JOINT_DIM: usize = 5050;
const RECORDED_BASELINE_ARM_SAMPLES: u64 = 3360;
const RECORDED_BASELINE_TOTAL_SAMPLES: u64 = 6720;
const RECORDED_BASELINE_INITIAL_RESIDUAL_NORM: f64 = 5349.718093388344;
const RECORDED_BASELINE_INITIAL_RHO: f64 = 197287.57355924824;
const RECORDED_BASELINE_REFRESH_RESIDUAL_NORM: f64 = 5949.212563222129;
const RECORDED_BASELINE_REFRESH_RELATIVE_RESIDUAL: f64 = 1.1120609458981947;

fn recorded_half_ulp(recorded: f64) -> f64 {
    if !(recorded.is_finite() && recorded != 0.0) {
        return 0.0;
    }
    let exponent = libm::log10(recorded.abs()).floor();
    0.5 * libm::pow(10.0, exponent - 9.0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackArm {
    Baseline,
    ZeroedOffDiagonals,
}

impl FallbackArm {
    pub fn label(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::ZeroedOffDiagonals => "zeroed-off-diagonals",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackArmStatus {
    Compared,
    /// The arm used its whole inner allowance; no partial iterate is ranked or tried.
    WorkLimit,
    /// The solver returned success on its last permitted inner product; the arm is not scored.
    AllowanceExhausted,
    PreconditionerFailed,
    TrustSolveFailed,
    PredictionFailed,
    NotEvaluated,
}

impl FallbackArmStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Compared => "compared",
            Self::WorkLimit => "work-limit",
            Self::AllowanceExhausted => "allowance-exhausted",
            Self::PreconditionerFailed => "preconditioner-failed",
            Self::TrustSolveFailed => "trust-solve-failed",
            Self::PredictionFailed => "prediction-failed",
            Self::NotEvaluated => "not-evaluated",
        }
    }

    pub fn completed(self) -> bool {
        matches!(self, Self::Compared)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackComparison {
    CandidateAccepted,
    CandidateRejected,
    Tied,
    Inconclusive,
    BaselineMismatch,
}

impl FallbackComparison {
    pub fn label(self) -> &'static str {
        match self {
            Self::CandidateAccepted => "candidate-accepted",
            Self::CandidateRejected => "candidate-rejected",
            Self::Tied => "tied",
            Self::Inconclusive => "inconclusive",
            Self::BaselineMismatch => "baseline-mismatch",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FallbackArmReport {
    pub arm: FallbackArm,
    pub status: FallbackArmStatus,
    pub note: String,
    pub groups: Vec<PreconditionerGroupChoice>,
    pub work_limit: usize,
    pub tr_termination: Option<&'static str>,
    pub tr_iterations: usize,
    pub tr_relative_residual: f64,
    pub tr_scale: f64,
    pub tr_limiter: Option<&'static str>,
    pub tr: FixedLinearizationCandidate,
    pub cauchy_alpha: f64,
    pub cauchy_limiter: Option<&'static str>,
    pub cauchy: FixedLinearizationCandidate,
    pub box_source: FixedLinearizationBoxSource,
    pub box_clipped: usize,
    pub box_candidate: FixedLinearizationCandidate,
    pub ordinary: FixedLinearizationOrdinary,
    pub winner: FixedLinearizationWinner,
    pub winner_candidate: FixedLinearizationCandidate,
    pub trial: Option<FixedLinearizationTrial>,
    pub inner_products: usize,
    pub post_products: usize,
    pub total_products: usize,
    pub objective_passes: usize,
    pub sample_evaluations: u64,
    pub trace: TrustRegionTrace,
}

impl FallbackArmReport {
    fn new(arm: FallbackArm, work_limit: usize) -> Self {
        Self {
            arm,
            status: FallbackArmStatus::NotEvaluated,
            note: String::new(),
            groups: Vec::new(),
            work_limit,
            tr_termination: None,
            tr_iterations: 0,
            tr_relative_residual: f64::NAN,
            tr_scale: f64::NAN,
            tr_limiter: None,
            tr: FixedLinearizationCandidate::default(),
            cauchy_alpha: f64::NAN,
            cauchy_limiter: None,
            cauchy: FixedLinearizationCandidate::default(),
            box_source: FixedLinearizationBoxSource::TrustRegionProposal,
            box_clipped: 0,
            box_candidate: FixedLinearizationCandidate::default(),
            ordinary: FixedLinearizationOrdinary::None,
            winner: FixedLinearizationWinner::None,
            winner_candidate: FixedLinearizationCandidate::default(),
            trial: None,
            inner_products: 0,
            post_products: 0,
            total_products: 0,
            objective_passes: 0,
            sample_evaluations: 0,
            trace: TrustRegionTrace::default(),
        }
    }

    pub fn decrease(&self) -> f64 {
        self.trial.map_or(f64::NAN, |trial| trial.decrease)
    }

    pub fn gain(&self) -> f64 {
        self.trial.map_or(f64::NAN, |trial| trial.ratio)
    }

    pub fn accepted(&self) -> bool {
        self.trial.is_some_and(|trial| trial.accepted)
    }

    fn finish_products(&mut self) {
        self.total_products = self.inner_products + self.post_products;
    }
}

#[derive(Clone, Debug)]
pub struct FallbackComparisonReport {
    pub comparison: FallbackComparison,
    pub note: String,
    pub checkpoint_generation: u32,
    pub checkpoint_stage: usize,
    pub checkpoint_sweep: usize,
    pub images: usize,
    pub samples: usize,
    pub excluded: usize,
    pub excluded_samples: usize,
    pub coordinates: usize,
    pub joint_dim: usize,
    pub residual_coordinates: usize,
    pub sigma_log: Vec<f64>,
    pub lambdas: StageLambdas,
    pub cost: f64,
    pub infeasible: bool,
    pub hessian_rows: usize,
    pub active_samples: usize,
    pub radius: f64,
    pub gradient_norm: f64,
    pub gradient_reference: f64,
    pub reference_source: FixedLinearizationReference,
    pub tolerance: f64,
    pub baseline: FallbackArmReport,
    pub candidate: FallbackArmReport,
    pub baseline_decrease: f64,
    pub candidate_decrease: f64,
    pub tau: f64,
    pub improvement: f64,
    pub baseline_recorded_decrease: f64,
    pub baseline_recorded_tolerance: f64,
    pub baseline_recorded_cost: f64,
    pub baseline_recorded_prediction: f64,
    pub baseline_recorded_gain: f64,
    pub baseline_recorded_termination: &'static str,
    pub baseline_recorded_inner_products: usize,
    pub baseline_recorded_post_products: usize,
    pub baseline_recorded_radius: f64,
    pub baseline_recorded_largest: f64,
    pub baseline_recorded_selection: &'static str,
    pub baseline_recorded_clipping: usize,
    pub baseline_mismatches: Vec<&'static str>,
    pub baseline_reproduced: bool,
    pub candidate_finite: bool,
    pub candidate_feasible: bool,
    pub candidate_accepted: bool,
    pub candidate_exceeds_tau: bool,
    pub candidate_improves: bool,
    pub accounting_complete: bool,
    pub shared_allowance_binding: bool,
    pub sample_ceiling_respected: bool,
    pub product_ceiling_respected: bool,
    pub operations: FixedLinearizationOperations,
}

impl FallbackComparisonReport {
    pub fn completed(&self) -> bool {
        matches!(
            self.comparison,
            FallbackComparison::CandidateAccepted
                | FallbackComparison::CandidateRejected
                | FallbackComparison::Tied
                | FallbackComparison::BaselineMismatch
        )
    }
}

fn trial_result(
    cost: f64,
    trial_cost: f64,
    infeasible: bool,
    prediction: f64,
) -> FixedLinearizationTrial {
    let decrease = cost - trial_cost;
    let ratio = decrease / prediction;
    FixedLinearizationTrial {
        cost: trial_cost,
        decrease,
        prediction,
        ratio,
        trial_infeasible: infeasible,
        accepted: !infeasible
            && trial_cost.is_finite()
            && decrease > 1e-12 * cost.abs().max(1.0)
            && ratio >= 1e-4,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FallbackLimits {
    baseline: usize,
    candidate: usize,
    binding: bool,
}

fn fallback_limits(baseline_products: usize) -> FallbackLimits {
    let production = JOINT_CG_LIMIT.saturating_sub(4);
    let remaining = SHARED_INNER_PRODUCTS.saturating_sub(baseline_products);
    FallbackLimits {
        baseline: production.min(SHARED_INNER_PRODUCTS),
        candidate: production.min(remaining),
        binding: remaining == 0,
    }
}

fn arm_exhausted(inner_products: usize, work_limit: usize) -> bool {
    work_limit > 0 && inner_products >= work_limit
}

fn comparison_verdict(
    accounting_complete: bool,
    baseline_reproduced: bool,
    candidate_accepted: bool,
    improvement: f64,
    tau: f64,
) -> FallbackComparison {
    if !accounting_complete {
        FallbackComparison::Inconclusive
    } else if !baseline_reproduced {
        FallbackComparison::BaselineMismatch
    } else if !candidate_accepted {
        FallbackComparison::CandidateRejected
    } else if improvement <= tau {
        FallbackComparison::Tied
    } else {
        FallbackComparison::CandidateAccepted
    }
}

fn record_float(
    mismatches: &mut Vec<&'static str>,
    field: &'static str,
    fresh: f64,
    recorded: f64,
) {
    if !(fresh.is_finite() && (fresh - recorded).abs() <= recorded_half_ulp(recorded)) {
        mismatches.push(field);
    }
}

fn record_trace_float(
    mismatches: &mut Vec<&'static str>,
    field: &'static str,
    fresh: f64,
    recorded: f64,
) {
    if !(fresh.is_finite() && (fresh - recorded).abs() <= 1e-12 * recorded.abs().max(1.0)) {
        mismatches.push(field);
    }
}

fn record_count(
    mismatches: &mut Vec<&'static str>,
    field: &'static str,
    fresh: usize,
    recorded: usize,
) {
    if fresh != recorded {
        mismatches.push(field);
    }
}

fn record_flag(
    mismatches: &mut Vec<&'static str>,
    field: &'static str,
    fresh: bool,
    recorded: bool,
) {
    if fresh != recorded {
        mismatches.push(field);
    }
}

fn record_label(
    mismatches: &mut Vec<&'static str>,
    field: &'static str,
    fresh: Option<&'static str>,
    recorded: &'static str,
) {
    if fresh != Some(recorded) {
        mismatches.push(field);
    }
}

fn baseline_record_mismatches(
    report: &FallbackComparisonReport,
    baseline: &FallbackArmReport,
) -> Vec<&'static str> {
    use crate::trust_region::TrustRegionRefreshReason;

    let mut mismatches = Vec::new();
    record_count(
        &mut mismatches,
        "images",
        report.images,
        RECORDED_BASELINE_IMAGES,
    );
    record_count(
        &mut mismatches,
        "samples",
        report.samples,
        RECORDED_BASELINE_ROWS,
    );
    record_count(&mut mismatches, "excluded", report.excluded, 0);
    record_count(
        &mut mismatches,
        "excluded_samples",
        report.excluded_samples,
        0,
    );
    record_count(
        &mut mismatches,
        "coordinates",
        report.coordinates,
        RECORDED_BASELINE_COORDINATES,
    );
    record_count(
        &mut mismatches,
        "joint_dim",
        report.joint_dim,
        RECORDED_BASELINE_JOINT_DIM,
    );
    record_count(
        &mut mismatches,
        "residual_coordinates",
        report.residual_coordinates,
        0,
    );
    record_count(
        &mut mismatches,
        "hessian_rows",
        report.hessian_rows,
        RECORDED_BASELINE_ROWS,
    );
    record_count(
        &mut mismatches,
        "active_samples",
        report.active_samples,
        RECORDED_BASELINE_ROWS,
    );
    record_flag(&mut mismatches, "infeasible", report.infeasible, false);
    record_float(&mut mismatches, "cost", report.cost, RECORDED_BASELINE_COST);
    record_float(
        &mut mismatches,
        "radius",
        report.radius,
        RECORDED_BASELINE_RADIUS,
    );
    record_float(
        &mut mismatches,
        "gradient_norm",
        report.gradient_norm,
        RECORDED_BASELINE_GRADIENT_NORM,
    );
    record_float(
        &mut mismatches,
        "gradient_reference",
        report.gradient_reference,
        RECORDED_BASELINE_GRADIENT_NORM,
    );
    record_flag(
        &mut mismatches,
        "reference_source",
        report.reference_source == FixedLinearizationReference::FirstResume,
        true,
    );
    record_float(
        &mut mismatches,
        "tolerance",
        report.tolerance,
        RECORDED_BASELINE_TOLERANCE,
    );
    if let [sigma] = report.sigma_log.as_slice() {
        record_trace_float(
            &mut mismatches,
            "sigma_log",
            *sigma,
            -9.578_217_358_286_38e-1,
        );
    } else {
        mismatches.push("sigma_log");
    }

    record_flag(
        &mut mismatches,
        "baseline_status",
        baseline.status == FallbackArmStatus::Compared,
        true,
    );
    record_label(
        &mut mismatches,
        "tr_termination",
        baseline.tr_termination,
        RECORDED_BASELINE_TERMINATION,
    );
    record_count(
        &mut mismatches,
        "tr_iterations",
        baseline.tr_iterations,
        RECORDED_BASELINE_INNER_PRODUCTS,
    );
    record_count(
        &mut mismatches,
        "inner_products",
        baseline.inner_products,
        RECORDED_BASELINE_INNER_PRODUCTS,
    );
    record_count(
        &mut mismatches,
        "post_products",
        baseline.post_products,
        RECORDED_BASELINE_POST_PRODUCTS,
    );
    record_count(
        &mut mismatches,
        "total_products",
        baseline.total_products,
        RECORDED_BASELINE_INNER_PRODUCTS + RECORDED_BASELINE_POST_PRODUCTS,
    );
    record_float(
        &mut mismatches,
        "tr_scale",
        baseline.tr_scale,
        RECORDED_BASELINE_TR_SCALE,
    );
    record_label(
        &mut mismatches,
        "tr_limiter",
        baseline.tr_limiter,
        RECORDED_BASELINE_TR_LIMITER,
    );
    record_float(
        &mut mismatches,
        "tr_relative_residual",
        baseline.tr_relative_residual,
        RECORDED_BASELINE_TR_RELATIVE_RESIDUAL,
    );
    record_float(
        &mut mismatches,
        "tr_prediction",
        baseline.tr.prediction.unwrap_or(f64::NAN),
        RECORDED_BASELINE_TR_PREDICTION,
    );
    record_float(
        &mut mismatches,
        "tr_metric_norm",
        baseline.tr.metric_norm.unwrap_or(f64::NAN),
        RECORDED_BASELINE_TR_METRIC_NORM,
    );
    record_float(
        &mut mismatches,
        "tr_largest",
        baseline.tr.largest.unwrap_or(f64::NAN),
        RECORDED_BASELINE_LARGEST,
    );
    record_flag(&mut mismatches, "tr_available", baseline.tr.available, true);
    record_flag(
        &mut mismatches,
        "tr_coordinate_feasible",
        baseline.tr.coordinate_feasible,
        true,
    );
    record_flag(
        &mut mismatches,
        "tr_radius_feasible",
        baseline.tr.radius_feasible,
        true,
    );

    record_float(
        &mut mismatches,
        "cauchy_alpha",
        baseline.cauchy_alpha,
        RECORDED_BASELINE_CAUCHY_ALPHA,
    );
    record_label(
        &mut mismatches,
        "cauchy_limiter",
        baseline.cauchy_limiter,
        RECORDED_BASELINE_CAUCHY_LIMITER,
    );
    record_float(
        &mut mismatches,
        "cauchy_prediction",
        baseline.cauchy.prediction.unwrap_or(f64::NAN),
        RECORDED_BASELINE_CAUCHY_PREDICTION,
    );
    record_float(
        &mut mismatches,
        "cauchy_metric_norm",
        baseline.cauchy.metric_norm.unwrap_or(f64::NAN),
        RECORDED_BASELINE_CAUCHY_METRIC_NORM,
    );
    record_float(
        &mut mismatches,
        "cauchy_largest",
        baseline.cauchy.largest.unwrap_or(f64::NAN),
        RECORDED_BASELINE_CAUCHY_LARGEST,
    );
    record_flag(
        &mut mismatches,
        "cauchy_available",
        baseline.cauchy.available,
        true,
    );
    record_flag(
        &mut mismatches,
        "cauchy_coordinate_feasible",
        baseline.cauchy.coordinate_feasible,
        true,
    );
    record_flag(
        &mut mismatches,
        "cauchy_radius_feasible",
        baseline.cauchy.radius_feasible,
        true,
    );

    record_flag(
        &mut mismatches,
        "ordinary",
        baseline.ordinary == FixedLinearizationOrdinary::TrustRegion,
        true,
    );
    record_flag(
        &mut mismatches,
        "winner",
        baseline.winner == FixedLinearizationWinner::TrustRegion,
        true,
    );
    record_float(
        &mut mismatches,
        "winner_prediction",
        baseline.winner_candidate.prediction.unwrap_or(f64::NAN),
        RECORDED_BASELINE_PREDICTION,
    );
    record_float(
        &mut mismatches,
        "winner_metric_norm",
        baseline.winner_candidate.metric_norm.unwrap_or(f64::NAN),
        RECORDED_BASELINE_TR_METRIC_NORM,
    );
    record_float(
        &mut mismatches,
        "winner_largest",
        baseline.winner_candidate.largest.unwrap_or(f64::NAN),
        RECORDED_BASELINE_LARGEST,
    );
    record_flag(
        &mut mismatches,
        "winner_available",
        baseline.winner_candidate.available,
        true,
    );
    record_flag(
        &mut mismatches,
        "winner_coordinate_feasible",
        baseline.winner_candidate.coordinate_feasible,
        true,
    );
    record_flag(
        &mut mismatches,
        "winner_radius_feasible",
        baseline.winner_candidate.radius_feasible,
        true,
    );

    record_flag(
        &mut mismatches,
        "box_source",
        baseline.box_source == FixedLinearizationBoxSource::TrustRegionProposal,
        true,
    );
    record_count(
        &mut mismatches,
        "box_clipped",
        baseline.box_clipped,
        RECORDED_BASELINE_CLIPPING,
    );
    record_flag(
        &mut mismatches,
        "box_available",
        baseline.box_candidate.available,
        false,
    );
    record_flag(
        &mut mismatches,
        "box_descent",
        baseline.box_candidate.descent(),
        false,
    );

    match baseline.trial {
        None => mismatches.push("trial"),
        Some(trial) => {
            record_flag(&mut mismatches, "trial_accepted", trial.accepted, true);
            record_flag(
                &mut mismatches,
                "trial_infeasible",
                trial.trial_infeasible,
                false,
            );
            record_float(
                &mut mismatches,
                "trial_cost",
                trial.cost,
                RECORDED_BASELINE_TRIAL_COST,
            );
            record_float(
                &mut mismatches,
                "trial_decrease",
                trial.decrease,
                RECORDED_BASELINE_DECREASE,
            );
            record_float(
                &mut mismatches,
                "trial_prediction",
                trial.prediction,
                RECORDED_BASELINE_PREDICTION,
            );
            record_float(
                &mut mismatches,
                "trial_gain",
                trial.ratio,
                RECORDED_BASELINE_GAIN,
            );
        }
    }
    record_count(
        &mut mismatches,
        "arm_sample_evaluations",
        baseline.sample_evaluations as usize,
        RECORDED_BASELINE_ARM_SAMPLES as usize,
    );
    record_count(
        &mut mismatches,
        "baseline_sample_evaluations",
        (report.samples as u64 + baseline.sample_evaluations) as usize,
        RECORDED_BASELINE_TOTAL_SAMPLES as usize,
    );

    record_trace_float(
        &mut mismatches,
        "trace_initial_residual_norm",
        baseline.trace.initial_residual_norm,
        RECORDED_BASELINE_INITIAL_RESIDUAL_NORM,
    );
    record_trace_float(
        &mut mismatches,
        "trace_initial_rho",
        baseline.trace.initial_preconditioned_rho,
        RECORDED_BASELINE_INITIAL_RHO,
    );
    record_count(
        &mut mismatches,
        "trace_hessian_products",
        baseline.trace.hessian_products,
        RECORDED_BASELINE_INNER_PRODUCTS,
    );
    record_count(
        &mut mismatches,
        "trace_restarts",
        baseline.trace.restarts,
        0,
    );
    record_flag(
        &mut mismatches,
        "trace_iterations",
        baseline.trace.iterations.is_empty(),
        true,
    );
    record_count(
        &mut mismatches,
        "trace_refreshes",
        baseline.trace.refreshes.len(),
        1,
    );
    if let Some(refresh) = baseline.trace.refreshes.first() {
        record_count(
            &mut mismatches,
            "refresh_products",
            refresh.hessian_products,
            RECORDED_BASELINE_INNER_PRODUCTS,
        );
        record_flag(
            &mut mismatches,
            "refresh_reason",
            refresh.reason == TrustRegionRefreshReason::Boundary,
            true,
        );
        match refresh.fresh_residual {
            None => mismatches.push("refresh_residual"),
            Some(residual) => {
                record_trace_float(
                    &mut mismatches,
                    "refresh_residual_norm",
                    residual.residual_norm,
                    RECORDED_BASELINE_REFRESH_RESIDUAL_NORM,
                );
                record_trace_float(
                    &mut mismatches,
                    "refresh_relative_residual",
                    residual.relative_residual,
                    RECORDED_BASELINE_REFRESH_RELATIVE_RESIDUAL,
                );
            }
        }
    }
    mismatches
}

/// One arm: its own preconditioner and trust-region solve over the shared linearization.
#[allow(clippy::too_many_arguments)]
fn evaluate_arm(
    arm: FallbackArm,
    diagonal_fallback: DiagonalFallback,
    model: &JointModel<'_>,
    full: &AnalyticModel<'_, '_>,
    layout: &Layout,
    problem: &Problem,
    params: &Params,
    blocks: &[BlockAcc],
    penalties: &[Vec<f64>],
    lambdas: &StageLambdas,
    options: &FitOptions,
    mask: &ResidualMask,
    residual_active: bool,
    gradient: &[f64],
    diagonal: &[f64],
    radius: TrustRadius,
    tolerance: f64,
    cost: f64,
    work_limit: usize,
    hessian: &mut AnalyticProductScratch,
    product: &mut [f64],
    cauchy_product: &mut [f64],
    reduced: &mut [f64],
    reduced_out: &mut [f64],
    solver: &mut TrustRegionScratch,
) -> FallbackArmReport {
    let mut report = FallbackArmReport::new(arm, work_limit);
    let (preconditioner, groups) = match JointSolve::diagnostic(model, 0.0, diagonal_fallback) {
        Ok(value) => value,
        Err(failure) => {
            report.status = FallbackArmStatus::PreconditionerFailed;
            report.note = failure.label().to_string();
            return report;
        }
    };
    report.groups = groups;

    let mut trace = TrustRegionTrace::default();
    let outcome = solve_diagnostic(
        DiagnosticTrustRegionInput {
            gradient,
            metric_diagonal: diagonal,
            radius,
            work_limit,
            relative_tolerance: tolerance,
        },
        |direction, out| full.apply(direction, out, hessian),
        |residual, out| {
            preconditioner.precondition_full(model, residual, out, reduced, reduced_out)
        },
        solver,
        &mut trace,
    );
    report.inner_products = trace.hessian_products;
    report.tr_iterations = trace.hessian_products;
    let (raw_proposal, termination, tr_relative_residual) = match outcome {
        Ok(solution) => {
            let step = solution.step.to_vec();
            let termination = solution.termination;
            let relative_residual = solution.relative_residual;
            if arm_exhausted(trace.hessian_products, work_limit) {
                report.status = FallbackArmStatus::AllowanceExhausted;
                report.note = "arm terminated at its inner product allowance".to_string();
                report.tr_termination = Some(fixed_linearization_termination(termination));
                report.tr_relative_residual = relative_residual;
                report.trace = trace;
                report.finish_products();
                return report;
            }
            (step, termination, relative_residual)
        }
        Err(TrustRegionError::WorkLimit { .. }) => {
            report.status = FallbackArmStatus::WorkLimit;
            report.note = "arm exhausted its inner product allowance".to_string();
            report.trace = trace;
            report.finish_products();
            return report;
        }
        Err(failure) => {
            report.status = FallbackArmStatus::TrustSolveFailed;
            report.note = format!("{failure:?}");
            report.trace = trace;
            report.finish_products();
            return report;
        }
    };
    report.tr_termination = Some(fixed_linearization_termination(termination));
    report.tr_relative_residual = tr_relative_residual;
    if termination == TrustRegionTermination::ZeroGradient {
        report.status = FallbackArmStatus::TrustSolveFailed;
        report.note = "trust-region solve returned a zero gradient".to_string();
        report.trace = trace;
        report.finish_products();
        return report;
    }

    let realize = |delta: &[f64], trial: &mut Params| -> Vec<f64> {
        for (index, global) in model.globals.iter().enumerate() {
            trial.theta[*global] = params.theta[*global] + delta[index];
        }
        if residual_active {
            mask.pin(&mut trial.theta[layout.residual.clone()]);
        }
        model
            .globals
            .iter()
            .map(|global| trial.theta[*global] - params.theta[*global])
            .collect()
    };
    let mut post_products = 0usize;
    let slack = 1.0 + 8.0 * f64::EPSILON;

    let proposal_largest = fixed_linearization_largest(&raw_proposal);
    let tr_scale = if proposal_largest > STEP_RADIUS {
        STEP_RADIUS / proposal_largest
    } else {
        1.0
    };
    report.tr_scale = tr_scale;
    let tr_delta: Vec<f64> = raw_proposal.iter().map(|value| tr_scale * *value).collect();
    let mut tr_params = params.clone();
    let tr_effective = realize(&tr_delta, &mut tr_params);
    let tr_metric_norm = fixed_linearization_metric_norm(diagonal, &tr_effective);
    let tr_largest = fixed_linearization_largest(&tr_effective);
    let tr_zero = tr_effective.iter().all(|value| *value == 0.0);
    let tr_eligible =
        termination != TrustRegionTermination::Stationary || tr_relative_residual <= tolerance;
    let tr_coordinate_feasible = tr_largest.is_finite() && tr_largest <= STEP_RADIUS * slack;
    let tr_radius_feasible = tr_metric_norm.is_finite() && tr_metric_norm <= radius.get() * slack;
    let tr_prediction = if tr_zero || !tr_eligible {
        None
    } else {
        post_products += 1;
        match full.apply(&tr_effective, product, hessian) {
            Ok(()) => predicted_decrease(gradient, &tr_effective, product),
            Err(failure) => {
                report.status = FallbackArmStatus::PredictionFailed;
                report.note = failure.label().to_string();
                report.post_products = post_products;
                report.trace = trace;
                report.finish_products();
                return report;
            }
        }
    };
    report.tr = FixedLinearizationCandidate {
        available: !tr_zero && tr_eligible,
        prediction: tr_prediction,
        metric_norm: Some(tr_metric_norm),
        largest: Some(tr_largest),
        coordinate_feasible: tr_coordinate_feasible,
        radius_feasible: tr_radius_feasible,
    };
    report.tr_limiter = Some(if tr_zero {
        "zero-step"
    } else if !tr_eligible {
        "ineligible"
    } else if tr_scale < 1.0 {
        "coordinate-cap"
    } else {
        match termination {
            TrustRegionTermination::Boundary => "metric-radius",
            TrustRegionTermination::NegativeCurvature => "negative-curvature",
            TrustRegionTermination::Stationary => "interior",
            TrustRegionTermination::ZeroGradient => "zero-gradient",
        }
    });

    let gradient_largest = fixed_linearization_largest(gradient);
    let metric_gradient_norm = fixed_linearization_metric_norm(diagonal, gradient);
    let gradient_squared: f64 = gradient.iter().map(|value| *value * *value).sum();
    post_products += 1;
    let cauchy_ray = match full.apply(gradient, cauchy_product, hessian) {
        Ok(()) => CauchyRay::new(gradient, diagonal, STEP_RADIUS, cauchy_product),
        Err(failure) => {
            report.status = FallbackArmStatus::PredictionFailed;
            report.note = failure.label().to_string();
            report.post_products = post_products;
            report.trace = trace;
            report.finish_products();
            return report;
        }
    };
    let cauchy_curvature = gradient
        .iter()
        .zip(cauchy_product.iter())
        .map(|(gradient, product)| *gradient * *product)
        .sum::<f64>();
    let cauchy_cap = if gradient_largest > 0.0 {
        STEP_RADIUS / gradient_largest
    } else {
        f64::INFINITY
    };
    let cauchy_maximum = if cauchy_curvature > 0.0 {
        cauchy_cap.min(gradient_squared / cauchy_curvature)
    } else {
        cauchy_cap
    };
    let mut cauchy_prediction = None;
    let mut cauchy_metric_norm = f64::NAN;
    let mut cauchy_largest = f64::NAN;
    let mut cauchy_coordinate_feasible = false;
    let mut cauchy_radius_feasible = false;
    let mut cauchy_available = false;
    let mut cauchy_trial = None;
    if let Some(alpha) = cauchy_ray.and_then(|ray| ray.step_length(radius)) {
        report.cauchy_alpha = alpha;
        let cauchy_delta: Vec<f64> = gradient.iter().map(|value| -alpha * *value).collect();
        let mut cauchy_params = params.clone();
        let cauchy_effective = realize(&cauchy_delta, &mut cauchy_params);
        cauchy_metric_norm = fixed_linearization_metric_norm(diagonal, &cauchy_effective);
        cauchy_largest = fixed_linearization_largest(&cauchy_effective);
        cauchy_coordinate_feasible =
            cauchy_largest.is_finite() && cauchy_largest <= STEP_RADIUS * slack;
        cauchy_radius_feasible =
            cauchy_metric_norm.is_finite() && cauchy_metric_norm <= radius.get() * slack;
        let cauchy_zero = cauchy_effective.iter().all(|value| *value == 0.0);
        cauchy_available = !cauchy_zero && cauchy_coordinate_feasible && cauchy_radius_feasible;
        if cauchy_available {
            post_products += 1;
            match full.apply(&cauchy_effective, cauchy_product, hessian) {
                Ok(()) => {
                    cauchy_prediction =
                        predicted_decrease(gradient, &cauchy_effective, cauchy_product);
                }
                Err(failure) => {
                    report.status = FallbackArmStatus::PredictionFailed;
                    report.note = failure.label().to_string();
                    report.post_products = post_products;
                    report.trace = trace;
                    report.finish_products();
                    return report;
                }
            }
        }
        report.cauchy_limiter = Some(if cauchy_maximum < radius.get() / metric_gradient_norm {
            if cauchy_curvature > 0.0 && gradient_squared / cauchy_curvature < cauchy_cap {
                "ray-minimum"
            } else {
                "coordinate-cap"
            }
        } else {
            "metric-radius"
        });
        cauchy_trial = Some(cauchy_params);
    }
    report.cauchy = FixedLinearizationCandidate {
        available: cauchy_available,
        prediction: cauchy_prediction,
        metric_norm: cauchy_metric_norm.is_finite().then_some(cauchy_metric_norm),
        largest: cauchy_largest.is_finite().then_some(cauchy_largest),
        coordinate_feasible: cauchy_coordinate_feasible,
        radius_feasible: cauchy_radius_feasible,
    };

    let tr_positive = report.tr.prediction.filter(|value| *value > 0.0);
    let cauchy_positive = report.cauchy.prediction.filter(|value| *value > 0.0);
    let cauchy_wins = match (tr_positive, cauchy_positive) {
        (Some(tr), Some(cauchy)) => cauchy > tr,
        (None, Some(_)) => true,
        _ => false,
    };
    report.ordinary = if cauchy_wins {
        FixedLinearizationOrdinary::Cauchy
    } else if tr_positive.is_some() {
        FixedLinearizationOrdinary::TrustRegion
    } else {
        FixedLinearizationOrdinary::None
    };
    let ordinary_prediction = match report.ordinary {
        FixedLinearizationOrdinary::TrustRegion => tr_positive,
        FixedLinearizationOrdinary::Cauchy => cauchy_positive,
        FixedLinearizationOrdinary::None => None,
    };

    report.box_clipped = raw_proposal
        .iter()
        .filter(|value| (*value).abs() > STEP_RADIUS)
        .count();
    let box_delta: Vec<f64> = raw_proposal
        .iter()
        .map(|value| (*value).clamp(-STEP_RADIUS, STEP_RADIUS))
        .collect();
    let mut box_params = params.clone();
    let box_effective = realize(&box_delta, &mut box_params);
    let box_metric_norm = fixed_linearization_metric_norm(diagonal, &box_effective);
    let box_largest = fixed_linearization_largest(&box_effective);
    let box_zero = box_effective.iter().all(|value| *value == 0.0);
    let box_coordinate_feasible = box_largest.is_finite() && box_largest <= STEP_RADIUS * slack;
    let box_radius_feasible =
        box_metric_norm.is_finite() && box_metric_norm <= radius.get() * slack;
    let box_considered = !box_zero
        && box_effective != tr_effective
        && report.ordinary != FixedLinearizationOrdinary::None;
    let box_prediction = if box_considered && box_coordinate_feasible && box_radius_feasible {
        post_products += 1;
        match full.apply(&box_effective, product, hessian) {
            Ok(()) => predicted_decrease(gradient, &box_effective, product),
            Err(failure) => {
                report.status = FallbackArmStatus::PredictionFailed;
                report.note = failure.label().to_string();
                report.post_products = post_products;
                report.trace = trace;
                report.finish_products();
                return report;
            }
        }
    } else {
        None
    };
    report.box_source = FixedLinearizationBoxSource::TrustRegionProposal;
    report.box_candidate = FixedLinearizationCandidate {
        available: box_considered,
        prediction: box_prediction,
        metric_norm: Some(box_metric_norm),
        largest: Some(box_largest),
        coordinate_feasible: box_coordinate_feasible,
        radius_feasible: box_radius_feasible,
    };
    let box_positive = box_prediction.filter(|value| *value > 0.0);
    let box_ranked_first = ordinary_prediction
        .zip(box_positive)
        .is_some_and(|(ordinary, boxed)| boxed > ordinary);
    let stage = &STAGES[4];
    let mut evaluate = |trial_params: &Params, prediction: f64| -> FixedLinearizationTrial {
        let candidate = problem.pass(
            trial_params,
            stage.active,
            &mut [],
            false,
            false,
            stage.neutral,
            IntentFilter::NativeOnly,
        );
        let trial_cost =
            candidate.cost + total_cost(problem, trial_params, blocks, penalties, lambdas, options);
        report.objective_passes += 1;
        report.sample_evaluations += problem.samples.len() as u64;
        trial_result(cost, trial_cost, candidate.infeasible, prediction)
    };
    let (ordinary_candidate, ordinary_params) = match report.ordinary {
        FixedLinearizationOrdinary::TrustRegion => (report.tr, Some(&tr_params)),
        FixedLinearizationOrdinary::Cauchy => (report.cauchy, cauchy_trial.as_ref()),
        FixedLinearizationOrdinary::None => (FixedLinearizationCandidate::default(), None),
    };
    let ordinary_trial = match (ordinary_prediction, ordinary_params) {
        (Some(prediction), Some(trial_params)) => Some(evaluate(trial_params, prediction)),
        _ => None,
    };
    let mut winner = report.ordinary.winner();
    let mut winner_candidate = ordinary_candidate;
    let mut trial = ordinary_trial;
    if box_ranked_first && let Some(prediction) = box_positive {
        let box_trial = evaluate(&box_params, prediction);
        if box_trial.decrease > trial.map_or(f64::NEG_INFINITY, |trial| trial.decrease) {
            winner = FixedLinearizationWinner::Box;
            winner_candidate = report.box_candidate;
            trial = Some(box_trial);
        }
    }
    report.winner = winner;
    report.winner_candidate = winner_candidate;
    report.trial = trial;
    report.status = FallbackArmStatus::Compared;
    report.post_products = post_products;
    report.trace = trace;
    report.finish_products();
    report
}

fn shared_report(
    input: &FixedLinearizationInput,
    fields: FixedLinearizationBaseFields,
    note: String,
) -> FallbackComparisonReport {
    FallbackComparisonReport {
        comparison: FallbackComparison::Inconclusive,
        note,
        checkpoint_generation: input.checkpoint_generation,
        checkpoint_stage: input.checkpoint_stage,
        checkpoint_sweep: input.checkpoint_sweep,
        images: fields.images,
        samples: fields.samples,
        excluded: fields.excluded,
        excluded_samples: fields.excluded_samples,
        coordinates: fields.coordinates,
        joint_dim: 0,
        residual_coordinates: 0,
        sigma_log: fields.sigma_log,
        lambdas: input.lambdas,
        cost: fields.cost,
        infeasible: fields.infeasible,
        hessian_rows: fields.hessian_rows,
        active_samples: fields.active_samples,
        radius: f64::NAN,
        gradient_norm: f64::NAN,
        gradient_reference: f64::NAN,
        reference_source: FixedLinearizationReference::FirstResume,
        tolerance: f64::NAN,
        baseline: FallbackArmReport::new(FallbackArm::Baseline, 0),
        candidate: FallbackArmReport::new(FallbackArm::ZeroedOffDiagonals, 0),
        baseline_decrease: f64::NAN,
        candidate_decrease: f64::NAN,
        tau: f64::NAN,
        improvement: f64::NAN,
        baseline_recorded_decrease: RECORDED_BASELINE_DECREASE,
        baseline_recorded_tolerance: recorded_half_ulp(RECORDED_BASELINE_DECREASE),
        baseline_recorded_cost: RECORDED_BASELINE_COST,
        baseline_recorded_prediction: RECORDED_BASELINE_PREDICTION,
        baseline_recorded_gain: RECORDED_BASELINE_GAIN,
        baseline_recorded_termination: RECORDED_BASELINE_TERMINATION,
        baseline_recorded_inner_products: RECORDED_BASELINE_INNER_PRODUCTS,
        baseline_recorded_post_products: RECORDED_BASELINE_POST_PRODUCTS,
        baseline_recorded_radius: RECORDED_BASELINE_RADIUS,
        baseline_recorded_largest: RECORDED_BASELINE_LARGEST,
        baseline_recorded_selection: "tr",
        baseline_recorded_clipping: RECORDED_BASELINE_CLIPPING,
        baseline_mismatches: Vec::new(),
        baseline_reproduced: false,
        candidate_finite: false,
        candidate_feasible: false,
        candidate_accepted: false,
        candidate_exceeds_tau: false,
        candidate_improves: false,
        accounting_complete: false,
        shared_allowance_binding: false,
        sample_ceiling_respected: false,
        product_ceiling_respected: false,
        operations: FixedLinearizationOperations {
            linearization_passes: 1,
            objective_passes: 0,
            sample_evaluations: fields.samples as u64,
            inner_products: 0,
            post_products: 0,
            total_products: 0,
        },
    }
}

/// Compares the current diagonal fallback with the zeroed-off-diagonal candidate at one frozen
/// linearization. Both arms start from the saved state and use a first-resume reference.
#[allow(clippy::too_many_arguments)]
pub fn diagnose_fallback_ab(
    images: &[FitImage],
    schema: &looklab_schema::ControlSchema,
    epoch: u32,
    base_sim: &looklab_schema::SimKey,
    options: &FitOptions,
    input: &FixedLinearizationInput,
) -> Result<FallbackComparisonReport> {
    if input.gradient_reference.is_some() {
        return Err(FitError::RowContext(
            "fallback A/B requires first-resume gradient reference".to_string(),
        ));
    }
    let base = match fixed_linearization_base(images, schema, epoch, base_sim, options, input) {
        Ok(base) => base,
        Err(FixedLinearizationSetupError::Fit(error)) => return Err(error),
        Err(FixedLinearizationSetupError::Reported(status, note, fields)) => {
            return Ok(shared_report(
                input,
                fields,
                format!("linearization failed ({}) {note}", status.label()),
            ));
        }
    };
    let FixedLinearizationBase {
        problem,
        layout,
        params,
        pass,
        blocks,
        penalties,
        mask,
        roles,
        residual_active,
        data_scale,
        fields,
    } = base;
    let mut report = shared_report(input, fields, String::new());

    let model = match JointModel::new(
        &layout,
        &problem,
        &params,
        options,
        &blocks,
        &penalties,
        &input.lambdas,
        &roles,
        &pass,
        data_scale,
        false,
        false,
        &mask,
    ) {
        Ok(model) => model,
        Err(failure) => {
            report.note = format!("joint model failed ({})", failure.label());
            return Ok(report);
        }
    };
    report.joint_dim = model.dim;
    report.residual_coordinates = model.residual_count;
    let full = match AnalyticModel::new(&model, &pass, &params, options, model.residual_data_scale)
    {
        Ok(full) => full,
        Err(failure) => {
            report.note = format!("analytic model failed ({})", failure.label());
            return Ok(report);
        }
    };
    let mut scratch = FinalScratch::default();
    if let Err(failure) = full.prepare(&mut scratch) {
        report.note = format!("analytic workspace failed ({})", failure.label());
        return Ok(report);
    }
    let norm = model
        .rhs
        .iter()
        .fold(0.0, |norm, value| libm::hypot(norm, *value));
    report.gradient_norm = norm;
    if norm == 0.0 {
        report.note = "model right-hand side is zero".to_string();
        return Ok(report);
    }
    let reference = norm.max(1e-300);
    report.gradient_reference = reference;
    let tolerance = (0.02 * norm / reference).clamp(1e-3, 0.02);
    report.tolerance = tolerance;
    let Some(radius) = input.trust_radius else {
        report.note = "saved trust radius is required".to_string();
        return Ok(report);
    };
    report.radius = radius.get();

    let FinalScratch {
        hessian,
        gradient,
        diagonal,
        product,
        cauchy_product,
        reduced,
        reduced_out,
        ..
    } = &mut scratch;
    let mut baseline_solver = TrustRegionScratch::default();
    let mut candidate_solver = TrustRegionScratch::default();
    let cost = report.cost;

    let baseline_limit = fallback_limits(0).baseline;
    let baseline = evaluate_arm(
        FallbackArm::Baseline,
        DiagonalFallback::Baseline,
        &model,
        &full,
        &layout,
        &problem,
        &params,
        &blocks,
        &penalties,
        &input.lambdas,
        options,
        &mask,
        residual_active,
        gradient,
        diagonal,
        radius,
        tolerance,
        cost,
        baseline_limit,
        hessian,
        product,
        cauchy_product,
        reduced,
        reduced_out,
        &mut baseline_solver,
    );
    let limits = fallback_limits(baseline.inner_products);
    let candidate_limit = limits.candidate;
    let candidate = if candidate_limit == 0 {
        FallbackArmReport::new(FallbackArm::ZeroedOffDiagonals, candidate_limit)
    } else {
        evaluate_arm(
            FallbackArm::ZeroedOffDiagonals,
            DiagonalFallback::ZeroedOffDiagonals,
            &model,
            &full,
            &layout,
            &problem,
            &params,
            &blocks,
            &penalties,
            &input.lambdas,
            options,
            &mask,
            residual_active,
            gradient,
            diagonal,
            radius,
            tolerance,
            cost,
            candidate_limit,
            hessian,
            product,
            cauchy_product,
            reduced,
            reduced_out,
            &mut candidate_solver,
        )
    };

    let baseline_decrease = baseline.decrease();
    let candidate_decrease = candidate.decrease();
    report.baseline_decrease = baseline_decrease;
    report.candidate_decrease = candidate_decrease;
    report.tau = 1e-12 * cost.abs().max(1.0);
    report.improvement = candidate_decrease - baseline_decrease;
    report.baseline_mismatches = baseline_record_mismatches(&report, &baseline);
    report.baseline_reproduced = report.baseline_mismatches.is_empty();
    report.candidate_finite = candidate_decrease.is_finite()
        && candidate
            .trial
            .is_some_and(|trial| trial.cost.is_finite() && trial.prediction.is_finite());
    report.candidate_feasible = candidate.winner_candidate.feasible();
    report.candidate_exceeds_tau =
        candidate_decrease.is_finite() && candidate_decrease > report.tau;
    report.candidate_accepted = candidate.accepted()
        && report.candidate_finite
        && report.candidate_feasible
        && report.candidate_exceeds_tau;
    report.candidate_improves = report.candidate_accepted && report.improvement > report.tau;
    let baseline_exhausted = arm_exhausted(baseline.inner_products, baseline.work_limit);
    let candidate_exhausted = arm_exhausted(candidate.inner_products, candidate.work_limit);
    report.shared_allowance_binding = limits.binding || baseline_exhausted || candidate_exhausted;
    report.operations.linearization_passes = 1;
    report.operations.objective_passes = baseline.objective_passes + candidate.objective_passes;
    report.operations.sample_evaluations =
        report.samples as u64 + baseline.sample_evaluations + candidate.sample_evaluations;
    report.operations.inner_products = baseline.inner_products + candidate.inner_products;
    report.operations.post_products = baseline.post_products + candidate.post_products;
    report.operations.total_products =
        report.operations.inner_products + report.operations.post_products;
    report.sample_ceiling_respected = report.operations.sample_evaluations <= SAMPLE_CEILING;
    report.product_ceiling_respected = report.operations.inner_products <= SHARED_INNER_PRODUCTS
        && report.operations.post_products <= RESERVED_POST_PRODUCTS
        && report.operations.total_products <= TOTAL_PRODUCT_CEILING;
    report.accounting_complete = baseline.status.completed()
        && candidate.status.completed()
        && !report.shared_allowance_binding
        && baseline.trial.is_some()
        && candidate.trial.is_some()
        && baseline_decrease.is_finite()
        && candidate_decrease.is_finite()
        && report.sample_ceiling_respected
        && report.product_ceiling_respected;
    report.comparison = comparison_verdict(
        report.accounting_complete,
        report.baseline_reproduced,
        report.candidate_accepted,
        report.improvement,
        report.tau,
    );

    let mut notes = Vec::new();
    if report.comparison == FallbackComparison::Inconclusive {
        if baseline.status == FallbackArmStatus::WorkLimit
            || candidate.status == FallbackArmStatus::WorkLimit
        {
            notes.push("an arm exhausted its inner allowance");
        }
        if baseline.status == FallbackArmStatus::AllowanceExhausted
            || candidate.status == FallbackArmStatus::AllowanceExhausted
        {
            notes.push("an arm terminated at its inner allowance");
        }
        if report.shared_allowance_binding {
            notes.push("shared inner allowance bound");
        }
        if !report.sample_ceiling_respected {
            notes.push("sample evaluation ceiling exceeded");
        }
        if !report.product_ceiling_respected {
            notes.push("product ceiling exceeded");
        }
        if baseline.trial.is_none() {
            notes.push("baseline arm selected no objective trial");
        }
        if candidate.trial.is_none() {
            notes.push("candidate arm selected no objective trial");
        }
    }
    if report.comparison == FallbackComparison::CandidateRejected {
        if !report.candidate_finite {
            notes.push("candidate selected winner is not finite");
        }
        if !report.candidate_feasible {
            notes.push("candidate selected winner violates a trust-region constraint");
        }
        if !report.candidate_exceeds_tau {
            notes.push("candidate decrease does not exceed tau");
        }
        if candidate.trial.is_some_and(|trial| trial.ratio < 1e-4) {
            notes.push("candidate actual/model gain is below 1e-4");
        }
    }
    if report.comparison == FallbackComparison::Tied {
        notes.push("candidate decrease does not exceed baseline plus tau");
    }
    if report.comparison == FallbackComparison::BaselineMismatch {
        notes.push("baseline decrease does not reproduce the recorded audit at recorded precision");
    }
    report.note = notes.join("; ");
    report.baseline = baseline;
    report.candidate = candidate;
    Ok(report)
}

#[cfg(all(test, feature = "synth"))]
mod tests {
    use super::*;

    #[test]
    fn shared_inner_allowance_is_split_not_duplicated() {
        let fresh = fallback_limits(0);
        assert_eq!(fresh.baseline, SHARED_INNER_PRODUCTS);
        assert_eq!(fresh.candidate, SHARED_INNER_PRODUCTS);
        assert!(!fresh.binding);

        let one_left = fallback_limits(SHARED_INNER_PRODUCTS - 1);
        assert_eq!(one_left.candidate, 1);
        assert!(!one_left.binding);

        let exhausted = fallback_limits(SHARED_INNER_PRODUCTS);
        assert_eq!(exhausted.candidate, 0);
        assert!(exhausted.binding);

        let over = fallback_limits(SHARED_INNER_PRODUCTS + 7);
        assert_eq!(over.candidate, 0);
        assert!(over.binding);
    }

    #[test]
    fn success_on_the_last_permitted_product_is_exhaustion() {
        assert!(!arm_exhausted(0, 0));
        assert!(!arm_exhausted(
            SHARED_INNER_PRODUCTS - 1,
            SHARED_INNER_PRODUCTS
        ));
        assert!(arm_exhausted(SHARED_INNER_PRODUCTS, SHARED_INNER_PRODUCTS));
        assert!(arm_exhausted(
            SHARED_INNER_PRODUCTS + 1,
            SHARED_INNER_PRODUCTS
        ));
    }

    #[test]
    fn recorded_precision_accepts_the_printed_digit_only() {
        let mut mismatches = Vec::new();
        record_float(
            &mut mismatches,
            "cost",
            RECORDED_BASELINE_COST,
            RECORDED_BASELINE_COST,
        );
        record_float(
            &mut mismatches,
            "decrease",
            RECORDED_BASELINE_DECREASE,
            RECORDED_BASELINE_DECREASE,
        );
        assert!(mismatches.is_empty());

        record_float(
            &mut mismatches,
            "decrease",
            RECORDED_BASELINE_DECREASE + 2.0 * recorded_half_ulp(RECORDED_BASELINE_DECREASE),
            RECORDED_BASELINE_DECREASE,
        );
        assert_eq!(mismatches, vec!["decrease"]);
    }

    #[test]
    fn comparison_requires_strict_improvement_beyond_tau() {
        let tau = 1e-12;
        assert_eq!(
            comparison_verdict(true, true, true, tau, tau),
            FallbackComparison::Tied
        );
        assert_eq!(
            comparison_verdict(true, true, true, tau * 1.5, tau),
            FallbackComparison::CandidateAccepted
        );
        assert_eq!(
            comparison_verdict(true, true, false, tau * 2.0, tau),
            FallbackComparison::CandidateRejected
        );
        assert_eq!(
            comparison_verdict(true, false, true, tau * 2.0, tau),
            FallbackComparison::BaselineMismatch
        );
        assert_eq!(
            comparison_verdict(false, true, true, tau * 2.0, tau),
            FallbackComparison::Inconclusive
        );
    }

    #[test]
    fn trial_acceptance_uses_strict_tau_and_gain_floor() {
        let cost = 2.0;
        let zero = trial_result(cost, cost, false, 1.0);
        assert!(!zero.accepted);

        let full_gain = trial_result(cost, 1.0, false, 1.0);
        assert!(full_gain.accepted);

        let at_gain_floor = trial_result(cost, 1.0, false, 1.0 / 1e-4);
        assert!(at_gain_floor.accepted);

        let below_gain_floor = trial_result(cost, 1.0, false, 1.0 / 5e-5);
        assert!(!below_gain_floor.accepted);

        let infeasible = trial_result(cost, 1.0, true, 1.0);
        assert!(!infeasible.accepted);
    }
}
