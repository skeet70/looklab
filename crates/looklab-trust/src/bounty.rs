// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recipe scores, persistent-residual analysis and data bounties ([docs/05 §12]).
//!
//! Recipe data never enters a Look's θ_L. This module compares recipe outcomes
//! with the published prediction and turns the gaps into data requests.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use looklab_model::OutputContext;
use looklab_schema::{
    AssessmentStatus, BodyStateVersion, CellId, ModelReportId, RealizationRevisionId,
    ScoringMetric, SelectionScope,
};
use looklab_upload::{BodyId, ControlSettings, LookSlug, SimKey};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{EffectiveContributorId, VersionId};
use crate::params::TrustParams;
use crate::region::{Region, SceneCell};
use crate::standing::Tier;

/// A body epoch, as the ordinal in its control schema ([docs/03 §1]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EpochOrdinal(u32);

impl EpochOrdinal {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The identity of one recipe: body, epoch, base sim, settings and target Look.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecipeKey {
    pub body: BodyId,
    pub epoch: EpochOrdinal,
    pub base_sim: SimKey,
    pub settings: ControlSettings,
    pub target_look: LookSlug,
}

/// Model versions that produced a prediction ([docs/03 §1]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelVersions {
    pub input_transform: u32,
    pub control_response: u32,
    pub look: VersionId,
}

/// The immutable realization and body state a prediction was computed from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecipeSource {
    pub realization: RealizationRevisionId,
    pub body_state: BodyStateVersion,
    pub report: Option<ModelReportId>,
    pub scope: SelectionScope,
}

pub const fn scoring_metric_name(metric: ScoringMetric) -> &'static str {
    metric.identity()
}

pub fn parse_scoring_metric(value: &str) -> Option<ScoringMetric> {
    ScoringMetric::parse(value).ok()
}

fn default_metric() -> ScoringMetric {
    ScoringMetric::LegacyPreClipV1
}

/// A prediction from the match/realization path, before any upload was measured.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipePrediction {
    #[serde(default = "default_metric")]
    pub metric: ScoringMetric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub delta_e: f64,
    pub sigma: Option<f64>,
    pub versions: ModelVersions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RecipeSource>,
}

/// One verified recipe upload. Honest duplicates are merged by effective
/// contributor before scoring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifiedRecipe {
    pub contributor: EffectiveContributorId,
    pub measured_de: f64,
    /// Descriptive `measured - predicted` ΔE. Never used for significance.
    pub raw_gap: f64,
    /// Mean signed `o - m` in common UCS, when the producer retained it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_residual: Option<[f64; 3]>,
    pub sigma: f64,
    pub weight: f64,
}

/// Predicted and measured ΔE for one recipe ([docs/03 §1]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeScore {
    #[serde(default = "default_metric")]
    pub metric: ScoringMetric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub recipe: RecipeKey,
    pub predicted_de: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub predicted_sigma: Option<f64>,
    pub measured_de: Option<f64>,
    pub measured_sigma: Option<f64>,
    /// Descriptive `measured - predicted` gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_gap: Option<f64>,
    /// Mean signed `o - m` in common UCS, when the producer retained it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_residual: Option<[f64; 3]>,
    /// Distinct effective contributors. Repeated uploads by one credential are one
    /// verification.
    pub n_verifications: usize,
    pub model_versions: ModelVersions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RecipeSource>,
}

/// Scores for one Look × body × metric × output context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeLeaderboard {
    pub look: LookSlug,
    pub body: BodyId,
    #[serde(default = "default_metric")]
    pub metric: ScoringMetric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub entries: Vec<RecipeScore>,
}

fn finite_non_negative(field: &'static str, value: f64) -> Result<f64> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(TrustError::InvalidRecipeValue(field))
    }
}

fn finite_positive(field: &'static str, value: f64) -> Result<f64> {
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Err(TrustError::InvalidRecipeValue(field))
    }
}

fn finite_vector(value: [f64; 3]) -> bool {
    value.iter().all(|component| component.is_finite())
}

fn add(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

fn sub(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

fn scale(value: [f64; 3], factor: f64) -> [f64; 3] {
    [value[0] * factor, value[1] * factor, value[2] * factor]
}

fn dot(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn norm(value: [f64; 3]) -> f64 {
    dot(value, value).sqrt()
}

/// Trust-weighted mean over effective contributors, with a weighted standard error.
pub fn score_recipe(
    recipe: RecipeKey,
    prediction: RecipePrediction,
    verifications: &[VerifiedRecipe],
) -> Result<RecipeScore> {
    let predicted_de = finite_non_negative("predicted_de", prediction.delta_e)?;
    let predicted_sigma = prediction
        .sigma
        .map(|value| finite_non_negative("predicted_sigma", value))
        .transpose()?;
    let metric = prediction.metric;
    let output = prediction.output;
    let source = prediction.source;

    struct Contributor {
        weight: f64,
        weighted_measured: f64,
        weighted_gap: f64,
        variance: f64,
        signed: Option<[f64; 3]>,
    }
    let mut contributors: BTreeMap<EffectiveContributorId, Contributor> = BTreeMap::new();
    for verification in verifications {
        let measured = finite_non_negative("measured_de", verification.measured_de)?;
        if !verification.raw_gap.is_finite() {
            return Err(TrustError::InvalidRecipeValue("raw_gap"));
        }
        let raw_gap = verification.raw_gap;
        let sigma = finite_positive("sigma", verification.sigma)?;
        let weight = finite_positive("weight", verification.weight)?;
        if let Some(signed) = verification.signed_residual
            && !finite_vector(signed)
        {
            return Err(TrustError::InvalidRecipeValue("signed_residual"));
        }
        let entry = contributors
            .entry(verification.contributor)
            .or_insert(Contributor {
                weight: 0.0,
                weighted_measured: 0.0,
                weighted_gap: 0.0,
                variance: 0.0,
                signed: Some([0.0; 3]),
            });
        entry.weight += weight;
        entry.weighted_measured += weight * measured;
        entry.weighted_gap += weight * raw_gap;
        entry.variance += weight * weight * sigma * sigma;
        entry.signed = match (entry.signed, verification.signed_residual) {
            (Some(total), Some(value)) => Some(add(total, scale(value, weight))),
            _ => None,
        };
    }

    if contributors.is_empty() {
        return Ok(RecipeScore {
            metric,
            output,
            recipe,
            predicted_de,
            predicted_sigma,
            measured_de: None,
            measured_sigma: None,
            raw_gap: None,
            signed_residual: None,
            n_verifications: 0,
            model_versions: prediction.versions,
            source,
        });
    }

    let mut total_weight = 0.0;
    let mut weighted = 0.0;
    let mut weighted_gap = 0.0;
    let mut signed_total = [0.0; 3];
    let mut all_signed = true;
    for contributor in contributors.values() {
        total_weight += contributor.weight;
        weighted += contributor.weighted_measured;
        weighted_gap += contributor.weighted_gap;
        match contributor.signed {
            Some(value) => signed_total = add(signed_total, value),
            None => all_signed = false,
        }
    }
    let measured_de = weighted / total_weight;
    let raw_gap = weighted_gap / total_weight;
    let signed_residual = all_signed.then(|| scale(signed_total, 1.0 / total_weight));
    let mut variance = 0.0;
    for contributor in contributors.values() {
        let share = contributor.weight / total_weight;
        let sigma = contributor.variance.sqrt() / contributor.weight;
        variance += share * share * sigma * sigma;
    }
    let measured_sigma = variance.sqrt();
    Ok(RecipeScore {
        metric,
        output,
        recipe,
        predicted_de,
        predicted_sigma,
        measured_de: Some(measured_de),
        measured_sigma: Some(measured_sigma),
        raw_gap: Some(raw_gap),
        signed_residual,
        n_verifications: contributors.len(),
        model_versions: prediction.versions,
        source,
    })
}

fn settings_cmp(left: &ControlSettings, right: &ControlSettings) -> Ordering {
    for (a, b) in left.entries().iter().zip(right.entries()) {
        let order = a.key.cmp(&b.key).then_with(|| a.step.cmp(&b.step));
        if order != Ordering::Equal {
            return order;
        }
    }
    left.entries().len().cmp(&right.entries().len())
}

fn score_order(left: &RecipeScore, right: &RecipeScore) -> Ordering {
    let measured = left
        .measured_de
        .unwrap_or(f64::INFINITY)
        .total_cmp(&right.measured_de.unwrap_or(f64::INFINITY));
    measured
        .then_with(|| left.predicted_de.total_cmp(&right.predicted_de))
        .then_with(|| {
            left.measured_sigma
                .unwrap_or(f64::INFINITY)
                .total_cmp(&right.measured_sigma.unwrap_or(f64::INFINITY))
        })
        .then_with(|| {
            left.predicted_sigma
                .unwrap_or(f64::INFINITY)
                .total_cmp(&right.predicted_sigma.unwrap_or(f64::INFINITY))
        })
        .then_with(|| left.recipe.epoch.cmp(&right.recipe.epoch))
        .then_with(|| left.recipe.base_sim.cmp(&right.recipe.base_sim))
        .then_with(|| settings_cmp(&left.recipe.settings, &right.recipe.settings))
}

/// Groups scores by Look × body × metric × output context and orders each
/// group by measured ΔE, then predicted ΔE and uncertainty ([docs/08 §1]).
pub fn leaderboards(scores: impl IntoIterator<Item = RecipeScore>) -> Vec<RecipeLeaderboard> {
    let mut boards: Vec<RecipeLeaderboard> = Vec::new();
    for score in scores {
        let position = boards.iter().position(|board| {
            board.look == score.recipe.target_look
                && board.body == score.recipe.body
                && board.metric == score.metric
                && board.output == score.output
        });
        match position {
            Some(index) => boards[index].entries.push(score),
            None => boards.push(RecipeLeaderboard {
                look: score.recipe.target_look.clone(),
                body: score.recipe.body.clone(),
                metric: score.metric,
                output: score.output,
                entries: vec![score],
            }),
        }
    }
    for board in &mut boards {
        board.entries.sort_by(score_order);
    }
    boards
}

/// Whether the control model claims a region reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reachability {
    Reachable,
    Unreachable,
    Unassessed,
}

/// A component whose covariance-scaled response explains the residual.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    InputTransform,
    ControlModel,
    Look,
    Mixed,
    Unassessed,
}

/// A feasible forward perturbation and its induced post-clip response.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttributionResponse {
    /// Induced response in the common post-clip residual frame.
    pub response: [f64; 3],
    pub amplitude: f64,
    /// Joint covariance of the induced response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covariance: Option<[[f64; 3]; 3]>,
    pub feasible: bool,
}

/// Signed conditional-mean camera residual and the three non-overlapping
/// attribution groups.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttributionEvidence {
    /// Signed conditional mean `mu_o - m`.
    pub residual: [f64; 3],
    /// Joint covariance used for whitening and compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub residual_covariance: Option<[[f64; 3]; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_transform: Option<AttributionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_model: Option<AttributionResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub look: Option<AttributionResponse>,
}

/// Joint covariance of `(target, prediction, observed conditional mean)` in
/// common UCS, row-major over the concatenated nine-vector.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct JointResidualCovariance {
    pub values: [[f64; 9]; 9],
}

impl JointResidualCovariance {
    fn finite(&self) -> bool {
        self.values.iter().flatten().all(|value| value.is_finite())
    }
}

/// A producer-declared calibrated colour condition.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ColourCondition(CellId);

impl ColourCondition {
    pub fn new(cell: CellId) -> Self {
        Self(cell)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn cell(&self) -> &CellId {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct ConditionWeight(f64);

impl ConditionWeight {
    pub fn new(value: f64) -> Result<Self> {
        if value.is_finite() && value > 0.0 {
            Ok(Self(value))
        } else {
            Err(TrustError::InvalidRecipeValue("condition_weight"))
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for ConditionWeight {
    type Error = TrustError;

    fn try_from(value: f64) -> Result<Self> {
        Self::new(value)
    }
}

impl From<ConditionWeight> for f64 {
    fn from(value: ConditionWeight) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct RegionalSigma(f64);

impl RegionalSigma {
    pub fn new(value: f64) -> Result<Self> {
        if value.is_finite() && value > 0.0 {
            Ok(Self(value))
        } else {
            Err(TrustError::InvalidRecipeValue("aggregate_sigma"))
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for RegionalSigma {
    type Error = TrustError;

    fn try_from(value: f64) -> Result<Self> {
        Self::new(value)
    }
}

impl From<RegionalSigma> for f64 {
    fn from(value: RegionalSigma) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeclaredCondition {
    pub colour: ColourCondition,
    pub weight: ConditionWeight,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionConditionScope {
    pub cell: SceneCell,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub conditions: Vec<DeclaredCondition>,
    pub contributors: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate_sigma: Option<RegionalSigma>,
}

impl RegionConditionScope {
    fn validate(&self) -> Result<()> {
        if self.contributors == 0 {
            return Err(TrustError::InvalidObservation("contributors"));
        }
        if self.conditions.is_empty() {
            return Err(TrustError::InvalidObservation("conditions"));
        }
        let mut colours = BTreeSet::new();
        let mut total = 0.0;
        for condition in &self.conditions {
            if !colours.insert(condition.colour.clone()) {
                return Err(TrustError::InvalidObservation("duplicate_colour"));
            }
            total += condition.weight.get();
        }
        if !(total.is_finite() && total > 0.0) {
            return Err(TrustError::InvalidRecipeValue("condition_weight_total"));
        }
        Ok(())
    }
}

/// One verified recipe outcome in one declared colour/settings cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualObservation {
    pub contributor: EffectiveContributorId,
    pub cell: SceneCell,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<ColourCondition>,
    pub settings: ControlSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    /// Descriptive `measured - predicted` ΔE. Never used for significance.
    pub raw_gap: f64,
    /// Mean signed `o - m` over the cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_residual: Option<[f64; 3]>,
    /// Mean `m - t` over the cell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prediction_residual: Option<[f64; 3]>,
    /// Distinct observation scatter, not a standard error of the mean.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_sigma: Option<f64>,
    /// Joint `(target, prediction, conditional mean)` covariance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joint_covariance: Option<JointResidualCovariance>,
    pub assessment: AssessmentStatus,
    pub reachable: Reachability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<AttributionEvidence>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionResidual {
    pub cell: SceneCell,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<ColourCondition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub contributors: usize,
    pub settings_cells: usize,
    pub raw_gap: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_sigma: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systematic_excess: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systematic_sigma: Option<f64>,
    pub assessment: AssessmentStatus,
    pub reachable: Reachability,
    pub persistent: bool,
    pub attribution: Attribution,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alignment: Option<[f64; 3]>,
}

const ATTRIBUTION_COMPATIBILITY: f64 = 2.0;

type Mat3 = [[f64; 3]; 3];

fn scale_matrix(matrix: Mat3, factor: f64) -> Mat3 {
    let mut out = [[0.0; 3]; 3];
    for (row, values) in out.iter_mut().enumerate() {
        for (column, value) in values.iter_mut().enumerate() {
            *value = matrix[row][column] * factor;
        }
    }
    out
}

fn finite_matrix(matrix: Mat3) -> bool {
    matrix.iter().flatten().all(|value| value.is_finite())
}

/// Returns `L^{-1}` for the Cholesky factor `L Lᵀ = matrix`.
fn cholesky_inverse(matrix: Mat3) -> Option<Mat3> {
    let mut lower = [[0.0; 3]; 3];
    for row in 0..3 {
        for column in 0..=row {
            let mut sum = matrix[row][column];
            for (row_term, column_term) in lower[row][..column].iter().zip(&lower[column][..column])
            {
                sum -= row_term * column_term;
            }
            if row == column {
                if !(sum.is_finite() && sum > 0.0) {
                    return None;
                }
                lower[row][column] = sum.sqrt();
            } else {
                if lower[column][column] <= 0.0 {
                    return None;
                }
                lower[row][column] = sum / lower[column][column];
            }
        }
    }
    let mut inverse = [[0.0; 3]; 3];
    for row in 0..3 {
        inverse[row][row] = 1.0 / lower[row][row];
        let (computed, current) = inverse.split_at_mut(row);
        for (column, slot) in current[0].iter_mut().enumerate().take(row) {
            let mut sum = 0.0;
            for index in column..row {
                sum += lower[row][index] * computed[index][column];
            }
            *slot = -sum / lower[row][row];
        }
    }
    Some(inverse)
}

fn whiten(inverse: &Mat3, value: [f64; 3]) -> [f64; 3] {
    [
        inverse[0][0] * value[0],
        inverse[1][0] * value[0] + inverse[1][1] * value[1],
        inverse[2][0] * value[0] + inverse[2][1] * value[1] + inverse[2][2] * value[2],
    ]
}

fn systematic_sigma(
    prediction_residual: [f64; 3],
    signed_residual: [f64; 3],
    covariance: &JointResidualCovariance,
) -> Option<f64> {
    if !finite_vector(prediction_residual)
        || !finite_vector(signed_residual)
        || !covariance.finite()
    {
        return None;
    }
    let observed = add(prediction_residual, signed_residual);
    let predicted_norm = norm(prediction_residual);
    let observed_norm = norm(observed);
    if predicted_norm <= 0.0 || observed_norm <= 0.0 {
        return None;
    }
    let unit_m = scale(prediction_residual, 1.0 / predicted_norm);
    let unit_o = scale(observed, 1.0 / observed_norm);
    let difference = sub(unit_m, unit_o);
    let mut jacobian = [0.0; 9];
    for index in 0..3 {
        jacobian[index] = difference[index];
        jacobian[3 + index] = -unit_m[index];
        jacobian[6 + index] = unit_o[index];
    }
    let mut variance = 0.0;
    for left in 0..9 {
        for right in 0..9 {
            variance += jacobian[left] * covariance.values[left][right] * jacobian[right];
        }
    }
    (variance.is_finite() && variance >= 0.0).then(|| variance.sqrt())
}

#[derive(Clone, Copy, Default)]
struct GroupAccum {
    response: [f64; 3],
    weight: f64,
    covariance: Mat3,
    covariance_count: usize,
    missing: bool,
}

fn accumulate_group(
    slot: &mut Option<GroupAccum>,
    response: Option<AttributionResponse>,
    weight: f64,
) {
    match response {
        Some(response)
            if response.feasible
                && response.amplitude.is_finite()
                && response.amplitude > 0.0
                && finite_vector(response.response) =>
        {
            let entry = slot.get_or_insert_with(GroupAccum::default);
            entry.response = add(
                entry.response,
                scale(response.response, weight * response.amplitude),
            );
            entry.weight += weight;
            match response.covariance {
                Some(covariance) if finite_matrix(covariance) => {
                    for (row, values) in covariance.iter().enumerate() {
                        for (column, value) in values.iter().enumerate() {
                            entry.covariance[row][column] += weight * weight * value;
                        }
                    }
                    entry.covariance_count += 1;
                }
                _ => entry.missing = true,
            }
        }
        Some(_) => {
            slot.get_or_insert_with(GroupAccum::default).missing = true;
        }
        None => {}
    }
}

fn attribution_from_evidence(
    evidence: &[(&AttributionEvidence, AssessmentStatus)],
) -> (Option<[f64; 3]>, Attribution) {
    if evidence.len() != 1 {
        return (None, Attribution::Unassessed);
    }
    let mut weight_total = 0.0;
    let mut residual = [0.0; 3];
    let mut residual_covariance = Some([[0.0; 3]; 3]);
    let mut residual_missing = false;
    let mut groups: [Option<GroupAccum>; 3] = [None, None, None];
    for (evidence, assessment) in evidence {
        if *assessment != AssessmentStatus::Assessed {
            return (None, Attribution::Unassessed);
        }
        if !finite_vector(evidence.residual) {
            return (None, Attribution::Unassessed);
        }
        let weight = 1.0;
        residual = add(residual, scale(evidence.residual, weight));
        weight_total += weight;
        match evidence.residual_covariance {
            Some(covariance) if finite_matrix(covariance) => {
                if let Some(total) = residual_covariance.as_mut() {
                    for (row, values) in covariance.iter().enumerate() {
                        for (column, value) in values.iter().enumerate() {
                            total[row][column] += weight * weight * value;
                        }
                    }
                }
            }
            _ => {
                residual_missing = true;
                residual_covariance = None;
            }
        }
        accumulate_group(&mut groups[0], evidence.input_transform, weight);
        accumulate_group(&mut groups[1], evidence.control_model, weight);
        accumulate_group(&mut groups[2], evidence.look, weight);
    }
    if residual_missing || weight_total <= 0.0 {
        return (None, Attribution::Unassessed);
    }
    let Some(residual_covariance) = residual_covariance else {
        return (None, Attribution::Unassessed);
    };
    let residual_covariance =
        scale_matrix(residual_covariance, 1.0 / (weight_total * weight_total));
    let Some(whitening) = cholesky_inverse(residual_covariance) else {
        return (None, Attribution::Unassessed);
    };
    let residual = scale(residual, 1.0 / weight_total);
    let whitened_residual = whiten(&whitening, residual);
    if norm(whitened_residual) <= 0.0 {
        return (None, Attribution::Unassessed);
    }

    let mut alignment = [0.0; 3];
    let mut compatible = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let Some(group) = group else {
            return (None, Attribution::Unassessed);
        };
        if group.missing || group.weight <= 0.0 || group.covariance_count == 0 {
            return (None, Attribution::Unassessed);
        }
        let covariance = scale_matrix(group.covariance, 1.0 / (group.weight * group.weight));
        if cholesky_inverse(covariance).is_none() {
            return (None, Attribution::Unassessed);
        }
        let response = scale(group.response, 1.0 / group.weight);
        let direction = whiten(&whitening, response);
        let squared = dot(direction, direction);
        if squared <= 0.0 {
            return (None, Attribution::Unassessed);
        }
        let factor = dot(whitened_residual, direction) / squared;
        if factor <= 0.0 || factor > 1.0 + 1e-9 {
            continue;
        }
        let unexplained = sub(whitened_residual, scale(direction, factor));
        if norm(unexplained) <= ATTRIBUTION_COMPATIBILITY {
            compatible.push(index);
            alignment[index] = factor;
        }
    }
    let label = match compatible.as_slice() {
        [] => Attribution::Unassessed,
        [0] => Attribution::InputTransform,
        [1] => Attribution::ControlModel,
        [2] => Attribution::Look,
        _ => Attribution::Mixed,
    };
    (Some(alignment), label)
}

fn attribution(observations: &[&ResidualObservation]) -> (Option<[f64; 3]>, Attribution) {
    let mut evidence = Vec::with_capacity(observations.len());
    for observation in observations {
        let Some(value) = observation.attribution.as_ref() else {
            return (None, Attribution::Unassessed);
        };
        evidence.push((value, observation.assessment));
    }
    attribution_from_evidence(&evidence)
}

struct Pooled {
    raw_gap: f64,
    observation_sigma: Option<f64>,
    signed: Option<[f64; 3]>,
    prediction: Option<[f64; 3]>,
    assessment: AssessmentStatus,
}

fn pool_observations(observations: &[&ResidualObservation]) -> Pooled {
    let count = observations.len() as f64;
    let raw_gap = observations
        .iter()
        .map(|observation| observation.raw_gap)
        .sum::<f64>()
        / count;
    let mut signed = Some([0.0; 3]);
    let mut prediction = Some([0.0; 3]);
    let mut observation_sigma = Some(0.0);
    let mut all_assessed = true;
    let mut any_assessed = false;
    for observation in observations {
        match observation.signed_residual {
            Some(value) if finite_vector(value) => signed = signed.map(|sum| add(sum, value)),
            _ => signed = None,
        }
        match observation.prediction_residual {
            Some(value) if finite_vector(value) => {
                prediction = prediction.map(|sum| add(sum, value));
            }
            _ => prediction = None,
        }
        match observation.observation_sigma {
            Some(value) if value.is_finite() && value > 0.0 => {
                observation_sigma = observation_sigma.map(|sum| sum + value);
            }
            _ => observation_sigma = None,
        }
        match observation.assessment {
            AssessmentStatus::Assessed => any_assessed = true,
            AssessmentStatus::Partial => {
                all_assessed = false;
                any_assessed = true;
            }
            AssessmentStatus::Unassessed => all_assessed = false,
        }
    }
    Pooled {
        raw_gap,
        observation_sigma: observation_sigma.map(|value| value / count),
        signed: signed.map(|value| scale(value, 1.0 / count)),
        prediction: prediction.map(|value| scale(value, 1.0 / count)),
        assessment: if all_assessed {
            AssessmentStatus::Assessed
        } else if any_assessed {
            AssessmentStatus::Partial
        } else {
            AssessmentStatus::Unassessed
        },
    }
}

fn pool_pooled(pools: &[Pooled]) -> Pooled {
    let count = pools.len() as f64;
    let raw_gap = pools.iter().map(|pool| pool.raw_gap).sum::<f64>() / count;
    let mut signed = Some([0.0; 3]);
    let mut prediction = Some([0.0; 3]);
    let mut observation_sigma = Some(0.0);
    let mut all_assessed = true;
    let mut any_assessed = false;
    for pool in pools {
        match pool.signed {
            Some(value) => signed = signed.map(|sum| add(sum, value)),
            None => signed = None,
        }
        match pool.prediction {
            Some(value) => prediction = prediction.map(|sum| add(sum, value)),
            None => prediction = None,
        }
        match pool.observation_sigma {
            Some(value) => observation_sigma = observation_sigma.map(|sum| sum + value),
            None => observation_sigma = None,
        }
        match pool.assessment {
            AssessmentStatus::Assessed => any_assessed = true,
            AssessmentStatus::Partial => {
                all_assessed = false;
                any_assessed = true;
            }
            AssessmentStatus::Unassessed => all_assessed = false,
        }
    }
    Pooled {
        raw_gap,
        observation_sigma: observation_sigma.map(|value| value / count),
        signed: signed.map(|value| scale(value, 1.0 / count)),
        prediction: prediction.map(|value| scale(value, 1.0 / count)),
        assessment: if all_assessed {
            AssessmentStatus::Assessed
        } else if any_assessed {
            AssessmentStatus::Partial
        } else {
            AssessmentStatus::Unassessed
        },
    }
}

fn settings_key(settings: &ControlSettings) -> Vec<(String, i64)> {
    settings
        .entries()
        .iter()
        .map(|entry| (entry.key.as_str().to_owned(), entry.step.get()))
        .collect()
}

fn aggregate_region_residuals(
    params: &TrustParams,
    residuals: Vec<RegionResidual>,
    scopes: &[RegionConditionScope],
) -> Result<Vec<RegionResidual>> {
    let mut scope_keys = BTreeSet::new();
    for scope in scopes {
        scope.validate()?;
        let key = (scope.cell, scope.output.map(|output| output.canonical()));
        if !scope_keys.insert(key) {
            return Err(TrustError::InvalidObservation("duplicate_scope"));
        }
    }
    let mut grouped: BTreeMap<(SceneCell, Option<String>), Vec<RegionResidual>> = BTreeMap::new();
    for residual in residuals {
        grouped
            .entry((
                residual.cell,
                residual.output.map(|output| output.canonical()),
            ))
            .or_default()
            .push(residual);
    }
    let mut out = Vec::with_capacity(grouped.len());
    for ((cell, output_key), group) in grouped {
        let scope = scopes.iter().find(|scope| {
            scope.cell == cell && scope.output.map(|output| output.canonical()) == output_key
        });
        let Some(scope) = scope else {
            let count = group.len() as f64;
            let reachable = group
                .iter()
                .fold(Reachability::Reachable, |state, residual| {
                    match (state, residual.reachable) {
                        (_, Reachability::Unreachable) => Reachability::Unreachable,
                        (Reachability::Reachable, Reachability::Unassessed) => {
                            Reachability::Unassessed
                        }
                        (current, _) => current,
                    }
                });
            out.push(RegionResidual {
                cell,
                colour: None,
                output: group[0].output,
                contributors: group
                    .iter()
                    .map(|residual| residual.contributors)
                    .max()
                    .unwrap_or(0),
                settings_cells: group.iter().map(|residual| residual.settings_cells).sum(),
                raw_gap: group.iter().map(|residual| residual.raw_gap).sum::<f64>() / count,
                observation_sigma: group
                    .iter()
                    .all(|residual| residual.observation_sigma.is_some())
                    .then(|| {
                        group
                            .iter()
                            .filter_map(|residual| residual.observation_sigma)
                            .sum::<f64>()
                            / count
                    }),
                systematic_excess: None,
                systematic_sigma: None,
                assessment: AssessmentStatus::Unassessed,
                reachable,
                persistent: false,
                attribution: Attribution::Unassessed,
                alignment: None,
            });
            continue;
        };

        let total_weight: f64 = scope
            .conditions
            .iter()
            .map(|condition| condition.weight.get())
            .sum();
        let mut raw_total = 0.0;
        let mut excess_total = 0.0;
        let mut observation_total = 0.0;
        let mut all_present = !scope.conditions.is_empty();
        let mut all_excess = true;
        let mut all_assessed = true;
        let mut observation_complete = true;
        let mut reachable = Reachability::Reachable;
        let mut settings_cells = 0;
        let mut single = None;
        for condition in &scope.conditions {
            let matching: Vec<&RegionResidual> = group
                .iter()
                .filter(|residual| residual.colour.as_ref() == Some(&condition.colour))
                .collect();
            if matching.is_empty() {
                all_present = false;
                continue;
            }
            if matching
                .iter()
                .any(|residual| residual.assessment != AssessmentStatus::Assessed)
            {
                all_assessed = false;
            }
            let count = matching.len() as f64;
            raw_total += condition.weight.get()
                * (matching
                    .iter()
                    .map(|residual| residual.raw_gap)
                    .sum::<f64>()
                    / count);
            match matching
                .iter()
                .all(|residual| residual.systematic_excess.is_some())
                .then(|| {
                    matching
                        .iter()
                        .filter_map(|residual| residual.systematic_excess)
                        .sum::<f64>()
                        / count
                }) {
                Some(value) => excess_total += condition.weight.get() * value,
                None => all_excess = false,
            }
            if matching
                .iter()
                .all(|residual| residual.observation_sigma.is_some())
            {
                observation_total += condition.weight.get()
                    * (matching
                        .iter()
                        .filter_map(|residual| residual.observation_sigma)
                        .sum::<f64>()
                        / count);
            } else {
                observation_complete = false;
            }
            for residual in &matching {
                reachable = match (reachable, residual.reachable) {
                    (_, Reachability::Unreachable) => Reachability::Unreachable,
                    (Reachability::Reachable, Reachability::Unassessed) => Reachability::Unassessed,
                    (current, _) => current,
                };
                settings_cells += residual.settings_cells;
            }
            if scope.conditions.len() == 1 {
                single = matching.first().copied();
            }
        }
        let weight = total_weight;
        let systematic_excess = (all_present && all_excess).then_some(excess_total / weight);
        let aggregate_sigma = scope.aggregate_sigma.map(RegionalSigma::get);
        let assessment = if all_present && all_excess && all_assessed && aggregate_sigma.is_some() {
            AssessmentStatus::Assessed
        } else {
            AssessmentStatus::Unassessed
        };
        let persistent = assessment == AssessmentStatus::Assessed
            && reachable == Reachability::Reachable
            && scope.contributors >= params.residual_min_contributors
            && systematic_excess
                .zip(aggregate_sigma)
                .is_some_and(|(excess, sigma)| excess > params.residual_sigma_threshold * sigma);
        out.push(RegionResidual {
            cell,
            colour: if scope.conditions.len() == 1 {
                Some(scope.conditions[0].colour.clone())
            } else {
                None
            },
            output: group[0].output,
            contributors: scope.contributors,
            settings_cells,
            raw_gap: raw_total / weight,
            observation_sigma: observation_complete.then_some(observation_total / weight),
            systematic_excess,
            systematic_sigma: aggregate_sigma.filter(|_| assessment == AssessmentStatus::Assessed),
            assessment,
            reachable,
            persistent,
            attribution: single
                .map(|residual| residual.attribution)
                .unwrap_or(Attribution::Unassessed),
            alignment: single.and_then(|residual| residual.alignment),
        });
    }
    Ok(out)
}

type CellKey = (SceneCell, Option<String>, Option<String>);

type ResidualCellGroup<'a> = (
    Option<OutputContext>,
    Option<ColourCondition>,
    bool,
    BTreeMap<Vec<(String, i64)>, Vec<&'a ResidualObservation>>,
);

type ConditionalCellGroup<'a> = (
    Option<OutputContext>,
    Option<ColourCondition>,
    bool,
    Vec<&'a ConditionalCellMoment>,
);

/// Computes descriptive cell evidence; pooled significance needs producer covariance.
pub fn analyze_residuals(
    params: &TrustParams,
    observations: &[ResidualObservation],
    scopes: &[RegionConditionScope],
) -> Result<Vec<RegionResidual>> {
    let mut by_cell: BTreeMap<CellKey, ResidualCellGroup<'_>> = BTreeMap::new();
    for observation in observations {
        if !observation.raw_gap.is_finite() {
            return Err(TrustError::InvalidObservation("raw_gap"));
        }
        if let Some(value) = observation.signed_residual
            && !finite_vector(value)
        {
            return Err(TrustError::InvalidObservation("signed_residual"));
        }
        if let Some(value) = observation.prediction_residual
            && !finite_vector(value)
        {
            return Err(TrustError::InvalidObservation("prediction_residual"));
        }
        if let Some(value) = observation.observation_sigma
            && !(value.is_finite() && value > 0.0)
        {
            return Err(TrustError::InvalidObservation("observation_sigma"));
        }
        if let Some(value) = observation.joint_covariance
            && !value.finite()
        {
            return Err(TrustError::InvalidObservation("joint_covariance"));
        }
        if let Some(evidence) = observation.attribution
            && (!finite_vector(evidence.residual)
                || evidence
                    .residual_covariance
                    .is_some_and(|value| !finite_matrix(value)))
        {
            return Err(TrustError::InvalidObservation("attribution"));
        }
        let colour = observation
            .colour
            .as_ref()
            .map(|colour| colour.as_str().to_owned());
        let declared = observation.colour.is_some();
        let key = (
            observation.cell,
            colour,
            observation.output.map(|output| output.canonical()),
        );
        by_cell
            .entry(key)
            .or_insert_with(|| {
                (
                    observation.output,
                    observation.colour.clone(),
                    declared,
                    BTreeMap::new(),
                )
            })
            .3
            .entry(settings_key(&observation.settings))
            .or_default()
            .push(observation);
    }

    let mut out = Vec::with_capacity(by_cell.len());
    for ((cell, _, _), (output, colour, declared, settings_cells)) in by_cell {
        let mut contributors = BTreeSet::new();
        let mut settings_pools = Vec::with_capacity(settings_cells.len());
        let mut labels = BTreeSet::new();
        let mut unassessed_attribution = false;
        let mut alignment_total = Some([0.0; 3]);
        let mut any_non_unassessed = false;
        for observations in settings_cells.values() {
            let mut by_contributor: BTreeMap<EffectiveContributorId, Vec<&ResidualObservation>> =
                BTreeMap::new();
            for observation in observations {
                contributors.insert(observation.contributor);
                by_contributor
                    .entry(observation.contributor)
                    .or_default()
                    .push(observation);
            }
            let per_contributor: Vec<Pooled> = by_contributor
                .values()
                .map(|values| pool_observations(values))
                .collect();
            let settings_pool = pool_pooled(&per_contributor);
            any_non_unassessed |= settings_pool.assessment != AssessmentStatus::Unassessed;
            let (alignment, label) = attribution(observations);
            if label == Attribution::Unassessed {
                unassessed_attribution = true;
            } else {
                labels.insert(label);
            }
            match (alignment_total, alignment) {
                (Some(total), Some(value)) => alignment_total = Some(add(total, value)),
                _ => alignment_total = None,
            }
            settings_pools.push(settings_pool);
        }
        let mut excess_total = 0.0;
        let mut excess_complete = true;
        let mut observation_sigma_total = 0.0;
        let mut observation_sigma_complete = true;
        let mut raw_gap_total = 0.0;
        for pool in &settings_pools {
            match pool.signed.zip(pool.prediction) {
                Some((signed, prediction)) => {
                    excess_total += norm(add(prediction, signed)) - norm(prediction);
                }
                None => excess_complete = false,
            }
            match pool.observation_sigma {
                Some(value) => observation_sigma_total += value,
                None => observation_sigma_complete = false,
            }
            raw_gap_total += pool.raw_gap;
        }
        let count = settings_pools.len() as f64;
        let all: Vec<&ResidualObservation> = settings_cells.values().flatten().copied().collect();
        let reachable = all
            .iter()
            .fold(Reachability::Reachable, |state, observation| {
                match (state, observation.reachable) {
                    (_, Reachability::Unreachable) => Reachability::Unreachable,
                    (Reachability::Reachable, Reachability::Unassessed) => Reachability::Unassessed,
                    (current, _) => current,
                }
            });
        let attribution = if !declared || unassessed_attribution || labels.is_empty() {
            Attribution::Unassessed
        } else if labels.len() > 1 {
            Attribution::Mixed
        } else {
            *labels.iter().next().expect("one label")
        };
        out.push(RegionResidual {
            cell,
            colour,
            output,
            contributors: contributors.len(),
            settings_cells: settings_pools.len(),
            raw_gap: raw_gap_total / count,
            observation_sigma: observation_sigma_complete
                .then_some(observation_sigma_total / count),
            systematic_excess: (declared && excess_complete).then_some(excess_total / count),
            systematic_sigma: None,
            assessment: if declared && any_non_unassessed {
                AssessmentStatus::Partial
            } else {
                AssessmentStatus::Unassessed
            },
            reachable,
            persistent: false,
            attribution,
            alignment: if declared && !unassessed_attribution && !labels.is_empty() {
                alignment_total.map(|value| scale(value, 1.0 / count))
            } else {
                None
            },
        });
    }
    aggregate_region_residuals(params, out, scopes)
}

/// A producer-supplied pooled conditional cell mean and its joint covariance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConditionalCellMoment {
    pub cell: SceneCell,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colour: Option<ColourCondition>,
    pub settings: ControlSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub contributors: usize,
    pub raw_gap: f64,
    pub signed_residual: [f64; 3],
    pub prediction_residual: [f64; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_sigma: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joint_covariance: Option<JointResidualCovariance>,
    pub assessment: AssessmentStatus,
    pub reachable: Reachability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<AttributionEvidence>,
}

/// Aggregates producer-pooled conditional cells; significance requires the pooled joint covariance.
pub fn analyze_conditional_cells(
    params: &TrustParams,
    cells: &[ConditionalCellMoment],
    scopes: &[RegionConditionScope],
) -> Result<Vec<RegionResidual>> {
    let mut by_cell: BTreeMap<CellKey, ConditionalCellGroup<'_>> = BTreeMap::new();
    for cell in cells {
        if cell.contributors == 0 {
            return Err(TrustError::InvalidObservation("contributors"));
        }
        if !cell.raw_gap.is_finite()
            || !finite_vector(cell.signed_residual)
            || !finite_vector(cell.prediction_residual)
        {
            return Err(TrustError::InvalidObservation("conditional_moment"));
        }
        if let Some(value) = cell.observation_sigma
            && !(value.is_finite() && value > 0.0)
        {
            return Err(TrustError::InvalidObservation("observation_sigma"));
        }
        if let Some(value) = cell.joint_covariance
            && !value.finite()
        {
            return Err(TrustError::InvalidObservation("joint_covariance"));
        }
        let colour = cell
            .colour
            .as_ref()
            .map(|colour| colour.as_str().to_owned());
        let declared = cell.colour.is_some();
        let key = (
            cell.cell,
            colour,
            cell.output.map(|output| output.canonical()),
        );
        by_cell
            .entry(key)
            .or_insert_with(|| (cell.output, cell.colour.clone(), declared, Vec::new()))
            .3
            .push(cell);
    }

    let mut out = Vec::with_capacity(by_cell.len());
    for ((cell, _, _), (output, colour, declared, group)) in by_cell {
        let count = group.len() as f64;
        let raw_gap = group.iter().map(|cell| cell.raw_gap).sum::<f64>() / count;
        let mut observation_sigma = Some(0.0);
        let mut any_partial = false;
        let mut excess_total = 0.0;
        let mut labels = BTreeSet::new();
        let mut unassessed_attribution = false;
        let mut alignment_total = Some([0.0; 3]);
        for cell in &group {
            match cell.observation_sigma {
                Some(value) => observation_sigma = observation_sigma.map(|sum| sum + value),
                None => observation_sigma = None,
            }
            if cell.assessment == AssessmentStatus::Partial {
                any_partial = true;
            }
            excess_total += norm(add(cell.prediction_residual, cell.signed_residual))
                - norm(cell.prediction_residual);
            match cell.attribution.as_ref() {
                Some(value) if cell.assessment == AssessmentStatus::Assessed => {
                    let (alignment, label) = attribution_from_evidence(&[(value, cell.assessment)]);
                    if label == Attribution::Unassessed {
                        unassessed_attribution = true;
                    } else {
                        labels.insert(label);
                    }
                    match (alignment_total, alignment) {
                        (Some(total), Some(value)) => {
                            alignment_total = Some(add(total, value));
                        }
                        _ => alignment_total = None,
                    }
                }
                _ => unassessed_attribution = true,
            }
        }
        let systematic_excess = declared.then_some(excess_total / count);
        let systematic_sigma =
            if declared && group.len() == 1 && group[0].assessment == AssessmentStatus::Assessed {
                group[0].joint_covariance.as_ref().and_then(|covariance| {
                    systematic_sigma(
                        group[0].prediction_residual,
                        group[0].signed_residual,
                        covariance,
                    )
                })
            } else {
                None
            };
        let assessment = if systematic_sigma.is_some() {
            AssessmentStatus::Assessed
        } else if declared && any_partial {
            AssessmentStatus::Partial
        } else {
            AssessmentStatus::Unassessed
        };
        let reachable = group.iter().fold(Reachability::Reachable, |state, cell| {
            match (state, cell.reachable) {
                (_, Reachability::Unreachable) => Reachability::Unreachable,
                (Reachability::Reachable, Reachability::Unassessed) => Reachability::Unassessed,
                (current, _) => current,
            }
        });
        let attribution = if !declared || unassessed_attribution || labels.is_empty() {
            Attribution::Unassessed
        } else if labels.len() > 1 {
            Attribution::Mixed
        } else {
            *labels.iter().next().expect("one label")
        };
        let alignment = if declared && !unassessed_attribution && !labels.is_empty() {
            alignment_total.map(|value| scale(value, 1.0 / count))
        } else {
            None
        };
        let contributors = group
            .iter()
            .map(|cell| cell.contributors)
            .max()
            .unwrap_or(0);
        let persistent = assessment == AssessmentStatus::Assessed
            && reachable == Reachability::Reachable
            && contributors >= params.residual_min_contributors
            && systematic_excess
                .zip(systematic_sigma)
                .is_some_and(|(excess, sigma)| excess > params.residual_sigma_threshold * sigma);
        out.push(RegionResidual {
            cell,
            colour,
            output,
            contributors,
            settings_cells: group.len(),
            raw_gap,
            observation_sigma: observation_sigma.map(|value| value / count),
            systematic_excess,
            systematic_sigma,
            assessment,
            reachable,
            persistent,
            attribution,
            alignment,
        });
    }
    aggregate_region_residuals(params, out, scopes)
}

/// A region's current uncertainty and the share of users it affects.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionUncertainty {
    pub region: Region,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub sigma: f64,
    pub contributors: usize,
    pub reachable: Reachability,
    /// User demand weight. Uniform until usage statistics exist ([docs/05 §12]).
    pub usage: f64,
    /// Assessed systematic excess when the region is persistent, else zero.
    pub residual_gap: f64,
    pub assessment: AssessmentStatus,
    pub attribution: Attribution,
}

/// Converts region-cell residuals into bounty inputs with uniform usage.
pub fn region_uncertainties(regions: &[RegionResidual]) -> Vec<RegionUncertainty> {
    regions
        .iter()
        .filter_map(|residual| {
            let region = residual.cell.region()?;
            if residual.assessment != AssessmentStatus::Assessed {
                return None;
            }
            let sigma = residual.systematic_sigma?;
            if !(sigma.is_finite() && sigma > 0.0) {
                return None;
            }
            Some(RegionUncertainty {
                region,
                output: residual.output,
                sigma,
                contributors: residual.contributors,
                reachable: residual.reachable,
                usage: 1.0,
                residual_gap: if residual.persistent {
                    residual.systematic_excess.unwrap_or(0.0).max(0.0)
                } else {
                    0.0
                },
                assessment: residual.assessment,
                attribution: residual.attribution,
            })
        })
        .collect()
}

/// Why the system asks for data.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BountyReason {
    UnverifiedRegion { sigma: f64 },
    PersistentResidual { gap: f64, attribution: Attribution },
}

/// Whether a bounty still wants data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BountyStatus {
    Open,
    Resolved,
}

/// A published data request ([docs/03 §1]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bounty {
    pub look: LookSlug,
    pub look_version: VersionId,
    pub body: Option<BodyId>,
    pub region: Region,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<OutputContext>,
    pub expected_variance_reduction: f64,
    pub reason: BountyReason,
    pub status: BountyStatus,
}

/// The Look version a bounty asks about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BountyTarget {
    pub look: LookSlug,
    pub look_version: VersionId,
    pub body: Option<BodyId>,
}

/// Error variance is `sigma² + gap²`; one more image moves the prior variance
/// from `prior` to `prior · image / (prior + image)`.
pub fn rank_bounties(
    params: &TrustParams,
    target: &BountyTarget,
    regions: &[RegionUncertainty],
    limit: usize,
) -> Result<Vec<Bounty>> {
    let image_variance = finite_positive("bounty_image_sigma", params.bounty_image_sigma)?;
    let image_variance = image_variance * image_variance;
    let mut ranked = Vec::new();
    for region in regions {
        if region.reachable != Reachability::Reachable {
            continue;
        }
        if !region.sigma.is_finite() || region.sigma <= 0.0 {
            return Err(TrustError::InvalidBountyUncertainty(region.sigma));
        }
        if !region.usage.is_finite() || region.usage < 0.0 {
            return Err(TrustError::InvalidRecipeValue("usage"));
        }
        let gap = region.residual_gap.abs();
        let prior = region.sigma * region.sigma + gap * gap;
        let posterior = prior * image_variance / (prior + image_variance);
        let expected_variance_reduction = region.usage * (prior - posterior);
        let reason = if gap > 0.0 {
            BountyReason::PersistentResidual {
                gap,
                attribution: region.attribution,
            }
        } else {
            BountyReason::UnverifiedRegion {
                sigma: region.sigma,
            }
        };
        ranked.push(Bounty {
            look: target.look.clone(),
            look_version: target.look_version,
            body: target.body.clone(),
            region: region.region,
            output: region.output,
            expected_variance_reduction,
            reason,
            status: BountyStatus::Open,
        });
    }
    ranked.sort_by(|left, right| {
        right
            .expected_variance_reduction
            .total_cmp(&left.expected_variance_reduction)
            .then_with(|| left.region.cmp(&right.region))
    });
    ranked.truncate(limit);
    Ok(ranked)
}

/// One piece of data offered against a bounty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BountyVerification {
    pub contributor: EffectiveContributorId,
    pub tier: Tier,
    /// Admitted root data counts as trusted regardless of tier ([docs/05 §7]).
    pub root: bool,
    pub sigma: f64,
}

impl BountyVerification {
    fn trusted(&self) -> bool {
        self.root || self.tier != Tier::New
    }
}

/// Only endorsed, seeder or root verifications may resolve a bounty.
pub fn resolution_status(
    params: &TrustParams,
    current_sigma: f64,
    verifications: &[BountyVerification],
) -> Result<BountyStatus> {
    finite_positive("current_sigma", current_sigma)?;
    let mut precision = 1.0 / (current_sigma * current_sigma);
    let mut trusted = 0;
    for verification in verifications {
        let sigma = finite_positive("verification_sigma", verification.sigma)?;
        if verification.trusted() {
            trusted += 1;
            precision += 1.0 / (sigma * sigma);
        }
    }
    if trusted == 0 {
        return Ok(BountyStatus::Open);
    }
    let posterior = (1.0 / precision).sqrt();
    Ok(if posterior < params.bounty_resolution_sigma {
        BountyStatus::Resolved
    } else {
        BountyStatus::Open
    })
}

pub fn resolve_bounty(
    params: &TrustParams,
    bounty: &mut Bounty,
    current_sigma: f64,
    verifications: &[BountyVerification],
) -> Result<()> {
    bounty.status = resolution_status(params, current_sigma, verifications)?;
    Ok(())
}

/// The numerical tolerance for one reachable settings witness.
pub const REACHABILITY_DELTA_E: f64 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReachabilityPoint {
    pub cell: SceneCell,
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_e: Option<f64>,
}

/// One legal settings vector evaluated at every declared assessment point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReachabilityWitness {
    pub metric: ScoringMetric,
    pub settings: ControlSettings,
    pub points: Vec<ReachabilityPoint>,
}

/// A model-relative reachability result. A failed witness is no recipe found,
/// not a proof that no recipe exists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReachabilityClaim {
    Reachable {
        witness: ReachabilityWitness,
    },
    Partial {
        witness: ReachabilityWitness,
        assessed: usize,
        total: usize,
    },
    NoRecipeFound {
        witness: ReachabilityWitness,
    },
    Unreachable {
        bound: f64,
    },
    Unassessed,
}

pub fn reachability_from_witness(witness: ReachabilityWitness) -> ReachabilityClaim {
    if !witness.metric.is_rendered() {
        return ReachabilityClaim::Unassessed;
    }
    let total = witness.points.len();
    if total == 0 {
        return ReachabilityClaim::Unassessed;
    }
    let mut assessed = 0;
    let mut exceeded = false;
    for point in &witness.points {
        match point.delta_e {
            Some(value) if point.supported && value.is_finite() => {
                assessed += 1;
                exceeded |= value > REACHABILITY_DELTA_E;
            }
            _ => {}
        }
    }
    if assessed < total {
        return ReachabilityClaim::Partial {
            witness,
            assessed,
            total,
        };
    }
    if exceeded {
        ReachabilityClaim::NoRecipeFound { witness }
    } else {
        ReachabilityClaim::Reachable { witness }
    }
}

/// Builds an unreachable claim only from a valid lower bound.
pub fn unreachable_from_bound(bound: f64) -> Result<ReachabilityClaim> {
    finite_positive("reachability_bound", bound)?;
    if bound <= REACHABILITY_DELTA_E {
        return Err(TrustError::InvalidRecipeValue("reachability_bound"));
    }
    Ok(ReachabilityClaim::Unreachable { bound })
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_upload::{ControlEntry, StepIndex};

    fn contributor(value: u128) -> EffectiveContributorId {
        crate::ids::CredentialId::from_u128(value).into()
    }

    fn body() -> BodyId {
        BodyId::new("fujifilm/x-t5").expect("valid body id")
    }

    fn look() -> LookSlug {
        LookSlug::new("classic-chrome").expect("valid slug")
    }

    fn sim() -> SimKey {
        SimKey::new("std").expect("valid sim key")
    }

    fn settings(exposure: i64) -> ControlSettings {
        ControlSettings::new(vec![ControlEntry::reproduced(
            looklab_upload::ControlKey::new("exposure").expect("valid control key"),
            StepIndex::new(exposure),
        )])
        .expect("valid settings")
    }

    fn key(exposure: i64) -> RecipeKey {
        RecipeKey {
            body: body(),
            epoch: EpochOrdinal::new(1),
            base_sim: sim(),
            settings: settings(exposure),
            target_look: look(),
        }
    }

    fn prediction(delta_e: f64, metric: ScoringMetric) -> RecipePrediction {
        RecipePrediction {
            metric,
            output: None,
            delta_e,
            sigma: Some(0.5),
            versions: ModelVersions {
                input_transform: 3,
                control_response: 2,
                look: VersionId::new(7),
            },
            source: None,
        }
    }

    fn verified(contributor_value: u128, measured: f64, weight: f64) -> VerifiedRecipe {
        VerifiedRecipe {
            contributor: contributor(contributor_value),
            measured_de: measured,
            raw_gap: measured - 1.0,
            signed_residual: Some([0.1, 0.0, 0.0]),
            sigma: 0.2,
            weight,
        }
    }

    fn diagonal_covariance(scale: f64) -> [[f64; 9]; 9] {
        let mut values = [[0.0; 9]; 9];
        for (index, row) in values.iter_mut().enumerate() {
            row[index] = scale;
        }
        values
    }

    fn observation(
        contributor_value: usize,
        region: Region,
        raw_gap: f64,
        sigma: f64,
    ) -> ResidualObservation {
        ResidualObservation {
            contributor: contributor(contributor_value as u128),
            cell: SceneCell::Region { region },
            colour: Some(ColourCondition::new(
                CellId::new("test/colour").expect("colour"),
            )),
            settings: settings(0),
            output: None,
            raw_gap,
            signed_residual: Some([0.0; 3]),
            prediction_residual: Some([raw_gap, 0.0, 0.0]),
            observation_sigma: Some(sigma),
            joint_covariance: None,
            assessment: AssessmentStatus::Unassessed,
            reachable: Reachability::Reachable,
            attribution: None,
        }
    }

    fn conditional(
        region: Region,
        contributors: usize,
        signed: [f64; 3],
        prediction: [f64; 3],
        covariance: Option<f64>,
        reachable: Reachability,
        assessment: AssessmentStatus,
    ) -> ConditionalCellMoment {
        ConditionalCellMoment {
            cell: SceneCell::Region { region },
            colour: Some(ColourCondition::new(
                CellId::new("test/colour").expect("colour"),
            )),
            settings: settings(0),
            output: None,
            contributors,
            raw_gap: norm(add(prediction, signed)) - norm(prediction),
            signed_residual: signed,
            prediction_residual: prediction,
            observation_sigma: Some(0.5),
            joint_covariance: covariance.map(|scale| JointResidualCovariance {
                values: diagonal_covariance(scale),
            }),
            assessment,
            reachable,
            attribution: None,
        }
    }

    fn scope(
        region: Region,
        conditions: &[(&str, f64)],
        contributors: usize,
        aggregate_sigma: Option<f64>,
    ) -> RegionConditionScope {
        RegionConditionScope {
            cell: SceneCell::Region { region },
            output: None,
            conditions: conditions
                .iter()
                .map(|(colour, weight)| DeclaredCondition {
                    colour: ColourCondition::new(CellId::new(*colour).expect("colour")),
                    weight: ConditionWeight::new(*weight).expect("weight"),
                })
                .collect(),
            contributors,
            aggregate_sigma: aggregate_sigma.map(|sigma| RegionalSigma::new(sigma).expect("sigma")),
        }
    }

    #[test]
    fn score_counts_distinct_contributors_and_merges_repeats() {
        let mut verifications = vec![verified(1, 2.0, 1.0)];
        verifications.push(verified(1, 4.0, 1.0));
        verifications.push(verified(2, 3.0, 2.0));
        let score = score_recipe(
            key(0),
            prediction(1.0, ScoringMetric::RenderedPostClipV1),
            &verifications,
        )
        .expect("score");
        assert_eq!(score.n_verifications, 2);
        assert!((score.measured_de.expect("measured") - 3.0).abs() < 1e-9);
        assert_eq!(score.model_versions.input_transform, 3);
        assert_eq!(score.metric, ScoringMetric::RenderedPostClipV1);
        assert!(score.raw_gap.is_some());
        assert!(score.signed_residual.is_some());
    }

    #[test]
    fn legacy_score_keeps_its_pre_clip_meaning() {
        let score = score_recipe(
            key(0),
            prediction(1.0, ScoringMetric::LegacyPreClipV1),
            &[verified(1, 2.0, 1.0)],
        )
        .expect("score");
        assert_eq!(score.metric, ScoringMetric::LegacyPreClipV1);
        assert_eq!(score.output, None);
        let boards = leaderboards(vec![score]);
        assert_eq!(boards[0].metric, ScoringMetric::LegacyPreClipV1);
        assert_eq!(boards[0].output, None);
    }

    #[test]
    fn leaderboard_orders_by_measured_then_predicted_and_uncertainty() {
        let measured_slow = score_recipe(
            key(1),
            prediction(0.1, ScoringMetric::RenderedPostClipV1),
            &[verified(1, 2.0, 1.0)],
        )
        .expect("score");
        let measured_fast = score_recipe(
            key(2),
            prediction(0.9, ScoringMetric::RenderedPostClipV1),
            &[verified(2, 1.0, 1.0)],
        )
        .expect("score");
        let unmeasured_low = score_recipe(
            key(3),
            prediction(0.3, ScoringMetric::RenderedPostClipV1),
            &[],
        )
        .expect("score");
        let unmeasured_high = score_recipe(
            key(4),
            prediction(0.8, ScoringMetric::RenderedPostClipV1),
            &[],
        )
        .expect("score");
        let boards = leaderboards(vec![
            measured_slow,
            measured_fast,
            unmeasured_high,
            unmeasured_low,
        ]);
        assert_eq!(boards.len(), 1);
        let entries = &boards[0].entries;
        assert_eq!(entries[0].recipe.settings.entries()[0].step.get(), 2);
        assert_eq!(entries[1].recipe.settings.entries()[0].step.get(), 1);
        assert_eq!(entries[2].recipe.settings.entries()[0].step.get(), 3);
        assert_eq!(entries[3].recipe.settings.entries()[0].step.get(), 4);
    }

    #[test]
    fn noise_only_raw_gap_does_not_earn_a_systematic_bounty() {
        let params = TrustParams::default();
        let region = Region::new(5, 3).expect("region");
        let cells = vec![conditional(
            region,
            8,
            [0.0, 0.0, 0.0],
            [4.0, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(region, &[("test/colour", 1.0)], 8, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].assessment, AssessmentStatus::Assessed);
        assert!(!regions[0].persistent);
        assert_eq!(regions[0].systematic_excess, Some(0.0));
        let uncertainties = region_uncertainties(&regions);
        let target = BountyTarget {
            look: look(),
            look_version: VersionId::new(7),
            body: Some(body()),
        };
        let bounties = rank_bounties(&params, &target, &uncertainties, 10).expect("bounties");
        assert!(
            bounties
                .iter()
                .all(|bounty| !matches!(bounty.reason, BountyReason::PersistentResidual { .. }))
        );
    }

    #[test]
    fn persistent_residual_needs_five_contributors_and_assessed_joint_moment() {
        let params = TrustParams::default();
        let region = Region::new(5, 3).expect("region");
        let four = vec![conditional(
            region,
            4,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &four,
            &[scope(region, &[("test/colour", 1.0)], 4, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions.len(), 1);
        assert!(!regions[0].persistent);
        assert_eq!(regions[0].contributors, 4);

        let five = vec![conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &five,
            &[scope(region, &[("test/colour", 1.0)], 5, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].assessment, AssessmentStatus::Assessed);
        assert!(regions[0].persistent);
        assert!(regions[0].systematic_excess.expect("excess") > 0.0);
    }

    #[test]
    fn missing_joint_covariance_stays_unassessed() {
        let params = TrustParams::default();
        let region = Region::new(1, 2).expect("region");
        let cells = vec![conditional(
            region,
            6,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            None,
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(region, &[("test/colour", 1.0)], 6, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].assessment, AssessmentStatus::Unassessed);
        assert!(!regions[0].persistent);
        assert!(regions[0].systematic_excess.is_some());
        assert!(regions[0].systematic_sigma.is_none());
    }

    #[test]
    fn injected_region_is_the_top_bounty() {
        let params = TrustParams::default();
        let injected = Region::new(4, 2).expect("region");
        let mut cells = Vec::new();
        for region in [
            Region::new(1, 0).expect("region"),
            Region::new(9, 5).expect("region"),
        ] {
            cells.push(conditional(
                region,
                5,
                [0.1, 0.0, 0.0],
                [0.5, 0.0, 0.0],
                Some(1.0),
                Reachability::Reachable,
                AssessmentStatus::Assessed,
            ));
        }
        cells.push(conditional(
            injected,
            6,
            [10.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        ));
        let scopes = vec![
            scope(
                Region::new(1, 0).expect("region"),
                &[("test/colour", 1.0)],
                5,
                Some(0.1),
            ),
            scope(
                Region::new(9, 5).expect("region"),
                &[("test/colour", 1.0)],
                5,
                Some(0.1),
            ),
            scope(injected, &[("test/colour", 1.0)], 6, Some(0.1)),
        ];
        let regions = analyze_conditional_cells(&params, &cells, &scopes).expect("analysis");
        let flagged: Vec<_> = regions.iter().filter(|region| region.persistent).collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].cell, SceneCell::Region { region: injected });
        assert!(flagged[0].systematic_excess.expect("excess") > 0.0);

        let uncertainties = region_uncertainties(&regions);
        let target = BountyTarget {
            look: look(),
            look_version: VersionId::new(7),
            body: Some(body()),
        };
        let bounties = rank_bounties(&params, &target, &uncertainties, 10).expect("bounties");
        assert_eq!(bounties[0].region, injected);
        assert!(matches!(
            bounties[0].reason,
            BountyReason::PersistentResidual { .. }
        ));
    }

    #[test]
    fn new_tier_data_alone_cannot_resolve_a_bounty() {
        let params = TrustParams::default();
        let new_tier: Vec<_> = (0..20)
            .map(|index| BountyVerification {
                contributor: contributor(index),
                tier: Tier::New,
                root: false,
                sigma: 0.05,
            })
            .collect();
        assert_eq!(
            resolution_status(&params, 2.0, &new_tier).expect("status"),
            BountyStatus::Open
        );
        assert_eq!(
            resolution_status(&params, 0.3, &new_tier).expect("status"),
            BountyStatus::Open
        );
        let mut trusted = new_tier;
        trusted.push(BountyVerification {
            contributor: contributor(99),
            tier: Tier::Endorsed,
            root: false,
            sigma: 0.1,
        });
        assert_eq!(
            resolution_status(&params, 2.0, &trusted).expect("status"),
            BountyStatus::Resolved
        );
    }

    #[test]
    fn attribution_needs_joint_covariance_and_keeps_mixed_distinct() {
        let params = TrustParams::default();
        let region = Region::new(2, 1).expect("region");
        let evidence = |groups: [AttributionResponse; 3]| AttributionEvidence {
            residual: [1.0, 0.0, 0.0],
            residual_covariance: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            input_transform: Some(groups[0]),
            control_model: Some(groups[1]),
            look: Some(groups[2]),
        };
        let response = |value: [f64; 3]| AttributionResponse {
            response: value,
            amplitude: 1.0,
            covariance: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            feasible: true,
        };
        let mut cell = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        cell.attribution = Some(evidence([
            response([0.0, 1.0, 0.0]),
            response([1.0, 0.0, 0.0]),
            response([0.0, 0.0, 1.0]),
        ]));
        let regions = analyze_conditional_cells(
            &params,
            &[cell],
            &[scope(region, &[("test/colour", 1.0)], 5, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].attribution, Attribution::ControlModel);

        let mut cell = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        cell.attribution = Some(evidence([
            response([1.0, 0.0, 0.0]),
            response([1.0, 0.0, 0.0]),
            response([0.0, 1.0, 0.0]),
        ]));
        let regions = analyze_conditional_cells(
            &params,
            &[cell],
            &[scope(region, &[("test/colour", 1.0)], 5, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].attribution, Attribution::Mixed);

        let mut cell = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        let mut missing = evidence([
            response([1.0, 0.0, 0.0]),
            response([0.0, 1.0, 0.0]),
            response([0.0, 0.0, 1.0]),
        ]);
        missing.residual_covariance = None;
        cell.attribution = Some(missing);
        let regions = analyze_conditional_cells(
            &params,
            &[cell],
            &[scope(region, &[("test/colour", 1.0)], 5, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].attribution, Attribution::Unassessed);
    }

    #[test]
    fn balanced_signed_residuals_do_not_create_excess() {
        let params = TrustParams::default();
        let region = Region::new(3, 2).expect("region");
        let mut observations = Vec::new();
        for (index, signed) in [[0.0, 1.0, 0.0], [0.0, -1.0, 0.0]].into_iter().enumerate() {
            let mut observation = observation(index, region, 1.0, 0.5);
            observation.signed_residual = Some(signed);
            observation.prediction_residual = Some([1.0, 0.0, 0.0]);
            observations.push(observation);
        }
        let regions = analyze_residuals(
            &params,
            &observations,
            &[scope(region, &[("test/colour", 1.0)], 2, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].systematic_excess, Some(0.0));
        assert!(!regions[0].persistent);
    }

    #[test]
    fn producer_status_cannot_be_upgraded_by_covariance_presence() {
        let params = TrustParams::default();
        let region = Region::new(2, 4).expect("region");
        let cells = vec![conditional(
            region,
            6,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Unassessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(region, &[("test/colour", 1.0)], 6, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].assessment, AssessmentStatus::Unassessed);
        assert!(!regions[0].persistent);
    }

    #[test]
    fn multiple_settings_cells_do_not_invent_independence() {
        let params = TrustParams::default();
        let region = Region::new(2, 4).expect("region");
        let mut first = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        let mut second = first.clone();
        second.settings = settings(1);
        first.settings = settings(0);
        let regions = analyze_conditional_cells(
            &params,
            &[first, second],
            &[scope(region, &[("test/colour", 1.0)], 5, None)],
        )
        .expect("analysis");
        assert!(regions[0].systematic_sigma.is_none());
        assert_eq!(regions[0].assessment, AssessmentStatus::Unassessed);
        assert!(!regions[0].persistent);
    }

    #[test]
    fn declared_scope_rejects_invalid_weights_duplicates_and_sigma() {
        let params = TrustParams::default();
        let region = Region::new(2, 4).expect("region");
        let cell = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        assert!(ConditionWeight::new(0.0).is_err());
        assert!(ConditionWeight::new(-1.0).is_err());
        assert!(ConditionWeight::new(f64::NAN).is_err());
        assert!(RegionalSigma::new(0.0).is_err());
        assert!(RegionalSigma::new(f64::INFINITY).is_err());
        assert_eq!(ConditionWeight::new(2.0).expect("weight").get(), 2.0);
        assert_eq!(RegionalSigma::new(0.1).expect("sigma").get(), 0.1);

        let colour = ColourCondition::new(CellId::new("test/colour").expect("colour"));
        let duplicate = RegionConditionScope {
            cell: SceneCell::Region { region },
            output: None,
            conditions: vec![
                DeclaredCondition {
                    colour: colour.clone(),
                    weight: ConditionWeight::new(1.0).expect("weight"),
                },
                DeclaredCondition {
                    colour,
                    weight: ConditionWeight::new(1.0).expect("weight"),
                },
            ],
            contributors: 5,
            aggregate_sigma: Some(RegionalSigma::new(0.1).expect("sigma")),
        };
        assert!(
            analyze_conditional_cells(&params, std::slice::from_ref(&cell), &[duplicate]).is_err()
        );
        let valid = scope(region, &[("test/colour", 1.0)], 5, Some(0.1));
        assert!(
            analyze_conditional_cells(
                &params,
                std::slice::from_ref(&cell),
                std::slice::from_ref(&valid)
            )
            .is_ok()
        );
        assert!(analyze_conditional_cells(&params, &[cell], &[valid.clone(), valid]).is_err());
    }

    #[test]
    fn two_colour_conditions_share_one_regional_result() {
        let params = TrustParams::default();
        let region = Region::new(2, 4).expect("region");
        let mut first = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        first.colour = Some(ColourCondition::new(
            CellId::new("colour/a").expect("colour"),
        ));
        let mut second = conditional(
            region,
            5,
            [3.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        second.colour = Some(ColourCondition::new(
            CellId::new("colour/b").expect("colour"),
        ));
        let cells = [first, second];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(
                region,
                &[("colour/a", 1.0), ("colour/b", 3.0)],
                5,
                None,
            )],
        )
        .expect("analysis");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].colour, None);
        assert_eq!(regions[0].systematic_excess, Some(2.5));
        assert_eq!(regions[0].systematic_sigma, None);
        assert_eq!(regions[0].assessment, AssessmentStatus::Unassessed);
        assert!(!regions[0].persistent);
        let uncertainties = region_uncertainties(&regions);
        let target = BountyTarget {
            look: look(),
            look_version: VersionId::new(7),
            body: Some(body()),
        };
        let bounties = rank_bounties(&params, &target, &uncertainties, 10).expect("bounties");
        assert!(bounties.is_empty());

        let assessed = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(
                region,
                &[("colour/a", 1.0), ("colour/b", 3.0)],
                5,
                Some(0.1),
            )],
        )
        .expect("assessed analysis");
        let bounties = rank_bounties(&params, &target, &region_uncertainties(&assessed), 10)
            .expect("assessed bounties");
        assert_eq!(bounties.len(), 1);
        assert_eq!(bounties[0].region, region);
        assert!(matches!(
            bounties[0].reason,
            BountyReason::PersistentResidual { gap: 2.5, .. }
        ));
    }

    #[test]
    fn attribution_rejects_amplitude_beyond_feasible_response() {
        let params = TrustParams::default();
        let region = Region::new(4, 1).expect("region");
        let response = |amplitude: f64| AttributionResponse {
            response: [1.0, 0.0, 0.0],
            amplitude,
            covariance: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            feasible: true,
        };
        let mut cell = conditional(
            region,
            5,
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            Some(0.01),
            Reachability::Reachable,
            AssessmentStatus::Assessed,
        );
        cell.attribution = Some(AttributionEvidence {
            residual: [1.0, 0.0, 0.0],
            residual_covariance: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
            input_transform: Some(response(0.1)),
            control_model: Some(response(0.1)),
            look: Some(response(0.1)),
        });
        let regions = analyze_conditional_cells(
            &params,
            &[cell],
            &[scope(region, &[("test/colour", 1.0)], 5, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].attribution, Attribution::Unassessed);
    }

    #[test]
    fn neutral_and_out_of_domain_cells_are_not_hue_zero() {
        assert_eq!(
            SceneCell::from_input(0.0, 0.0, 0.0),
            SceneCell::Neutral { exposure: 4 }
        );
        assert_eq!(
            SceneCell::from_input(7.0, 0.5, 30.0),
            SceneCell::OutOfDomain
        );
        assert_eq!(
            SceneCell::from_input(0.0, 0.5, 30.0),
            SceneCell::Region {
                region: Region::new(1, 4).expect("region")
            }
        );
        assert_eq!("neutral/e3".parse(), Ok(SceneCell::Neutral { exposure: 3 }));
        assert_eq!("out_of_domain".parse(), Ok(SceneCell::OutOfDomain));
    }

    #[test]
    fn reachability_distinguishes_one_witness_partial_and_no_recipe() {
        let point = |delta_e: Option<f64>, supported: bool| ReachabilityPoint {
            cell: SceneCell::Region {
                region: Region::new(1, 1).expect("region"),
            },
            supported,
            delta_e,
        };
        let witness = |points: Vec<ReachabilityPoint>| ReachabilityWitness {
            metric: ScoringMetric::RenderedPostClipV1,
            settings: settings(0),
            points,
        };
        assert!(matches!(
            reachability_from_witness(witness(vec![
                point(Some(1.0), true),
                point(Some(2.0), true)
            ])),
            ReachabilityClaim::Reachable { .. }
        ));
        assert!(matches!(
            reachability_from_witness(witness(vec![point(Some(1.0), true), point(None, true)])),
            ReachabilityClaim::Partial {
                assessed: 1,
                total: 2,
                ..
            }
        ));
        assert!(matches!(
            reachability_from_witness(witness(vec![
                point(Some(1.0), true),
                point(Some(2.5), true)
            ])),
            ReachabilityClaim::NoRecipeFound { .. }
        ));
        assert!(matches!(
            unreachable_from_bound(3.0).expect("bound"),
            ReachabilityClaim::Unreachable { .. }
        ));
        assert!(unreachable_from_bound(2.0).is_err());
        assert!(unreachable_from_bound(2.1).is_ok());
        assert!(unreachable_from_bound(0.0).is_err());
    }

    #[test]
    fn unreachable_regions_do_not_become_persistent_or_bounties() {
        let params = TrustParams::default();
        let region = Region::new(7, 4).expect("region");
        let cells = vec![conditional(
            region,
            6,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Unreachable,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(region, &[("test/colour", 1.0)], 6, Some(0.1))],
        )
        .expect("analysis");
        assert!(!regions[0].persistent);
        let uncertainties = region_uncertainties(&regions);
        let target = BountyTarget {
            look: look(),
            look_version: VersionId::new(7),
            body: None,
        };
        let bounties = rank_bounties(&params, &target, &uncertainties, 10).expect("bounties");
        assert!(bounties.is_empty());
    }

    #[test]
    fn unassessed_reachability_and_missing_directions_stay_unassessed() {
        let params = TrustParams::default();
        let region = Region::new(1, 2).expect("region");
        let cells = vec![conditional(
            region,
            6,
            [1.0, 0.0, 0.0],
            [0.5, 0.0, 0.0],
            Some(0.01),
            Reachability::Unassessed,
            AssessmentStatus::Assessed,
        )];
        let regions = analyze_conditional_cells(
            &params,
            &cells,
            &[scope(region, &[("test/colour", 1.0)], 6, Some(0.1))],
        )
        .expect("analysis");
        assert_eq!(regions[0].reachable, Reachability::Unassessed);
        assert!(!regions[0].persistent);
        assert_eq!(regions[0].attribution, Attribution::Unassessed);
        assert_eq!(regions[0].alignment, None);
        let uncertainties = region_uncertainties(&regions);
        let target = BountyTarget {
            look: look(),
            look_version: VersionId::new(7),
            body: None,
        };
        let bounties = rank_bounties(&params, &target, &uncertainties, 10).expect("bounties");
        assert!(bounties.is_empty());
    }
}
