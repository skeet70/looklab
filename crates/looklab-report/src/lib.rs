// SPDX-License-Identifier: AGPL-3.0-or-later
//! Static SVG/HTML reports for Looks and comparisons.
//!
//! The comparison engine ([`compare`]) is pure computation over a probe set;
//! [`svg`] and [`html`] turn its output into files with no JavaScript and no
//! timestamps, so native and wasm runs produce identical bytes.

pub mod compare;
pub mod html;
pub mod summary;
pub mod svg;

use serde::Serialize;

pub use compare::{
    CURVE_MAX_S, CURVE_MIN_S, CURVE_STEP_S, Comparison, ComparisonInput, Covariance3,
    DeltaEDistribution, DistributionMetric, FunctionName, GRL_S_VALUES, GrlComponentKind, GrlPlot,
    GrlSeries, GrlValue, HUE_BIN_COUNT, HUE_BIN_WIDTH, HistogramBin, HueBin, LayerContribution,
    LayerCovariance, LayerId, LookDiagnostics, LookFunction, NO_DIFFERENCE_DE, RegionCell,
    RegionMap, Support, TintComparison, TintCurve, TintPoint, TintSample, ToneComparison,
    ToneCurve, TonePoint, ToneSample, WorstProbe, compare, hue_bin_index, hue_bins,
    look_diagnostics,
};
pub use html::{ReportSource, comparison_page, look_page};
pub use summary::{HueRegion, summary_line};
pub use svg::{
    DELTA_E_BANDS, DELTA_E_FILLS, SERIES_A_COLOR, SERIES_B_COLOR, delta_e_histogram_svg, grl_svg,
    region_map_svg, tint_comparison_svg, tint_curve_svg, tone_comparison_svg, tone_curve_svg,
};

/// A content-addressed artifact hash: 64 lowercase hex characters (SHA-256).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ArtifactHash(String);

impl ArtifactHash {
    pub fn from_sha256_hex(value: &str) -> Option<Self> {
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        valid.then(|| Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}
