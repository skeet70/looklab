// SPDX-License-Identifier: AGPL-3.0-or-later
//! Recipe scores, persistent-residual analysis and data bounties ([docs/05 §12]).
//!
//! Recipe data never enters a Look's θ_L. This module compares recipe outcomes
//! with the published prediction and turns the gaps into data requests.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use looklab_upload::{BodyId, ControlSettings, LookSlug, SimKey};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{EffectiveContributorId, VersionId};
use crate::params::TrustParams;
use crate::region::Region;
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

/// A prediction from the match/realization path, before any upload was measured.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipePrediction {
    pub delta_e: f64,
    pub sigma: f64,
    pub versions: ModelVersions,
}

/// One verified recipe upload. Honest duplicates are merged by effective
/// contributor before scoring.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifiedRecipe {
    pub contributor: EffectiveContributorId,
    pub measured_de: f64,
    pub sigma: f64,
    pub weight: f64,
}

/// Predicted and measured ΔE for one recipe ([docs/03 §1]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeScore {
    pub recipe: RecipeKey,
    pub predicted_de: f64,
    pub predicted_sigma: f64,
    pub measured_de: Option<f64>,
    pub measured_sigma: Option<f64>,
    /// Distinct effective contributors. Repeated uploads by one credential are one
    /// verification.
    pub n_verifications: usize,
    pub model_versions: ModelVersions,
}

/// Scores for one Look × body, ordered for the leaderboard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecipeLeaderboard {
    pub look: LookSlug,
    pub body: BodyId,
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

/// Combines verified uploads into one score. The measured value is the
/// trust-weighted mean over effective contributors and its one-sigma is the
/// weighted standard error.
pub fn score_recipe(
    recipe: RecipeKey,
    prediction: RecipePrediction,
    verifications: &[VerifiedRecipe],
) -> Result<RecipeScore> {
    let predicted_de = finite_non_negative("predicted_de", prediction.delta_e)?;
    let predicted_sigma = finite_non_negative("predicted_sigma", prediction.sigma)?;

    struct Contributor {
        weight: f64,
        weighted: f64,
        variance: f64,
    }
    let mut contributors: BTreeMap<EffectiveContributorId, Contributor> = BTreeMap::new();
    for verification in verifications {
        let measured = finite_non_negative("measured_de", verification.measured_de)?;
        let sigma = finite_positive("sigma", verification.sigma)?;
        let weight = finite_positive("weight", verification.weight)?;
        let entry = contributors
            .entry(verification.contributor)
            .or_insert(Contributor {
                weight: 0.0,
                weighted: 0.0,
                variance: 0.0,
            });
        entry.weight += weight;
        entry.weighted += weight * measured;
        entry.variance += weight * weight * sigma * sigma;
    }

    if contributors.is_empty() {
        return Ok(RecipeScore {
            recipe,
            predicted_de,
            predicted_sigma,
            measured_de: None,
            measured_sigma: None,
            n_verifications: 0,
            model_versions: prediction.versions,
        });
    }

    let mut total_weight = 0.0;
    let mut weighted = 0.0;
    for contributor in contributors.values() {
        total_weight += contributor.weight;
        weighted += contributor.weighted;
    }
    let measured_de = weighted / total_weight;
    let mut variance = 0.0;
    for contributor in contributors.values() {
        let share = contributor.weight / total_weight;
        let sigma = contributor.variance.sqrt() / contributor.weight;
        variance += share * share * sigma * sigma;
    }
    let measured_sigma = variance.sqrt();
    Ok(RecipeScore {
        recipe,
        predicted_de,
        predicted_sigma,
        measured_de: Some(measured_de),
        measured_sigma: Some(measured_sigma),
        n_verifications: contributors.len(),
        model_versions: prediction.versions,
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
        .then_with(|| left.predicted_sigma.total_cmp(&right.predicted_sigma))
        .then_with(|| left.recipe.epoch.cmp(&right.recipe.epoch))
        .then_with(|| left.recipe.base_sim.cmp(&right.recipe.base_sim))
        .then_with(|| settings_cmp(&left.recipe.settings, &right.recipe.settings))
}

/// Groups scores by Look × body and orders each group by measured ΔE, then
/// predicted ΔE and uncertainty ([docs/08 §1]).
pub fn leaderboards(scores: impl IntoIterator<Item = RecipeScore>) -> Vec<RecipeLeaderboard> {
    let mut groups: BTreeMap<(LookSlug, BodyId), Vec<RecipeScore>> = BTreeMap::new();
    for score in scores {
        groups
            .entry((score.recipe.target_look.clone(), score.recipe.body.clone()))
            .or_default()
            .push(score);
    }
    groups
        .into_iter()
        .map(|((look, body), mut entries)| {
            entries.sort_by(score_order);
            RecipeLeaderboard {
                look,
                body,
                entries,
            }
        })
        .collect()
}

/// A component whose first-order Jacobian direction a region residual follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    InputTransform,
    ControlModel,
    Look,
    Mixed,
}

/// A component's signed first-order effect on the region's error.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct JacobianDirections {
    pub input_transform: f64,
    pub control_model: f64,
    pub look: f64,
}

impl JacobianDirections {
    fn components(self) -> [f64; 3] {
        [self.input_transform, self.control_model, self.look]
    }
}

/// One verified recipe outcome in one region.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualObservation {
    pub contributor: EffectiveContributorId,
    pub region: Region,
    /// `measured - predicted` in ΔE; positive means the recipe missed more than predicted.
    pub residual_de: f64,
    pub sigma: f64,
    /// Whether the control model claims this region reachable ([docs/05 §12]).
    pub reachable: bool,
    pub jacobian: JacobianDirections,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionResidual {
    pub region: Region,
    pub contributors: usize,
    pub mean_residual: f64,
    pub sigma: f64,
    pub reachable: bool,
    pub persistent: bool,
    pub attribution: Attribution,
    /// Component alignment along the three normalized Jacobian directions.
    pub alignment: [f64; 3],
}

fn attribution(
    observations: &[&ResidualObservation],
    total_weight: f64,
) -> ([f64; 3], Attribution) {
    let mut alignment = [0.0_f64; 3];
    for observation in observations {
        let norm = observation
            .jacobian
            .components()
            .iter()
            .map(|value| value * value)
            .sum::<f64>()
            .sqrt();
        if norm <= 0.0 {
            continue;
        }
        let weight = 1.0 / (observation.sigma * observation.sigma);
        for (index, component) in observation.jacobian.components().iter().enumerate() {
            alignment[index] += weight * observation.residual_de * (component / norm);
        }
    }
    if total_weight > 0.0 {
        for value in &mut alignment {
            *value /= total_weight;
        }
    }
    let mut best = 0;
    for index in 1..alignment.len() {
        if alignment[index] > alignment[best] {
            best = index;
        }
    }
    let positive = alignment[best] > 0.0;
    if !positive {
        return (alignment, Attribution::Mixed);
    }
    let ties = alignment
        .iter()
        .filter(|value| (**value - alignment[best]).abs() <= 1e-9)
        .count();
    let label = if ties > 1 {
        Attribution::Mixed
    } else {
        match best {
            0 => Attribution::InputTransform,
            1 => Attribution::ControlModel,
            _ => Attribution::Look,
        }
    };
    (alignment, label)
}

/// Flags regions whose mean residual exceeds `residual_sigma_threshold` standard
/// errors across at least `residual_min_contributors` effective contributors. A
/// region is reachable when every observation reports reachability; attribution
/// projects the residual onto each Jacobian direction, the largest positive
/// alignment wins and a tie or no positive alignment is `Mixed`.
pub fn analyze_residuals(
    params: &TrustParams,
    observations: &[ResidualObservation],
) -> Result<Vec<RegionResidual>> {
    let mut groups: BTreeMap<Region, Vec<&ResidualObservation>> = BTreeMap::new();
    for observation in observations {
        if !observation.residual_de.is_finite() {
            return Err(TrustError::InvalidObservation("residual_de"));
        }
        if !observation.sigma.is_finite() || observation.sigma <= 0.0 {
            return Err(TrustError::InvalidObservation("sigma"));
        }
        let directions = observation.jacobian.components();
        if directions.iter().any(|value| !value.is_finite()) {
            return Err(TrustError::InvalidObservation("jacobian"));
        }
        groups
            .entry(observation.region)
            .or_default()
            .push(observation);
    }

    let mut out = Vec::with_capacity(groups.len());
    for (region, region_observations) in groups {
        let mut weight_total = 0.0;
        let mut weighted = 0.0;
        let mut contributors = BTreeSet::new();
        let mut reachable = true;
        for observation in &region_observations {
            let weight = 1.0 / (observation.sigma * observation.sigma);
            weight_total += weight;
            weighted += weight * observation.residual_de;
            contributors.insert(observation.contributor);
            reachable &= observation.reachable;
        }
        let mean_residual = weighted / weight_total;
        let sigma = (1.0 / weight_total).sqrt();
        let persistent = reachable
            && contributors.len() >= params.residual_min_contributors
            && mean_residual > params.residual_sigma_threshold * sigma;
        let (alignment, attribution) = attribution(&region_observations, weight_total);
        out.push(RegionResidual {
            region,
            contributors: contributors.len(),
            mean_residual,
            sigma,
            reachable,
            persistent,
            attribution,
            alignment,
        });
    }
    Ok(out)
}

/// A region's current uncertainty and the share of users it affects.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionUncertainty {
    pub region: Region,
    pub sigma: f64,
    pub contributors: usize,
    pub reachable: bool,
    /// User demand weight. Uniform until usage statistics exist ([docs/05 §12]).
    pub usage: f64,
    /// Mean positive residual gap when the region is persistent, else zero.
    pub residual_gap: f64,
    pub attribution: Attribution,
}

/// Converts residual regions into bounty inputs with uniform usage.
pub fn region_uncertainties(regions: &[RegionResidual]) -> Vec<RegionUncertainty> {
    regions
        .iter()
        .map(|region| RegionUncertainty {
            region: region.region,
            sigma: region.sigma,
            contributors: region.contributors,
            reachable: region.reachable,
            usage: 1.0,
            residual_gap: if region.persistent {
                region.mean_residual.max(0.0)
            } else {
                0.0
            },
            attribution: region.attribution,
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

/// Ranks regions by the expected reduction in uncertainty-weighted user error
/// from one more verified image and returns the top `limit`.
///
/// Error variance is `sigma² + gap²`. One more image with variance
/// `bounty_image_sigma²` moves the prior variance from `prior` to
/// `prior · image / (prior + image)`; the usage weight scales the reduction.
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
        if !region.reachable {
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

/// New-tier data is ignored for resolution: only endorsed, seeder or root
/// verifications may move a region below `bounty_resolution_sigma`, and at least
/// one trusted verification must exist.
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

    fn key(exposure: i64) -> RecipeKey {
        let settings = ControlSettings::new(vec![ControlEntry {
            key: looklab_upload::ControlKey::new("exposure").expect("valid control key"),
            step: StepIndex::new(exposure),
        }])
        .expect("valid settings");
        RecipeKey {
            body: body(),
            epoch: EpochOrdinal::new(1),
            base_sim: sim(),
            settings,
            target_look: look(),
        }
    }

    fn prediction(delta_e: f64) -> RecipePrediction {
        RecipePrediction {
            delta_e,
            sigma: 0.5,
            versions: ModelVersions {
                input_transform: 3,
                control_response: 2,
                look: VersionId::new(7),
            },
        }
    }

    fn verified(contributor_value: u128, measured: f64, weight: f64) -> VerifiedRecipe {
        VerifiedRecipe {
            contributor: contributor(contributor_value),
            measured_de: measured,
            sigma: 0.2,
            weight,
        }
    }

    fn observation(
        contributor_value: u128,
        region: Region,
        residual: f64,
        sigma: f64,
    ) -> ResidualObservation {
        ResidualObservation {
            contributor: contributor(contributor_value),
            region,
            residual_de: residual,
            sigma,
            reachable: true,
            jacobian: JacobianDirections {
                input_transform: 0.0,
                control_model: 0.0,
                look: 0.0,
            },
        }
    }

    #[test]
    fn score_counts_distinct_contributors_and_merges_repeats() {
        let mut verifications = vec![verified(1, 2.0, 1.0)];
        verifications.push(verified(1, 4.0, 1.0));
        verifications.push(verified(2, 3.0, 2.0));
        let score = score_recipe(key(0), prediction(1.0), &verifications).expect("score");
        assert_eq!(score.n_verifications, 2);
        assert!((score.measured_de.expect("measured") - 3.0).abs() < 1e-9);
        assert_eq!(score.model_versions.input_transform, 3);
    }

    #[test]
    fn leaderboard_orders_by_measured_then_predicted_and_uncertainty() {
        let measured_slow =
            score_recipe(key(1), prediction(0.1), &[verified(1, 2.0, 1.0)]).expect("score");
        let measured_fast =
            score_recipe(key(2), prediction(0.9), &[verified(2, 1.0, 1.0)]).expect("score");
        let unmeasured_low = score_recipe(key(3), prediction(0.3), &[]).expect("score");
        let unmeasured_high = score_recipe(key(4), prediction(0.8), &[]).expect("score");
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
    fn persistent_residual_needs_five_contributors() {
        let params = TrustParams::default();
        let region = Region::new(5, 3).expect("region");
        let observations: Vec<_> = (0..4)
            .map(|index| observation(index, region, 5.0, 0.5))
            .collect();
        let regions = analyze_residuals(&params, &observations).expect("analysis");
        assert_eq!(regions.len(), 1);
        assert!(!regions[0].persistent);
        assert_eq!(regions[0].contributors, 4);
    }

    #[test]
    fn injected_region_is_the_top_bounty() {
        let params = TrustParams::default();
        let injected = Region::new(4, 2).expect("region");
        let mut observations = Vec::new();
        for region in [
            Region::new(1, 0).expect("region"),
            Region::new(9, 5).expect("region"),
        ] {
            for index in 0..5 {
                observations.push(observation(index, region, 0.1, 0.2));
            }
        }
        for index in 0..6 {
            observations.push(observation(index, injected, 4.0, 0.3));
        }
        let regions = analyze_residuals(&params, &observations).expect("analysis");
        let flagged: Vec<_> = regions.iter().filter(|region| region.persistent).collect();
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].region, injected);
        assert!(flagged[0].mean_residual > 2.0 * flagged[0].sigma);

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
    fn residual_attribution_follows_the_largest_jacobian_projection() {
        let params = TrustParams::default();
        let region = Region::new(2, 1).expect("region");
        let observations: Vec<_> = (0..5)
            .map(|index| ResidualObservation {
                contributor: contributor(index),
                region,
                residual_de: 3.0,
                sigma: 0.5,
                reachable: true,
                jacobian: JacobianDirections {
                    input_transform: 0.2,
                    control_model: 2.0,
                    look: 0.1,
                },
            })
            .collect();
        let regions = analyze_residuals(&params, &observations).expect("analysis");
        assert_eq!(regions[0].attribution, Attribution::ControlModel);
        assert!(regions[0].alignment[1] > regions[0].alignment[0]);
    }

    #[test]
    fn unreachable_regions_do_not_become_persistent_or_bounties() {
        let params = TrustParams::default();
        let region = Region::new(7, 4).expect("region");
        let observations: Vec<_> = (0..6)
            .map(|index| {
                let mut observation = observation(index, region, 5.0, 0.2);
                observation.reachable = false;
                observation
            })
            .collect();
        let regions = analyze_residuals(&params, &observations).expect("analysis");
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
}
