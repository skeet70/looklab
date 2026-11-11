// SPDX-License-Identifier: AGPL-3.0-or-later
//! Executes observed-JPEG controls and settings recovery on exact candidate evidence.

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::realization::{ControlValue, Settings};
use looklab_schema::{ControlKey, ControlType, Epoch};

use crate::assessment::{
    AssessmentBinding, ControlClaim, ControlsAssessmentInput, EvidenceSource, ObservedJpeg,
    ProtocolAssessment, RecoveryObservation, SettingObservation, assess_controls, default_value,
    median,
};
use crate::model_assessment::artifacts::LoadedRealization;
use crate::model_assessment::evidence::{self, Evidence};
use crate::model_assessment::predict::{condition_nuisances, observed_ucs, output_space};

pub(crate) fn run(
    mut binding: AssessmentBinding,
    loaded: &LoadedRealization,
    evidence: &Evidence,
    max_samples_per_image: usize,
) -> Result<(ProtocolAssessment, Vec<usize>), String> {
    if !loaded.model.has_control_response() {
        return Err("controls matching needs a fitted control response".to_owned());
    }
    let epoch_id = loaded.revision.subject.epoch.get();
    let epoch = evidence
        .pool
        .resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == epoch_id)
        .ok_or_else(|| format!("epoch {epoch_id} is not declared"))?;
    let space = loaded.model.space();
    let mut training = Vec::new();
    let mut held_out = Vec::new();
    let mut recoveries = Vec::new();
    let mut claims: BTreeMap<(ControlKey, ControlValue), ControlClaim> = BTreeMap::new();
    let mut assessed = Vec::new();
    let mut cells = BTreeSet::new();
    let mut evaluated = BTreeSet::new();
    let default_settings = default_settings(epoch);
    for image in &evidence.images {
        let pool_image = &evidence.pool.images[image.pool_index];
        if output_space(pool_image.samples.header().output_space) != space {
            continue;
        }
        let is_assessment = evidence.assessment_groups.contains(&image.group);
        if !is_assessment {
            training.push(SettingObservation {
                group: image.group.clone(),
                settings: image.settings.clone(),
                firmware: Some(image.context.firmware.clone()),
                wb_mode: Some(image.context.wb_mode.clone()),
                source: EvidenceSource::Observed,
            });
            continue;
        }
        assessed.push(image.point());
        let transform = pool_image
            .transform
            .mul(&loaded.transform.delta().transform());
        let exposure_mode = crate::pool::exposure_mode_key(
            &evidence.pool.resources.schema,
            epoch_id,
            &image.settings,
        )
        .map_err(|error| error.to_string())?;
        let realization = loaded.model.realize(
            &image.settings,
            &image.context.firmware,
            &image.context.wb_mode,
            &exposure_mode,
        )?;
        let nuisance = condition_nuisances(
            &loaded.model,
            &realization,
            transform,
            &pool_image.samples,
            &pool_image.admitted,
            space,
        );
        let admitted: Vec<usize> = pool_image
            .admitted
            .iter()
            .enumerate()
            .filter_map(|(index, admitted)| (*admitted).then_some(index))
            .collect();
        let stride = admitted.len().div_ceil(max_samples_per_image.max(1));
        let sample_indices: Vec<usize> = admitted.iter().copied().step_by(stride.max(1)).collect();
        let mut observed_samples = Vec::new();
        let mut predicted_samples = Vec::new();
        for index in sample_indices.iter().copied() {
            let (predicted, input) = loaded.model.predict_camera(
                &realization,
                transform,
                pool_image.samples.in_rgb()[index].map(f64::from),
                nuisance,
            );
            let observed = observed_ucs(&pool_image.samples, index, space);
            let Some(cell) = evidence::cell_for_scene(input.xyz) else {
                continue;
            };
            cells.insert(cell.clone());
            evaluated.insert(image.pool_index);
            observed_samples.push(observed);
            predicted_samples.push(predicted);
            held_out.push(ObservedJpeg {
                group: image.group.clone(),
                settings: image.settings.clone(),
                firmware: Some(image.context.firmware.clone()),
                wb_mode: Some(image.context.wb_mode.clone()),
                cell,
                predicted,
                observed,
                source: EvidenceSource::Observed,
            });
        }
        collect_claims(epoch, image, &mut claims);
        if image.settings != default_settings
            && !observed_samples.is_empty()
            && recoveries.len() < 8
        {
            let recovered = recover_settings(
                &loaded.model,
                &image.context.firmware,
                &image.context.wb_mode,
                &nuisance,
                transform,
                &pool_image.samples,
                &sample_indices,
                space,
                &evidence.pool.resources.schema,
                epoch,
                &default_settings,
            )?;
            let recovered_exposure = crate::pool::exposure_mode_key(
                &evidence.pool.resources.schema,
                epoch_id,
                &recovered,
            )
            .map_err(|error| error.to_string())?;
            let recovered_realization = loaded.model.realize(
                &recovered,
                &image.context.firmware,
                &image.context.wb_mode,
                &recovered_exposure,
            )?;
            let recovered_predictions: Vec<Ucs> = sample_indices
                .iter()
                .map(|index| {
                    loaded
                        .model
                        .predict_camera(
                            &recovered_realization,
                            transform,
                            pool_image.samples.in_rgb()[*index].map(f64::from),
                            nuisance,
                        )
                        .0
                })
                .collect();
            let true_errors: Vec<f64> = predicted_samples
                .iter()
                .zip(&observed_samples)
                .map(|(predicted, observed)| delta_e_ucs(*predicted, *observed))
                .collect();
            let recovered_errors: Vec<f64> = recovered_predictions
                .iter()
                .zip(&observed_samples)
                .map(|(predicted, observed)| delta_e_ucs(*predicted, *observed))
                .collect();
            let base = observed_samples
                .first()
                .copied()
                .unwrap_or_else(|| mean(&observed_samples));
            let true_error = median(&true_errors).unwrap_or(f64::NAN);
            let recovered_error = median(&recovered_errors).unwrap_or(f64::NAN);
            recoveries.push(RecoveryObservation {
                group: image.group.clone(),
                true_settings: image.settings.clone(),
                recovered_settings: recovered,
                firmware: Some(image.context.firmware.clone()),
                wb_mode: Some(image.context.wb_mode.clone()),
                predicted_true: Ucs::new(base.j + true_error, base.a, base.b),
                observed_true: base,
                predicted_recovered: Ucs::new(base.j + recovered_error, base.a, base.b),
                source: EvidenceSource::Observed,
            });
        }
    }
    if assessed.is_empty() {
        assessed.push(looklab_schema::AssessedPoint {
            settings: crate::assessment::setting_combination(&Settings::new(), epoch),
            firmware: None,
            wb_mode: None,
        });
    }
    assessed.sort();
    assessed.dedup();
    binding.scope.points = assessed;
    binding.scope.defaults_assessed = binding
        .scope
        .points
        .iter()
        .any(|point| point.settings.is_empty());
    binding.scope.colour_cells = cells.iter().cloned().collect();
    binding.scope.exposure_cells = Vec::new();
    Ok((
        assess_controls(ControlsAssessmentInput {
            binding,
            schema: evidence.pool.resources.schema.clone(),
            epoch: epoch_id,
            pinned: evidence::pinned_modes(evidence),
            training,
            held_out,
            recoveries,
            claims: claims.into_values().collect(),
        }),
        evaluated.into_iter().collect(),
    ))
}

fn collect_claims(
    epoch: &Epoch,
    image: &crate::model_assessment::evidence::EvidenceImage,
    claims: &mut BTreeMap<(ControlKey, ControlValue), ControlClaim>,
) {
    for control in &epoch.controls {
        if !control.look_affecting {
            continue;
        }
        let Some(value) = image.settings.get(&control.key) else {
            continue;
        };
        if *value == default_value(control) {
            continue;
        }
        let isolated = epoch.controls.iter().all(|other| {
            if !other.look_affecting || other.key == control.key {
                return true;
            }
            image.settings.get(&other.key) == Some(&default_value(other))
        });
        if !isolated {
            continue;
        }
        claims
            .entry((control.key.clone(), value.clone()))
            .or_insert_with(|| ControlClaim {
                key: control.key.clone(),
                values: vec![value.clone()],
                interval: None,
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn recover_settings(
    model: &crate::model_assessment::predict::ExactModel,
    firmware: &looklab_schema::FirmwareId,
    wb_mode: &looklab_schema::WbModeKey,
    nuisance: &crate::model_assessment::predict::Nuisance,
    transform: looklab_color::Mat3,
    samples: &looklab_samples::SampleSet,
    probe: &[usize],
    space: looklab_fit::RgbSpace,
    schema: &looklab_schema::ControlSchema,
    epoch: &Epoch,
    default_settings: &Settings,
) -> Result<Settings, String> {
    let observed: Vec<Ucs> = probe
        .iter()
        .map(|index| observed_ucs(samples, *index, space))
        .collect();
    let mut settings = default_settings.clone();
    for _ in 0..2 {
        for control in &epoch.controls {
            if !control.look_affecting {
                continue;
            }
            let candidates = candidate_values(control);
            let mut best = settings.get(&control.key).cloned();
            let mut best_cost = f64::INFINITY;
            for value in candidates {
                let mut trial = settings.clone();
                trial.set(control.key.clone(), value.clone());
                let exposure_mode = crate::pool::exposure_mode_key(schema, epoch.id, &trial)
                    .map_err(|error| error.to_string())?;
                let realization = model.realize(&trial, firmware, wb_mode, &exposure_mode)?;
                let errors: Vec<f64> = probe
                    .iter()
                    .zip(&observed)
                    .map(|(index, target)| {
                        let (predicted, _) = model.predict_camera(
                            &realization,
                            transform,
                            samples.in_rgb()[*index].map(f64::from),
                            *nuisance,
                        );
                        delta_e_ucs(predicted, *target)
                    })
                    .collect();
                let cost = median(&errors).unwrap_or(f64::INFINITY);
                if cost < best_cost {
                    best_cost = cost;
                    best = Some(value);
                }
            }
            if let Some(value) = best {
                settings.set(control.key.clone(), value);
            }
        }
    }
    Ok(settings)
}

fn candidate_values(control: &looklab_schema::Control) -> Vec<ControlValue> {
    match control.kind {
        ControlType::Int | ControlType::Decimal => {
            let steps: Vec<ControlValue> = control
                .steps()
                .into_iter()
                .map(ControlValue::Step)
                .collect();
            if steps.len() <= 9 {
                return steps;
            }
            let stride = steps.len().div_ceil(9);
            steps.into_iter().step_by(stride).collect()
        }
        ControlType::Enum => control
            .values
            .as_ref()
            .map(|values| values.iter().cloned().map(ControlValue::Enum).collect())
            .unwrap_or_default(),
        ControlType::Bool => vec![ControlValue::Bool(false), ControlValue::Bool(true)],
    }
}

fn default_settings(epoch: &Epoch) -> Settings {
    let mut settings = Settings::new();
    for control in &epoch.controls {
        if control.look_affecting {
            settings.set(control.key.clone(), default_value(control));
        }
    }
    settings
}

fn mean(values: &[Ucs]) -> Ucs {
    if values.is_empty() {
        return Ucs::new(0.0, 0.0, 0.0);
    }
    let count = values.len() as f64;
    Ucs::new(
        values.iter().map(|value| value.j).sum::<f64>() / count,
        values.iter().map(|value| value.a).sum::<f64>() / count,
        values.iter().map(|value| value.b).sum::<f64>() / count,
    )
}
