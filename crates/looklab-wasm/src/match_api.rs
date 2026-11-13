// SPDX-License-Identifier: AGPL-3.0-or-later
//! Match and verify over published site data ([docs/04 §3-§5](../../../docs/04-matching-and-export.md)).

use looklab_color::Mat3;
use looklab_match::{
    AssessedDomain, Calibration, ControlCovariance, Emphasis, FitObservedOptions, ImageCount,
    MatchModel, MatchOptions, OutputContext, OutputPin, PRE_CLIP_OPTIMIZER_VERSION, Permalink,
    PermalinkSources, Plan, PreparedTarget, ReportPin, ScaleFitter, ScoringMetric,
    SelectionContext, SelectionMode, TargetAppearance, TargetSpec, measure_target_error, optimize,
    settings_card_with_support,
};
use looklab_model::file::LookDocument;
use looklab_model::realization::decode_crm;
use looklab_model::{ProbeSet, ProbeSetVersion};
use looklab_publish::ExcludedSim;
use looklab_schema::{
    AssessmentOutcome, BodyRealizationVersionId, ControlSchema, MatchParams, ModelSubject,
    ModelVersionId, OutputEncoding, QualificationCapability, ReportScope, Sha256Hex, SimKey,
    SupportedMask,
};
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
    #[serde(default)]
    pub realization_version: Option<u32>,
    /// Published `looklab-match-params/1` body parameters.
    #[serde(default)]
    pub params: Option<Vec<u8>>,
    /// Published `looklab-match-params/1` per-sim realization and firmware params.
    #[serde(default)]
    pub realization_params: Option<Vec<u8>>,
    #[serde(default)]
    pub wb_mode: Option<looklab_schema::WbModeKey>,
    #[serde(default)]
    pub firmware: Option<looklab_schema::FirmwareId>,
}

/// The worker form of a controls report; `PublishedReportSummary` cannot decode from postcard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlsReport {
    pub candidate: BodyRealizationVersionId,
    pub capability: QualificationCapability,
    pub outcome: AssessmentOutcome,
    pub scope: ReportScope,
    pub supported_mask: SupportedMask,
}

impl ControlsReport {
    /// # Errors
    /// A report that is not a passing controls report, or whose subject is not
    /// the body realization its candidate names.
    pub fn from_summary(
        summary: &looklab_publish::PublishedReportSummary,
    ) -> Result<Self, FacadeError> {
        if summary.capability != QualificationCapability::ControlsMatching {
            return Err(FacadeError::Input(format!(
                "the controls report is a {} report",
                summary.capability.as_str()
            )));
        }
        if summary.outcome != AssessmentOutcome::Pass {
            return Err(FacadeError::Input(format!(
                "the controls report outcome is {}",
                summary.outcome.as_str()
            )));
        }
        let ModelSubject::BodyRealization { realization } = &summary.subject else {
            return Err(FacadeError::Input(
                "a qualified match needs a controls report per body realization".to_owned(),
            ));
        };
        let ModelVersionId::BodyRealization {
            realization: candidate,
        } = &summary.candidate
        else {
            return Err(FacadeError::Input(
                "the controls report candidate is not a body realization".to_owned(),
            ));
        };
        if &candidate.realization != realization {
            return Err(FacadeError::Input(format!(
                "the controls report candidate names {}, not its subject {}",
                candidate.realization.native_sim, realization.native_sim
            )));
        }
        Ok(Self {
            candidate: candidate.clone(),
            capability: summary.capability,
            outcome: summary.outcome,
            scope: summary.scope.clone(),
            supported_mask: summary.supported_mask.clone(),
        })
    }

    /// # Errors
    /// A report that [`Self::from_summary`] refuses, or a candidate that differs.
    pub fn from_pinned_summary(
        summary: &looklab_publish::PublishedReportSummary,
        realization: &BodyRealizationVersionId,
    ) -> Result<Self, FacadeError> {
        let ModelVersionId::BodyRealization {
            realization: candidate,
        } = &summary.candidate
        else {
            return Err(FacadeError::Input(
                "the controls report candidate is not a body realization".to_owned(),
            ));
        };
        if candidate != realization {
            return Err(FacadeError::Input(format!(
                "the controls report for {} names another realization version or target than the pinned {}",
                candidate.realization.native_sim, realization.realization.native_sim
            )));
        }
        Self::from_summary(summary)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchSelection {
    pub bundle: Sha256Hex,
    pub mode: SelectionMode,
    pub reports: Vec<ReportPin>,
    #[serde(default)]
    pub firmware: Option<looklab_schema::FirmwareId>,
    #[serde(default)]
    pub wb_mode: Option<looklab_schema::WbModeKey>,
    /// One passing controls report per ranked destination realization.
    pub controls: Vec<ControlsReport>,
}

impl MatchSelection {
    /// # Errors
    /// A qualified selection with no pinned report.
    pub fn context(&self) -> Result<SelectionContext, looklab_match::MatchError> {
        let context = SelectionContext {
            bundle: self.bundle,
            mode: self.mode,
            reports: self.reports.clone(),
            firmware: self.firmware.clone(),
            wb_mode: self.wb_mode.clone(),
        };
        context.validate()?;
        Ok(context)
    }
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
    pub selection: Option<MatchSelection>,
    pub metric: ScoringMetric,
    pub output_encoding: Option<OutputEncoding>,
    pub approximated_base: Option<String>,
    pub destination_scope: Option<String>,
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
    params: Option<MatchParams>,
    realization: Option<looklab_schema::MatchRealizationParams>,
}

struct ScopedProbes {
    probes: ProbeSet,
    uncertainty: Option<looklab_model::file::UncertaintyData>,
    declared_points: usize,
}

struct Case {
    target: LookDocument,
    appearance: TargetAppearance,
    schema: ControlSchema,
    emphasis: Emphasis,
    probes: ProbeSet,
    options: MatchOptions,
    sources: SourceNames,
    inputs: MatchInputs,
    sims: Vec<LoadedSim>,
    excluded: Vec<ExcludedSim>,
}

impl Case {
    fn new(inputs: &MatchInputs) -> Result<Self, FacadeError> {
        if inputs.selection.as_ref().is_some_and(|selection| {
            selection.mode == SelectionMode::Qualified && !inputs.calibration.is_empty()
        }) {
            return Err(FacadeError::Input(
                "a qualified match requires an identity local calibration".to_owned(),
            ));
        }
        match inputs.metric {
            ScoringMetric::RenderedPostClipV1 if inputs.output_encoding.is_none() => {
                return Err(FacadeError::Input(
                    "a rendered match needs the selected camera output encoding".to_owned(),
                ));
            }
            ScoringMetric::LegacyPreClipV1 if inputs.output_encoding.is_some() => {
                return Err(FacadeError::Input(
                    "a historical pre-clip match has no output encoding".to_owned(),
                ));
            }
            _ => {}
        }
        let target = LookDocument::decode(&inputs.target_look)?;
        let appearance = match inputs.metric {
            ScoringMetric::RenderedPostClipV1 => {
                TargetAppearance::Rendered(OutputPin::from_document(&target)?)
            }
            ScoringMetric::LegacyPreClipV1 => TargetAppearance::PreClip,
        };
        let output_encoding = inputs.output_encoding.unwrap_or(OutputEncoding::Srgb);
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
        let mut excluded = inputs.excluded.clone();
        let chosen_firmware = inputs
            .selection
            .as_ref()
            .and_then(|selection| selection.firmware.as_ref());
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
            let params = match &input.params {
                Some(bytes) => {
                    let params: MatchParams = serde_json::from_slice(bytes).map_err(|error| {
                        FacadeError::Input(format!("match params for {}: {error}", input.sim))
                    })?;
                    params.validate(&schema.body).map_err(|error| {
                        FacadeError::Input(format!("match params for {}: {error}", input.sim))
                    })?;
                    Some(params)
                }
                None => None,
            };
            let realization = match &input.realization_params {
                Some(bytes) => {
                    let params: looklab_schema::MatchRealizationParams =
                        serde_json::from_slice(bytes).map_err(|error| {
                            FacadeError::Input(format!(
                                "realization params for {}: {error}",
                                input.sim
                            ))
                        })?;
                    params.validate().map_err(|error| {
                        FacadeError::Input(format!("realization params for {}: {error}", input.sim))
                    })?;
                    if params.realization.realization.native_sim != input.sim
                        || params.realization.realization.body != schema.body
                        || params.realization.realization.epoch.get() != inputs.epoch
                    {
                        return Err(FacadeError::Input(format!(
                            "realization params for {} name another realization",
                            input.sim
                        )));
                    }
                    if params.control_response_version != crm.version {
                        return Err(FacadeError::Input(format!(
                            "realization params for {} pin control-response version {}, not {}",
                            input.sim, params.control_response_version, crm.version
                        )));
                    }
                    if let Some(firmware) = chosen_firmware
                        && !params
                            .firmware
                            .iter()
                            .any(|entry| &entry.firmware == firmware)
                    {
                        excluded.push(ExcludedSim {
                            sim: input.sim.clone(),
                            reason: looklab_publish::SimExclusion::MissingFirmware,
                        });
                        continue;
                    }
                    Some(params)
                }
                None => None,
            };
            let mut model = MatchModel::new(
                base.look.clone(),
                crm,
                transform,
                covariance,
                inputs.input_transform_version,
                realization.as_ref().map_or(
                    input
                        .realization_version
                        .unwrap_or(inputs.realization_version),
                    |params| params.version,
                ),
                output_encoding,
            )?;
            if let Some(params) = &params {
                model = model.with_params(params.clone())?;
            }
            if let Some(realization) = &realization {
                model = model.with_realization(realization.clone());
            }
            model = model.with_modes(input.wb_mode.clone(), input.firmware.clone());
            sims.push(LoadedSim {
                input: input.clone(),
                base,
                model,
                params,
                realization,
            });
        }
        Ok(Self {
            target,
            appearance,
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
            excluded,
        })
    }

    fn target<'a>(
        &'a self,
        scoped: &'a ScopedProbes,
        mode: Option<SelectionMode>,
    ) -> Result<PreparedTarget<'a>, FacadeError> {
        let uncertainty = scoped
            .uncertainty
            .as_ref()
            .unwrap_or(&self.target.uncertainty);
        let spec = TargetSpec {
            probes: &scoped.probes,
            function: &self.target.look,
            uncertainty: Some(uncertainty),
            emphasis: self.emphasis,
            tau: 1.0,
            support_sigma: 1.5,
            density_floor: None,
            appearance: self.appearance,
        };
        let experimental = mode == Some(SelectionMode::Experimental)
            && self.target.file.look.kind == looklab_model::LookKind::NativeFamily;
        Ok(if experimental {
            PreparedTarget::experimental(spec, ImageCount::of_document(&self.target))?
        } else {
            PreparedTarget::new(spec)?
        })
    }

    fn probes(
        &self,
        domains: &std::collections::BTreeMap<SimKey, AssessedDomain>,
    ) -> Result<ScopedProbes, FacadeError> {
        let Some(first) = domains.values().next() else {
            return Ok(ScopedProbes {
                probes: self.probes.clone(),
                uncertainty: None,
                declared_points: self.probes.probes.len(),
            });
        };
        let mut common = first.clone();
        for domain in domains.values().skip(1) {
            common.intersect_cells(domain);
        }
        let (probes, indices) = common.filter_probes_with_indices(&self.probes)?;
        let uncertainty = (indices.len() != self.probes.probes.len())
            .then(|| self.target.uncertainty.restrict_to(&indices))
            .transpose()?;
        Ok(ScopedProbes {
            probes,
            uncertainty,
            declared_points: self.probes.probes.len(),
        })
    }

    fn plan(&self) -> Result<Plan, FacadeError> {
        Ok(Plan::new(&self.schema, self.inputs.epoch)?)
    }

    fn domains(
        &self,
        plan: &Plan,
    ) -> Result<std::collections::BTreeMap<SimKey, AssessedDomain>, FacadeError> {
        let Some(selection) = &self.inputs.selection else {
            return Ok(std::collections::BTreeMap::new());
        };
        selection.context()?;
        if selection.mode != SelectionMode::Qualified {
            return Ok(std::collections::BTreeMap::new());
        }
        if selection.controls.is_empty() {
            return Err(FacadeError::Input(
                "a qualified match needs the destination realization's controls reports".to_owned(),
            ));
        }
        let mut domains = std::collections::BTreeMap::new();
        for report in &selection.controls {
            let realization = &report.candidate.realization;
            if realization.body != self.schema.body || realization.epoch.get() != self.inputs.epoch
            {
                return Err(FacadeError::Input(format!(
                    "the controls report for {} is for {} epoch {}",
                    realization.native_sim,
                    realization.body,
                    realization.epoch.get()
                )));
            }
            let Some(loaded) = self
                .sims
                .iter()
                .find(|loaded| loaded.input.sim == realization.native_sim)
            else {
                continue;
            };
            let params = loaded.params.as_ref().ok_or_else(|| {
                FacadeError::Input(format!(
                    "no pinned body parameters for {}",
                    realization.native_sim
                ))
            })?;
            let realization_params = loaded.realization.as_ref().ok_or_else(|| {
                FacadeError::Input(format!(
                    "no pinned realization parameters for {}",
                    realization.native_sim
                ))
            })?;
            if report.candidate != realization_params.realization {
                return Err(FacadeError::Input(format!(
                    "the controls report for {} names another realization version or target than the pinned {}",
                    realization.native_sim, realization_params.realization.realization.native_sim
                )));
            }
            let pinned_firmware: Vec<_> = realization_params
                .firmware
                .iter()
                .map(|entry| entry.firmware.clone())
                .collect();
            let pinned_wb: Vec<_> = params.wb.iter().map(|entry| entry.mode.clone()).collect();
            domains.insert(
                realization.native_sim.clone(),
                AssessedDomain::from_parts(
                    plan,
                    report.capability,
                    report.outcome,
                    &report.scope,
                    &report.supported_mask,
                    selection.firmware.as_ref(),
                    selection.wb_mode.as_ref(),
                    &pinned_firmware,
                    &pinned_wb,
                )?,
            );
        }
        if domains.is_empty() {
            return Err(FacadeError::Input(
                "no ranked sim has a passing controls report".to_owned(),
            ));
        }
        Ok(domains)
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

    fn permalink(
        &self,
        loaded: &LoadedSim,
        sim: &SimKey,
        calibration: &Calibration,
        mode: Option<SelectionMode>,
    ) -> Result<String, FacadeError> {
        let output = match self.appearance.pin() {
            Some(target) => Some(OutputContext {
                target,
                camera: OutputPin::new(
                    loaded.model.output_encoding,
                    loaded.model.realized_look()?.clip.k,
                )?,
            }),
            None => None,
        };
        Ok(Permalink {
            target_slug: self.target.file.look.slug.to_string(),
            target_version: self.target.file.look.version.to_string(),
            base_slug: loaded.base.file.look.slug.to_string(),
            base_version: loaded.base.file.look.version.to_string(),
            body: self.schema.body.to_string(),
            epoch: self.inputs.epoch,
            sim: sim.to_string(),
            crm_version: loaded.model.version(),
            input_transform_version: self.inputs.input_transform_version,
            realization_params_version: loaded.model.realization_params_version,
            epoch_map_version: self.epoch_map_version(),
            probe_set: self.target.file.probe_set,
            optimizer: if self.inputs.metric.is_rendered() {
                looklab_match::OPTIMIZER_VERSION
            } else {
                PRE_CLIP_OPTIMIZER_VERSION
            }
            .to_owned(),
            metric: self.inputs.metric,
            output,
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
            selection: self
                .inputs
                .selection
                .as_ref()
                .map(|selection| SelectionContext {
                    bundle: selection.bundle,
                    mode: mode.unwrap_or(selection.mode),
                    reports: selection.reports.clone(),
                    firmware: selection.firmware.clone(),
                    wb_mode: selection.wb_mode.clone(),
                }),
        }
        .canonical())
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
    let domains = case.domains(&plan)?;
    let scoped = case.probes(&domains)?;
    let mode = case
        .inputs
        .selection
        .as_ref()
        .map(|selection| selection.mode);
    let target = case
        .target(&scoped, mode)?
        .with_declared_points(scoped.declared_points)?;
    let models = case.models(&plan)?;
    let outcome = if domains.is_empty() {
        optimize(&models, &target, &plan, &case.options)?
    } else {
        looklab_match::optimize_scoped(&models, &target, &plan, &case.options, &domains)?
    };
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
    let mut card =
        settings_card_with_support(&target, &plan, winner_model, best, domains.get(&best.sim))?;
    card.approximated_base = case.inputs.approximated_base.clone();
    card.destination_scope = case.inputs.destination_scope.clone();
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
        permalink: case.permalink(winner, &best.sim, &case.inputs.calibration, None)?,
        selection: case
            .inputs
            .selection
            .as_ref()
            .map(|selection| SelectionContext {
                bundle: selection.bundle,
                mode: selection.mode,
                reports: selection.reports.clone(),
                firmware: selection.firmware.clone(),
                wb_mode: selection.wb_mode.clone(),
            }),
        card: CardDto::from_card(&card),
        results: outcome
            .results
            .iter()
            .map(SuggestionDto::from_suggestion)
            .collect(),
        stats: StatsDto::from_outcome(&outcome),
        realization_lut,
        excluded: case.excluded.clone(),
    })
}

/// Runs one verification-loop iteration on local re-renders.
///
/// # Errors
/// Reports malformed site artifacts, an empty observed set or a failed fit.
pub fn run_verify(message: &VerifyRequestMessage) -> Result<VerifyReply, FacadeError> {
    let case = Case::new(&message.inputs)?;
    let plan = case.plan()?;
    let domains = case.domains(&plan)?;
    let scoped = case.probes(&domains)?;
    let mode = case
        .inputs
        .selection
        .as_ref()
        .map(|selection| selection.mode);
    let target = case
        .target(&scoped, mode)?
        .with_declared_points(scoped.declared_points)?;
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
    let first_outcome = if domains.is_empty() {
        optimize(&models, &target, &plan, &case.options)?
    } else {
        looklab_match::optimize_scoped(&models, &target, &plan, &case.options, &domains)?
    };
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
        case.inputs.metric,
        &observed,
        &FitObservedOptions::default(),
    )?;
    let updated = models[winner_index].with_crm(fitted)?;
    let next_calibration = case.inputs.calibration.merged(&first.sim, &scales)?;
    let downgraded = case
        .inputs
        .selection
        .as_ref()
        .is_some_and(|selection| selection.mode == SelectionMode::Qualified)
        && !next_calibration.is_empty();
    let next_mode = case.inputs.selection.as_ref().map(|selection| {
        if downgraded {
            SelectionMode::Experimental
        } else {
            selection.mode
        }
    });
    let mut updated_models = models;
    updated_models[winner_index] = updated.clone();
    let second_scoped = if downgraded {
        Some(case.probes(&std::collections::BTreeMap::new())?)
    } else {
        None
    };
    let second_target = match &second_scoped {
        Some(scoped) => Some(
            case.target(scoped, next_mode)?
                .with_declared_points(scoped.declared_points)?,
        ),
        None => None,
    };
    let target_for_second = second_target.as_ref().unwrap_or(&target);
    let second_outcome = if second_target.is_some() || domains.is_empty() {
        optimize(&updated_models, target_for_second, &plan, &case.options)?
    } else {
        looklab_match::optimize_scoped(
            &updated_models,
            target_for_second,
            &plan,
            &case.options,
            &domains,
        )?
    };
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
    let support = if downgraded {
        None
    } else {
        domains.get(&second.sim)
    };
    let mut card =
        settings_card_with_support(target_for_second, &plan, card_model, &second, support)?;
    card.approximated_base = case.inputs.approximated_base.clone();
    card.destination_scope = if downgraded {
        let excluded: Vec<String> = case
            .excluded
            .iter()
            .map(|entry| entry.sim.as_str().to_owned())
            .collect();
        looklab_match::destination_scope(next_mode, &excluded)
    } else {
        case.inputs.destination_scope.clone()
    };
    let next_sim = case.sim(&second.sim)?;
    Ok(VerifyReply {
        permalink: case.permalink(next_sim, &second.sim, &next_calibration, next_mode)?,
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
    use looklab_match::{
        AssessedDomain, Calibration, ControlValue, Permalink, PermalinkSources, Plan,
        QualificationRef, ReportPin, ReportSubject, ScoringMetric, SelectionContext, SelectionMode,
        Settings, synth,
    };
    use looklab_match::{AssessmentStatus, PRE_CLIP_OPTIMIZER_VERSION};
    use looklab_model::file::{Density, ProbeCovariance, ProbeScalars, UncertaintyData, identity};
    use looklab_model::realization::{ControlResponseModel, encode_crm};
    use looklab_model::{Look, ProbeSetVersion};
    use looklab_publish::{ExcludedSim, PublishedReportSummary, ReportFormat, SimExclusion};
    use looklab_schema::{
        AssessedPoint, AssessmentOutcome, BodyId, BodyRealizationId, BodyRealizationVersionId,
        CellId, CombinationBasis, ControlEpochId, DomainId, FamilyTargetId, FirmwareId,
        FirmwareOffset, LookId, MatchParams, MatchParamsFormat, MatchRealizationParams, MatchStage,
        MatchTransform, ModelReportId, ModelSubject, ModelVersionId, OutputEncoding,
        QualificationCapability, RealizationRevisionId, RealizationVersion, ReportMetrics,
        ReportScope, RuleRevision, SettingCombination, SettingValue, Sha256Hex, SimKey,
        SupportedMask, TargetVersion, WbModeKey, WbModeOffset,
    };

    use super::{
        ControlsReport, FacadeError, LookDocument, MatchInputs, MatchRequest, MatchSelection,
        SimInputs, SimSources, SourceNames,
    };

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
            realization_version: None,
            params: None,
            realization_params: None,
            wb_mode: None,
            firmware: None,
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
                selection: None,
                metric: ScoringMetric::LegacyPreClipV1,
                output_encoding: None,
                approximated_base: None,
                destination_scope: None,
            },
        };
        (request, document)
    }

    #[test]
    fn worker_encoding_preserves_optional_sim_fields() {
        let body = synth::body(9).expect("synthetic body");
        let mut sim = sim_input(body.model.base_look.clone(), &body.model.crm);
        sim.params = Some(vec![0xAA, 0xBB]);
        sim.wb_mode = Some(looklab_schema::WbModeKey::new("auto").expect("wb mode"));
        let (message, _) = request(body.model.base_look.clone(), vec![sim]);
        let wire = crate::encode_match_request(&message);
        let decoded: MatchRequest = postcard::from_bytes(&wire).expect("worker request decodes");
        assert_eq!(decoded, message);
    }

    fn two_probe_cells() -> (CellId, CellId) {
        let probes = synth::probes();
        let mut counts: std::collections::BTreeMap<CellId, usize> =
            std::collections::BTreeMap::new();
        for probe in &probes.probes {
            if let Some(cell) = AssessedDomain::cell_of(probe) {
                *counts.entry(cell).or_default() += 1;
            }
        }
        let mut cells: Vec<(usize, CellId)> = counts
            .into_iter()
            .map(|(cell, count)| (count, cell))
            .collect();
        cells.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
        assert!(cells.len() > 1, "two assessed cells");
        (cells[0].1.clone(), cells[1].1.clone())
    }

    fn firmware(version: &str) -> FirmwareId {
        FirmwareId {
            body: BodyId::new("fujifilm/x-t5").expect("body"),
            epoch: ControlEpochId::new(1),
            version: version.parse().expect("firmware version"),
        }
    }

    fn realization_candidate(sim: &SimKey, version: u32) -> BodyRealizationVersionId {
        BodyRealizationVersionId {
            realization: BodyRealizationId {
                body: BodyId::new("fujifilm/x-t5").expect("body"),
                epoch: ControlEpochId::new(1),
                native_sim: sim.clone(),
            },
            version: RealizationVersion::new(version).expect("realization version"),
            target: FamilyTargetId {
                family: LookId::new("classic-chrome").expect("look"),
                version: TargetVersion::new(1).expect("target version"),
            },
        }
    }

    fn setting_value(value: &ControlValue) -> SettingValue {
        match value {
            ControlValue::Step(step) => SettingValue::Step(*step),
            ControlValue::Enum(value) => SettingValue::Enum(value.clone()),
            ControlValue::Bool(value) => SettingValue::Bool(*value),
        }
    }

    fn nondefault_point(plan: &Plan) -> (SettingCombination, Settings) {
        let index = plan
            .plan_controls
            .iter()
            .copied()
            .find(|&index| plan.domains[index].values.len() > 1)
            .expect("a planned control with alternatives");
        let domain = &plan.domains[index];
        let value = (domain.default + 1) % domain.values.len();
        let mut settings = plan.default.clone();
        plan.set(index, value, &mut settings);
        let mut combination = SettingCombination::new();
        combination.insert(domain.key.clone(), setting_value(&domain.values[value]));
        (combination, settings)
    }

    fn mask_coverage(mask: &SupportedMask) -> usize {
        let probes = synth::probes();
        probes
            .probes
            .iter()
            .filter(|probe| AssessedDomain::cell_of(probe).is_some_and(|cell| mask.contains(&cell)))
            .count()
    }

    fn document(look: Look) -> LookDocument {
        LookDocument::from_look(
            identity("preview-regression", "1.0.0").expect("identity"),
            look,
        )
        .expect("document")
    }

    fn round_trip(message: &MatchRequest) -> MatchRequest {
        let wire = crate::encode_match_request(message);
        postcard::from_bytes(&wire).expect("worker request decodes")
    }

    fn controls_summary(
        candidate: &BodyRealizationVersionId,
        points: Vec<AssessedPoint>,
        mask: SupportedMask,
    ) -> PublishedReportSummary {
        PublishedReportSummary {
            format: ReportFormat::V1,
            report: ModelReportId::from_digest([7; 32]),
            subject: ModelSubject::BodyRealization {
                realization: candidate.realization.clone(),
            },
            candidate: ModelVersionId::BodyRealization {
                realization: candidate.clone(),
            },
            capability: QualificationCapability::ControlsMatching,
            scope: ReportScope {
                points,
                combination_basis: CombinationBasis::Enumerated,
                defaults_assessed: false,
                output_encoding: OutputEncoding::Srgb,
                colour_cells: Vec::new(),
                exposure_cells: Vec::new(),
                assessed_bodies: vec![candidate.realization.body.clone()],
                assessed_domain: DomainId::new("supported-probes").expect("domain"),
            },
            rule_revision: RuleRevision::new("controls-matching/1").expect("rule revision"),
            outcome: AssessmentOutcome::Pass,
            metrics: ReportMetrics::new(),
            supported_mask: mask,
            limitations: Vec::new(),
            evidence_requests: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    struct Qualified {
        message: MatchRequest,
        sim: SimKey,
        body: synth::SyntheticBody,
        settings: Settings,
        selection: SelectionContext,
        mask: SupportedMask,
    }

    impl Qualified {
        fn wire(&self) -> MatchRequest {
            round_trip(&self.message)
        }
    }

    fn qualified_request_with(
        pinned_firmware: &[FirmwareId],
        pinned_wb: &[WbModeKey],
        assessed: &[(FirmwareId, WbModeKey)],
        requested: Option<(FirmwareId, WbModeKey)>,
        mask: SupportedMask,
    ) -> Qualified {
        let sim = SimKey::new(synth::SYNTH_SIM).expect("sim");
        let body = synth::body(9).expect("synthetic body");
        let (combination, settings) = nondefault_point(&synth::plan());
        let candidate = realization_candidate(&sim, 2);
        let points = assessed
            .iter()
            .map(|(firmware, wb)| AssessedPoint {
                settings: combination.clone(),
                firmware: Some(firmware.clone()),
                wb_mode: Some(wb.clone()),
            })
            .collect();
        let summary = controls_summary(&candidate, points, mask.clone());
        let controls = vec![ControlsReport::from_summary(&summary).expect("controls report")];
        let mut sim_input = sim_input(body.model.base_look.clone(), &body.model.crm);
        sim_input.realization_version = Some(candidate.version.get() + 1);
        sim_input.params = Some(
            serde_json::to_vec(&MatchParams {
                format: MatchParamsFormat::V1,
                stage: MatchStage::Published,
                body: candidate.realization.body.clone(),
                transform: MatchTransform {
                    version: 1,
                    camera_to_xyz: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    delta: [0.0; 6],
                },
                exposure: Vec::new(),
                wb: pinned_wb
                    .iter()
                    .map(|mode| WbModeOffset {
                        mode: mode.clone(),
                        r: 0.0,
                        b: 0.0,
                    })
                    .collect(),
            })
            .expect("body params"),
        );
        sim_input.realization_params = Some(
            serde_json::to_vec(&MatchRealizationParams {
                format: MatchParamsFormat::V1,
                realization: candidate.clone(),
                family: candidate.target.clone(),
                version: candidate.version.get(),
                control_response_version: body.model.crm.version,
                clip_k: 0.03,
                firmware: pinned_firmware
                    .iter()
                    .map(|firmware| FirmwareOffset {
                        firmware: firmware.clone(),
                        t_shift: 0.0,
                        log_g_shift: 0.0,
                    })
                    .collect(),
            })
            .expect("realization params"),
        );
        sim_input.wb_mode = requested.as_ref().map(|(_, wb)| wb.clone());
        sim_input.firmware = requested.as_ref().map(|(firmware, _)| firmware.clone());
        let (mut message, _) = request(Look::colorimetric(), vec![sim_input]);
        message.inputs.selection = Some(MatchSelection {
            bundle: Sha256Hex::from_bytes([3; 32]),
            mode: SelectionMode::Qualified,
            reports: vec![ReportPin {
                subject: ReportSubject::Realization(RealizationRevisionId::for_version(&candidate)),
                reference: QualificationRef::Report(ModelReportId::from_digest([7; 32])),
            }],
            firmware: requested.as_ref().map(|(firmware, _)| firmware.clone()),
            wb_mode: requested.as_ref().map(|(_, wb)| wb.clone()),
            controls,
        });
        let selection = message
            .inputs
            .selection
            .as_ref()
            .expect("selection")
            .context()
            .expect("selection context");
        Qualified {
            message,
            sim,
            body,
            settings,
            selection,
            mask,
        }
    }

    fn qualified_request() -> Qualified {
        let firmware_id = firmware("1.00");
        let wb = WbModeKey::new("daylight").expect("wb mode");
        let mut mask = SupportedMask::new();
        mask.insert(two_probe_cells().0);
        qualified_request_with(
            std::slice::from_ref(&firmware_id),
            std::slice::from_ref(&wb),
            &[(firmware_id.clone(), wb.clone())],
            None,
            mask,
        )
    }

    #[test]
    fn an_encoded_qualified_match_stays_inside_its_report_scope() {
        let fixture = qualified_request();
        let reply = crate::run_match(&fixture.wire()).expect("match");
        let best = reply.results.first().expect("a suggestion");
        assert_eq!(best.sim, fixture.sim);
        assert_eq!(best.settings, fixture.settings);
        assert_eq!(reply.stats.supported_probes, mask_coverage(&fixture.mask));
        let permalink = Permalink::parse(&reply.permalink).expect("permalink");
        assert_eq!(permalink.selection, Some(fixture.selection.clone()));
        assert_eq!(permalink.sim, fixture.sim.to_string());
    }

    #[test]
    fn approximation_labels_survive_the_worker_wire() {
        let mut fixture = qualified_request();
        fixture.message.inputs.approximated_base = Some("classic-chrome@2.0.0".to_owned());
        fixture.message.inputs.destination_scope =
            Some("the camera realization is experimental".to_owned());
        let reply = crate::run_match(&fixture.wire()).expect("match");
        assert_eq!(
            reply.card.approximated_base.as_deref(),
            Some("classic-chrome@2.0.0")
        );
        assert_eq!(
            reply.card.destination_scope.as_deref(),
            Some("the camera realization is experimental")
        );
        let encoded = postcard::to_allocvec(&Ok::<crate::dto::MatchReply, String>(reply.clone()))
            .expect("reply encodes");
        let decoded = crate::decode_match_reply(&encoded).expect("reply decodes");
        assert_eq!(decoded.card.approximated_base, reply.card.approximated_base);
        assert_eq!(decoded.card.destination_scope, reply.card.destination_scope);
    }

    #[test]
    fn a_qualified_match_restricts_target_uncertainty_to_the_assessed_probes() {
        let fixture = qualified_request();
        let count = synth::probes().probes.len();
        let uncertainty = UncertaintyData {
            probe_cov: Some(ProbeCovariance::new(count, vec![0.0; count * 9]).expect("covariance")),
            probe_cov_post: Some(
                ProbeCovariance::new(count, vec![0.0; count * 9]).expect("covariance"),
            ),
            probe_bias: Some(ProbeScalars::new(vec![0.0; count]).expect("scalars")),
            probe_sigma: Some(ProbeScalars::new(vec![0.0; count]).expect("scalars")),
            probe_sigma_post: Some(ProbeScalars::new(vec![0.0; count]).expect("scalars")),
            probe_pre_reliable: Some(ProbeScalars::new(vec![1.0; count]).expect("scalars")),
            support: Some(ProbeScalars::new(vec![1.0; count]).expect("scalars")),
            density: Some(Density::new(vec![1.0; count]).expect("density")),
        };
        let mut message = fixture.message.clone();
        message.inputs.target_look = document(Look::colorimetric())
            .with_uncertainty(uncertainty)
            .expect("uncertainty")
            .encode()
            .expect("encode look");
        let reply = crate::run_match(&round_trip(&message)).expect("match");
        assert_eq!(
            reply.results.first().expect("a suggestion").settings,
            fixture.settings
        );
        assert!(reply.stats.supported_probes < count);
    }

    #[test]
    fn qualified_verification_keeps_the_first_search_scoped_and_downgrades_the_second() {
        let fixture = qualified_request();
        let unreported_sim = SimKey::new("provia").expect("sim");
        let unreported = synth::body_for_sim(10, &unreported_sim).expect("synthetic body");
        let mut message = fixture.message.clone();
        message.inputs.sims.push(sim_input(
            unreported.model.base_look.clone(),
            &unreported.model.crm,
        ));
        let default_look = unreported.model.base_look.clone();
        message.inputs.target_look = document(default_look).encode().expect("encode look");
        let observed = synth::observed(
            &fixture.body.model,
            &fixture.settings,
            &synth::probes(),
            300,
        )
        .expect("observed");
        let blob = looklab_samples::encode(&observed).expect("encode samples");
        let reply = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: round_trip(&message).inputs,
            observed_blobs: vec![blob],
        })
        .expect("verify");
        assert_eq!(reply.first.sim, fixture.sim);
        assert_eq!(reply.first.settings, fixture.settings);
        assert_eq!(reply.second.sim, unreported_sim);

        let permalink = Permalink::parse(&reply.permalink).expect("permalink");
        let selection = permalink.selection.as_ref().expect("selection");
        assert_eq!(selection.mode, SelectionMode::Experimental);
        assert_eq!(selection.reports, fixture.selection.reports);
        assert!(!permalink.calibration.is_empty());
    }

    #[test]
    fn a_qualified_match_binds_the_complete_candidate() {
        let fixture = qualified_request();
        let reply = crate::run_match(&fixture.wire()).expect("match");
        assert_eq!(
            reply.results.first().expect("a suggestion").settings,
            fixture.settings
        );

        let mut changed_version = fixture.message.clone();
        changed_version
            .inputs
            .selection
            .as_mut()
            .expect("selection")
            .controls[0]
            .candidate
            .version = RealizationVersion::new(3).expect("realization version");
        assert!(matches!(
            crate::run_match(&round_trip(&changed_version)),
            Err(FacadeError::Input(_))
        ));

        let mut changed_target = fixture.message.clone();
        changed_target
            .inputs
            .selection
            .as_mut()
            .expect("selection")
            .controls[0]
            .candidate
            .target
            .version = TargetVersion::new(2).expect("target version");
        assert!(matches!(
            crate::run_match(&round_trip(&changed_target)),
            Err(FacadeError::Input(_))
        ));
    }

    #[test]
    fn a_rendered_match_pins_output_context_and_replays() {
        let mut fixture = qualified_request();
        fixture.message.inputs.metric = ScoringMetric::RenderedPostClipV1;
        fixture.message.inputs.output_encoding = Some(OutputEncoding::Srgb);
        let reply = crate::run_match(&fixture.wire()).expect("match");
        assert_eq!(reply.card.metric, ScoringMetric::RenderedPostClipV1);
        let context = reply.card.output.expect("output context");
        assert_eq!(context.camera.encoding, OutputEncoding::Srgb);
        let permalink = Permalink::parse(&reply.permalink).expect("permalink");
        assert_eq!(permalink.metric, ScoringMetric::RenderedPostClipV1);
        assert_eq!(permalink.output, Some(context));
        let replay = crate::run_match(&round_trip(&fixture.message)).expect("replay");
        assert_eq!(replay.permalink, reply.permalink);
        assert_eq!(
            replay.results.first().expect("a suggestion").output,
            Some(context)
        );
    }

    #[test]
    fn a_historical_match_keeps_pre_clip_metric_and_no_output_context() {
        let fixture = qualified_request();
        let reply = crate::run_match(&fixture.wire()).expect("match");
        assert_eq!(reply.card.metric, ScoringMetric::LegacyPreClipV1);
        assert!(reply.card.output.is_none());
        let permalink = Permalink::parse(&reply.permalink).expect("permalink");
        assert_eq!(permalink.metric, ScoringMetric::LegacyPreClipV1);
        assert!(permalink.output.is_none());
        assert_eq!(permalink.optimizer, PRE_CLIP_OPTIMIZER_VERSION);
        assert_eq!(
            reply.results.first().expect("a suggestion").metric,
            ScoringMetric::LegacyPreClipV1
        );
    }

    #[test]
    fn an_unassessed_match_never_claims_reachable() {
        let body = synth::body(9).expect("synthetic body");
        let (request, _) = request(
            body.model.base_look.clone(),
            vec![sim_input(body.model.base_look.clone(), &body.model.crm)],
        );
        let reply = crate::run_match(&request).expect("match");
        assert_ne!(
            reply.card.reachability.status,
            looklab_match::ReachabilityStatus::Reachable
        );
        assert!(!reply.card.reachability.domain_assessed);
    }

    #[test]
    fn qualified_reachability_keeps_the_declared_probe_denominator() {
        let fixture = qualified_request();
        let reply = crate::run_match(&fixture.wire()).expect("match");
        assert!(reply.card.reachability.declared > reply.card.reachability.assessed);
        assert!(reply.card.reachability.domain_assessed);
        assert_ne!(
            reply.card.reachability.status,
            looklab_match::ReachabilityStatus::Reachable
        );
    }

    #[test]
    fn a_rendered_request_requires_an_output_encoding() {
        let mut fixture = qualified_request();
        fixture.message.inputs.metric = ScoringMetric::RenderedPostClipV1;
        fixture.message.inputs.output_encoding = None;
        assert!(matches!(
            crate::run_match(&round_trip(&fixture.message)),
            Err(FacadeError::Input(_))
        ));
    }

    #[test]
    fn rendered_verification_uses_the_pinned_camera_encoding() {
        let mut fixture = qualified_request();
        fixture.message.inputs.metric = ScoringMetric::RenderedPostClipV1;
        fixture.message.inputs.output_encoding = Some(OutputEncoding::Srgb);
        let observed = synth::observed(
            &fixture.body.model,
            &synth::plan().default,
            &synth::probes(),
            200,
        )
        .expect("observed");
        let blob = looklab_samples::encode(&observed).expect("encode samples");
        let verified = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: round_trip(&fixture.message).inputs,
            observed_blobs: vec![blob.clone()],
        })
        .expect("verify");
        assert_eq!(verified.first.metric, ScoringMetric::RenderedPostClipV1);
        assert_eq!(
            verified.first_measured.metric,
            ScoringMetric::RenderedPostClipV1
        );
        assert_eq!(verified.first_measured.status, AssessmentStatus::Unassessed);
        assert_eq!(
            verified
                .card
                .output
                .expect("output context")
                .camera
                .encoding,
            OutputEncoding::Srgb
        );

        let mut mismatch = fixture.message.clone();
        mismatch.inputs.output_encoding = Some(OutputEncoding::AdobeRgb);
        let error = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: round_trip(&mismatch).inputs,
            observed_blobs: vec![blob],
        })
        .expect_err("observed encoding mismatch");
        assert!(matches!(
            error,
            FacadeError::Match(looklab_match::MatchError::ObservedOutputEncoding { .. })
        ));
    }

    #[test]
    fn a_qualified_match_respects_the_assessed_firmware_and_white_balance() {
        let first_firmware = firmware("1.00");
        let second_firmware = firmware("1.01");
        let daylight = WbModeKey::new("daylight").expect("wb mode");
        let shade = WbModeKey::new("shade").expect("wb mode");
        let mut mask = SupportedMask::new();
        mask.insert(two_probe_cells().0);
        let fixture = qualified_request_with(
            &[first_firmware.clone(), second_firmware],
            &[daylight.clone(), shade.clone()],
            &[(first_firmware.clone(), daylight.clone())],
            Some((first_firmware, daylight)),
            mask,
        );
        let covered = crate::run_match(&fixture.wire()).expect("covered pair");
        assert_eq!(
            covered.results.first().expect("a suggestion").settings,
            fixture.settings
        );

        let mut uncovered = fixture.message.clone();
        uncovered
            .inputs
            .selection
            .as_mut()
            .expect("selection")
            .wb_mode = Some(shade);
        assert!(matches!(
            crate::run_match(&round_trip(&uncovered)),
            Err(FacadeError::Match(_))
        ));
    }

    #[test]
    fn a_replayed_controls_report_must_name_the_pinned_realization() {
        let pinned = realization_candidate(&SimKey::new(synth::SYNTH_SIM).expect("sim"), 2);
        let other = realization_candidate(&SimKey::new("provia").expect("sim"), 2);
        let (combination, _) = nondefault_point(&synth::plan());
        let mut mask = SupportedMask::new();
        mask.insert(two_probe_cells().0);
        let summary = controls_summary(
            &other,
            vec![AssessedPoint {
                settings: combination,
                firmware: None,
                wb_mode: None,
            }],
            mask,
        );
        assert!(ControlsReport::from_pinned_summary(&summary, &pinned).is_err());
        assert!(ControlsReport::from_pinned_summary(&summary, &other).is_ok());
    }

    #[test]
    fn a_controls_report_must_name_the_realization_its_candidate_does() {
        let candidate = realization_candidate(&SimKey::new(synth::SYNTH_SIM).expect("sim"), 2);
        let (combination, _) = nondefault_point(&synth::plan());
        let mut mask = SupportedMask::new();
        mask.insert(two_probe_cells().0);
        let mut summary = controls_summary(
            &candidate,
            vec![AssessedPoint {
                settings: combination,
                firmware: None,
                wb_mode: None,
            }],
            mask,
        );
        summary.subject = ModelSubject::BodyRealization {
            realization: BodyRealizationId {
                native_sim: SimKey::new("provia").expect("sim"),
                ..candidate.realization.clone()
            },
        };
        assert!(ControlsReport::from_summary(&summary).is_err());
    }

    fn nonidentity_calibration() -> Calibration {
        Calibration::parse("classic_chrome@color=1.2").expect("calibration")
    }

    #[test]
    fn an_encoded_qualified_request_rejects_nonidentity_calibration() {
        let mut fixture = qualified_request();
        fixture.message.inputs.calibration = nonidentity_calibration();
        let request = round_trip(&fixture.message);
        assert!(matches!(
            crate::run_match(&request),
            Err(FacadeError::Input(_))
        ));
        assert!(matches!(
            crate::run_verify(&crate::VerifyRequestMessage {
                inputs: request.inputs,
                observed_blobs: Vec::new(),
            }),
            Err(FacadeError::Input(_))
        ));
    }

    #[test]
    fn a_qualified_permalink_with_nonidentity_calibration_is_rejected() {
        let fixture = qualified_request();
        let permalink = Permalink {
            target_slug: "preview-regression".to_owned(),
            target_version: "1.0.0".to_owned(),
            base_slug: "base-classic_chrome".to_owned(),
            base_version: "1.0.0".to_owned(),
            body: "fujifilm/x-t5".to_owned(),
            epoch: 1,
            sim: "classic_chrome".to_owned(),
            crm_version: 1,
            input_transform_version: 1,
            realization_params_version: 2,
            epoch_map_version: 0,
            probe_set: ProbeSetVersion::V1,
            optimizer: PRE_CLIP_OPTIMIZER_VERSION.to_owned(),
            emphasis: looklab_match::Emphasis::default(),
            seed: 7,
            sources: PermalinkSources::default(),
            calibration: nonidentity_calibration(),
            metric: ScoringMetric::LegacyPreClipV1,
            output: None,
            selection: Some(fixture.selection),
        };
        let error = Permalink::parse(&permalink.canonical()).expect_err("qualified calibration");
        assert!(error.to_string().contains("identity calibration"));
    }

    #[test]
    fn a_qualified_permalink_with_explicit_identity_calibration_is_accepted() {
        let fixture = qualified_request();
        let permalink = Permalink {
            target_slug: "preview-regression".to_owned(),
            target_version: "1.0.0".to_owned(),
            base_slug: "base-classic_chrome".to_owned(),
            base_version: "1.0.0".to_owned(),
            body: "fujifilm/x-t5".to_owned(),
            epoch: 1,
            sim: "classic_chrome".to_owned(),
            crm_version: 1,
            input_transform_version: 1,
            realization_params_version: 2,
            epoch_map_version: 0,
            probe_set: ProbeSetVersion::V1,
            optimizer: PRE_CLIP_OPTIMIZER_VERSION.to_owned(),
            emphasis: looklab_match::Emphasis::default(),
            seed: 7,
            sources: PermalinkSources::default(),
            calibration: Calibration::default(),
            metric: ScoringMetric::LegacyPreClipV1,
            output: None,
            selection: Some(fixture.selection),
        };
        let text = permalink.canonical().replacen(
            "|bundle=",
            "|calibration=classic_chrome@color=1.0|bundle=",
            1,
        );
        let parsed = Permalink::parse(&text).expect("identity calibration");
        assert!(parsed.calibration.is_empty());
        assert_eq!(
            parsed.selection.as_ref().expect("selection").mode,
            SelectionMode::Qualified
        );
    }

    #[test]
    fn a_nonidentity_verification_downgrades_to_experimental_and_replays() {
        let fixture = qualified_request();
        let second_sim = SimKey::new("provia").expect("sim");
        let second_body = synth::body_for_sim(9, &second_sim).expect("synthetic body");
        let mut message = fixture.message.clone();
        message.inputs.sims.push(sim_input(
            second_body.model.base_look.clone(),
            &second_body.model.crm,
        ));
        let target_look = second_body
            .model
            .realize(&synth::plan().default)
            .expect("target realization")
            .look;
        message.inputs.target_look = document(target_look).encode().expect("encode look");
        let observed = synth::observed(
            &fixture.body.model,
            &synth::plan().default,
            &synth::probes(),
            200,
        )
        .expect("observed");
        let blob = looklab_samples::encode(&observed).expect("encode samples");
        let verified = crate::run_verify(&crate::VerifyRequestMessage {
            inputs: round_trip(&message).inputs,
            observed_blobs: vec![blob],
        })
        .expect("verify");
        assert_eq!(verified.first.sim, fixture.sim);
        assert_eq!(verified.first.settings, fixture.settings);

        let next = Permalink::parse(&verified.permalink).expect("permalink");
        let selection = next.selection.as_ref().expect("selection");
        assert_eq!(selection.mode, SelectionMode::Experimental);
        assert_eq!(selection.reports, fixture.selection.reports);
        assert!(!next.calibration.is_empty());

        let mut reloaded = message.inputs.clone();
        reloaded.calibration = next.calibration;
        reloaded.selection.as_mut().expect("selection").mode = SelectionMode::Experimental;
        let replay = crate::run_match(&crate::MatchRequest { inputs: reloaded }).expect("replay");
        let replayed = replay.results.first().expect("a suggestion");
        assert_eq!(replayed.sim, verified.second.sim);
        assert_eq!(replayed.settings, verified.second.settings);
        assert_eq!(replay.permalink, verified.permalink);
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
