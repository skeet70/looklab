// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_color::Xyz;
use looklab_color::ucs::{Ucs, delta_e_ucs};
use looklab_model::file::BaseReference;
use looklab_model::realization::{Realization, Settings};
use looklab_model::{LayerValues, Look, LookInput, ProbeSet};
use looklab_report::compare::LookFunction;
use looklab_report::summary::{HueRegion, summary_line};
use looklab_report::{ComparisonInput, FunctionName, LayerContribution, LayerId, compare};
use looklab_schema::{ControlKey, SimKey};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::metric::{AssessmentStatus, OutputContext, ScoringMetric, TargetAppearance};
use crate::model::MatchModel;
use crate::optimizer::{Suggestion, quantile};
use crate::settings::Plan;
use crate::target::{ImageCount, PreparedTarget, TargetSupport};

/// The numerical tolerance a reachability witness must meet at every declared point.
pub const REACHABILITY_TOLERANCE_DE: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSupport {
    Assessed,
    Extrapolated,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReachabilityStatus {
    Reachable,
    Partial,
    NoRecipeFound,
    Unassessed,
}

impl ReachabilityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reachable => "reachable",
            Self::Partial => "partial",
            Self::NoRecipeFound => "no_recipe_found",
            Self::Unassessed => "unassessed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reachability {
    pub witness: Settings,
    pub tolerance: f64,
    pub declared: usize,
    pub evaluated: usize,
    pub assessed: usize,
    pub within_tolerance: usize,
    pub within_assessed: usize,
    pub settings: SettingsSupport,
    pub domain_assessed: bool,
    pub uncertainty: AssessmentStatus,
    pub status: ReachabilityStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ControlEntry {
    pub key: ControlKey,
    pub value: String,
    pub default_value: String,
    pub changed: bool,
    pub affects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MissedRegion {
    pub region: String,
    pub layer: String,
    pub remaining_delta_e: f64,
    pub probes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
}

impl ConfidenceLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

impl fmt::Display for ConfidenceLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Confidence {
    pub level: ConfidenceLevel,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsCard {
    pub sim: SimKey,
    pub metric: ScoringMetric,
    pub output: Option<OutputContext>,
    pub entries: Vec<ControlEntry>,
    pub exposure_note: Option<String>,
    pub median_delta_e: f64,
    pub p95_delta_e: f64,
    pub sigma_band: Option<f64>,
    pub joint_uncertainty: AssessmentStatus,
    pub residual_summary: String,
    pub reachability: Reachability,
    pub missed: Vec<MissedRegion>,
    pub confidence: Confidence,
    /// Set when the target's published support mask supported nothing and the
    /// sigma and density predicates chose the probes.
    pub experimental: Option<ImageCount>,
    /// Set when the target's fitted coefficients approximate a declared base.
    pub approximated_base: Option<String>,
    /// Set when the destination realization excludes or approximates the target.
    pub destination_scope: Option<String>,
}

impl SettingsCard {
    pub fn render_text(&self) -> String {
        let mut out = format!("base sim: {}\n", self.sim);
        out.push_str(&format!("metric: {}\n", self.metric.identity()));
        match &self.output {
            Some(context) => out.push_str(&format!("output: {}\n", context.canonical())),
            None => out.push_str("output: not rendered\n"),
        }
        if let Some(base) = &self.approximated_base {
            out.push_str(&format!(
                "approximation: the target's fitted coefficients approximate the declared base {base}\n"
            ));
        }
        if let Some(scope) = &self.destination_scope {
            out.push_str(&format!("destination realization: {scope}\n"));
        }
        for entry in &self.entries {
            out.push_str(&format!("  {}: {}", entry.key, entry.value));
            if entry.changed {
                out.push_str(&format!(" (default {})", entry.default_value));
            }
            out.push('\n');
        }
        if let Some(note) = &self.exposure_note {
            out.push_str(&format!("  {note}\n"));
        }
        match self.sigma_band {
            Some(band) => out.push_str(&format!(
                "predicted ΔE: median {:.2}, p95 {:.2}, band ±{:.2}\n",
                self.median_delta_e, self.p95_delta_e, band
            )),
            None => out.push_str(&format!(
                "predicted ΔE: median {:.2}, p95 {:.2}, band unavailable\n",
                self.median_delta_e, self.p95_delta_e
            )),
        }
        out.push_str(&format!("residual: {}\n", self.residual_summary));
        let joint = match self.joint_uncertainty {
            AssessmentStatus::Assessed => "assessed",
            AssessmentStatus::Partial => "partial",
            AssessmentStatus::Unassessed => "unassessed (no target/camera cross-covariance)",
        };
        out.push_str(&format!("joint uncertainty: {joint}\n"));
        out.push_str(&format!(
            "reachability: {} ({} of {} declared points assessed, {} within {:.2} ΔE; {} evaluated, {} within tolerance)\n",
            self.reachability.status.as_str(),
            self.reachability.assessed,
            self.reachability.declared,
            self.reachability.within_assessed,
            self.reachability.tolerance,
            self.reachability.evaluated,
            self.reachability.within_tolerance
        ));
        for region in &self.missed {
            out.push_str(&format!(
                "missed: {} {} exceeds the tolerance at {:.1} ΔE ({} probes)\n",
                region.region, region.layer, region.remaining_delta_e, region.probes
            ));
        }
        out.push_str(&format!(
            "confidence: {} ({})\n",
            self.confidence.level, self.confidence.reason
        ));
        if let Some(images) = self.experimental {
            out.push_str(&format!(
                "support: experimental estimate resting on {} {}; no region has an independent held-out check\n",
                images.get(),
                if images.get() == 1 { "image" } else { "images" }
            ));
        }
        out
    }
}

pub fn settings_card(
    target: &PreparedTarget<'_>,
    plan: &Plan,
    model: &MatchModel,
    suggestion: &Suggestion,
) -> Result<SettingsCard> {
    settings_card_with_support(target, plan, model, suggestion, None)
}

pub fn settings_card_with_support(
    target: &PreparedTarget<'_>,
    plan: &Plan,
    model: &MatchModel,
    suggestion: &Suggestion,
    support: Option<&crate::domain::AssessedDomain>,
) -> Result<SettingsCard> {
    let metric = target.metric();
    let realization = model.realize(&suggestion.settings)?;
    let mut deltas = Vec::with_capacity(target.supported.len());
    let mut traces = Vec::with_capacity(target.supported.len());
    let mut assessed_flags = Vec::with_capacity(target.supported.len());
    for index in &target.supported {
        let probe = &target.probes.probes[*index];
        assessed_flags.push(match support {
            Some(domain) => crate::domain::AssessedDomain::cell_of(probe)
                .is_some_and(|cell| domain.supports(&cell)),
            None => true,
        });
        let prediction = model.scored_ucs(metric, &realization, probe.xyz);
        let expected = target.target[*index];
        let delta = [
            prediction.j - expected.j,
            prediction.a - expected.a,
            prediction.b - expected.b,
        ];
        deltas.push((delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt());
        traces.push(crate::covariance::trace_with(
            model,
            &realization,
            &suggestion.settings,
            metric,
            probe.xyz,
        )?);
    }
    let mut sorted = deltas.clone();
    sorted.sort_by(f64::total_cmp);
    let band =
        if model.covariance.is_none() || traces.is_empty() || traces.iter().any(Option::is_none) {
            None
        } else {
            Some(
                traces
                    .iter()
                    .map(|trace| trace.unwrap_or(0.0).max(0.0).sqrt())
                    .sum::<f64>()
                    / traces.len() as f64,
            )
        };

    let filtered = ProbeSet {
        version: target.probes.version,
        notes: target.probes.notes.clone(),
        probes: target
            .supported
            .iter()
            .map(|index| target.probes.probes[*index].clone())
            .collect(),
    };
    let (residual_summary, attribution) = match target.function.look() {
        Some(look) => {
            let realized = RealizedFunction {
                model,
                realization: &realization,
                metric,
            };
            let target_function = AppearanceFunction {
                look,
                appearance: target.appearance,
            };
            let comparison = compare(&ComparisonInput {
                a: &target_function,
                b: &realized,
                probes: &filtered,
                a_name: &FunctionName::new("target"),
                b_name: &FunctionName::new(model.sim().to_string()),
            });
            let attribution =
                realized_attribution(look, target.appearance, model, &realization, &filtered);
            let summary = summary_line(
                &FunctionName::new("target"),
                &FunctionName::new(model.sim().to_string()),
                &comparison.tone,
                &comparison.tint,
                comparison.grl.as_ref(),
                &comparison.delta_e,
                &attribution,
            );
            (summary, attribution)
        }
        None => (
            format!(
                "realization differs from the target: median ΔE {:.2}, p95 {:.2}.",
                suggestion.median_delta_e, suggestion.p95_delta_e
            ),
            Vec::new(),
        ),
    };
    let layer = attribution
        .first()
        .map(|entry| layer_noun(entry.layer).to_string())
        .unwrap_or_else(|| "color".to_string());

    let domain_assessed =
        support.is_some_and(|domain| domain.combinations().contains(&suggestion.settings));
    let mut regions: Vec<(String, Vec<f64>)> = Vec::new();
    for (slot, index) in target.supported.iter().enumerate() {
        if !domain_assessed || !assessed_flags[slot] {
            continue;
        }
        let delta = deltas[slot];
        if delta <= REACHABILITY_TOLERANCE_DE {
            continue;
        }
        let probe = &target.probes.probes[*index];
        let input = LookInput::new(probe.xyz);
        let region = if input.c_in < 5.0 {
            "Neutral".to_string()
        } else {
            adjective(HueRegion::of_degrees(input.h_in)).to_string()
        };
        match regions.iter_mut().find(|(name, _)| name == &region) {
            Some((_, values)) => values.push(delta),
            None => regions.push((region, vec![delta])),
        }
    }
    let mut missed: Vec<MissedRegion> = regions
        .into_iter()
        .map(|(region, mut values)| {
            values.sort_by(f64::total_cmp);
            MissedRegion {
                region,
                layer: layer.clone(),
                remaining_delta_e: quantile(&values, 0.95),
                probes: values.len(),
            }
        })
        .collect();
    missed.sort_by(|left, right| right.remaining_delta_e.total_cmp(&left.remaining_delta_e));
    missed.truncate(3);

    let declared = target.declared_points;
    let evaluated = target.supported.len();
    let within_tolerance = deltas
        .iter()
        .filter(|delta| **delta <= REACHABILITY_TOLERANCE_DE)
        .count();
    let settings = settings_support(model, plan, &suggestion.settings);
    let assessed = if domain_assessed {
        assessed_flags.iter().filter(|assessed| **assessed).count()
    } else {
        0
    };
    let within_assessed = if domain_assessed {
        deltas
            .iter()
            .zip(&assessed_flags)
            .filter(|(delta, assessed)| **assessed && **delta <= REACHABILITY_TOLERANCE_DE)
            .count()
    } else {
        0
    };
    let status = if assessed == 0 {
        if within_tolerance < evaluated {
            ReachabilityStatus::NoRecipeFound
        } else {
            ReachabilityStatus::Unassessed
        }
    } else if within_assessed < assessed {
        ReachabilityStatus::NoRecipeFound
    } else if assessed < declared
        || settings != SettingsSupport::Assessed
        || target.target_uncertainty != AssessmentStatus::Assessed
    {
        ReachabilityStatus::Partial
    } else {
        ReachabilityStatus::Reachable
    };
    let reachability = Reachability {
        witness: suggestion.settings.clone(),
        tolerance: REACHABILITY_TOLERANCE_DE,
        declared,
        evaluated,
        assessed,
        within_tolerance,
        within_assessed,
        settings,
        domain_assessed,
        uncertainty: target.target_uncertainty,
        status,
    };

    let entries = plan
        .controls
        .iter()
        .enumerate()
        .map(|(index, control)| {
            let value = plan.value_index(index, &suggestion.settings);
            ControlEntry {
                key: control.key.clone(),
                value: plan.label(index, value),
                default_value: plan.label(index, plan.default_index(index)),
                changed: value != plan.default_index(index),
                affects: control.affects.iter().map(ToString::to_string).collect(),
            }
        })
        .collect();

    let exposure_note = exposure_note(plan, &suggestion.settings);
    let confidence = confidence(target, plan, model, &suggestion.settings);
    let output = target.optional_output_context(model, &suggestion.settings)?;
    let experimental = match target.support {
        TargetSupport::Published => None,
        TargetSupport::Experimental(images) => Some(images),
    };
    Ok(SettingsCard {
        sim: model.sim().clone(),
        metric,
        output,
        entries,
        exposure_note,
        median_delta_e: quantile(&sorted, 0.5),
        p95_delta_e: quantile(&sorted, 0.95),
        sigma_band: band,
        joint_uncertainty: AssessmentStatus::Unassessed,
        residual_summary,
        reachability,
        missed,
        confidence,
        experimental,
        approximated_base: None,
        destination_scope: None,
    })
}

/// The plain settings-text label for how the destination realization stands.
pub fn destination_scope(
    mode: Option<crate::permalink::SelectionMode>,
    excluded: &[String],
) -> Option<String> {
    let mut parts = Vec::new();
    match mode {
        Some(crate::permalink::SelectionMode::Qualified) => {}
        Some(crate::permalink::SelectionMode::Experimental) => {
            parts.push("the camera realization is experimental".to_owned());
        }
        None => parts.push("the camera realization is unassessed".to_owned()),
    }
    if !excluded.is_empty() {
        parts.push(format!(
            "the camera bundle excludes {}",
            excluded.join(", ")
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The plain label a settings card uses for a declared base.
pub fn base_label(base: &BaseReference) -> String {
    match base {
        BaseReference::Look { slug, version } => {
            format!("{}@{}", slug.as_str(), version.as_str())
        }
        BaseReference::FamilyRendering {
            maker, rendering, ..
        } => {
            format!("{maker} {rendering}")
        }
        BaseReference::BodyRealization { body, .. } => body.as_str().to_owned(),
    }
}

struct RealizedFunction<'a> {
    model: &'a MatchModel,
    realization: &'a Realization,
    metric: ScoringMetric,
}

struct AppearanceFunction<'a> {
    look: &'a Look,
    appearance: TargetAppearance,
}

impl LookFunction for AppearanceFunction<'_> {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.appearance.render(self.look.eval(xyz))
    }

    fn layer_values(&self, input: &LookInput) -> Option<LayerValues> {
        Some(self.look.layer_values(input))
    }

    fn look(&self) -> Option<&Look> {
        Some(self.look)
    }
}

impl LookFunction for RealizedFunction<'_> {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.model.scored_ucs(self.metric, self.realization, xyz)
    }

    fn layer_values(&self, input: &LookInput) -> Option<LayerValues> {
        let warped = self.model.input_xyz(self.realization, input.xyz);
        Some(self.realization.look.layer_values(&LookInput::new(warped)))
    }

    fn look(&self) -> Option<&Look> {
        Some(&self.realization.look)
    }
}

fn realized_attribution(
    target: &Look,
    appearance: TargetAppearance,
    model: &MatchModel,
    realization: &Realization,
    probes: &ProbeSet,
) -> Vec<LayerContribution> {
    if probes.probes.is_empty() {
        return Vec::new();
    }
    let realized = RealizedFunction {
        model,
        realization,
        metric: appearance.metric(),
    };
    let baseline = probes
        .probes
        .iter()
        .map(|probe| {
            delta_e_ucs(
                appearance.render(target.eval(probe.xyz)),
                realized.predict(probe.xyz),
            )
        })
        .sum::<f64>()
        / probes.probes.len() as f64;
    let mut contributions = Vec::with_capacity(LayerId::ALL.len());
    for layer in LayerId::ALL {
        let hybrid = with_layer(target, &realization.look, layer);
        let mut error = 0.0_f64;
        for probe in &probes.probes {
            let warped = model.input_xyz(realization, probe.xyz);
            error += delta_e_ucs(
                appearance.render(hybrid.eval(warped)),
                realized.predict(probe.xyz),
            );
        }
        contributions.push(LayerContribution {
            layer,
            contribution: (baseline - error / probes.probes.len() as f64).max(0.0),
        });
    }
    let mut input_error = 0.0_f64;
    for probe in &probes.probes {
        let warped = model.input_xyz(realization, probe.xyz);
        input_error += delta_e_ucs(
            appearance.render(target.eval(warped)),
            realized.predict(probe.xyz),
        );
    }
    let input_contribution = (baseline - input_error / probes.probes.len() as f64).max(0.0);
    if input_contribution > 0.0
        && let Some(tone) = contributions
            .iter_mut()
            .find(|entry| entry.layer == LayerId::Tone)
    {
        tone.contribution += input_contribution;
    }
    contributions.sort_by(|left, right| right.contribution.total_cmp(&left.contribution));
    contributions
}

fn with_layer(base: &Look, other: &Look, layer: LayerId) -> Look {
    let mut hybrid = base.clone();
    match layer {
        LayerId::Tone => hybrid.tone = other.tone,
        LayerId::Tint => hybrid.tint = other.tint.clone(),
        LayerId::Chroma => hybrid.color.g = other.color.g.clone(),
        LayerId::Hue => hybrid.color.r = other.color.r.clone(),
        LayerId::Lightness => hybrid.color.lambda = other.color.lambda.clone(),
        LayerId::Residual => hybrid.residual = other.residual.clone(),
    }
    hybrid
}

fn layer_noun(layer: LayerId) -> &'static str {
    match layer {
        LayerId::Tone => "tone",
        LayerId::Tint => "tint",
        LayerId::Chroma => "chroma",
        LayerId::Hue => "hue",
        LayerId::Lightness => "lightness",
        LayerId::Residual => "residual",
    }
}

fn adjective(region: HueRegion) -> &'static str {
    match region {
        HueRegion::Reds => "Red",
        HueRegion::Oranges => "Orange",
        HueRegion::Yellows => "Yellow",
        HueRegion::Greens => "Green",
        HueRegion::Cyans => "Cyan",
        HueRegion::Blues => "Blue",
        HueRegion::Purples => "Purple",
        HueRegion::Magentas => "Magenta",
    }
}

fn exposure_note(plan: &Plan, settings: &looklab_model::realization::Settings) -> Option<String> {
    let mut parts = Vec::new();
    for index in 0..plan.domains.len() {
        if plan.is_default(index, settings) {
            continue;
        }
        let control = &plan.controls[index];
        let affects = &control.affects;
        if affects.contains(&looklab_schema::Affects::Exposure)
            || affects.contains(&looklab_schema::Affects::Wb)
        {
            parts.push(format!(
                "{} {}",
                control.key,
                plan.current_label(index, settings)
            ));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!(
            "this body renders differently at these settings: {}.",
            parts.join(", ")
        ))
    }
}

struct SettingsEvidence {
    covered: usize,
    extrapolated: usize,
    missing: usize,
}

fn settings_evidence(
    plan: &Plan,
    model: &MatchModel,
    settings: &looklab_model::realization::Settings,
) -> SettingsEvidence {
    let mut evidence = SettingsEvidence {
        covered: 0,
        extrapolated: 0,
        missing: 0,
    };
    for control in plan.stage_controls() {
        let domain = &plan.domains[control];
        let Some(support) = model.crm.support.observed(&domain.key) else {
            evidence.missing += 1;
            continue;
        };
        let has_nondefault = support
            .observed
            .iter()
            .any(|value| value != domain.default_value());
        if !has_nondefault {
            evidence.missing += 1;
            continue;
        }
        let value = plan.value(control, settings);
        if model.crm.support.value_inflation(support, &value) > 1.0 {
            evidence.extrapolated += 1;
        } else {
            evidence.covered += 1;
        }
    }
    evidence
}

fn settings_support(
    model: &MatchModel,
    plan: &Plan,
    settings: &looklab_model::realization::Settings,
) -> SettingsSupport {
    let evidence = settings_evidence(plan, model, settings);
    if evidence.missing > 0 {
        SettingsSupport::Missing
    } else if evidence.extrapolated > 0 {
        SettingsSupport::Extrapolated
    } else {
        SettingsSupport::Assessed
    }
}

fn confidence(
    target: &PreparedTarget<'_>,
    plan: &Plan,
    model: &MatchModel,
    settings: &looklab_model::realization::Settings,
) -> Confidence {
    let SettingsEvidence {
        covered,
        extrapolated,
        missing,
    } = settings_evidence(plan, model, settings);
    let planned = plan.plan_controls.len();
    let fraction = target.support_fraction();
    let target_uncertainty = target.target_uncertainty == AssessmentStatus::Assessed;
    let experimental = matches!(target.support, TargetSupport::Experimental(_));
    let level = if experimental {
        ConfidenceLevel::Low
    } else if covered == planned
        && extrapolated == 0
        && fraction >= 0.9
        && model.covariance.is_some()
        && target_uncertainty
    {
        ConfidenceLevel::High
    } else if target_uncertainty && covered + extrapolated >= planned.div_ceil(2) && fraction >= 0.6
    {
        ConfidenceLevel::Medium
    } else {
        ConfidenceLevel::Low
    };
    let support = if experimental {
        "the published support has no independently checked region".to_string()
    } else if target_uncertainty {
        format!("{:.0}% of probes are supported", fraction * 100.0)
    } else {
        "probe support is unassessed".to_string()
    };
    let reason = format!(
        "{covered} of {planned} solve-plan controls have nondefault observed responses; {extrapolated} recommended values extrapolate; {missing} controls lack nondefault support; {support}{}{}{}",
        if model.covariance.is_none() {
            "; no control-response covariance, so the predicted band is unavailable"
        } else {
            ""
        },
        if target_uncertainty {
            ""
        } else {
            "; target probe uncertainty is unassessed for this metric"
        },
        if experimental {
            "; the estimate is not a checked rating"
        } else {
            ""
        }
    );
    Confidence { level, reason }
}
