// SPDX-License-Identifier: AGPL-3.0-or-later
//! Chart anchoring of the input-transform delta (docs/02 §2, docs/05 §5, §7).
//!
//! A chart session contributes a robust likelihood over the six delta
//! parameters. [`chart_anchor_terms`] returns its value, gradient and
//! Gauss-Newton Hessian for the fit to add to its objective;
//! [`anchor_from_charts`] solves the posterior from endorsed sessions only.

use looklab_color::xyy::xyz_to_xy;
use looklab_model::solve::reference_white;
use looklab_samples::{PatchId, PatchTable};
use serde::{Deserialize, Serialize};

use crate::cct::Cct;
use crate::chart::{ChartError, ChartReference, chart_error};
use crate::delta::{Cov6, Delta6, PARAMETERS};
use crate::error::{BodyError, Result};
use crate::linalg;
use crate::transform::InputTransform;

/// Residual scale in XYZ units, where white has `Y = 1` (tentative).
pub const DEFAULT_SIGMA: f64 = 0.02;
pub const DEFAULT_NU: f64 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnchorOptions {
    nu: f64,
    sigma: f64,
    max_iterations: usize,
}

impl AnchorOptions {
    pub fn new(nu: f64, sigma: f64, max_iterations: usize) -> Result<Self> {
        if !(nu.is_finite() && nu > 0.0) {
            return Err(BodyError::InvalidAnchorOptions { what: "nu" });
        }
        if !(sigma.is_finite() && sigma > 0.0) {
            return Err(BodyError::InvalidAnchorOptions { what: "sigma" });
        }
        if max_iterations == 0 || max_iterations > 1000 {
            return Err(BodyError::InvalidAnchorOptions {
                what: "max_iterations",
            });
        }
        Ok(Self {
            nu,
            sigma,
            max_iterations,
        })
    }

    pub fn nu(self) -> f64 {
        self.nu
    }

    pub fn sigma(self) -> f64 {
        self.sigma
    }

    pub fn max_iterations(self) -> usize {
        self.max_iterations
    }
}

impl Default for AnchorOptions {
    fn default() -> Self {
        Self {
            nu: DEFAULT_NU,
            sigma: DEFAULT_SIGMA,
            max_iterations: 12,
        }
    }
}

/// One chart session: white-balanced camera RGB means at a known CCT, with the
/// reference tristimulus values they should map to.
#[derive(Clone, Debug, PartialEq)]
pub struct ChartSession {
    cct: Cct,
    rgb: Vec<[f64; 3]>,
    ids: Vec<PatchId>,
    reference: ChartReference,
    weight: f64,
    endorsed: bool,
}

impl ChartSession {
    pub fn from_patch_table(
        table: &PatchTable,
        cct: Cct,
        reference: ChartReference,
        weight: f64,
        endorsed: bool,
    ) -> Result<Self> {
        if !weight.is_finite() || weight < 0.0 {
            return Err(BodyError::InvalidWeight);
        }
        if table.rows().is_empty() {
            return Err(BodyError::EmptyChart);
        }
        let mut rgb = Vec::with_capacity(table.rows().len());
        let mut ids = Vec::with_capacity(table.rows().len());
        for row in table.rows() {
            if reference.patch(row.id).is_none() {
                return Err(BodyError::MissingChartReference { id: row.id.get() });
            }
            ids.push(row.id);
            rgb.push([
                f64::from(row.in_mean[0]),
                f64::from(row.in_mean[1]),
                f64::from(row.in_mean[2]),
            ]);
        }
        let gain = exposure_gain(&rgb, &ids, &reference);
        for value in &mut rgb {
            for channel in value {
                *channel *= gain;
            }
        }
        Ok(Self {
            cct,
            rgb,
            ids,
            reference,
            weight,
            endorsed,
        })
    }

    pub fn cct(&self) -> Cct {
        self.cct
    }

    pub fn weight(&self) -> f64 {
        self.weight
    }

    pub fn endorsed(&self) -> bool {
        self.endorsed
    }

    pub fn patch_count(&self) -> usize {
        self.rgb.len()
    }

    pub fn error(&self, transform: &InputTransform) -> Result<ChartError> {
        chart_error(transform, self.cct, &self.ids, &self.rgb, &self.reference)
    }

    fn observations(&self) -> impl Iterator<Item = ([f64; 3], [f64; 3])> + '_ {
        self.rgb.iter().zip(&self.ids).filter_map(move |(rgb, id)| {
            self.reference.patch(*id).map(|xyz| (*rgb, xyz.to_array()))
        })
    }
}

/// Chromaticity distance from the reference white that still counts as neutral.
const NEUTRAL_CHROMA_TOLERANCE: f64 = 0.02;

/// Scalar gain putting the neutral patches on the reference's perfect-diffuser
/// scale. Homogeneous of degree -1 in `rgb`.
fn exposure_gain(rgb: &[[f64; 3]], ids: &[PatchId], reference: &ChartReference) -> f64 {
    let (white_x, white_y) = xyz_to_xy(reference_white());
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    let mut brightest = (0.0_f64, 0.0_f64);
    for (value, id) in rgb.iter().zip(ids) {
        let Some(xyz) = reference.patch(*id) else {
            continue;
        };
        let measured = (value[0] + value[1] + value[2]) / 3.0;
        let (x, y) = xyz_to_xy(xyz);
        if (x - white_x).abs() + (y - white_y).abs() <= NEUTRAL_CHROMA_TOLERANCE {
            numerator += measured * xyz.y;
            denominator += measured * measured;
        }
        if xyz.y > brightest.1 {
            brightest = (measured, xyz.y);
        }
    }
    let gain = if denominator > 0.0 {
        numerator / denominator
    } else if brightest.0 > 0.0 {
        brightest.1 / brightest.0
    } else {
        1.0
    };
    if gain.is_finite() && gain > 0.0 {
        gain
    } else {
        1.0
    }
}

/// Value, gradient and Gauss-Newton Hessian of the chart likelihood, in
/// [`Delta6`] coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct AnchorTerms {
    pub value: f64,
    pub gradient: [f64; PARAMETERS],
    pub hessian: [[f64; PARAMETERS]; PARAMETERS],
    pub patches: usize,
}

impl AnchorTerms {
    pub fn zero() -> Self {
        Self {
            value: 0.0,
            gradient: [0.0; PARAMETERS],
            hessian: [[0.0; PARAMETERS]; PARAMETERS],
            patches: 0,
        }
    }
}

/// Chart terms over every session, endorsed or not; weights come from
/// [`ChartSession::weight`].
pub fn chart_anchor_terms(
    transform: &InputTransform,
    sessions: &[ChartSession],
    options: &AnchorOptions,
) -> AnchorTerms {
    let sessions: Vec<&ChartSession> = sessions.iter().collect();
    terms_over(transform, &sessions, options)
}

fn terms_over(
    transform: &InputTransform,
    sessions: &[&ChartSession],
    options: &AnchorOptions,
) -> AnchorTerms {
    let mut terms = AnchorTerms::zero();
    let denominator = options.nu * options.sigma * options.sigma;
    let coefficient = (options.nu + 3.0) / 2.0;
    let curvature = 2.0 * coefficient / denominator;
    for session in sessions {
        let a0 = transform.a0_at(session.cct());
        for (rgb, reference) in session.observations() {
            let predicted = a0.mul_vec(transform.delta().apply(rgb));
            let residual = [
                predicted[0] - reference[0],
                predicted[1] - reference[1],
                predicted[2] - reference[2],
            ];
            let radius2 = dot(residual, residual);
            let scaled = radius2 / denominator;
            let weight = session.weight();
            terms.value += weight * coefficient * libm::log1p(scaled);
            let c = weight * curvature / (1.0 + scaled);
            let mut jacobian = [[0.0_f64; PARAMETERS]; 3];
            for index in 0..PARAMETERS {
                let column = a0.mul_vec(Delta6::basis(index).mul_vec(rgb));
                for row in 0..3 {
                    jacobian[row][index] = column[row];
                }
                terms.gradient[index] += c * dot(column, residual);
            }
            for index in 0..PARAMETERS {
                for other in index..PARAMETERS {
                    let mut sum = 0.0;
                    for row in 0..3 {
                        sum += jacobian[row][index] * jacobian[row][other];
                    }
                    terms.hessian[index][other] += c * sum;
                    if other != index {
                        terms.hessian[other][index] += c * sum;
                    }
                }
            }
            terms.patches += 1;
        }
    }
    terms
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnchorSolution {
    pub delta: Delta6,
    pub covariance: Cov6,
    pub before: ChartError,
    pub after: ChartError,
    pub endorsed_sessions: usize,
    pub patches: usize,
    pub iterations: usize,
}

/// Posterior delta and covariance from the endorsed sessions and the prior.
pub fn anchor_from_charts(
    prior: &InputTransform,
    sessions: &[ChartSession],
    options: &AnchorOptions,
) -> Result<AnchorSolution> {
    let endorsed: Vec<&ChartSession> = sessions.iter().filter(|s| s.endorsed()).collect();
    if endorsed.is_empty() {
        return Err(BodyError::NoEndorsedSessions);
    }
    let precision = prior.covariance().precision()?;
    let prior_params = prior.delta().params();
    let evaluate = |delta: Delta6| {
        let transform = prior.with_delta(delta, prior.covariance());
        let mut terms = terms_over(&transform, &endorsed, options);
        let mut difference = [0.0_f64; PARAMETERS];
        for (index, value) in delta.params().iter().enumerate() {
            difference[index] = value - prior_params[index];
        }
        let mut precision_times = [0.0_f64; PARAMETERS];
        let mut quadratic = 0.0;
        for row in 0..PARAMETERS {
            for column in 0..PARAMETERS {
                precision_times[row] += precision[row][column] * difference[column];
            }
            quadratic += difference[row] * precision_times[row];
        }
        terms.value += 0.5 * quadratic;
        for index in 0..PARAMETERS {
            terms.gradient[index] += precision_times[index];
            for other in 0..PARAMETERS {
                terms.hessian[index][other] += precision[index][other];
            }
        }
        terms
    };

    let mut delta = prior.delta();
    let mut terms = evaluate(delta);
    let mut iterations = 0;
    for _ in 0..options.max_iterations() {
        let gradient_norm = terms
            .gradient
            .iter()
            .fold(0.0_f64, |norm, value| norm.max(value.abs()));
        if gradient_norm < 1e-12 {
            break;
        }
        let mut a = terms.hessian;
        let mut b = [0.0_f64; PARAMETERS];
        for (index, value) in terms.gradient.iter().enumerate() {
            b[index] = -value;
        }
        let Some(step) = linalg::solve(&mut a, &mut b) else {
            break;
        };
        let mut scale = 1.0;
        let mut accepted = false;
        for _ in 0..16 {
            let mut params = delta.params();
            for (index, value) in step.iter().enumerate() {
                params[index] += scale * value;
            }
            if let Ok(candidate) = Delta6::new(params) {
                let candidate_terms = evaluate(candidate);
                if candidate_terms.value < terms.value {
                    delta = candidate;
                    terms = candidate_terms;
                    accepted = true;
                    break;
                }
            }
            scale *= 0.5;
        }
        iterations += 1;
        if !accepted {
            break;
        }
    }

    let covariance = Cov6::new(linalg::invert(&terms.hessian).ok_or(BodyError::Singular {
        what: "anchor hessian",
    })?)?;
    let before = combined_error(&endorsed, prior)?;
    let anchored = prior.with_delta(delta, covariance);
    let after = combined_error(&endorsed, &anchored)?;
    let patches = after.len();
    Ok(AnchorSolution {
        delta,
        covariance,
        before,
        after,
        endorsed_sessions: endorsed.len(),
        patches,
        iterations,
    })
}

fn combined_error(sessions: &[&ChartSession], transform: &InputTransform) -> Result<ChartError> {
    let mut per_patch = Vec::new();
    for session in sessions {
        per_patch.extend(session.error(transform)?.per_patch);
    }
    Ok(ChartError::new(per_patch))
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectral::{SsfIlluminant, camera_responses, synthetic_sensitivity, training_data};
    use crate::transform::{
        A0Source, BodyId, BodyPrior, TransformVersion, prior_covariance, prior_from_ssf,
    };
    use looklab_color::Mat3;
    use looklab_samples::{InputSpace, OutputSpace, PatchRow, PatchTableHeader, PrepVersion};

    fn body() -> BodyId {
        BodyId::new("test/body").expect("non-empty")
    }

    fn session_for(
        ssf: &crate::spectral::SpectralSensitivity,
        cct: Cct,
        endorsed: bool,
    ) -> ChartSession {
        session_with(ssf, cct, 1.0, 1.0, endorsed)
    }

    fn session_with(
        ssf: &crate::spectral::SpectralSensitivity,
        cct: Cct,
        exposure: f64,
        weight: f64,
        endorsed: bool,
    ) -> ChartSession {
        let data = training_data();
        let responses = camera_responses(ssf, SsfIlluminant::D65, data).expect("positive white");
        let rows: Vec<PatchRow> = responses
            .iter()
            .enumerate()
            .map(|(index, rgb)| PatchRow {
                id: PatchId::new(index as u16),
                in_mean: [
                    (rgb[0] * exposure) as f32,
                    (rgb[1] * exposure) as f32,
                    (rgb[2] * exposure) as f32,
                ],
                in_std: [0.0; 3],
                out_mean: [0.0; 3],
                out_std: [0.0; 3],
            })
            .collect();
        let table = PatchTable::new(
            PatchTableHeader {
                n: rows.len() as u32,
                input_space: InputSpace::CameraRgbWbLinear,
                output_space: OutputSpace::Srgb,
                prep_version: PrepVersion::new(1),
            },
            rows,
        )
        .expect("valid patch table");
        let reference = if cct == Cct::D65 {
            ChartReference::colorchecker24_d65()
        } else {
            ChartReference::colorchecker24(cct)
        };
        ChartSession::from_patch_table(&table, cct, reference, weight, endorsed)
            .expect("reference covers every patch")
    }

    fn true_sensitivity() -> crate::spectral::SpectralSensitivity {
        synthetic_sensitivity(training_data().grid(), [610.0, 545.0, 455.0], 45.0)
    }

    #[test]
    fn anchoring_reduces_the_chart_error() {
        let data = training_data();
        let misspecified = synthetic_sensitivity(data.grid(), [610.0, 545.0, 455.0], 55.0);
        let loose = prior_from_ssf(&misspecified, data).expect("well-formed");
        let prior = BodyPrior {
            source: A0Source::Db,
            a0_a: loose.a0_a,
            a0_d65: loose.a0_d65,
        };
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let session = session_for(&true_sensitivity(), Cct::D65, true);
        let solution =
            anchor_from_charts(&transform, &[session], &AnchorOptions::default()).expect("solves");
        println!(
            "anchoring: median {:.3} -> {:.3} dE",
            solution.before.median, solution.after.median
        );
        assert!(solution.after.median < solution.before.median);
        assert!(solution.after.median < 0.75 * solution.before.median);
        assert_eq!(solution.delta.matrix().mul_vec([1.0; 3]), [0.0; 3]);
        assert_eq!(solution.patches, training_data().patch_count());
    }

    #[test]
    fn exposure_scaling_does_not_change_the_delta() {
        let data = training_data();
        let misspecified = synthetic_sensitivity(data.grid(), [610.0, 545.0, 455.0], 55.0);
        let prior = prior_from_ssf(&misspecified, data).expect("well-formed");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let unscaled = session_with(&true_sensitivity(), Cct::D65, 1.0, 1.0, true);
        let scaled = session_with(&true_sensitivity(), Cct::D65, 2.5, 1.0, true);
        let base =
            anchor_from_charts(&transform, &[unscaled], &AnchorOptions::default()).expect("solves");
        let exposed =
            anchor_from_charts(&transform, &[scaled], &AnchorOptions::default()).expect("solves");
        let base_params = base.delta.params();
        let exposed_params = exposed.delta.params();
        for (base, exposed) in base_params.iter().zip(exposed_params) {
            assert!(
                (base - exposed).abs() < 1e-9,
                "{base_params:?} vs {exposed_params:?}"
            );
        }
    }

    #[test]
    fn re_anchoring_with_zero_weight_sessions_keeps_the_delta() {
        let prior = prior_from_ssf(&true_sensitivity(), training_data()).expect("well-formed");
        let delta = Delta6::new([0.02, -0.01, 0.015, -0.02, 0.01, 0.005]).expect("in range");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior)
            .with_delta(delta, prior_covariance(A0Source::Dng));
        let session = session_with(&true_sensitivity(), Cct::D65, 1.0, 0.0, true);
        let solution =
            anchor_from_charts(&transform, &[session], &AnchorOptions::default()).expect("solves");
        assert_eq!(solution.delta, delta);
    }

    #[test]
    fn unendorsed_sessions_do_not_define_the_gauge() {
        let prior = prior_from_ssf(&true_sensitivity(), training_data()).expect("well-formed");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let session = session_for(&true_sensitivity(), Cct::D65, false);
        assert!(matches!(
            anchor_from_charts(
                &transform,
                std::slice::from_ref(&session),
                &AnchorOptions::default()
            ),
            Err(BodyError::NoEndorsedSessions)
        ));
        let terms = chart_anchor_terms(&transform, &[session], &AnchorOptions::default());
        assert_eq!(terms.patches, training_data().patch_count());
    }

    #[test]
    fn gradient_matches_finite_differences() {
        let prior = BodyPrior {
            source: A0Source::Db,
            a0_a: Mat3::IDENTITY,
            a0_d65: Mat3::IDENTITY,
        };
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let session = session_for(&true_sensitivity(), Cct::D65, true);
        let options = AnchorOptions::default();
        let terms = chart_anchor_terms(&transform, std::slice::from_ref(&session), &options);
        let epsilon = 1e-6;
        for index in 0..PARAMETERS {
            let mut plus = Delta6::ZERO.params();
            plus[index] += epsilon;
            let mut minus = Delta6::ZERO.params();
            minus[index] -= epsilon;
            let value_plus = chart_anchor_terms(
                &transform.with_delta(
                    Delta6::new(plus).expect("in range"),
                    prior_covariance(A0Source::Db),
                ),
                std::slice::from_ref(&session),
                &options,
            )
            .value;
            let value_minus = chart_anchor_terms(
                &transform.with_delta(
                    Delta6::new(minus).expect("in range"),
                    prior_covariance(A0Source::Db),
                ),
                std::slice::from_ref(&session),
                &options,
            )
            .value;
            let finite_difference = (value_plus - value_minus) / (2.0 * epsilon);
            assert!((finite_difference - terms.gradient[index]).abs() < 1e-4);
        }
    }

    #[test]
    fn a_perturbed_session_still_recovers_the_prior() {
        let prior = prior_from_ssf(&true_sensitivity(), training_data()).expect("well-formed");
        let transform = InputTransform::prior(body(), TransformVersion::new(1), prior);
        let session = session_for(&true_sensitivity(), Cct::D65, true);
        let solution =
            anchor_from_charts(&transform, &[session], &AnchorOptions::default()).expect("solves");
        assert!(
            solution
                .delta
                .params()
                .iter()
                .all(|value| value.abs() < 1e-3)
        );
        assert_eq!(transform.anchored(&solution).source(), A0Source::Chart);
    }
}
