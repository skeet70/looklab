// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chart reference values and the chart error of an input transform.
//!
//! References are integrated from the chart reflectances and adapted to the
//! reference viewing white of docs/02 §1.

use looklab_color::ciede2000::delta_e_ciede2000;
use looklab_color::lab::xyz_to_lab;
use looklab_color::{Cat, Xyz};
use looklab_model::solve::reference_white;
use looklab_samples::PatchId;
use serde::{Deserialize, Serialize};

use crate::cct::Cct;
use crate::error::{BodyError, Result};
use crate::spectral::{SpectralData, SsfIlluminant, training_data};
use crate::transform::InputTransform;

/// Pinned to `colour.sd_blackbody`.
const C2_METRE_KELVIN: f64 = 1.438_8e-2;

/// Per-patch CIEDE2000 differences of a transform prediction against a chart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartError {
    pub per_patch: Vec<f64>,
    pub mean: f64,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl ChartError {
    pub fn new(mut per_patch: Vec<f64>) -> Self {
        per_patch.sort_by(f64::total_cmp);
        let count = per_patch.len();
        let sum: f64 = per_patch.iter().sum();
        let mean = if count == 0 { 0.0 } else { sum / count as f64 };
        let median = if count == 0 {
            0.0
        } else if count % 2 == 1 {
            per_patch[count / 2]
        } else {
            (per_patch[count / 2 - 1] + per_patch[count / 2]) / 2.0
        };
        let rank = ((count as f64 * 0.95).ceil() as usize)
            .saturating_sub(1)
            .min(count.saturating_sub(1));
        let p95 = per_patch.get(rank).copied().unwrap_or(0.0);
        let max = per_patch.last().copied().unwrap_or(0.0);
        Self {
            per_patch,
            mean,
            median,
            p95,
            max,
        }
    }

    pub fn len(&self) -> usize {
        self.per_patch.len()
    }

    pub fn is_empty(&self) -> bool {
        self.per_patch.is_empty()
    }
}

/// Reference tristimulus values, adapted to D65, keyed by patch id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChartReference {
    patches: Vec<(u16, Xyz)>,
}

impl ChartReference {
    /// The training chart under a blackbody at `cct`.
    pub fn colorchecker24(cct: Cct) -> Self {
        let data = training_data();
        Self::from_spectral(data, &blackbody(data.grid(), cct.kelvin()))
    }

    pub fn colorchecker24_d65() -> Self {
        let data = training_data();
        Self::from_spectral(data, data.illuminant(SsfIlluminant::D65))
    }

    fn from_spectral(data: &SpectralData, spd: &[f64]) -> Self {
        let white = data.white_xyz(spd);
        let mut patches = Vec::with_capacity(data.patch_count());
        for index in 0..data.patch_count() {
            let Some(reflectance) = data.reflectance(index) else {
                continue;
            };
            let xyz = looklab_color::cat::adapt(
                data.xyz_under(spd, reflectance),
                Cat::Bradford,
                white,
                reference_white(),
            );
            patches.push((index as u16, xyz));
        }
        Self { patches }
    }

    pub fn patch(&self, id: PatchId) -> Option<Xyz> {
        self.patch_by_id(id.get())
    }

    pub fn patch_by_id(&self, id: u16) -> Option<Xyz> {
        self.patches
            .iter()
            .find(|(patch, _)| *patch == id)
            .map(|(_, xyz)| *xyz)
    }

    pub fn len(&self) -> usize {
        self.patches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.patches.is_empty()
    }
}

/// Planck's law, unnormalized, from `colour.sd_blackbody`.
fn blackbody(grid: &[f64], kelvin: f64) -> Vec<f64> {
    grid.iter()
        .map(|wavelength| {
            let metres = wavelength * 1.0e-9;
            let exponent = C2_METRE_KELVIN / (metres * kelvin);
            1.0 / (libm::pow(metres, 5.0) * libm::expm1(exponent))
        })
        .collect()
}

/// CIEDE2000 differences of `transform` against `reference` at `cct`.
pub fn chart_error(
    transform: &InputTransform,
    cct: Cct,
    ids: &[PatchId],
    rgb: &[[f64; 3]],
    reference: &ChartReference,
) -> Result<ChartError> {
    if rgb.is_empty() {
        return Err(BodyError::EmptyChart);
    }
    if rgb.len() != ids.len() {
        return Err(BodyError::EmptyChart);
    }
    let mut per_patch = Vec::with_capacity(ids.len());
    for (id, rgb) in ids.iter().zip(rgb) {
        let expected = reference
            .patch(*id)
            .ok_or(BodyError::MissingChartReference { id: id.get() })?;
        let predicted = Xyz::from_array(transform.camera_to_xyz_at_cct(*rgb, cct));
        let white = reference_white();
        let delta = delta_e_ciede2000(xyz_to_lab(predicted, white), xyz_to_lab(expected, white));
        if !delta.is_finite() {
            return Err(BodyError::NonFinite {
                what: "chart delta e",
            });
        }
        per_patch.push(delta);
    }
    Ok(ChartError::new(per_patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blackbody_white_is_on_the_locus() {
        let data = training_data();
        for kelvin in [2856.0, 5000.0, 6504.0] {
            let white = data.white_xyz(&blackbody(data.grid(), kelvin));
            let (x, y) = looklab_color::xyy::xyz_to_xy(white);
            let solved = looklab_color::dng::xy_to_cct_robertson(x, y);
            assert!((solved - kelvin).abs() < 60.0);
        }
    }

    #[test]
    fn reference_covers_the_training_patches() {
        let reference = ChartReference::colorchecker24_d65();
        assert_eq!(reference.len(), training_data().patch_count());
        let white = reference.patch_by_id(18).expect("white patch is patch 18");
        assert!(white.y > 0.7);
    }

    #[test]
    fn error_statistics_are_ordered() {
        let error = ChartError::new(vec![4.0, 1.0, 3.0, 2.0, 5.0]);
        assert_eq!(error.per_patch, vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(error.median, 3.0);
        assert_eq!(error.max, 5.0);
        assert_eq!(error.p95, 5.0);
        assert!((error.mean - 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_missing_reference_patch_is_reported() {
        let reference = ChartReference::colorchecker24_d65();
        let ids = [PatchId::new(24)];
        let rgb = [[0.5, 0.5, 0.5]];
        let transform = crate::transform::InputTransform::prior(
            crate::transform::BodyId::new("test/body").expect("non-empty"),
            crate::transform::TransformVersion::new(1),
            crate::transform::BodyPrior {
                source: crate::transform::A0Source::Db,
                a0_a: looklab_color::Mat3::IDENTITY,
                a0_d65: looklab_color::Mat3::IDENTITY,
            },
        );
        assert!(matches!(
            chart_error(&transform, Cct::D65, &ids, &rgb, &reference),
            Err(BodyError::MissingChartReference { id: 24 })
        ));
    }
}
