// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_match::{
    ConfidenceLevel, DeltaStats, MatchOutcome, SettingsCard, Suggestion, UnreachableRegion,
};
use looklab_model::file::{LookVersion, Slug};
use looklab_model::realization::Settings;
use looklab_schema::{BodyId, ControlKey, SimKey};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardEntryDto {
    pub key: ControlKey,
    pub value: String,
    pub default_value: String,
    pub changed: bool,
    pub affects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UnreachableDto {
    pub region: String,
    pub layer: String,
    pub remaining_delta_e: f64,
    pub probes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardDto {
    pub sim: SimKey,
    pub entries: Vec<CardEntryDto>,
    pub exposure_note: Option<String>,
    pub median_delta_e: f64,
    pub p95_delta_e: f64,
    pub sigma_band: Option<f64>,
    pub residual_summary: String,
    pub unreachable: Vec<UnreachableDto>,
    pub confidence: ConfidenceDto,
    pub confidence_reason: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceDto {
    High,
    Medium,
    Low,
}

impl ConfidenceDto {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

impl CardDto {
    pub fn from_card(card: &SettingsCard) -> Self {
        Self {
            sim: card.sim.clone(),
            entries: card
                .entries
                .iter()
                .map(|entry| CardEntryDto {
                    key: entry.key.clone(),
                    value: entry.value.clone(),
                    default_value: entry.default_value.clone(),
                    changed: entry.changed,
                    affects: entry.affects.clone(),
                })
                .collect(),
            exposure_note: card.exposure_note.clone(),
            median_delta_e: card.median_delta_e,
            p95_delta_e: card.p95_delta_e,
            sigma_band: card.sigma_band,
            residual_summary: card.residual_summary.clone(),
            unreachable: card
                .unreachable
                .iter()
                .map(UnreachableDto::from_region)
                .collect(),
            confidence: level_value(card.confidence.level),
            confidence_reason: card.confidence.reason.clone(),
        }
    }
}

impl UnreachableDto {
    fn from_region(region: &UnreachableRegion) -> Self {
        Self {
            region: region.region.clone(),
            layer: region.layer.clone(),
            remaining_delta_e: region.remaining_delta_e,
            probes: region.probes,
        }
    }
}

fn level_value(level: ConfidenceLevel) -> ConfidenceDto {
    match level {
        ConfidenceLevel::High => ConfidenceDto::High,
        ConfidenceLevel::Medium => ConfidenceDto::Medium,
        ConfidenceLevel::Low => ConfidenceDto::Low,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuggestionDto {
    pub sim: SimKey,
    pub settings: Settings,
    pub objective: f64,
    pub median_delta_e: f64,
    pub p95_delta_e: f64,
    pub max_delta_e: f64,
    pub sigma_band: f64,
}

impl SuggestionDto {
    pub fn from_suggestion(suggestion: &Suggestion) -> Self {
        Self {
            sim: suggestion.sim.clone(),
            settings: suggestion.settings.clone(),
            objective: suggestion.objective,
            median_delta_e: suggestion.median_delta_e,
            p95_delta_e: suggestion.p95_delta_e,
            max_delta_e: suggestion.max_delta_e,
            sigma_band: suggestion.sigma_band,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatsDto {
    pub sims_considered: usize,
    pub sims_kept: usize,
    pub supported_probes: usize,
    pub probe_subset: usize,
    pub linear_evaluations: usize,
    pub exact_evaluations: usize,
    pub relinearizations: usize,
}

impl StatsDto {
    pub fn from_outcome(outcome: &MatchOutcome) -> Self {
        Self {
            sims_considered: outcome.stats.sims_considered,
            sims_kept: outcome.stats.sims_kept,
            supported_probes: outcome.stats.supported_probes,
            probe_subset: outcome.stats.probe_subset,
            linear_evaluations: outcome.stats.linear_evaluations,
            exact_evaluations: outcome.stats.exact_evaluations,
            relinearizations: outcome.stats.relinearizations,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeltaStatsDto {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl DeltaStatsDto {
    pub fn from_stats(stats: &DeltaStats) -> Self {
        Self {
            count: stats.count,
            median: stats.median,
            p95: stats.p95,
            max: stats.max,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MatchReply {
    pub target_slug: Slug,
    pub target_version: LookVersion,
    pub base_slug: Slug,
    pub base_version: LookVersion,
    pub body: BodyId,
    pub epoch: u32,
    pub epoch_map_version: u32,
    pub crm_version: u32,
    pub permalink: String,
    pub card: CardDto,
    pub results: Vec<SuggestionDto>,
    pub stats: StatsDto,
    pub realization_lut: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyReply {
    pub permalink: String,
    pub first: SuggestionDto,
    pub second: SuggestionDto,
    pub first_measured: DeltaStatsDto,
    pub second_measured: Option<DeltaStatsDto>,
    pub reduction: Option<f64>,
    pub updated_crm_version: u32,
    pub card: CardDto,
}
