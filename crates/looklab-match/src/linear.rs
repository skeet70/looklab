// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::realization::{Realization, Settings};

use crate::covariance::SparseDelta;
use crate::error::Result;
use crate::model::MatchModel;
use crate::settings::Plan;
use crate::target::PreparedTarget;

#[derive(Clone, Debug, Default)]
pub struct ProbeSubset {
    pub indices: Vec<usize>,
    pub weights: Vec<f64>,
    pub covariance_weights: Vec<f64>,
}

/// Per-probe linearization of a base sim at one settings point (docs/04 §3
/// step 3).
#[derive(Clone, Debug)]
pub(crate) struct RealizationScratch {
    pub look: looklab_model::Look,
    pub coef: Vec<f64>,
    pub base_coef: Vec<f64>,
}

#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub delta: SparseDelta,
    /// Camera-space log-gain increment from the control.
    pub wb: [f64; 2],
    pub exposure: f64,
}

#[derive(Clone, Debug)]
pub struct LinearCache {
    pub model: MatchModel,
    pub sim: looklab_schema::SimKey,
    pub settings: Settings,
    pub indices: Vec<usize>,
    pub weights: Vec<f64>,
    pub covariance_weights: Vec<f64>,
    pub target: Vec<[f64; 3]>,
    pub output: Vec<[f64; 3]>,
    /// `[probe][control][value]`, relative to `settings`.
    pub contributions: Vec<Vec<Vec<[f64; 3]>>>,
    /// `[probe][control][value]`, absolute predicted trace contribution.
    pub covariance: Vec<Vec<Vec<f64>>>,
    pub beta: f64,
    pub support: looklab_model::realization::Support,
    pub(crate) rows: Vec<Vec<Row>>,
}

struct Derivatives {
    d_r: [f64; 3],
    d_b: [f64; 3],
    d_e: [f64; 3],
}

impl LinearCache {
    pub fn new(
        model: &MatchModel,
        plan: &Plan,
        target: &PreparedTarget<'_>,
        settings: &Settings,
        subset: &ProbeSubset,
        beta: f64,
    ) -> Result<Self> {
        let realization = model.realize(settings)?;
        let mut output = Vec::with_capacity(subset.indices.len());
        let mut target_out = Vec::with_capacity(subset.indices.len());
        let mut jacobians = Vec::with_capacity(subset.indices.len());
        let mut derivatives = Vec::with_capacity(subset.indices.len());
        for index in &subset.indices {
            let probe = &target.probes.probes[*index];
            let xyz = probe.xyz;
            output.push(
                model
                    .input_ucs(&realization, xyz, [0.0, 0.0], 0.0)
                    .to_array(),
            );
            target_out.push(target.target[*index].to_array());
            jacobians.push(model.coefficient_jacobian(&realization, xyz));
            derivatives.push(derivatives_at(model, &realization, xyz));
        }

        let mut rows: Vec<Vec<Row>> = vec![Vec::new(); plan.domains.len()];
        for (control, domain) in plan.domains.iter().enumerate() {
            let key = &domain.key;
            for value in &domain.values {
                let (delta, wb, exposure) = model.value_row(key, value)?;
                rows[control].push(Row {
                    delta,
                    wb,
                    exposure,
                });
            }
        }

        let mut contributions = vec![vec![Vec::new(); plan.domains.len()]; subset.indices.len()];
        let mut covariance = vec![vec![Vec::new(); plan.domains.len()]; subset.indices.len()];
        for control in 0..plan.domains.len() {
            let key = &plan.domains[control].key;
            let current_index = plan.value_index(control, settings);
            for (value_index, value) in plan.domains[control].values.iter().enumerate() {
                let row = &rows[control][value_index];
                let current_row = &rows[control][current_index];
                let single_value = Settings::new().with(key.clone(), value.clone());
                for probe in 0..subset.indices.len() {
                    let jacobian = &jacobians[probe];
                    let mut linear = project_difference(&row.delta, &current_row.delta, jacobian);
                    let d = &derivatives[probe];
                    let dwb_r = row.wb[0] - current_row.wb[0];
                    let dwb_b = row.wb[1] - current_row.wb[1];
                    let de = row.exposure - current_row.exposure;
                    for (channel, value) in linear.iter_mut().enumerate() {
                        *value +=
                            dwb_r * d.d_r[channel] + dwb_b * d.d_b[channel] + de * d.d_e[channel];
                    }
                    contributions[probe][control].push(linear);
                    covariance[probe][control].push(covariance_at(
                        model,
                        &single_value,
                        key,
                        jacobian,
                        d,
                    ));
                }
            }
        }

        Ok(Self {
            model: model.clone(),
            sim: model.sim().clone(),
            settings: settings.clone(),
            indices: subset.indices.clone(),
            weights: subset.weights.clone(),
            covariance_weights: subset.covariance_weights.clone(),
            target: target_out,
            output,
            contributions,
            covariance,
            beta,
            support: model.crm.support.clone(),
            rows,
        })
    }

    pub(crate) fn scratch(&self) -> RealizationScratch {
        let base_coef = self.model.base_look.coef_vector();
        RealizationScratch {
            look: self.model.base_look.clone(),
            coef: base_coef.clone(),
            base_coef,
        }
    }

    pub(crate) fn realize_into(
        &self,
        plan: &Plan,
        settings: &Settings,
        scratch: &mut RealizationScratch,
    ) -> Result<([f64; 3], f64)> {
        if !self.model.crm.interactions.is_empty() {
            let realization = self.model.realize(settings)?;
            scratch.look = realization.look;
            return Ok((realization.wb_gain, realization.exposure_ev));
        }
        scratch.coef.copy_from_slice(&scratch.base_coef);
        let layout = scratch.look.layout();
        let mut wb = self.model.crm.wb.omega;
        let mut exposure = 0.0_f64;
        for control in 0..plan.domains.len() {
            let value = plan.value_index(control, settings);
            let row = &self.rows[control][value];
            for (index, coefficient) in &row.delta.terms {
                if let Some(slot) = scratch.coef.get_mut(*index) {
                    *slot += *coefficient;
                }
            }
            wb[0] += row.wb[0];
            wb[1] += row.wb[1];
            exposure += row.exposure;
        }
        if !scratch.look.tone.is_colorimetric() {
            for value in &mut scratch.coef[layout.tone.start + 1..layout.tone.end] {
                *value = value.max(0.0);
            }
        }
        scratch.look.apply_coef(&scratch.coef)?;
        Ok(([libm::exp(wb[0]), 1.0, libm::exp(wb[1])], exposure))
    }

    pub(crate) fn input_xyz(
        &self,
        xyz: looklab_color::Xyz,
        gains: [f64; 3],
        exposure: f64,
    ) -> looklab_color::Xyz {
        let camera = self.model.inverse.mul_vec(xyz.to_array());
        let scale = libm::pow(2.0, exposure);
        looklab_color::Xyz::from_array(self.model.transform.mul_vec([
            camera[0] * gains[0] * scale,
            camera[1] * gains[1] * scale,
            camera[2] * gains[2] * scale,
        ]))
    }

    pub fn realization(&self, plan: &Plan, settings: &Settings) -> Result<Realization> {
        if !self.model.crm.interactions.is_empty() {
            return self.model.realize(settings);
        }
        let mut coef = self.model.base_look.coef_vector();
        let layout = self.model.base_look.layout();
        let mut wb = self.model.crm.wb.omega;
        let mut exposure = 0.0_f64;
        for control in 0..plan.domains.len() {
            let value = plan.value_index(control, settings);
            let row = &self.rows[control][value];
            for (index, coefficient) in &row.delta.terms {
                if let Some(slot) = coef.get_mut(*index) {
                    *slot += *coefficient;
                }
            }
            wb[0] += row.wb[0];
            wb[1] += row.wb[1];
            exposure += row.exposure;
        }
        if !self.model.base_look.tone.is_colorimetric() {
            for value in &mut coef[layout.tone.start + 1..layout.tone.end] {
                *value = value.max(0.0);
            }
        }
        let mut look = self.model.base_look.clone();
        look.apply_coef(&coef)?;
        Ok(Realization {
            look,
            wb_gain: [libm::exp(wb[0]), 1.0, libm::exp(wb[1])],
            exposure_ev: exposure,
        })
    }

    pub fn effective_predict(
        &self,
        plan: &Plan,
        settings: &Settings,
        xyz: looklab_color::Xyz,
    ) -> Result<looklab_color::ucs::Ucs> {
        let realization = self.realization(plan, settings)?;
        Ok(self.model.input_ucs(&realization, xyz, [0.0, 0.0], 0.0))
    }

    pub fn score(&self, plan: &Plan, settings: &Settings) -> f64 {
        let values: Vec<usize> = (0..plan.domains.len())
            .map(|control| plan.value_index(control, settings))
            .collect();
        let inflation = self.support.inflation(settings);
        let mut total = 0.0_f64;
        for (probe, weight) in self.weights.iter().enumerate() {
            let mut prediction = self.output[probe];
            let mut trace = 0.0_f64;
            for (control, value) in values.iter().enumerate() {
                let contribution = self.contributions[probe][control][*value];
                for channel in 0..3 {
                    prediction[channel] += contribution[channel];
                }
                trace += self.covariance[probe][control][*value];
            }
            let delta = [
                self.target[probe][0] - prediction[0],
                self.target[probe][1] - prediction[1],
                self.target[probe][2] - prediction[2],
            ];
            let squared = delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2];
            total +=
                weight * squared + self.beta * self.covariance_weights[probe] * trace * inflation;
        }
        total
    }

    pub fn predicted_output(&self, plan: &Plan, settings: &Settings) -> Vec<[f64; 3]> {
        let values: Vec<usize> = (0..plan.domains.len())
            .map(|control| plan.value_index(control, settings))
            .collect();
        let mut out = self.output.clone();
        for (probe, row) in out.iter_mut().enumerate() {
            for (control, value) in values.iter().enumerate() {
                let contribution = self.contributions[probe][control][*value];
                for channel in 0..3 {
                    row[channel] += contribution[channel];
                }
            }
        }
        out
    }
}

fn project_difference(
    value: &SparseDelta,
    current: &SparseDelta,
    jacobian: &[looklab_model::CoefTerm],
) -> [f64; 3] {
    let mut out = value.project(jacobian);
    let base = current.project(jacobian);
    for channel in 0..3 {
        out[channel] -= base[channel];
    }
    out
}

fn derivatives_at(
    model: &MatchModel,
    realization: &Realization,
    xyz: looklab_color::Xyz,
) -> Derivatives {
    let step = 1e-4;
    let base = model.input_ucs(realization, xyz, [0.0, 0.0], 0.0);
    let red = model.input_ucs(realization, xyz, [step, 0.0], 0.0);
    let blue = model.input_ucs(realization, xyz, [0.0, step], 0.0);
    let exposure = model.input_ucs(realization, xyz, [0.0, 0.0], step);
    Derivatives {
        d_r: [
            (red.j - base.j) / step,
            (red.a - base.a) / step,
            (red.b - base.b) / step,
        ],
        d_b: [
            (blue.j - base.j) / step,
            (blue.a - base.a) / step,
            (blue.b - base.b) / step,
        ],
        d_e: [
            (exposure.j - base.j) / step,
            (exposure.a - base.a) / step,
            (exposure.b - base.b) / step,
        ],
    }
}

fn covariance_at(
    model: &MatchModel,
    settings: &Settings,
    key: &looklab_schema::ControlKey,
    jacobian: &[looklab_model::CoefTerm],
    derivatives: &Derivatives,
) -> f64 {
    let mut trace = 0.0_f64;
    for group in &model.groups {
        if &group.key != key {
            continue;
        }
        let weight = model.value_weight(key, &group.value, settings);
        if weight == 0.0 {
            continue;
        }
        for term in &group.terms {
            let mut vector = term.delta.project(jacobian);
            for (channel, value) in vector.iter_mut().enumerate() {
                *value +=
                    term.wb[0] * derivatives.d_r[channel] + term.wb[1] * derivatives.d_b[channel];
            }
            trace += weight
                * weight
                * term.eigenvalue
                * (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]);
        }
    }
    trace
}
