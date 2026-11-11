// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control schemas, control-response models and the settings optimizer
//! (docs/04 §1-§5). Candidate objective:
//!
//! ```text
//! E(k,u) = Σ_p w_p c_p ΔE_UCS(𝒯(p), ℛ_{k,u}(p))² + β Σ_p w_p tr Cov_pred(k,u,p)
//! ```
//!
//! `𝒯` is the target and `ℛ` the camera realization in the [`ScoringMetric`] frame.

mod calibration;
mod card;
mod covariance;
mod domain;
mod emphasis;
mod error;
mod linear;
mod metric;
mod model;
mod optimizer;
mod permalink;
mod settings;
mod target;
mod verify;

pub mod synth;

pub use calibration::Calibration;
pub use card::{
    Confidence, ConfidenceLevel, ControlEntry, MissedRegion, REACHABILITY_TOLERANCE_DE,
    Reachability, ReachabilityStatus, SettingsCard, SettingsSupport, settings_card,
    settings_card_with_support,
};
pub use covariance::{
    ControlCovariance, CovTerm, CovarianceFile, CovarianceGroup, SparseDelta, predicted_trace,
    realized_trace,
};
pub use domain::{AssessedDomain, MAX_ASSESSED_EVALUATIONS};
pub use emphasis::Emphasis;
pub use error::{MatchError, Result};
pub use linear::{LinearCache, ProbeSubset};
pub use metric::{
    AssessmentStatus, OutputContext, OutputPin, RenderJacobian, ScoringMetric, TargetAppearance,
};
pub use model::MatchModel;
pub use optimizer::{
    MatchOptions, MatchOutcome, OPTIMIZER_VERSION, PRE_CLIP_OPTIMIZER_VERSION, RunStats,
    Suggestion, optimize, optimize_scoped,
};
pub use permalink::{
    CALIBRATED_PERMALINK_FORMAT, PERMALINK_FORMAT, Permalink, PermalinkSources, QualificationRef,
    RENDERED_PERMALINK_FORMAT, ReportPin, ReportSubject, SELECTION_PERMALINK_FORMAT,
    SelectionContext, SelectionMode,
};
pub use settings::{ControlDomain, Plan};
pub use target::{ImageCount, PreparedTarget, TargetFunction, TargetSpec, TargetSupport};
pub use verify::{
    DeltaStats, FitObservedOptions, ObservedFitter, ObservedRender, ScaleFitter, TruthRenderer,
    VerifyOutcome, VerifyRequest, measure_target_error, select_verification_samples,
    validate_observed_encoding, verify_once,
};

pub use looklab_model::realization::{ControlResponseModel, ControlValue, Settings};
