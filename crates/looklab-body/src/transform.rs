// SPDX-License-Identifier: AGPL-3.0-or-later
//! Body input transforms: the two prior matrices, the shared white-preserving
//! delta and its covariance (docs/02 §2).

use looklab_color::Mat3;
use serde::{Deserialize, Serialize};

use crate::anchor::AnchorSolution;
use crate::cct::{CCT_A, CCT_D65, CameraProfile, Cct};
use crate::delta::{Cov6, Delta6};
use crate::error::{BodyError, Result};
use crate::spectral::{
    SpectralData, SpectralSensitivity, SsfIlluminant, ssf_matrix, training_data,
};

/// Prior standard deviation per delta parameter (tentative).
pub const SSF_PRIOR_SD: f64 = 0.002;
pub const DNG_PRIOR_SD: f64 = 0.01;
pub const DB_PRIOR_SD: f64 = 0.04;
pub const CHART_PRIOR_SD: f64 = 0.005;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BodyId(String);

impl BodyId {
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        if id.is_empty() {
            return Err(BodyError::EmptyBodyId);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransformVersion(u32);

impl TransformVersion {
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

/// Provenance of the prior matrices, in the priority order of docs/02 §2.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum A0Source {
    Ssf,
    Dng,
    Db,
    Chart,
}

impl A0Source {
    pub fn prior_sd(self) -> f64 {
        match self {
            Self::Ssf => SSF_PRIOR_SD,
            Self::Dng => DNG_PRIOR_SD,
            Self::Db => DB_PRIOR_SD,
            Self::Chart => CHART_PRIOR_SD,
        }
    }
}

pub fn prior_covariance(source: A0Source) -> Cov6 {
    Cov6::isotropic(source.prior_sd())
}

/// The two prior matrices before any chart anchoring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyPrior {
    pub source: A0Source,
    pub a0_a: Mat3,
    pub a0_d65: Mat3,
}

/// Camera-to-XYZ(D65) at illuminant A and D65, with one shared delta.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputTransform {
    body: BodyId,
    version: TransformVersion,
    a0_a: Mat3,
    a0_d65: Mat3,
    a0_source: A0Source,
    delta: Delta6,
    covariance: Cov6,
}

impl InputTransform {
    pub fn prior(body: BodyId, version: TransformVersion, prior: BodyPrior) -> Self {
        Self {
            body,
            version,
            a0_a: prior.a0_a,
            a0_d65: prior.a0_d65,
            a0_source: prior.source,
            delta: Delta6::ZERO,
            covariance: prior_covariance(prior.source),
        }
    }

    pub fn body(&self) -> &BodyId {
        &self.body
    }

    pub fn version(&self) -> TransformVersion {
        self.version
    }

    pub fn a0_a(&self) -> &Mat3 {
        &self.a0_a
    }

    pub fn a0_d65(&self) -> &Mat3 {
        &self.a0_d65
    }

    pub fn source(&self) -> A0Source {
        self.a0_source
    }

    pub fn delta(&self) -> Delta6 {
        self.delta
    }

    pub fn covariance(&self) -> Cov6 {
        self.covariance
    }

    pub fn with_delta(&self, delta: Delta6, covariance: Cov6) -> Self {
        Self {
            delta,
            covariance,
            ..self.clone()
        }
    }

    pub fn anchored(&self, solution: &AnchorSolution) -> Self {
        Self {
            a0_source: A0Source::Chart,
            delta: solution.delta,
            covariance: solution.covariance,
            ..self.clone()
        }
    }

    /// `A(T) = A0(T)·(I + Δ)`, with `A0` interpolated in `1/T`.
    ///
    /// Interpolation follows the DNG specification 1.6 chapter 6 and
    /// `DngColorSpec::interpolation_fraction`.
    pub fn at_cct(&self, cct: Cct) -> Mat3 {
        self.a0_at(cct).mul(&self.delta.transform())
    }

    pub(crate) fn a0_at(&self, cct: Cct) -> Mat3 {
        let fraction = interpolation_fraction(cct.kelvin());
        add_scaled(&self.a0_a, &self.a0_d65, fraction, 1.0 - fraction)
    }

    /// `A0(T)·(I + Δ)·rgb`; the white is fixed by [`Delta6::apply`].
    pub fn camera_to_xyz_at_cct(&self, rgb: [f64; 3], cct: Cct) -> [f64; 3] {
        let neutral_fixed = self.delta.apply(rgb);
        let cooler = self.a0_a.mul_vec(neutral_fixed);
        let warmer = self.a0_d65.mul_vec(neutral_fixed);
        let fraction = interpolation_fraction(cct.kelvin());
        if fraction >= 1.0 {
            return cooler;
        }
        if fraction <= 0.0 {
            return warmer;
        }
        [
            fraction * cooler[0] + (1.0 - fraction) * warmer[0],
            fraction * cooler[1] + (1.0 - fraction) * warmer[1],
            fraction * cooler[2] + (1.0 - fraction) * warmer[2],
        ]
    }
}

fn interpolation_fraction(cct: f64) -> f64 {
    if cct <= CCT_A {
        return 1.0;
    }
    if cct >= CCT_D65 {
        return 0.0;
    }
    let inverse = 1.0 / cct;
    (inverse - 1.0 / CCT_D65) / (1.0 / CCT_A - 1.0 / CCT_D65)
}

fn add_scaled(a: &Mat3, b: &Mat3, wa: f64, wb: f64) -> Mat3 {
    let mut out = [[0.0_f64; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            out[row][column] = wa * a.0[row][column] + wb * b.0[row][column];
        }
    }
    Mat3(out)
}

pub fn prior_from_ssf(ssf: &SpectralSensitivity, data: &SpectralData) -> Result<BodyPrior> {
    Ok(BodyPrior {
        source: A0Source::Ssf,
        a0_a: ssf_matrix(ssf, SsfIlluminant::A, data)?,
        a0_d65: ssf_matrix(ssf, SsfIlluminant::D65, data)?,
    })
}

/// ForwardMatrix when the profile has one, else the inverted ColorMatrix.
pub fn prior_from_dng(profile: &CameraProfile) -> Result<BodyPrior> {
    let (a0_a, a0_d65) = profile.endpoints()?;
    Ok(BodyPrior {
        source: A0Source::Dng,
        a0_a,
        a0_d65,
    })
}

/// Both endpoints repeat the matrix when the database covers one illuminant.
pub fn prior_from_database(profile: &CameraProfile) -> Result<BodyPrior> {
    let (a0_a, a0_d65) = profile.endpoints()?;
    Ok(BodyPrior {
        source: A0Source::Db,
        a0_a,
        a0_d65,
    })
}

pub fn select_prior(
    ssf: Option<&SpectralSensitivity>,
    dng: Option<&CameraProfile>,
    database: Option<&CameraProfile>,
) -> Result<BodyPrior> {
    if let Some(ssf) = ssf
        && let Ok(prior) = prior_from_ssf(ssf, training_data())
    {
        return Ok(prior);
    }
    if let Some(profile) = dng
        && let Ok(prior) = prior_from_dng(profile)
    {
        return Ok(prior);
    }
    if let Some(profile) = database {
        return prior_from_database(profile);
    }
    Err(BodyError::NoMatrices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectral::synthetic_sensitivity;

    fn body() -> BodyId {
        BodyId::new("test/body").expect("non-empty")
    }

    fn synthetic_prior() -> BodyPrior {
        let data = training_data();
        prior_from_ssf(
            &synthetic_sensitivity(data.grid(), [610.0, 545.0, 455.0], 45.0),
            data,
        )
        .expect("well-formed sensitivity")
    }

    #[test]
    fn white_is_preserved_exactly_at_both_endpoints() {
        let prior = synthetic_prior();
        let delta = Delta6::new([0.02, -0.01, 0.03, 0.01, -0.02, 0.04]).expect("in range");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior)
            .with_delta(delta, prior_covariance(A0Source::Ssf));
        assert_eq!(
            transform.camera_to_xyz_at_cct([1.0; 3], Cct::A),
            prior.a0_a.mul_vec([1.0; 3])
        );
        assert_eq!(
            transform.camera_to_xyz_at_cct([1.0; 3], Cct::D65),
            prior.a0_d65.mul_vec([1.0; 3])
        );
    }

    #[test]
    fn at_cct_hits_the_priors_at_the_endpoints() {
        let prior = synthetic_prior();
        let delta = Delta6::new([0.01, 0.0, -0.02, 0.03, 0.0, -0.01]).expect("in range");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior)
            .with_delta(delta, prior_covariance(A0Source::Ssf));
        assert_eq!(transform.at_cct(Cct::A), prior.a0_a.mul(&delta.transform()));
        assert_eq!(
            transform.at_cct(Cct::D65),
            prior.a0_d65.mul(&delta.transform())
        );
    }

    #[test]
    fn at_cct_interpolates_in_inverse_temperature() {
        let prior = synthetic_prior();
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let cct = Cct::from_kelvin(4500.0).expect("in range");
        let expected = add_scaled(
            &prior.a0_a,
            &prior.a0_d65,
            interpolation_fraction(cct.kelvin()),
            1.0 - interpolation_fraction(cct.kelvin()),
        );
        assert_eq!(transform.at_cct(cct), expected);
    }

    #[test]
    fn source_priority_prefers_spectral_over_profile_over_database() {
        let ssf = synthetic_sensitivity(training_data().grid(), [610.0, 545.0, 455.0], 45.0);
        let identity = Mat3::IDENTITY;
        let profile = CameraProfile::new(
            vec![(crate::cct::CalibrationIlluminant::D65, identity)],
            Vec::new(),
        )
        .expect("one matrix");
        let prior = select_prior(Some(&ssf), Some(&profile), Some(&profile)).expect("spectral");
        assert_eq!(prior.source, A0Source::Ssf);

        let prior = select_prior(None, Some(&profile), Some(&profile)).expect("profile");
        assert_eq!(prior.source, A0Source::Dng);

        let prior = select_prior(None, None, Some(&profile)).expect("database");
        assert_eq!(prior.source, A0Source::Db);
        assert_eq!(prior.a0_a, prior.a0_d65);

        assert!(matches!(
            select_prior(None, None, None),
            Err(BodyError::NoMatrices)
        ));
    }

    #[test]
    fn prior_covariance_is_tighter_for_spectral_than_database() {
        let ssf = prior_covariance(A0Source::Ssf).standard_deviations()[0];
        let dng = prior_covariance(A0Source::Dng).standard_deviations()[0];
        let db = prior_covariance(A0Source::Db).standard_deviations()[0];
        assert!(ssf < dng && dng < db);
    }

    #[test]
    fn input_transform_round_trips_through_json() {
        let transform = InputTransform::prior(body(), TransformVersion::new(7), synthetic_prior());
        let text = serde_json::to_string(&transform).expect("serializes");
        let parsed: InputTransform = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(parsed.body(), transform.body());
        assert_eq!(parsed.version(), transform.version());
        assert_eq!(parsed.source(), transform.source());
        assert_eq!(parsed.delta(), transform.delta());
        assert_eq!(parsed.covariance(), transform.covariance());
        let original = transform.at_cct(crate::cct::Cct::D65);
        let recovered = parsed.at_cct(crate::cct::Cct::D65);
        for row in 0..3 {
            for column in 0..3 {
                assert!((original.0[row][column] - recovered.0[row][column]).abs() < 1e-12);
            }
        }
    }
}
