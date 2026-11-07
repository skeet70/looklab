// SPDX-License-Identifier: AGPL-3.0-or-later
//! Match and verify over published site data ([docs/04 §3-§5](../../../docs/04-matching-and-export.md)).

use looklab_color::Mat3;
use looklab_match::{
    Calibration, ControlCovariance, Emphasis, FitObservedOptions, MatchModel, MatchOptions,
    Permalink, PermalinkSources, Plan, PreparedTarget, ScaleFitter, TargetSpec,
    measure_target_error, optimize, settings_card,
};
use looklab_model::file::LookDocument;
use looklab_model::realization::decode_crm;
use looklab_model::{ProbeSet, ProbeSetVersion};
use looklab_publish::ExcludedSim;
use looklab_schema::{ControlSchema, SimKey};
use serde::{Deserialize, Serialize};

use crate::dto::{CardDto, DeltaStatsDto, MatchReply, StatsDto, SuggestionDto, VerifyReply};
use crate::error::FacadeError;

pub const TRANSFORM_FORMAT: &str = "looklab-transform/1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceNames {
    pub target_look: Option<String>,
    pub body: Option<String>,
    pub transform: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimSources {
    pub base: Option<String>,
    pub crm: Option<String>,
    pub covariance: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimInputs {
    pub sim: SimKey,
    pub base_look: Vec<u8>,
    pub crm: Vec<u8>,
    pub covariance: Option<Vec<u8>>,
    pub sources: SimSources,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchInputs {
    pub target_look: Vec<u8>,
    pub sims: Vec<SimInputs>,
    pub excluded: Vec<ExcludedSim>,
    pub body_yaml: String,
    pub transform_yaml: Option<String>,
    pub emphasis: String,
    pub epoch: u32,
    pub input_transform_version: u32,
    pub realization_version: u32,
    pub seed: u64,
    pub sources: SourceNames,
    pub calibration: Calibration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchRequest {
    pub inputs: MatchInputs,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyRequestMessage {
    pub inputs: MatchInputs,
    pub observed_blobs: Vec<Vec<u8>>,
}

struct LoadedSim {
    input: SimInputs,
    base: LookDocument,
    model: MatchModel,
}

struct Case {
    target: LookDocument,
    schema: ControlSchema,
    emphasis: Emphasis,
    probes: ProbeSet,
    options: MatchOptions,
    sources: SourceNames,
    inputs: MatchInputs,
    sims: Vec<LoadedSim>,
}

impl Case {
    fn new(inputs: &MatchInputs) -> Result<Self, FacadeError> {
        let target = LookDocument::decode(&inputs.target_look)?;
        let schema = looklab_schema::validate_control_schema(&inputs.body_yaml)
            .map_err(FacadeError::Schema)?;
        if target.file.probe_set != ProbeSetVersion::V1 {
            return Err(FacadeError::Input("unsupported probe set".to_owned()));
        }
        let transform = match &inputs.transform_yaml {
            Some(yaml) => parse_transform(yaml)?,
            None => Mat3::IDENTITY,
        };
        let emphasis = Emphasis::parse(&inputs.emphasis)
            .ok_or_else(|| FacadeError::Input(format!("unknown emphasis {}", inputs.emphasis)))?;
        let mut sims = Vec::with_capacity(inputs.sims.len());
        for input in &inputs.sims {
            let base = LookDocument::decode(&input.base_look)?;
            let crm = decode_crm(&input.crm)?;
            if crm.epoch != inputs.epoch {
                return Err(FacadeError::Input(format!(
                    "control-response model for {} is for epoch {}, not {}",
                    input.sim, crm.epoch, inputs.epoch
                )));
            }
            if crm.base_sim != input.sim {
                return Err(FacadeError::Input(format!(
                    "control-response model for {} names base sim {}",
                    input.sim, crm.base_sim
                )));
            }
            let covariance = match &input.covariance {
                Some(bytes) => Some(ControlCovariance::from_json(bytes)?),
                None => None,
            };
            let model = MatchModel::new(
                base.look.clone(),
                crm,
                transform,
                covariance,
                inputs.input_transform_version,
                inputs.realization_version,
            )?;
            sims.push(LoadedSim {
                input: input.clone(),
                base,
                model,
            });
        }
        Ok(Self {
            target,
            schema,
            emphasis,
            probes: looklab_model::probes::v1(),
            options: MatchOptions {
                seed: inputs.seed,
                ..MatchOptions::default()
            },
            sources: inputs.sources.clone(),
            inputs: inputs.clone(),
            sims,
        })
    }

    fn target<'a>(&'a self) -> Result<PreparedTarget<'a>, FacadeError> {
        Ok(PreparedTarget::new(TargetSpec {
            probes: &self.probes,
            function: &self.target.look,
            uncertainty: Some(&self.target.uncertainty),
            emphasis: self.emphasis,
            tau: 1.0,
            support_sigma: 1.5,
            density_floor: None,
        })?)
    }

    fn plan(&self) -> Result<Plan, FacadeError> {
        Ok(Plan::new(&self.schema, self.inputs.epoch)?)
    }

    fn sim(&self, sim: &SimKey) -> Result<&LoadedSim, FacadeError> {
        self.sims
            .iter()
            .find(|loaded| &loaded.input.sim == sim)
            .ok_or_else(|| FacadeError::Input(format!("no loaded input for sim {sim}")))
    }

    fn models(&self, plan: &Plan) -> Result<Vec<MatchModel>, FacadeError> {
        let sims: Vec<SimKey> = self
            .sims
            .iter()
            .map(|loaded| loaded.input.sim.clone())
            .collect();
        self.inputs.calibration.validate(&sims, plan)?;
        Ok(self
            .sims
            .iter()
            .map(|loaded| self.inputs.calibration.apply(&loaded.model))
            .collect::<looklab_match::Result<Vec<_>>>()?)
    }

    fn epoch_map_version(&self) -> u32 {
        self.target
            .file
            .provenance
            .bodies
            .first()
            .map(|body| body.epoch_map_version)
            .unwrap_or(0)
    }

    fn permalink(&self, loaded: &LoadedSim, sim: &SimKey, calibration: &Calibration) -> String {
        Permalink {
            target_slug: self.target.file.look.slug.to_string(),
            target_version: self.target.file.look.version.to_string(),
            base_slug: loaded.base.file.look.slug.to_string(),
            base_version: loaded.base.file.look.version.to_string(),
            body: self.schema.body.to_string(),
            epoch: self.inputs.epoch,
            sim: sim.to_string(),
            crm_version: loaded.model.version(),
            input_transform_version: self.inputs.input_transform_version,
            realization_params_version: self.inputs.realization_version,
            epoch_map_version: self.epoch_map_version(),
            probe_set: self.target.file.probe_set,
            optimizer: looklab_match::OPTIMIZER_VERSION.to_owned(),
            emphasis: self.emphasis,
            seed: self.inputs.seed,
            sources: PermalinkSources {
                target_look: self.sources.target_look.clone(),
                base_look: loaded.input.sources.base.clone(),
                body: self.sources.body.clone(),
                crm: loaded.input.sources.crm.clone(),
                transform: self.sources.transform.clone(),
                covariance: loaded.input.sources.covariance.clone(),
            },
            calibration: calibration.clone(),
        }
        .canonical()
    }
}

/// Runs the optimizer on every sim the published bundle supplies.
///
/// # Errors
/// Reports malformed site artifacts, an unsupported probe set or a match with
/// no supported probe.
pub fn run_match(message: &MatchRequest) -> Result<MatchReply, FacadeError> {
    let case = Case::new(&message.inputs)?;
    let plan = case.plan()?;
    let target = case.target()?;
    let models = case.models(&plan)?;
    let outcome = optimize(&models, &target, &plan, &case.options)?;
    let best = outcome
        .results
        .first()
        .ok_or_else(|| FacadeError::Input("no settings suggestion".to_owned()))?;
    let winner_index = case
        .sims
        .iter()
        .position(|loaded| loaded.input.sim == best.sim)
        .ok_or_else(|| FacadeError::Input(format!("no loaded input for sim {}", best.sim)))?;
    let winner = &case.sims[winner_index];
    let winner_model = &models[winner_index];
    let card = settings_card(&target, &plan, winner_model, best)?;
    let realization = winner_model.realize(&best.settings)?;
    let realization_lut = looklab_publish::bake_lut(&realization.look, |xyz| {
        winner_model.input_xyz(&realization, xyz)
    });
    Ok(MatchReply {
        target_slug: case.target.file.look.slug.clone(),
        target_version: case.target.file.look.version.clone(),
        base_slug: winner.base.file.look.slug.clone(),
        base_version: winner.base.file.look.version.clone(),
        body: case.schema.body.clone(),
        epoch: case.inputs.epoch,
        epoch_map_version: case.epoch_map_version(),
        crm_version: winner.model.version(),
        permalink: case.permalink(winner, &best.sim, &case.inputs.calibration),
        card: CardDto::from_card(&card),
        results: outcome
            .results
            .iter()
            .map(SuggestionDto::from_suggestion)
            .collect(),
        stats: StatsDto::from_outcome(&outcome),
        realization_lut,
        excluded: case.inputs.excluded.clone(),
    })
}

/// Runs one verification-loop iteration on local re-renders.
///
/// # Errors
/// Reports malformed site artifacts, an empty observed set or a failed fit.
pub fn run_verify(message: &VerifyRequestMessage) -> Result<VerifyReply, FacadeError> {
    let case = Case::new(&message.inputs)?;
    let plan = case.plan()?;
    let target = case.target()?;
    if case.sims.is_empty() {
        return Err(FacadeError::Input(
            "verification needs at least one sim input".to_owned(),
        ));
    }
    let observed = decode_observed(&message.observed_blobs)?;
    if observed.is_empty() {
        return Err(looklab_match::MatchError::EmptyObserved.into());
    }
    let models = case.models(&plan)?;
    let first_outcome = optimize(&models, &target, &plan, &case.options)?;
    let first = first_outcome
        .results
        .into_iter()
        .next()
        .ok_or_else(|| FacadeError::Input("no settings suggestion".to_owned()))?;
    let winner_index = case
        .sims
        .iter()
        .position(|loaded| loaded.input.sim == first.sim)
        .ok_or_else(|| FacadeError::Input(format!("no loaded input for sim {}", first.sim)))?;
    let winner = &case.sims[winner_index];
    let (fitted, scales) = ScaleFitter.fit_with_scales(
        &models[winner_index],
        &plan,
        &first.settings,
        &observed,
        &FitObservedOptions::default(),
    )?;
    let updated = models[winner_index].with_crm(fitted)?;
    let next_calibration = case.inputs.calibration.merged(&first.sim, &scales)?;
    let mut updated_models = models;
    updated_models[winner_index] = updated.clone();
    let second_outcome = optimize(&updated_models, &target, &plan, &case.options)?;
    let second = second_outcome
        .results
        .into_iter()
        .next()
        .ok_or_else(|| FacadeError::Input("no settings suggestion".to_owned()))?;
    let second_index = case
        .sims
        .iter()
        .position(|loaded| loaded.input.sim == second.sim)
        .ok_or_else(|| FacadeError::Input(format!("no loaded input for sim {}", second.sim)))?;
    let card_model = if second.sim == first.sim {
        &updated
    } else {
        &updated_models[second_index]
    };
    let first_measured = measure_target_error(&target, &winner.model, &first.settings, &observed)?;
    let card = settings_card(&target, &plan, card_model, &second)?;
    let next_sim = case.sim(&second.sim)?;
    Ok(VerifyReply {
        permalink: case.permalink(next_sim, &second.sim, &next_calibration),
        first: SuggestionDto::from_suggestion(&first),
        second: SuggestionDto::from_suggestion(&second),
        first_measured: DeltaStatsDto::from_stats(&first_measured),
        second_measured: None,
        reduction: None,
        updated_crm_version: updated.version(),
        card: CardDto::from_card(&card),
    })
}

fn decode_observed(blobs: &[Vec<u8>]) -> Result<Vec<looklab_match::ObservedRender>, FacadeError> {
    blobs
        .iter()
        .map(|blob| {
            let samples = looklab_samples::decode(blob)
                .map_err(|error| FacadeError::Reply(error.to_string()))?;
            Ok(looklab_match::ObservedRender { samples })
        })
        .collect()
}

/// Decodes the reply bytes `site/workers/match-worker.js` returns.
pub fn decode_match_reply(bytes: &[u8]) -> Result<MatchReply, String> {
    decode_postcard(bytes)
}

pub fn decode_verify_reply(bytes: &[u8]) -> Result<VerifyReply, String> {
    decode_postcard(bytes)
}

fn decode_postcard<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let (reply, rest) =
        postcard::take_from_bytes::<Result<T, String>>(bytes).map_err(|error| error.to_string())?;
    if !rest.is_empty() {
        return Err(format!(
            "{} trailing bytes after the worker reply",
            rest.len()
        ));
    }
    reply
}

/// The `version` field of a transform file, which the permalink pins.
pub fn transform_version(yaml: &str) -> Option<u32> {
    let file: TransformWire = serde_norway::from_str(yaml).ok()?;
    (file.version > 0).then_some(file.version)
}

fn parse_transform(yaml: &str) -> Result<Mat3, FacadeError> {
    let file: TransformWire =
        serde_norway::from_str(yaml).map_err(|error| FacadeError::Input(error.to_string()))?;
    if !file.format.is_empty() && file.format != TRANSFORM_FORMAT {
        return Err(FacadeError::Input(format!(
            "transform format {:?}, expected {TRANSFORM_FORMAT:?}",
            file.format
        )));
    }
    if file.version == 0 {
        return Err(FacadeError::Input("transform version is zero".to_owned()));
    }
    file.camera_to_xyz
        .or_else(|| {
            file.forward_matrices
                .first()
                .map(|entry| entry.camera_to_xyz)
        })
        .or_else(|| file.per_image.first().map(|entry| entry.camera_to_xyz))
        .ok_or_else(|| FacadeError::Input("transform file has no camera-to-XYZ matrix".to_owned()))
}

#[derive(Debug, Deserialize)]
struct TransformWire {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    camera_to_xyz: Option<Mat3>,
    #[serde(default)]
    forward_matrices: Vec<CameraToXyzWire>,
    #[serde(default)]
    per_image: Vec<PerImageWire>,
}

#[derive(Debug, Deserialize)]
struct CameraToXyzWire {
    camera_to_xyz: Mat3,
}

#[derive(Debug, Deserialize)]
struct PerImageWire {
    camera_to_xyz: Mat3,
}

#[expect(
    clippy::expect_used,
    reason = "these messages hold nothing postcard cannot encode"
)]
pub fn encode_match_request(request: &MatchRequest) -> Vec<u8> {
    postcard::to_allocvec(request).expect("match request is encodable")
}

#[expect(
    clippy::expect_used,
    reason = "these messages hold nothing postcard cannot encode"
)]
pub fn encode_verify_request(request: &VerifyRequestMessage) -> Vec<u8> {
    postcard::to_allocvec(request).expect("verify request is encodable")
}

#[cfg(test)]
mod tests {
    use looklab_match::{Calibration, Permalink, synth};
    use looklab_model::Look;
    use looklab_model::file::identity;
    use looklab_model::realization::{ControlResponseModel, encode_crm};
    use looklab_publish::{ExcludedSim, SimExclusion};
    use looklab_schema::SimKey;

    use super::{LookDocument, MatchInputs, MatchRequest, SimInputs, SimSources, SourceNames};

    fn sim_input(look: Look, crm: &ControlResponseModel) -> SimInputs {
        let sim = crm.base_sim.as_str();
        let document = LookDocument::from_look(
            identity(&format!("base-{sim}"), "1.0.0").expect("identity"),
            look,
        )
        .expect("document");
        SimInputs {
            sim: crm.base_sim.clone(),
            base_look: document.encode().expect("encode look"),
            crm: encode_crm(crm).expect("encode CRM"),
            covariance: None,
            sources: SimSources::default(),
        }
    }

    fn request(look: Look, sims: Vec<SimInputs>) -> (MatchRequest, LookDocument) {
        let document = LookDocument::from_look(
            identity("preview-regression", "1.0.0").expect("identity"),
            look,
        )
        .expect("document");
        let request = MatchRequest {
            inputs: MatchInputs {
                target_look: document.encode().expect("encode look"),
                sims,
                excluded: Vec::new(),
                body_yaml: synth::SYNTH_SCHEMA.to_owned(),
                transform_yaml: None,
                emphasis: "balanced".to_owned(),
                epoch: 1,
                input_transform_version: 1,
                realization_version: 1,
                seed: 7,
                sources: SourceNames::default(),
                calibration: Calibration::default(),
            },
        };
        (request, document)
    }

    fn assert_finite_bounded_lut(lut: &[u8]) {
        assert_eq!(lut.len(), looklab_publish::LUT_BYTES);
        for (index, bytes) in lut.as_chunks::<4>().0.iter().enumerate() {
            let value = f32::from_le_bytes(*bytes);
            assert!(
                value.is_finite() && (0.0..=1.0).contains(&value),
                "channel {index}: {value}"
            );
        }
    }

    #[test]
    fn identity_realization_and_target_share_full_grid_clipping() {
        let mut crm = ControlResponseModel::zeroed(
            &synth::schema(),
            1,
            &SimKey::new(synth::SYNTH_SIM).expect("sim"),
            1,
        )
        .expect("zeroed CRM");
        for step in &mut crm.exposure.steps {
            step.ev_per_step = 0.0;
        }
        let (request, document) = request(
            Look::colorimetric(),
            vec![sim_input(Look::colorimetric(), &crm)],
        );
        let reply = crate::run_match(&request).expect("match");
        assert_finite_bounded_lut(&reply.realization_lut);
        assert_eq!(
            reply.realization_lut,
            looklab_publish::bake_preview_lut(&document)
        );
    }

    #[test]
    fn nontrivial_realization_lut_is_finite_and_bounded_across_the_full_shaped_grid() {
        let body = synth::body(9).expect("synthetic body");
        let (request, _) = request(
            body.model.base_look.clone(),
            vec![sim_input(body.model.base_look, &body.model.crm)],
        );
        let reply = crate::run_match(&request).expect("match");
        assert_finite_bounded_lut(&reply.realization_lut);
    }

    #[test]
    fn two_sims_rank_the_one_whose_realization_is_the_target() {
        let provia = SimKey::new("provia").expect("sim");
        let classic = synth::body(9).expect("synthetic body");
        let provia_body = synth::body_for_sim(10, &provia).expect("synthetic body");
        let mut request = request(
            provia_body.model.base_look.clone(),
            vec![
                sim_input(classic.model.base_look.clone(), &classic.model.crm),
                sim_input(provia_body.model.base_look.clone(), &provia_body.model.crm),
            ],
        )
        .0;
        request.inputs.excluded = vec![ExcludedSim {
            sim: SimKey::new("eterna").expect("sim"),
            reason: SimExclusion::NoPublishedLook,
        }];
        let reply = crate::run_match(&request).expect("match");
        let best = reply.results.first().expect("a suggestion");
        assert_eq!(best.sim.as_str(), "provia");
        assert_eq!(reply.card.sim.as_str(), "provia");
        assert!(best.median_delta_e < 0.05, "median {}", best.median_delta_e);
        assert_eq!(reply.stats.sims_considered, 2);
        assert_eq!(reply.excluded[0].sim.as_str(), "eterna");
    }

    fn verify_replay(
        request: &MatchRequest,
        observed: &looklab_samples::SampleSet,
    ) -> super::VerifyReply {
        let blob = looklab_samples::encode(observed).expect("encode samples");
        crate::run_verify(&crate::VerifyRequestMessage {
            inputs: request.inputs.clone(),
            observed_blobs: vec![blob],
        })
        .expect("verify")
    }

    #[test]
    fn verification_fits_the_settings_the_full_bundle_showed() {
        let provia = SimKey::new("provia").expect("sim");
        let classic = synth::body(9).expect("synthetic body");
        let provia_body = synth::body_for_sim(10, &provia).expect("synthetic body");
        let (request, _) = request(
            provia_body.model.base_look.clone(),
            vec![
                sim_input(classic.model.base_look.clone(), &classic.model.crm),
                sim_input(provia_body.model.base_look.clone(), &provia_body.model.crm),
            ],
        );
        let reply = crate::run_match(&request).expect("match");
        let shown = reply.results.first().expect("a suggestion").clone();
        assert_eq!(shown.sim, provia);
        assert_ne!(request.inputs.sims[0].sim, shown.sim);
        assert_eq!(request.inputs.sims[1].sim, shown.sim);
        let observed = synth::observed(&provia_body.model, &shown.settings, &synth::probes(), 300)
            .expect("observed");
        let verified = verify_replay(&request, &observed);
        assert_eq!(verified.first.sim, shown.sim);
        assert_eq!(verified.first.settings, shown.settings);
        assert!(verified.first_measured.count > 0);
    }

    #[test]
    fn verification_keeps_the_single_sim_settings() {
        let body = synth::body(9).expect("synthetic body");
        let (request, _) = request(
            body.model.base_look.clone(),
            vec![sim_input(body.model.base_look.clone(), &body.model.crm)],
        );
        let reply = crate::run_match(&request).expect("match");
        let shown = reply.results.first().expect("a suggestion").clone();
        let observed =
            synth::observed(&body.model, &shown.settings, &synth::probes(), 200).expect("observed");
        let verified = verify_replay(&request, &observed);
        assert_eq!(verified.first.sim, shown.sim);
        assert_eq!(verified.first.settings, shown.settings);
    }

    #[test]
    fn verification_persists_calibration_when_the_winner_changes() {
        let plan = synth::plan();
        let first_sim = SimKey::new("classic_chrome").expect("sim");
        let second_sim = SimKey::new("provia").expect("sim");
        let first_body = synth::body_for_sim(9, &first_sim).expect("synthetic body");
        let second_body = synth::body_for_sim(9, &second_sim).expect("synthetic body");
        let target_look = first_body
            .model
            .realize(&first_body.settings)
            .expect("target realization")
            .look;
        let (request, _) = request(
            target_look,
            vec![
                sim_input(first_body.model.base_look.clone(), &first_body.model.crm),
                sim_input(second_body.model.base_look.clone(), &second_body.model.crm),
            ],
        );
        let matched = crate::run_match(&request).expect("match");
        let shown = matched.results.first().expect("a suggestion").clone();
        let observed = synth::observed(&first_body.model, &plan.default, &synth::probes(), 200)
            .expect("observed at defaults");
        let blob = looklab_samples::encode(&observed).expect("encode samples");
        let verified = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: request.inputs.clone(),
            observed_blobs: vec![blob.clone()],
        })
        .expect("verify");
        assert_eq!(verified.first.sim, shown.sim);
        assert_eq!(verified.first.settings, shown.settings);
        assert_ne!(verified.second.sim, verified.first.sim);

        let next = Permalink::parse(&verified.permalink).expect("next permalink");
        assert!(!next.calibration.is_empty());
        assert_eq!(next.sim, verified.second.sim.to_string());
        let mut reloaded = request.inputs.clone();
        reloaded.calibration = next.calibration;
        let replay = crate::run_match(&crate::MatchRequest {
            inputs: reloaded.clone(),
        })
        .expect("replay");
        let replayed = replay.results.first().expect("a suggestion");
        assert_eq!(replayed.sim, verified.second.sim);
        assert_eq!(replayed.settings, verified.second.settings);
        assert_eq!(replay.permalink, verified.permalink);

        let round_two = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: reloaded,
            observed_blobs: vec![blob],
        })
        .expect("verify again");
        assert_eq!(round_two.first.sim, verified.second.sim);
        assert_eq!(round_two.first.settings, verified.second.settings);
    }
}
