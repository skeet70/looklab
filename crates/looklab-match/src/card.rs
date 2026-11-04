// SPDX-License-Identifier: AGPL-3.0-or-later

use core::fmt;

use looklab_model::{LookInput, ProbeSet};
use looklab_report::summary::HueRegion;
use looklab_report::{ComparisonInput, FunctionName, LayerId, compare};
use looklab_schema::{ControlKey, SimKey};

use crate::error::Result;
use crate::model::MatchModel;
use crate::optimizer::{Suggestion, quantile};
use crate::settings::Plan;
use crate::target::PreparedTarget;

/// Probes above this ΔE are reported as unreachable regions.
pub const UNREACHABLE_DE: f64 = 2.0;

#[derive(Clone, Debug, PartialEq)]
pub struct ControlEntry {
    pub key: ControlKey,
    pub value: String,
    pub default_value: String,
    pub changed: bool,
    pub affects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UnreachableRegion {
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
    pub entries: Vec<ControlEntry>,
    pub exposure_note: Option<String>,
    pub median_delta_e: f64,
    pub p95_delta_e: f64,
    pub sigma_band: Option<f64>,
    pub residual_summary: String,
    pub unreachable: Vec<UnreachableRegion>,
    pub confidence: Confidence,
}

impl SettingsCard {
    pub fn render_text(&self) -> String {
        let mut out = format!("base sim: {}\n", self.sim);
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
        for region in &self.unreachable {
            out.push_str(&format!(
                "unreachable: {} {} can't be reduced enough; remaining error {:.1} ΔE ({} probes)\n",
                region.region, region.layer, region.remaining_delta_e, region.probes
            ));
        }
        out.push_str(&format!(
            "confidence: {} ({})\n",
            self.confidence.level, self.confidence.reason
        ));
        out
    }
}

pub fn settings_card(
    target: &PreparedTarget<'_>,
    plan: &Plan,
    model: &MatchModel,
    suggestion: &Suggestion,
) -> Result<SettingsCard> {
    let realization = model.realize(&suggestion.settings)?;
    let mut deltas = Vec::with_capacity(target.supported.len());
    let mut traces = Vec::with_capacity(target.supported.len());
    for index in &target.supported {
        let probe = &target.probes.probes[*index];
        let prediction = model.input_ucs(&realization, probe.xyz, [0.0, 0.0], 0.0);
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
            probe.xyz,
        )?);
    }
    let mut sorted = deltas.clone();
    sorted.sort_by(f64::total_cmp);
    let band = if model.covariance.is_none() || traces.is_empty() {
        None
    } else {
        Some(
            traces
                .iter()
                .map(|trace| trace.max(0.0).sqrt())
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
            let comparison = compare(&ComparisonInput {
                a: look,
                b: &realization.look,
                probes: &filtered,
                a_name: &FunctionName::new("target"),
                b_name: &FunctionName::new(model.sim().to_string()),
            });
            (comparison.summary, comparison.attribution)
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

    let mut regions: Vec<(String, Vec<f64>)> = Vec::new();
    for (slot, index) in target.supported.iter().enumerate() {
        let delta = deltas[slot];
        if delta <= UNREACHABLE_DE {
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
    let mut unreachable: Vec<UnreachableRegion> = regions
        .into_iter()
        .map(|(region, mut values)| {
            values.sort_by(f64::total_cmp);
            UnreachableRegion {
                region,
                layer: layer.clone(),
                remaining_delta_e: quantile(&values, 0.95),
                probes: values.len(),
            }
        })
        .collect();
    unreachable.sort_by(|left, right| right.remaining_delta_e.total_cmp(&left.remaining_delta_e));
    unreachable.truncate(3);

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
    let confidence = confidence(target, plan, model);
    Ok(SettingsCard {
        sim: model.sim().clone(),
        entries,
        exposure_note,
        median_delta_e: quantile(&sorted, 0.5),
        p95_delta_e: quantile(&sorted, 0.95),
        sigma_band: band,
        residual_summary,
        unreachable,
        confidence,
    })
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

fn confidence(target: &PreparedTarget<'_>, plan: &Plan, model: &MatchModel) -> Confidence {
    let observed = plan
        .stage_controls()
        .filter(|index| {
            model
                .crm
                .support
                .observed(&plan.domains[*index].key)
                .is_some()
        })
        .count();
    let planned = plan.plan_controls.len();
    let fraction = target.support_fraction();
    let mut level = if observed == planned && fraction >= 0.9 {
        ConfidenceLevel::High
    } else if observed * 2 >= planned && fraction >= 0.6 {
        ConfidenceLevel::Medium
    } else {
        ConfidenceLevel::Low
    };
    if model.covariance.is_none() && level == ConfidenceLevel::High {
        level = ConfidenceLevel::Medium;
    }
    let reason = format!(
        "{observed} of {planned} solve-plan controls have observed settings; {:.0}% of probes are supported{}",
        fraction * 100.0,
        if model.covariance.is_none() {
            "; no control-response covariance, so the predicted band is unavailable"
        } else {
            ""
        }
    );
    Confidence { level, reason }
}
