// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image weights, caps and honest-duplicate merging ([docs/05 §5, §6, §12]).

use std::collections::{BTreeMap, BTreeSet};

use looklab_meta::SanitizedMetadata;
use looklab_upload::{ContributionKind, Intent};
use serde::{Deserialize, Serialize};

use crate::error::{Result, TrustError};
use crate::ids::{CredentialId, EffectiveContributorId, ImageRef};
use crate::params::TrustParams;
use crate::region::{Region, RegionCoverage};
use crate::reputation::Reputation;
use crate::standing::Standing;

/// The capture-metadata fields that make two uploads honest duplicates. Compared
/// bytewise and only within one effective contributor.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CaptureTuple(Vec<u8>);

impl CaptureTuple {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    pub fn from_metadata(metadata: &SanitizedMetadata) -> Result<Self> {
        postcard::to_allocvec(metadata)
            .map(Self)
            .map_err(|error| TrustError::Encode(error.to_string()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// One image entering a weighting pass.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WeightImage {
    pub id: ImageRef,
    pub credential: CredentialId,
    pub contributor: EffectiveContributorId,
    pub kind: ContributionKind,
    pub intent: Intent,
    /// Square-root coverage gain times the clip term, from [`crate::evidence`].
    pub evidence: f64,
    pub reputation: Reputation,
    pub standing: Standing,
    pub capture: CaptureTuple,
    pub coverage: RegionCoverage,
}

impl WeightImage {
    pub fn raw_weight(&self, params: &TrustParams) -> f64 {
        params.base_weight(self.kind)
            * self.evidence
            * self.standing.factor(params)
            * self.reputation.value()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapReason {
    RecipeChannel,
    DuplicateMerge,
    DiminishingReturns { position: usize },
    NewTierGroupCap,
    ContributorCap,
}

/// One weight reduction, with the factor applied and the weight before and after.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapReduction {
    pub reason: CapReason,
    pub image: Option<ImageRef>,
    pub credential: Option<CredentialId>,
    pub contributor: Option<EffectiveContributorId>,
    pub region: Option<Region>,
    pub factor: f64,
    pub before: f64,
    pub after: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapReport {
    pub reductions: Vec<CapReduction>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImageWeight {
    pub id: ImageRef,
    pub credential: CredentialId,
    pub contributor: EffectiveContributorId,
    /// Base × evidence × tier × reputation, before caps.
    pub raw: f64,
    pub per_region: BTreeMap<Region, f64>,
    pub total: f64,
    /// Number of honest duplicates this entry absorbed.
    pub duplicates: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegionWeights {
    pub region: Region,
    /// Trusted weight plus capped new-tier weight.
    pub total: f64,
    /// Endorsed, seeder and anchored weight.
    pub trusted: f64,
    pub new: f64,
    /// No endorsed or rooted data in this region.
    pub provisional: bool,
    pub new_scale: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LookWeightInput {
    pub images: Vec<WeightImage>,
    /// Root and prior weight per region, outside contributions.
    pub anchored_weight: BTreeMap<Region, f64>,
    pub rooted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LookWeightReport {
    pub images: Vec<ImageWeight>,
    pub regions: Vec<RegionWeights>,
    /// Fewer effective contributors than the minimum, on an unrooted Look.
    pub provisional: bool,
    pub contributors: usize,
    pub total: f64,
    pub caps: CapReport,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct BodyWeightInput {
    pub images: Vec<WeightImage>,
    /// Root and prior weight on the body, outside contributions.
    pub anchored_weight: f64,
    pub rooted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyImageWeight {
    pub id: ImageRef,
    pub credential: CredentialId,
    pub contributor: EffectiveContributorId,
    pub raw: f64,
    pub final_weight: f64,
    pub duplicates: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BodyWeightReport {
    pub images: Vec<BodyImageWeight>,
    /// Fewer effective contributors than the minimum, on an unrooted body.
    pub provisional: bool,
    pub contributors: usize,
    pub total: f64,
    pub caps: CapReport,
}

/// Images split by the fit stage they may enter ([docs/02 §4.3]). Recipe images never
/// enter a Look's `θ_L`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StagePartition {
    pub native: Vec<ImageRef>,
    pub recipe: Vec<ImageRef>,
}

pub fn partition_stages(images: impl IntoIterator<Item = (ImageRef, Intent)>) -> StagePartition {
    let mut partition = StagePartition::default();
    for (id, intent) in images {
        match intent {
            Intent::Native => partition.native.push(id),
            Intent::Recipe { .. } => partition.recipe.push(id),
        }
    }
    partition
}

struct Working<'a> {
    image: &'a WeightImage,
    raw: f64,
    duplicates: usize,
    contributor_factor: f64,
}

struct Prepared<'a> {
    working: Vec<Working<'a>>,
    recipe: Vec<&'a WeightImage>,
}

type DuplicateGroups<'a> =
    BTreeMap<(EffectiveContributorId, CaptureTuple), Vec<(&'a WeightImage, f64)>>;

impl Working<'_> {
    fn reduction(
        &self,
        reason: CapReason,
        region: Option<Region>,
        factor: f64,
        before: f64,
        after: f64,
    ) -> CapReduction {
        CapReduction {
            reason,
            image: Some(self.image.id.clone()),
            credential: Some(self.image.credential),
            contributor: Some(self.image.contributor),
            region,
            factor,
            before,
            after,
        }
    }
}

/// Look weight with per-region group caps, per-contributor caps and diminishing returns.
pub fn weight_look(params: &TrustParams, input: &LookWeightInput) -> LookWeightReport {
    let mut caps = CapReport::default();
    let Prepared {
        mut working,
        recipe,
    } = prepare(params, &input.images, &mut caps);

    let mut trusted_by_region: BTreeMap<Region, f64> = input.anchored_weight.clone();
    let mut new_by_region: BTreeMap<Region, f64> = BTreeMap::new();
    for entry in &working {
        for (region, share) in entry.image.coverage.iter() {
            let weight = entry.raw * share;
            if entry.image.standing == Standing::New {
                *new_by_region.entry(region).or_insert(0.0) += weight;
            } else {
                *trusted_by_region.entry(region).or_insert(0.0) += weight;
            }
        }
    }

    let mut new_scale: BTreeMap<Region, f64> = BTreeMap::new();
    for (region, new) in &new_by_region {
        let trusted = trusted_by_region.get(region).copied().unwrap_or(0.0);
        let scale = group_cap_scale(params, trusted, *new);
        new_scale.insert(*region, scale);
        if scale < 1.0 {
            for entry in &working {
                if entry.image.standing != Standing::New {
                    continue;
                }
                let share = entry.image.coverage.share(*region);
                if share <= 0.0 {
                    continue;
                }
                let before = entry.raw * share;
                caps.reductions.push(entry.reduction(
                    CapReason::NewTierGroupCap,
                    Some(*region),
                    scale,
                    before,
                    before * scale,
                ));
            }
        }
    }

    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();
    // Counted by credential: serial merging must not be able to lift a cap by shrinking the pool.
    let credentials: BTreeSet<CredentialId> =
        working.iter().map(|entry| entry.image.credential).collect();
    let cap_base: f64 = working
        .iter()
        .map(|entry| look_image_total(entry, &new_scale))
        .sum::<f64>()
        + input.anchored_weight.values().sum::<f64>();

    if credentials.len() >= params.min_look_contributors {
        let cap = params.look_contributor_cap * cap_base;
        cap_contributors(&mut working, &mut caps, cap, |entry| {
            look_image_total(entry, &new_scale)
        });
    }
    let total: f64 = working
        .iter()
        .map(|entry| look_image_total(entry, &new_scale))
        .sum::<f64>()
        + input.anchored_weight.values().sum::<f64>();

    let mut images: Vec<ImageWeight> = working
        .iter()
        .map(|entry| {
            let mut per_region = BTreeMap::new();
            for (region, share) in entry.image.coverage.iter() {
                let weight = entry.raw
                    * share
                    * region_scale(entry.image.standing, region, &new_scale)
                    * entry.contributor_factor;
                per_region.insert(region, weight);
            }
            let image_total: f64 = per_region.values().sum();
            ImageWeight {
                id: entry.image.id.clone(),
                credential: entry.image.credential,
                contributor: entry.image.contributor,
                raw: entry.image.raw_weight(params),
                per_region,
                total: image_total,
                duplicates: entry.duplicates,
            }
        })
        .collect();
    images.extend(recipe.into_iter().map(|image| {
        ImageWeight {
            id: image.id.clone(),
            credential: image.credential,
            contributor: image.contributor,
            raw: image.raw_weight(params),
            per_region: image
                .coverage
                .regions()
                .map(|region| (region, 0.0))
                .collect(),
            total: 0.0,
            duplicates: 0,
        }
    }));
    images.sort_by(|a, b| a.id.cmp(&b.id));

    let mut report_regions: BTreeSet<Region> = new_scale.keys().copied().collect();
    report_regions.extend(input.anchored_weight.keys().copied());
    report_regions.extend(
        working
            .iter()
            .flat_map(|entry| entry.image.coverage.regions()),
    );

    let regions: Vec<RegionWeights> = report_regions
        .into_iter()
        .map(|region| {
            let mut trusted = input.anchored_weight.get(&region).copied().unwrap_or(0.0);
            let mut new = 0.0;
            for entry in &working {
                let weight = entry.raw
                    * entry.image.coverage.share(region)
                    * region_scale(entry.image.standing, region, &new_scale)
                    * entry.contributor_factor;
                if entry.image.standing == Standing::New {
                    new += weight;
                } else {
                    trusted += weight;
                }
            }
            RegionWeights {
                region,
                total: trusted + new,
                trusted,
                new,
                provisional: trusted <= 0.0,
                new_scale: new_scale.get(&region).copied().unwrap_or(1.0),
            }
        })
        .collect();

    LookWeightReport {
        images,
        regions,
        provisional: credentials.len() < params.min_look_contributors && !input.rooted,
        contributors: pool.len(),
        total,
        caps,
    }
}

pub fn weight_body(params: &TrustParams, input: &BodyWeightInput) -> BodyWeightReport {
    let mut caps = CapReport::default();
    let Prepared {
        mut working,
        recipe,
    } = prepare(params, &input.images, &mut caps);

    let mut trusted = input.anchored_weight;
    let mut new = 0.0;
    for entry in &working {
        if entry.image.standing == Standing::New {
            new += entry.raw;
        } else {
            trusted += entry.raw;
        }
    }
    let group_scale = group_cap_scale(params, trusted, new);
    if group_scale < 1.0 {
        for entry in &working {
            if entry.image.standing != Standing::New {
                continue;
            }
            let before = entry.raw;
            caps.reductions.push(entry.reduction(
                CapReason::NewTierGroupCap,
                None,
                group_scale,
                before,
                before * group_scale,
            ));
        }
    }

    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();
    let credentials: BTreeSet<CredentialId> =
        working.iter().map(|entry| entry.image.credential).collect();
    let cap_base: f64 = working
        .iter()
        .map(|entry| body_image_total(entry, group_scale))
        .sum::<f64>()
        + input.anchored_weight;

    if credentials.len() >= params.min_body_contributors {
        let cap = params.body_contributor_cap * cap_base;
        cap_contributors(&mut working, &mut caps, cap, |entry| {
            body_image_total(entry, group_scale)
        });
    }
    let total: f64 = working
        .iter()
        .map(|entry| body_image_total(entry, group_scale))
        .sum::<f64>()
        + input.anchored_weight;

    let mut images: Vec<BodyImageWeight> = working
        .iter()
        .map(|entry| BodyImageWeight {
            id: entry.image.id.clone(),
            credential: entry.image.credential,
            contributor: entry.image.contributor,
            raw: entry.image.raw_weight(params),
            final_weight: body_image_total(entry, group_scale),
            duplicates: entry.duplicates,
        })
        .collect();
    images.extend(recipe.into_iter().map(|image| BodyImageWeight {
        id: image.id.clone(),
        credential: image.credential,
        contributor: image.contributor,
        raw: image.raw_weight(params),
        final_weight: 0.0,
        duplicates: 0,
    }));
    images.sort_by(|a, b| a.id.cmp(&b.id));

    BodyWeightReport {
        images,
        provisional: (trusted <= 0.0)
            || (credentials.len() < params.min_body_contributors && !input.rooted),
        contributors: pool.len(),
        total,
        caps,
    }
}

fn prepare<'a>(
    params: &TrustParams,
    images: &'a [WeightImage],
    caps: &mut CapReport,
) -> Prepared<'a> {
    let mut groups: DuplicateGroups<'a> = BTreeMap::new();
    let mut recipe = Vec::new();
    for image in images {
        if matches!(image.intent, Intent::Recipe { .. }) {
            caps.reductions.push(CapReduction {
                reason: CapReason::RecipeChannel,
                image: Some(image.id.clone()),
                credential: Some(image.credential),
                contributor: Some(image.contributor),
                region: None,
                factor: 0.0,
                before: image.raw_weight(params),
                after: 0.0,
            });
            recipe.push(image);
            continue;
        }
        let raw = image.raw_weight(params);
        groups
            .entry((image.contributor, image.capture.clone()))
            .or_default()
            .push((image, raw));
    }

    let mut working = Vec::new();
    for (_, group) in groups {
        let mut best: Option<(&WeightImage, f64)> = None;
        for &(image, raw) in &group {
            let better = match best {
                None => true,
                Some((best_image, best_raw)) => {
                    raw > best_raw || (raw == best_raw && image.id < best_image.id)
                }
            };
            if better {
                best = Some((image, raw));
            }
        }
        let Some((representative, raw)) = best else {
            continue;
        };
        for &(image, other_raw) in &group {
            if image.id == representative.id {
                continue;
            }
            caps.reductions.push(CapReduction {
                reason: CapReason::DuplicateMerge,
                image: Some(image.id.clone()),
                credential: Some(image.credential),
                contributor: Some(image.contributor),
                region: None,
                factor: 0.0,
                before: other_raw,
                after: 0.0,
            });
        }
        working.push(Working {
            image: representative,
            raw,
            duplicates: group.len(),
            contributor_factor: 1.0,
        });
    }

    let mut counts: BTreeMap<EffectiveContributorId, usize> = BTreeMap::new();
    for entry in &mut working {
        let position = counts.entry(entry.image.contributor).or_insert(0);
        let factor = if params.diminishing_scale > 0.0 {
            1.0 / (1.0 + *position as f64 / params.diminishing_scale).sqrt()
        } else {
            1.0
        };
        if factor < 1.0 {
            let before = entry.raw;
            entry.raw *= factor;
            caps.reductions.push(entry.reduction(
                CapReason::DiminishingReturns {
                    position: *position,
                },
                None,
                factor,
                before,
                entry.raw,
            ));
        }
        *position += 1;
    }

    Prepared { working, recipe }
}

fn cap_contributors(
    working: &mut [Working<'_>],
    caps: &mut CapReport,
    cap: f64,
    total_of: impl Fn(&Working<'_>) -> f64,
) {
    let mut by_contributor: BTreeMap<EffectiveContributorId, Vec<usize>> = BTreeMap::new();
    for (index, entry) in working.iter().enumerate() {
        by_contributor
            .entry(entry.image.contributor)
            .or_default()
            .push(index);
    }

    for (_, mut indices) in by_contributor {
        let contributor_total: f64 = indices.iter().map(|&index| total_of(&working[index])).sum();
        if contributor_total <= cap {
            continue;
        }
        let mut excess = contributor_total - cap;
        indices.sort_by(|&a, &b| {
            let left = &working[a].image;
            let right = &working[b].image;
            left.standing
                .cmp(&right.standing)
                .then_with(|| left.credential.cmp(&right.credential))
                .then_with(|| left.capture.cmp(&right.capture))
                .then_with(|| left.id.cmp(&right.id))
        });
        for index in indices {
            if excess <= 0.0 {
                break;
            }
            let before = total_of(&working[index]);
            if before <= 0.0 {
                continue;
            }
            let take = if excess < before { excess } else { before };
            let factor = (before - take) / before;
            working[index].contributor_factor *= factor;
            excess -= take;
            caps.reductions.push(working[index].reduction(
                CapReason::ContributorCap,
                None,
                factor,
                before,
                before - take,
            ));
        }
    }
}

fn group_cap_scale(params: &TrustParams, trusted: f64, new: f64) -> f64 {
    let gamma = params.new_group_cap.clamp(0.0, 0.999_999);
    if trusted <= 0.0 || new <= 0.0 {
        return 1.0;
    }
    let cap = gamma * trusted / (1.0 - gamma);
    if new > cap { cap / new } else { 1.0 }
}

fn region_scale(standing: Standing, region: Region, new_scale: &BTreeMap<Region, f64>) -> f64 {
    if standing == Standing::New {
        new_scale.get(&region).copied().unwrap_or(1.0)
    } else {
        1.0
    }
}

fn look_image_total(entry: &Working<'_>, new_scale: &BTreeMap<Region, f64>) -> f64 {
    let mut coverage = 0.0;
    for (region, share) in entry.image.coverage.iter() {
        coverage += share * region_scale(entry.image.standing, region, new_scale);
    }
    entry.raw * entry.contributor_factor * coverage
}

fn body_image_total(entry: &Working<'_>, group_scale: f64) -> f64 {
    let scale = if entry.image.standing == Standing::New {
        group_scale
    } else {
        1.0
    };
    entry.raw * scale * entry.contributor_factor
}
