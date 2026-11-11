// SPDX-License-Identifier: AGPL-3.0-or-later
//! Executes the conditional native-prediction protocol on exact candidate evidence.

use std::collections::BTreeSet;

use looklab_fit::{
    FitOptions, ModelErrorField, ProbeEstimate, ProbeUncertainty, model_error, post_clip,
};
use looklab_schema::{CellId, ModelSubject};

use crate::assessment::{
    AssessmentBinding, EvidenceSource, NativeAssessmentInput, NativeObservation,
    NativeProbeSupport, ProtocolAssessment, assess_native, support_from_fit,
};
use crate::model_assessment::artifacts::LoadedRealization;
use crate::model_assessment::evidence::{self, Evidence};
use crate::model_assessment::predict::{condition_nuisances, observed_ucs, output_space};

pub(crate) fn run(
    mut binding: AssessmentBinding,
    loaded: &LoadedRealization,
    evidence: &Evidence,
    options: &FitOptions,
    max_samples_per_image: usize,
) -> Result<(ProtocolAssessment, Vec<usize>), String> {
    if !matches!(&binding.subject, ModelSubject::BodyRealization { .. }) {
        return Err("native prediction needs a body-realization subject".to_owned());
    }
    let space = loaded.model.space();
    let encoding = loaded.output_encoding;
    if super::artifacts::output_encoding(space) != encoding {
        return Err("release output encoding differs from the assessed encoding".to_owned());
    }
    let mut observations = Vec::new();
    let mut evaluated = BTreeSet::new();
    let controls_fitted = loaded.model.has_control_response();
    let epoch_id = loaded.revision.subject.epoch.get();
    let epoch = evidence
        .pool
        .resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == epoch_id)
        .ok_or_else(|| format!("epoch {epoch_id} is not declared"))?;
    if !controls_fitted {
        binding.scope.points = evidence::points_of(evidence.images.iter().filter(|image| {
            crate::pool::defaults_match(epoch, &loaded.revision.target.family, &image.settings)
        }));
        binding.scope.defaults_assessed = true;
    }
    for image in &evidence.images {
        let pool_image = &evidence.pool.images[image.pool_index];
        let sample_space = output_space(pool_image.samples.header().output_space);
        if sample_space != space {
            continue;
        }
        if !evidence.assessment_groups.contains(&image.group) {
            continue;
        }
        if !controls_fitted
            && !crate::pool::defaults_match(epoch, &loaded.revision.target.family, &image.settings)
        {
            continue;
        }
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
        for index in admitted.into_iter().step_by(stride.max(1)) {
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
            evaluated.insert(image.pool_index);
            observations.push(NativeObservation {
                group: image.group.clone(),
                settings: image.combination.clone(),
                firmware: Some(image.context.firmware.clone()),
                wb_mode: Some(image.context.wb_mode.clone()),
                cell,
                predicted,
                observed,
                source: EvidenceSource::Observed,
            });
        }
    }
    let probes = probe_uncertainty(loaded)?;
    let mut field = ModelErrorField::empty();
    let probe_set = looklab_model::probes::v1();
    let observed_cells: BTreeSet<CellId> = observations
        .iter()
        .map(|observation| observation.cell.clone())
        .collect();
    for probe in &probe_set.probes {
        let input = looklab_model::LookInput::new(probe.xyz);
        if evidence::cell_for_scene(probe.xyz).is_some_and(|cell| observed_cells.contains(&cell)) {
            field.evidence[model_error::post_index(&input, space)] = true;
        }
    }
    let support: Vec<NativeProbeSupport> =
        support_from_fit(&probes, &field, encoding).map_err(|error| error.to_string())?;
    if binding.scope.points.is_empty() {
        binding.scope.points = vec![looklab_schema::AssessedPoint {
            settings: crate::assessment::setting_combination(
                &looklab_model::realization::Settings::new(),
                epoch,
            ),
            firmware: None,
            wb_mode: None,
        }];
        binding.scope.defaults_assessed = true;
    }
    binding.scope.exposure_cells = Vec::new();
    Ok((
        assess_native(NativeAssessmentInput {
            binding,
            encoding,
            pinned: evidence::pinned_modes(evidence),
            support,
            training_groups: evidence.training_groups.iter().cloned().collect(),
            observations,
            support_sigma: options.support_sigma,
            min_independent_groups: options.min_probe_clusters.max(1),
        }),
        evaluated.into_iter().collect(),
    ))
}

fn probe_uncertainty(loaded: &LoadedRealization) -> Result<ProbeUncertainty, String> {
    let space = loaded.model.space();
    let probes = looklab_model::probes::v1();
    let mut estimates = Vec::with_capacity(loaded.release.probes.len());
    let mut ids = Vec::with_capacity(loaded.release.probes.len());
    for (index, stored) in loaded.release.probes.iter().enumerate() {
        let probe = probes
            .probes
            .get(index)
            .ok_or_else(|| format!("release probe {index} has no v1 definition"))?;
        if probe.id != stored.id {
            return Err(format!("release probe {index} is not the v1 probe order"));
        }
        let post = post_clip(loaded.model.look(), probe.xyz, space, loaded.release.clip_k);
        estimates.push(ProbeEstimate {
            id_hint: index,
            ucs: loaded.model.look().eval(probe.xyz),
            covariance: [[0.0; 3]; 3],
            sigma_de: stored.sigma,
            post_ucs: post,
            post_covariance: [[0.0; 3]; 3],
            post_sigma_de: stored.sigma_post,
            bias_de: stored.bias_de,
            pre_clip_reliable: stored.pre_clip_identified,
            gamut_edge: false,
            density: 0.0,
            supported_sigma: stored.support.sigma,
            supported_density: stored.support.density,
            supported_clusters: stored.support.clusters,
            supported_bias: stored.support.bias,
            supported_evidence: true,
            supported: stored.support.supported(),
            clusters: 0,
        });
        ids.push(stored.id.clone());
    }
    Ok(ProbeUncertainty {
        probes: estimates,
        ids,
        supported_fraction: 0.0,
    })
}
