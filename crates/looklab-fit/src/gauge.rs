// SPDX-License-Identifier: AGPL-3.0-or-later
//! Gauges of the shared body parameters and their conversion on a component merge
//! (docs/02 §2.1).

use looklab_body::BodyId;
use looklab_color::Xyz;
use looklab_color::ucs::Ucs;
use looklab_model::Look;

use crate::types::ExposureModeId;

/// WB mode class id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WbModeClass(pub u32);

/// One designated body fixes `ξ = 0` in its default mode and `ω = 1` in its
/// default WB mode class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentGauge {
    pub designated: BodyId,
    pub exposure_mode: ExposureModeId,
    pub wb_mode: WbModeClass,
    /// A chart- or SSF-anchored body fixes the `Δ` gauge; otherwise the weighted
    /// mean of `Δ` is held at zero.
    pub anchored_delta: bool,
}

/// `L'(XYZ) = L(2^Δξ · XYZ)`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExposureGauge {
    pub delta_xi: f64,
}

impl ExposureGauge {
    pub fn new(delta_xi: f64) -> Self {
        Self { delta_xi }
    }

    /// `old_reference` and `new_reference` are absolute `ξ`.
    pub fn between(old_reference: f64, new_reference: f64) -> Self {
        Self::new(new_reference - old_reference)
    }

    pub fn apply(&self, xyz: Xyz) -> Xyz {
        xyz.scale(libm::pow(2.0, self.delta_xi))
    }

    pub fn evaluate(&self, look: &Look, xyz: Xyz) -> Ucs {
        look.eval(self.apply(xyz))
    }

    /// The tone input clamp in the converted Look: the s-clamp sits at `6 − Δξ`
    /// (docs/02 §2.1), so top gray-ramp probes are excluded when `|Δξ| > 1`.
    pub fn s_ceiling(&self) -> f64 {
        6.0 - self.delta_xi
    }
}

/// A WB gauge change requires a refit; its result is a logged re-anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WbGaugeChange {
    Unchanged,
    RefitAndReanchor,
}

/// What a merge or re-designation does to a component's gauges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GaugeChange {
    pub exposure: ExposureGauge,
    pub wb: WbGaugeChange,
}

impl GaugeChange {
    /// Only an exposure shift can be applied to the old Look without a refit.
    pub fn is_pure_exposure(&self) -> bool {
        self.wb == WbGaugeChange::Unchanged
    }
}

pub fn merge_gauges(
    old_reference_xi: f64,
    new_reference_xi: f64,
    old: &ComponentGauge,
    new: &ComponentGauge,
) -> GaugeChange {
    let wb = if old.designated == new.designated && old.wb_mode == new.wb_mode {
        WbGaugeChange::Unchanged
    } else {
        WbGaugeChange::RefitAndReanchor
    };
    GaugeChange {
        exposure: ExposureGauge::between(old_reference_xi, new_reference_xi),
        wb,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_color::RgbSpace;

    fn xyz(values: [f64; 3]) -> Xyz {
        Xyz::new(values[0], values[1], values[2])
    }

    #[test]
    fn exposure_conversion_preserves_the_prediction() {
        let look = Look::colorimetric_spline().expect("the colorimetric spline builds");
        let shift = ExposureGauge::new(0.75);
        for rgb in [[0.1, 0.1, 0.1], [0.5, 0.3, 0.2], [0.9, 0.8, 0.6]] {
            let scene = RgbSpace::Srgb.rgb_linear_to_xyz(looklab_color::Rgb::from_array(rgb));
            let old = look.eval(scene.scale(libm::pow(2.0, 0.75)));
            let converted = shift.evaluate(&look, scene);
            assert!((old.j - converted.j).abs() < 1e-9);
            assert!((old.a - converted.a).abs() < 1e-9);
            assert!((old.b - converted.b).abs() < 1e-9);
        }
        assert!((shift.s_ceiling() - 5.25).abs() < 1e-12);
    }

    #[test]
    fn between_reports_the_reference_difference() {
        let gauge = ExposureGauge::between(-0.4, 0.35);
        assert!((gauge.delta_xi - 0.75).abs() < 1e-12);
        let xyz = xyz([0.2, 0.3, 0.4]);
        let applied = gauge.apply(xyz).to_array();
        assert!((applied[1] - 0.3 * libm::pow(2.0, 0.75)).abs() < 1e-12);
    }

    #[test]
    fn merging_is_exact_only_when_the_wb_reference_stays() {
        let body = |name: &str| BodyId::new(name).expect("non-empty id");
        let old = ComponentGauge {
            designated: body("a"),
            exposure_mode: ExposureModeId(0),
            wb_mode: WbModeClass(0),
            anchored_delta: false,
        };
        let same = old.clone();
        assert!(merge_gauges(0.0, 0.5, &old, &same).is_pure_exposure());

        let moved = ComponentGauge {
            designated: body("b"),
            ..same.clone()
        };
        assert_eq!(
            merge_gauges(0.0, 0.5, &old, &moved).wb,
            WbGaugeChange::RefitAndReanchor
        );
    }
}
