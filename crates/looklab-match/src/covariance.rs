// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use looklab_color::Xyz;
use looklab_model::realization::LayerDelta;
use looklab_model::realization::{
    ControlResponseModel, ControlValue, ResponseComponent, ResponseParamId, Settings,
};
use looklab_model::{CoefTerm, Look, LookInput, realization};
use serde::{Deserialize, Serialize};

use crate::error::{MatchError, Result};
use crate::metric::ScoringMetric;
use crate::model::MatchModel;

pub const MAX_RANK: usize = 20;

/// Sparse additive delta over `Look::coef_vector`, sorted by coefficient index.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SparseDelta {
    pub terms: Vec<(usize, f64)>,
}

impl SparseDelta {
    pub fn from_entries(mut entries: Vec<(usize, f64)>) -> Self {
        entries.retain(|(_, value)| *value != 0.0 && value.is_finite());
        entries.sort_by_key(|entry| entry.0);
        let mut merged: Vec<(usize, f64)> = Vec::with_capacity(entries.len());
        for (index, value) in entries {
            match merged.last_mut() {
                Some((last, total)) if *last == index => *total += value,
                _ => merged.push((index, value)),
            }
        }
        merged.retain(|(_, value)| *value != 0.0);
        Self { terms: merged }
    }

    pub fn project(&self, jacobian: &[CoefTerm]) -> [f64; 3] {
        let mut out = [0.0_f64; 3];
        let mut cursor = 0;
        for term in jacobian {
            while cursor < self.terms.len() && self.terms[cursor].0 < term.index {
                cursor += 1;
            }
            if cursor < self.terms.len() && self.terms[cursor].0 == term.index {
                for (slot, derivative) in out.iter_mut().zip(term.derivative.iter()) {
                    *slot += self.terms[cursor].1 * derivative;
                }
                cursor += 1;
            }
        }
        out
    }

    pub fn project_checked(&self, jacobian: &[CoefTerm]) -> Option<[f64; 3]> {
        let out = self.project(jacobian);
        out.iter().all(|value| value.is_finite()).then_some(out)
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CovTerm {
    pub eigenvalue: f64,
    pub delta: SparseDelta,
    /// Logarithmic camera-space gain in R and B.
    pub wb: [f64; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct CovarianceGroup {
    pub key: looklab_schema::ControlKey,
    pub value: ControlValue,
    pub terms: Vec<CovTerm>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CovarianceFormat {
    #[serde(rename = "looklab-crm-cov/1")]
    V1,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CovarianceFile {
    pub format: CovarianceFormat,
    pub parameter_ids: Vec<ResponseParamId>,
    /// Row-major over `parameter_ids`.
    pub covariance: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlCovariance {
    pub parameter_ids: Vec<ResponseParamId>,
    pub covariance: Vec<f64>,
}

impl ControlCovariance {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let file: CovarianceFile =
            serde_json::from_slice(bytes).map_err(|error| MatchError::Schema(error.to_string()))?;
        if file.format != CovarianceFormat::V1 {
            return Err(MatchError::Schema("covariance format".to_string()));
        }
        let covariance = Self {
            parameter_ids: file.parameter_ids,
            covariance: file.covariance,
        };
        covariance.validate()?;
        Ok(covariance)
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let file = CovarianceFile {
            format: CovarianceFormat::V1,
            parameter_ids: self.parameter_ids.clone(),
            covariance: self.covariance.clone(),
        };
        serde_json::to_vec_pretty(&file).map_err(|error| MatchError::Schema(error.to_string()))
    }

    pub fn validate(&self) -> Result<()> {
        let n = self.parameter_ids.len();
        if self.covariance.len() != n.saturating_mul(n) {
            return Err(MatchError::Dimension(format!(
                "covariance has {} values for {n} parameters",
                self.covariance.len()
            )));
        }
        if self.covariance.iter().any(|value| !value.is_finite()) {
            return Err(MatchError::Schema("non-finite covariance".to_string()));
        }
        Ok(())
    }

    pub fn groups(
        &self,
        look: &Look,
        model: &ControlResponseModel,
    ) -> Result<Vec<CovarianceGroup>> {
        self.validate()?;
        if self.parameter_ids != model.parameter_ids() {
            return Err(MatchError::Schema(
                "covariance parameter ids do not match the control-response model".to_string(),
            ));
        }
        let n = self.parameter_ids.len();
        let mut buckets: BTreeMap<(looklab_schema::ControlKey, ControlValue), Vec<usize>> =
            BTreeMap::new();
        for (index, id) in self.parameter_ids.iter().enumerate() {
            buckets
                .entry((id.control.clone(), id.value.clone()))
                .or_default()
                .push(index);
        }

        let mut groups = Vec::with_capacity(buckets.len());
        for ((key, value), indices) in buckets {
            let size = indices.len();
            let mut block = vec![vec![0.0_f64; size]; size];
            for (row, &first) in indices.iter().enumerate() {
                for (column, &second) in indices.iter().enumerate() {
                    let a = self.covariance[first * n + second];
                    let b = self.covariance[second * n + first];
                    block[row][column] = 0.5 * (a + b);
                }
            }
            let (eigenvalues, eigenvectors) = jacobi(&mut block);
            let mut order: Vec<usize> = (0..size).collect();
            order.sort_by(|left, right| eigenvalues[*right].total_cmp(&eigenvalues[*left]));
            order.truncate(MAX_RANK);
            let mut terms = Vec::new();
            for index in order {
                let eigenvalue = eigenvalues[index];
                if eigenvalue <= 0.0 {
                    continue;
                }
                let mut entries = Vec::new();
                let mut wb = [0.0_f64; 2];
                for (local, &parameter) in indices.iter().enumerate() {
                    let weight = eigenvectors[local][index];
                    if weight == 0.0 {
                        continue;
                    }
                    let id = &self.parameter_ids[parameter];
                    match id.component {
                        ResponseComponent::WbR => wb[0] += weight,
                        ResponseComponent::WbB => wb[1] += weight,
                        other => {
                            let Some(layer) = other.layer() else {
                                continue;
                            };
                            let delta = looklab_model::realization::Delta {
                                layers: vec![LayerDelta {
                                    layer,
                                    entries: vec![(id.feature, weight)],
                                }],
                            };
                            entries.extend(delta.coefficient_entries(look));
                        }
                    }
                }
                let delta = SparseDelta::from_entries(entries);
                if delta.is_empty() && wb == [0.0, 0.0] {
                    continue;
                }
                terms.push(CovTerm {
                    eigenvalue,
                    delta,
                    wb,
                });
            }
            if !terms.is_empty() {
                groups.push(CovarianceGroup { key, value, terms });
            }
        }
        Ok(groups)
    }
}

#[allow(clippy::needless_range_loop)]
fn jacobi(matrix: &mut [Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n = matrix.len();
    let mut vectors = vec![vec![0.0_f64; n]; n];
    for (index, row) in vectors.iter_mut().enumerate() {
        row[index] = 1.0;
    }
    for _ in 0..64 {
        let mut off_diagonal = 0.0_f64;
        for row in 0..n {
            for column in row + 1..n {
                off_diagonal += matrix[row][column] * matrix[row][column];
            }
        }
        if off_diagonal < 1e-24 {
            break;
        }
        for p in 0..n {
            for q in p + 1..n {
                let apq = matrix[p][q];
                if apq.abs() < 1e-300 {
                    continue;
                }
                let app = matrix[p][p];
                let aqq = matrix[q][q];
                let theta = 0.5 * (aqq - app) / apq;
                let tau = if theta >= 0.0 {
                    1.0 / (theta + (1.0 + theta * theta).sqrt())
                } else {
                    -1.0 / (-theta + (1.0 + theta * theta).sqrt())
                };
                let cosine = 1.0 / (1.0 + tau * tau).sqrt();
                let sine = tau * cosine;
                for row in 0..n {
                    let mrp = matrix[row][p];
                    let mrq = matrix[row][q];
                    matrix[row][p] = cosine * mrp - sine * mrq;
                    matrix[row][q] = sine * mrp + cosine * mrq;
                }
                for column in 0..n {
                    let mp = matrix[p][column];
                    let mq = matrix[q][column];
                    matrix[p][column] = cosine * mp - sine * mq;
                    matrix[q][column] = sine * mp + cosine * mq;
                }
                for row in vectors.iter_mut() {
                    let vp = row[p];
                    let vq = row[q];
                    row[p] = cosine * vp - sine * vq;
                    row[q] = sine * vp + cosine * vq;
                }
            }
        }
    }
    let eigenvalues = (0..n).map(|index| matrix[index][index]).collect();
    (eigenvalues, vectors)
}

/// `tr Cov_pred(k,u,p)`, including the extrapolation inflation of docs/04 §2.
pub fn predicted_trace(
    model: &MatchModel,
    settings: &Settings,
    metric: ScoringMetric,
    xyz: Xyz,
) -> Result<Option<f64>> {
    let realization = model.realize(settings)?;
    trace_with(model, &realization, settings, metric, xyz)
}

/// `tr Cov_pred(k,u,p)`, or `None` where the rendered derivative is unavailable.
pub fn realized_trace(
    model: &MatchModel,
    realization: &realization::Realization,
    settings: &Settings,
    metric: ScoringMetric,
    xyz: Xyz,
) -> Result<Option<f64>> {
    trace_with(model, realization, settings, metric, xyz)
}

pub(crate) fn trace_with(
    model: &MatchModel,
    realization: &realization::Realization,
    settings: &Settings,
    metric: ScoringMetric,
    xyz: Xyz,
) -> Result<Option<f64>> {
    if model.groups.is_empty() {
        return Ok(Some(0.0));
    }
    let mut active = false;
    for group in &model.groups {
        if value_weight(&model.crm, settings, &group.key, &group.value) != 0.0 {
            active = true;
            break;
        }
    }
    if !active {
        return Ok(Some(0.0));
    }
    let input = LookInput::new(model.input_xyz(realization, xyz));
    let jacobian = realization.look.coef_jacobian(&input);
    let render_jacobian = match metric {
        ScoringMetric::LegacyPreClipV1 => None,
        ScoringMetric::RenderedPostClipV1 => {
            let pre = model.pre_clip_ucs(realization, xyz, [0.0, 0.0], 0.0);
            match model.output_pin(realization).render_jacobian(pre) {
                Some(jacobian) => Some(jacobian),
                None => return Ok(None),
            }
        }
    };
    let mut trace = 0.0_f64;
    let mut derivatives: Option<([f64; 3], [f64; 3], [f64; 3])> = None;
    for group in &model.groups {
        let weight = value_weight(&model.crm, settings, &group.key, &group.value);
        if weight == 0.0 {
            continue;
        }
        for term in &group.terms {
            let Some(mut vector) = term.delta.project_checked(&jacobian) else {
                return Ok(None);
            };
            if let Some(render_jacobian) = &render_jacobian {
                vector = render_jacobian.mul_vec(vector);
            }
            if term.wb != [0.0, 0.0] {
                let (d_r, d_b, _) = match derivatives {
                    Some(value) => value,
                    None => {
                        let Some(value) = wb_derivatives(model, realization, metric, xyz) else {
                            return Ok(None);
                        };
                        derivatives = Some(value);
                        value
                    }
                };
                for channel in 0..3 {
                    vector[channel] += term.wb[0] * d_r[channel] + term.wb[1] * d_b[channel];
                }
            }
            let norm = vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2];
            trace += weight * weight * term.eigenvalue * norm;
        }
    }
    Ok(Some(trace * model.crm.support.inflation(settings)))
}

fn value_weight(
    model: &ControlResponseModel,
    settings: &Settings,
    key: &looklab_schema::ControlKey,
    value: &ControlValue,
) -> f64 {
    model
        .value_weights(key, settings)
        .into_iter()
        .find(|(entry, _)| entry == value)
        .map(|(_, weight)| weight)
        .unwrap_or(0.0)
}

fn wb_derivatives(
    model: &MatchModel,
    realization: &realization::Realization,
    metric: ScoringMetric,
    xyz: Xyz,
) -> Option<([f64; 3], [f64; 3], [f64; 3])> {
    let step = 1e-4;
    let evaluate = |wb: [f64; 2], exposure: f64| {
        model.try_scored_ucs_shifted(metric, realization, xyz, wb, exposure)
    };
    let base = evaluate([0.0, 0.0], 0.0)?;
    let red = evaluate([step, 0.0], 0.0)?;
    let blue = evaluate([0.0, step], 0.0)?;
    let exposure = evaluate([0.0, 0.0], step)?;
    Some((
        [
            (red.j - base.j) / step,
            (red.a - base.a) / step,
            (red.b - base.b) / step,
        ],
        [
            (blue.j - base.j) / step,
            (blue.a - base.a) / step,
            (blue.b - base.b) / step,
        ],
        [
            (exposure.j - base.j) / step,
            (exposure.a - base.a) / step,
            (exposure.b - base.b) / step,
        ],
    ))
}
