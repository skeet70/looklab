// SPDX-License-Identifier: AGPL-3.0-or-later
//! Control schemas, control-response models and the settings optimizer
//! (docs/04 §1-§5). Candidate objective:
//!
//! ```text
//! E(k,u) = Σ_p w_p c_p ΔE_UCS(L(p), Real_{k,u}(p))² + β Σ_p w_p tr Cov_pred(k,u,p)
//! ```

mod calibration;
mod card;
mod covariance;
mod emphasis;
mod error;
mod linear;
mod model;
mod optimizer;
mod permalink;
mod settings;
mod target;
mod verify;

pub mod synth;

pub use calibration::Calibration;
pub use card::{
    Confidence, ConfidenceLevel, ControlEntry, SettingsCard, UnreachableRegion, settings_card,
};
pub use covariance::{
    ControlCovariance, CovTerm, CovarianceFile, CovarianceGroup, SparseDelta, predicted_trace,
};
pub use emphasis::Emphasis;
pub use error::{MatchError, Result};
pub use linear::{LinearCache, ProbeSubset};
pub use model::MatchModel;
pub use optimizer::{
    MatchOptions, MatchOutcome, OPTIMIZER_VERSION, RunStats, Suggestion, optimize,
};
pub use permalink::{CALIBRATED_PERMALINK_FORMAT, PERMALINK_FORMAT, Permalink, PermalinkSources};
pub use settings::{ControlDomain, Plan};
pub use target::{PreparedTarget, TargetFunction, TargetSpec};
pub use verify::{
    DeltaStats, FitObservedOptions, ObservedFitter, ObservedRender, ScaleFitter, TruthRenderer,
    VerifyOutcome, VerifyRequest, measure_target_error, select_verification_samples, verify_once,
};

pub use looklab_model::realization::{ControlResponseModel, ControlValue, Settings};
