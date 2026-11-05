// SPDX-License-Identifier: AGPL-3.0-or-later
//! Match and verify over published site data ([docs/04 §3-§5](../../../docs/04-matching-and-export.md)).

use looklab_color::Mat3;
use looklab_match::{
    ControlCovariance, Emphasis, FitObservedOptions, MatchModel, MatchOptions, Permalink,
    PermalinkSources, Plan, PreparedTarget, ScaleFitter, TargetSpec, VerifyRequest, optimize,
    settings_card, verify_once,
};
use looklab_model::file::LookDocument;
use looklab_model::realization::{ControlResponseModel, decode_crm};
use looklab_model::{ProbeSet, ProbeSetVersion};
use looklab_schema::ControlSchema;
use serde::{Deserialize, Serialize};

use crate::dto::{CardDto, DeltaStatsDto, MatchReply, StatsDto, SuggestionDto, VerifyReply};
use crate::error::FacadeError;

pub const TRANSFORM_FORMAT: &str = "looklab-transform/1";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceNames {
    pub target_look: Option<String>,
    pub base_look: Option<String>,
    pub body: Option<String>,
    pub crm: Option<String>,
    pub transform: Option<String>,
    pub covariance: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchInputs {
    pub target_look: Vec<u8>,
    pub base_look: Option<Vec<u8>>,
    pub body_yaml: String,
    pub crm: Vec<u8>,
    pub transform_yaml: Option<String>,
    pub covariance: Option<Vec<u8>>,
    pub emphasis: String,
    pub epoch: u32,
    pub input_transform_version: u32,
    pub realization_version: u32,
    pub seed: u64,
    pub sources: SourceNames,
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

struct Case {
    target: LookDocument,
    base: LookDocument,
    schema: ControlSchema,
    crm: ControlResponseModel,
    transform: Mat3,
    covariance: Option<ControlCovariance>,
    emphasis: Emphasis,
    probes: ProbeSet,
    options: MatchOptions,
    sources: SourceNames,
    inputs: MatchInputs,
}

impl Case {
    fn new(inputs: &MatchInputs) -> Result<Self, FacadeError> {
        let target = LookDocument::decode(&inputs.target_look)?;
        let base = match &inputs.base_look {
            Some(bytes) => LookDocument::decode(bytes)?,
            None => target.clone(),
        };
        let schema = looklab_schema::validate_control_schema(&inputs.body_yaml)
            .map_err(FacadeError::Schema)?;
        let crm = decode_crm(&inputs.crm)?;
        if crm.epoch != inputs.epoch {
            return Err(FacadeError::Input(format!(
                "control-response model is for epoch {}, not {}",
                crm.epoch, inputs.epoch
            )));
        }
        if target.file.probe_set != ProbeSetVersion::V1 {
            return Err(FacadeError::Input("unsupported probe set".to_owned()));
        }
        let transform = match &inputs.transform_yaml {
            Some(yaml) => parse_transform(yaml)?,
            None => Mat3::IDENTITY,
        };
        let covariance = match &inputs.covariance {
            Some(bytes) => Some(ControlCovariance::from_json(bytes)?),
            None => None,
        };
        let emphasis = Emphasis::parse(&inputs.emphasis)
            .ok_or_else(|| FacadeError::Input(format!("unknown emphasis {}", inputs.emphasis)))?;
        Ok(Self {
            target,
            base,
            schema,
            crm,
            transform,
            covariance,
            emphasis,
            probes: looklab_model::probes::v1(),
            options: MatchOptions {
                seed: inputs.seed,
                ..MatchOptions::default()
            },
            sources: inputs.sources.clone(),
            inputs: inputs.clone(),
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

    fn model(&self) -> Result<MatchModel, FacadeError> {
        Ok(MatchModel::new(
            self.base.look.clone(),
            self.crm.clone(),
            self.transform,
            self.covariance.clone(),
            self.inputs.input_transform_version,
            self.inputs.realization_version,
        )?)
    }

    fn plan(&self) -> Result<Plan, FacadeError> {
        Ok(Plan::new(&self.schema, self.inputs.epoch)?)
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

    fn permalink(&self, model: &MatchModel, sim: &looklab_schema::SimKey) -> String {
        Permalink {
            target_slug: self.target.file.look.slug.to_string(),
            target_version: self.target.file.look.version.to_string(),
            base_slug: self.base.file.look.slug.to_string(),
            base_version: self.base.file.look.version.to_string(),
            body: self.schema.body.to_string(),
            epoch: self.inputs.epoch,
            sim: sim.to_string(),
            crm_version: model.version(),
            input_transform_version: self.inputs.input_transform_version,
            realization_params_version: self.inputs.realization_version,
            epoch_map_version: self.epoch_map_version(),
            probe_set: self.target.file.probe_set,
            optimizer: looklab_match::OPTIMIZER_VERSION.to_owned(),
            emphasis: self.emphasis,
            seed: self.inputs.seed,
            sources: PermalinkSources {
                target_look: self.sources.target_look.clone(),
                base_look: self.sources.base_look.clone(),
                body: self.sources.body.clone(),
                crm: self.sources.crm.clone(),
                transform: self.sources.transform.clone(),
                covariance: self.sources.covariance.clone(),
            },
        }
        .canonical()
    }
}

/// Runs the optimizer on the published inputs.
///
/// # Errors
/// Reports malformed site artifacts, an unsupported probe set or a match with
/// no supported probe.
pub fn run_match(message: &MatchRequest) -> Result<MatchReply, FacadeError> {
    let case = Case::new(&message.inputs)?;
    let plan = case.plan()?;
    let target = case.target()?;
    let model = case.model()?;
    let outcome = optimize(std::slice::from_ref(&model), &target, &plan, &case.options)?;
    let best = outcome
        .results
        .first()
        .ok_or_else(|| FacadeError::Input("no settings suggestion".to_owned()))?;
    let card = settings_card(&target, &plan, &model, best)?;
    let realization = model.realize(&best.settings)?;
    let realization_lut =
        looklab_publish::bake_lut(&realization.look, |xyz| model.input_xyz(&realization, xyz));
    Ok(MatchReply {
        target_slug: case.target.file.look.slug.clone(),
        target_version: case.target.file.look.version.clone(),
        base_slug: case.base.file.look.slug.clone(),
        base_version: case.base.file.look.version.clone(),
        body: case.schema.body.clone(),
        epoch: case.inputs.epoch,
        epoch_map_version: case.epoch_map_version(),
        crm_version: model.version(),
        permalink: case.permalink(&model, &best.sim),
        card: CardDto::from_card(&card),
        results: outcome
            .results
            .iter()
            .map(SuggestionDto::from_suggestion)
            .collect(),
        stats: StatsDto::from_outcome(&outcome),
        realization_lut,
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
    let model = case.model()?;
    let observed = decode_observed(&message.observed_blobs)?;
    let request = VerifyRequest {
        target: &target,
        plan: &plan,
        model: &model,
        observed: &observed,
        options: &case.options,
        fit_options: FitObservedOptions::default(),
        truth: None,
    };
    let outcome = verify_once(&request, &ScaleFitter)?;
    let card = settings_card(&target, &plan, &outcome.updated, &outcome.second)?;
    Ok(VerifyReply {
        permalink: case.permalink(&model, &outcome.first.sim),
        first: SuggestionDto::from_suggestion(&outcome.first),
        second: SuggestionDto::from_suggestion(&outcome.second),
        first_measured: DeltaStatsDto::from_stats(&outcome.first_measured),
        second_measured: outcome
            .second_measured
            .as_ref()
            .map(DeltaStatsDto::from_stats),
        reduction: outcome.reduction,
        updated_crm_version: outcome.updated.version(),
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
    use looklab_match::synth;
    use looklab_model::Look;
    use looklab_model::file::identity;
    use looklab_model::realization::encode_crm;
    use looklab_schema::SimKey;

    use super::{ControlResponseModel, LookDocument, MatchInputs, MatchRequest, SourceNames};

    fn request(look: Look, crm: &ControlResponseModel) -> (MatchRequest, LookDocument) {
        let document = LookDocument::from_look(
            identity("preview-regression", "1.0.0").expect("identity"),
            look,
        )
        .expect("document");
        let request = MatchRequest {
            inputs: MatchInputs {
                target_look: document.encode().expect("encode look"),
                base_look: None,
                body_yaml: synth::SYNTH_SCHEMA.to_owned(),
                crm: encode_crm(crm).expect("encode CRM"),
                transform_yaml: None,
                covariance: None,
                emphasis: "balanced".to_owned(),
                epoch: 1,
                input_transform_version: 1,
                realization_version: 1,
                seed: 7,
                sources: SourceNames::default(),
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
        let (request, document) = request(Look::colorimetric(), &crm);
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
        let (request, _) = request(body.model.base_look, &body.model.crm);
        let reply = crate::run_match(&request).expect("match");
        assert_finite_bounded_lut(&reply.realization_lut);
    }
}
