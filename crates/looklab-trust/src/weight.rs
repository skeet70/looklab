// SPDX-License-Identifier: AGPL-3.0-or-later
//! Image weights, caps and honest-duplicate merging ([docs/05 §5, §6, §12]).
//!
//! Every cap is applied as a multiplier `≤ 1` on the step-1 raw weight. The group
//! cap uses the fixed pre-merge new-tier total; the per-contributor cap is
//! `max(floor, factor / n)` over effective contributors and is water-filled against
//! the final total.

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

const MAX_CAP_ITERATIONS: usize = 128;

const CAP_TOLERANCE: f64 = 1e-12;

/// The capture-metadata fields that make two uploads honest duplicates.
///
/// One fixed slot per metadata field, each `Some` exactly when the field is present.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CaptureTuple {
    make: Option<Vec<u8>>,
    model: Option<Vec<u8>>,
    firmware: Option<Vec<u8>>,
    lens_model: Option<Vec<u8>>,
    focal_length: Option<Vec<u8>>,
    iso: Option<Vec<u8>>,
    shutter: Option<Vec<u8>>,
    aperture: Option<Vec<u8>>,
    exposure_compensation: Option<Vec<u8>>,
    as_shot_wb: Option<Vec<u8>>,
    capture_month: Option<Vec<u8>>,
    software: Option<Vec<u8>>,
    look: Option<Vec<u8>>,
}

impl CaptureTuple {
    pub fn from_metadata(metadata: &SanitizedMetadata) -> Result<Self> {
        Ok(Self {
            make: encode(&metadata.make)?,
            model: encode(&metadata.model)?,
            firmware: encode(&metadata.firmware)?,
            lens_model: encode(&metadata.lens_model)?,
            focal_length: encode(&metadata.focal_length)?,
            iso: encode(&metadata.iso)?,
            shutter: encode(&metadata.shutter)?,
            aperture: encode(&metadata.aperture)?,
            exposure_compensation: encode(&metadata.exposure_compensation)?,
            as_shot_wb: encode(&metadata.as_shot_wb)?,
            capture_month: encode(&metadata.capture_month)?,
            software: encode(&metadata.software)?,
            look: encode(&metadata.look)?,
        })
    }

    /// Sets the look slot from the stored wire mirror of the in-camera settings, so
    /// re-renders of one RAW at different settings stay separate weights.
    pub fn with_wire_look(mut self, look: Option<&looklab_upload::LookSettings>) -> Result<Self> {
        self.look = encode(&look)?;
        Ok(self)
    }
}

fn encode<T: Serialize>(value: &Option<T>) -> Result<Option<Vec<u8>>> {
    value
        .as_ref()
        .map(|inner| {
            postcard::to_allocvec(inner).map_err(|error| TrustError::Encode(error.to_string()))
        })
        .transpose()
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
    /// `base(kind) × evidence × tier × reputation`, with no merge-dependent factor.
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
    CardChannel,
    DuplicateMerge,
    DiminishingReturns { position: usize },
    NewTierGroupCap,
    ContributorCap,
    SerialMergeBound,
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
    /// Fewer effective contributors than the provisional threshold, on an unrooted Look.
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
    /// Fewer effective contributors than the provisional threshold, on an unrooted body,
    /// or no trusted weight at all.
    pub provisional: bool,
    pub contributors: usize,
    pub total: f64,
    pub caps: CapReport,
}

/// Images split by the fit stage they may enter ([docs/02 §4.3]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StagePartition {
    pub native: Vec<ImageRef>,
    pub recipe: Vec<ImageRef>,
    pub card: Vec<ImageRef>,
}

pub fn partition_stages(images: impl IntoIterator<Item = (ImageRef, Intent)>) -> StagePartition {
    let mut partition = StagePartition::default();
    for (id, intent) in images {
        match intent {
            Intent::Native => partition.native.push(id),
            Intent::Recipe { .. } => partition.recipe.push(id),
            Intent::Card(_) => partition.card.push(id),
        }
    }
    partition
}

struct Working<'a> {
    image: &'a WeightImage,
    raw: f64,
    duplicates: usize,
    /// Honest-duplicate and diminishing-returns factor, always `≤ 1`.
    merge_factor: f64,
    /// Product of every contributor-cap and group-cap re-check factor.
    contributor_factor: f64,
}

struct Prepared<'a> {
    working: Vec<Working<'a>>,
    recipe: Vec<&'a WeightImage>,
    card: Vec<&'a WeightImage>,
}

type DuplicateGroups<'a> =
    BTreeMap<(EffectiveContributorId, CaptureTuple), Vec<(&'a WeightImage, f64)>>;

struct LookState<'a> {
    working: Vec<Working<'a>>,
    recipe: Vec<&'a WeightImage>,
    card: Vec<&'a WeightImage>,
    new_scale: BTreeMap<Region, f64>,
}

struct BodyState<'a> {
    working: Vec<Working<'a>>,
    recipe: Vec<&'a WeightImage>,
    card: Vec<&'a WeightImage>,
    group_scale: f64,
}

pub fn weight_look(params: &TrustParams, input: &LookWeightInput) -> LookWeightReport {
    let mut caps = CapReport::default();
    let mut state = build_look(params, input, &mut caps);

    let merged = merged_credentials(&input.images);
    if !merged.is_empty() {
        let shadow = separate_look_input(input);
        let mut shadow_caps = CapReport::default();
        let shadow_state = build_look(params, &shadow, &mut shadow_caps);
        let targets: BTreeMap<ImageRef, f64> = shadow_state
            .working
            .iter()
            .map(|entry| {
                (
                    entry.image.id.clone(),
                    look_total(entry, &shadow_state.new_scale),
                )
            })
            .collect();

        let mut bounded = false;
        for entry in &mut state.working {
            if !merged.contains(&entry.image.credential) {
                continue;
            }
            let Some(target) = targets.get(&entry.image.id).copied() else {
                continue;
            };
            let before = look_total(entry, &state.new_scale);
            if before <= target + CAP_TOLERANCE || before <= 0.0 {
                continue;
            }
            let factor = target / before;
            entry.contributor_factor *= factor;
            let after = look_total(entry, &state.new_scale);
            caps.reductions.push(cap_reduction(
                entry.image,
                CapReason::SerialMergeBound,
                None,
                factor,
                before,
                after,
            ));
            bounded = true;
        }
        if bounded {
            let new_raw = look_new_raw_totals(params, &input.images);
            let anchored_total: f64 = input.anchored_weight.values().sum();
            let pool: BTreeSet<EffectiveContributorId> = state
                .working
                .iter()
                .map(|entry| entry.image.contributor)
                .collect();
            let share = look_contributor_cap(params, pool.len());
            let context = LookCapContext {
                params,
                share,
                new_raw: &new_raw,
                anchored: &input.anchored_weight,
                anchored_total,
            };
            apply_look_caps(
                &context,
                &mut state.working,
                &mut state.new_scale,
                &mut caps,
            );
        }
    }

    finalize_look(params, input, state, caps)
}

pub fn weight_body(params: &TrustParams, input: &BodyWeightInput) -> BodyWeightReport {
    let mut caps = CapReport::default();
    let mut state = build_body(params, input, &mut caps);

    let merged = merged_credentials(&input.images);
    if !merged.is_empty() {
        let shadow = separate_body_input(input);
        let mut shadow_caps = CapReport::default();
        let shadow_state = build_body(params, &shadow, &mut shadow_caps);
        let targets: BTreeMap<ImageRef, f64> = shadow_state
            .working
            .iter()
            .map(|entry| {
                (
                    entry.image.id.clone(),
                    body_weight(entry, shadow_state.group_scale),
                )
            })
            .collect();

        let mut bounded = false;
        for entry in &mut state.working {
            if !merged.contains(&entry.image.credential) {
                continue;
            }
            let Some(target) = targets.get(&entry.image.id).copied() else {
                continue;
            };
            let before = body_weight(entry, state.group_scale);
            if before <= target + CAP_TOLERANCE || before <= 0.0 {
                continue;
            }
            let factor = target / before;
            entry.contributor_factor *= factor;
            let after = body_weight(entry, state.group_scale);
            caps.reductions.push(cap_reduction(
                entry.image,
                CapReason::SerialMergeBound,
                None,
                factor,
                before,
                after,
            ));
            bounded = true;
        }
        if bounded {
            let new_raw = body_new_raw_total(params, &input.images);
            let pool: BTreeSet<EffectiveContributorId> = state
                .working
                .iter()
                .map(|entry| entry.image.contributor)
                .collect();
            let share = body_contributor_cap(params, pool.len());
            apply_body_caps(
                params,
                share,
                &mut state.working,
                &mut state.group_scale,
                new_raw,
                input.anchored_weight,
                &mut caps,
            );
        }
    }

    finalize_body(params, input, state, caps)
}

fn build_look<'a>(
    params: &TrustParams,
    input: &'a LookWeightInput,
    caps: &mut CapReport,
) -> LookState<'a> {
    let new_raw = look_new_raw_totals(params, &input.images);
    let mut new_scale: BTreeMap<Region, f64> =
        new_raw.keys().map(|region| (*region, 1.0)).collect();
    let Prepared {
        mut working,
        recipe,
        card,
    } = prepare(params, &input.images, false, caps);

    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();
    if !pool.is_empty() {
        let share = look_contributor_cap(params, pool.len());
        let context = LookCapContext {
            params,
            share,
            new_raw: &new_raw,
            anchored: &input.anchored_weight,
            anchored_total: input.anchored_weight.values().sum(),
        };
        apply_look_caps(&context, &mut working, &mut new_scale, caps);
    }
    LookState {
        working,
        recipe,
        card,
        new_scale,
    }
}

fn build_body<'a>(
    params: &TrustParams,
    input: &'a BodyWeightInput,
    caps: &mut CapReport,
) -> BodyState<'a> {
    let new_raw = body_new_raw_total(params, &input.images);
    let mut group_scale = 1.0;
    let Prepared {
        mut working,
        recipe,
        card,
    } = prepare(params, &input.images, true, caps);

    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();
    if !pool.is_empty() {
        let share = body_contributor_cap(params, pool.len());
        apply_body_caps(
            params,
            share,
            &mut working,
            &mut group_scale,
            new_raw,
            input.anchored_weight,
            caps,
        );
    }
    BodyState {
        working,
        recipe,
        card,
        group_scale,
    }
}

fn finalize_look(
    params: &TrustParams,
    input: &LookWeightInput,
    state: LookState<'_>,
    caps: CapReport,
) -> LookWeightReport {
    let LookState {
        working,
        recipe,
        card,
        new_scale,
    } = state;
    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();

    let mut images: Vec<ImageWeight> = working
        .iter()
        .map(|entry| {
            let mut per_region = BTreeMap::new();
            for region in entry.image.coverage.regions() {
                per_region.insert(region, look_weight(entry, region, &new_scale));
            }
            let total: f64 = per_region.values().sum();
            ImageWeight {
                id: entry.image.id.clone(),
                credential: entry.image.credential,
                contributor: entry.image.contributor,
                raw: entry.image.raw_weight(params),
                per_region,
                total,
                duplicates: entry.duplicates,
            }
        })
        .collect();
    images.extend(recipe.into_iter().chain(card).map(|image| {
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
                let weight = look_weight(entry, region, &new_scale);
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

    let total: f64 = working
        .iter()
        .map(|entry| look_total(entry, &new_scale))
        .sum::<f64>()
        + input.anchored_weight.values().sum::<f64>();

    LookWeightReport {
        images,
        regions,
        provisional: pool.len() < params.min_look_contributors && !input.rooted,
        contributors: pool.len(),
        total,
        caps,
    }
}

fn finalize_body(
    params: &TrustParams,
    input: &BodyWeightInput,
    state: BodyState<'_>,
    caps: CapReport,
) -> BodyWeightReport {
    let BodyState {
        working,
        recipe,
        card,
        group_scale,
    } = state;
    let pool: BTreeSet<EffectiveContributorId> = working
        .iter()
        .map(|entry| entry.image.contributor)
        .collect();

    let mut images: Vec<BodyImageWeight> = working
        .iter()
        .map(|entry| BodyImageWeight {
            id: entry.image.id.clone(),
            credential: entry.image.credential,
            contributor: entry.image.contributor,
            raw: entry.image.raw_weight(params),
            final_weight: body_weight(entry, group_scale),
            duplicates: entry.duplicates,
        })
        .collect();
    images.extend(recipe.into_iter().chain(card).map(|image| BodyImageWeight {
        id: image.id.clone(),
        credential: image.credential,
        contributor: image.contributor,
        raw: image.raw_weight(params),
        final_weight: 0.0,
        duplicates: 0,
    }));
    images.sort_by(|a, b| a.id.cmp(&b.id));

    let trusted: f64 = input.anchored_weight
        + working
            .iter()
            .filter(|entry| entry.image.standing != Standing::New)
            .map(|entry| body_weight(entry, group_scale))
            .sum::<f64>();
    let total: f64 = working
        .iter()
        .map(|entry| body_weight(entry, group_scale))
        .sum::<f64>()
        + input.anchored_weight;

    BodyWeightReport {
        images,
        provisional: (trusted <= 0.0)
            || (pool.len() < params.min_body_contributors && !input.rooted),
        contributors: pool.len(),
        total,
        caps,
    }
}

fn merged_credentials(images: &[WeightImage]) -> BTreeSet<CredentialId> {
    let mut groups: BTreeMap<EffectiveContributorId, BTreeSet<CredentialId>> = BTreeMap::new();
    for image in images {
        groups
            .entry(image.contributor)
            .or_default()
            .insert(image.credential);
    }
    groups
        .into_values()
        .filter(|credentials| credentials.len() > 1)
        .flatten()
        .collect()
}

fn separate_look_input(input: &LookWeightInput) -> LookWeightInput {
    LookWeightInput {
        images: input
            .images
            .iter()
            .map(|image| {
                let mut shadow = image.clone();
                shadow.contributor = EffectiveContributorId::from(image.credential);
                shadow
            })
            .collect(),
        anchored_weight: input.anchored_weight.clone(),
        rooted: input.rooted,
    }
}

fn separate_body_input(input: &BodyWeightInput) -> BodyWeightInput {
    BodyWeightInput {
        images: input
            .images
            .iter()
            .map(|image| {
                let mut shadow = image.clone();
                shadow.contributor = EffectiveContributorId::from(image.credential);
                shadow
            })
            .collect(),
        anchored_weight: input.anchored_weight,
        rooted: input.rooted,
    }
}

/// `max(floor, factor / n)` over effective contributors.
pub fn look_contributor_cap(params: &TrustParams, contributors: usize) -> f64 {
    let n = contributors.max(1) as f64;
    params
        .look_contributor_cap_floor
        .max(params.contributor_cap_factor / n)
}

pub fn body_contributor_cap(params: &TrustParams, contributors: usize) -> f64 {
    let n = contributors.max(1) as f64;
    params
        .body_contributor_cap_floor
        .max(params.contributor_cap_factor / n)
}

fn prepare<'a>(
    params: &TrustParams,
    images: &'a [WeightImage],
    include_recipe: bool,
    caps: &mut CapReport,
) -> Prepared<'a> {
    let mut groups: DuplicateGroups<'a> = BTreeMap::new();
    let mut recipe = Vec::new();
    let mut card = Vec::new();
    for image in images {
        if matches!(image.intent, Intent::Card(_)) {
            caps.reductions.push(cap_reduction(
                image,
                CapReason::CardChannel,
                None,
                0.0,
                image.raw_weight(params),
                0.0,
            ));
            card.push(image);
            continue;
        }
        if !include_recipe && matches!(image.intent, Intent::Recipe { .. }) {
            caps.reductions.push(cap_reduction(
                image,
                CapReason::RecipeChannel,
                None,
                0.0,
                image.raw_weight(params),
                0.0,
            ));
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
                    image.standing > best_image.standing
                        || (image.standing == best_image.standing
                            && (raw > best_raw || (raw == best_raw && image.id < best_image.id)))
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
            caps.reductions.push(cap_reduction(
                image,
                CapReason::DuplicateMerge,
                None,
                0.0,
                other_raw,
                0.0,
            ));
        }
        working.push(Working {
            image: representative,
            raw,
            duplicates: group.len(),
            merge_factor: 1.0,
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
        let before = entry.raw * entry.merge_factor;
        entry.merge_factor *= factor;
        if factor < 1.0 {
            caps.reductions.push(cap_reduction(
                entry.image,
                CapReason::DiminishingReturns {
                    position: *position,
                },
                None,
                factor,
                before,
                entry.raw * entry.merge_factor,
            ));
        }
        *position += 1;
    }

    Prepared {
        working,
        recipe,
        card,
    }
}

fn look_weight(entry: &Working<'_>, region: Region, new_scale: &BTreeMap<Region, f64>) -> f64 {
    entry.raw
        * entry.image.coverage.share(region)
        * region_scale(entry.image.standing, region, new_scale)
        * entry.merge_factor
        * entry.contributor_factor
}

fn look_total(entry: &Working<'_>, new_scale: &BTreeMap<Region, f64>) -> f64 {
    entry.raw
        * entry.merge_factor
        * entry.contributor_factor
        * entry
            .image
            .coverage
            .iter()
            .map(|(region, share)| share * region_scale(entry.image.standing, region, new_scale))
            .sum::<f64>()
}

fn body_weight(entry: &Working<'_>, group_scale: f64) -> f64 {
    let scale = if entry.image.standing == Standing::New {
        group_scale
    } else {
        1.0
    };
    entry.raw * scale * entry.merge_factor * entry.contributor_factor
}

fn region_scale(standing: Standing, region: Region, new_scale: &BTreeMap<Region, f64>) -> f64 {
    if standing == Standing::New {
        new_scale.get(&region).copied().unwrap_or(1.0)
    } else {
        1.0
    }
}

fn gamma_ratio(params: &TrustParams) -> f64 {
    let gamma = params.new_group_cap.clamp(0.0, 0.999_999);
    gamma / (1.0 - gamma)
}

/// Pre-merge new-tier raw weight per region. The group denominator uses this fixed
/// total, so dropping a merged image cannot release headroom to another credential.
fn look_new_raw_totals(params: &TrustParams, images: &[WeightImage]) -> BTreeMap<Region, f64> {
    let mut new: BTreeMap<Region, f64> = BTreeMap::new();
    for image in images {
        if image.standing != Standing::New
            || matches!(image.intent, Intent::Recipe { .. } | Intent::Card(_))
        {
            continue;
        }
        let raw = image.raw_weight(params);
        for (region, share) in image.coverage.iter() {
            *new.entry(region).or_insert(0.0) += raw * share;
        }
    }
    new
}

fn body_new_raw_total(params: &TrustParams, images: &[WeightImage]) -> f64 {
    images
        .iter()
        .filter(|image| image.standing == Standing::New && !matches!(image.intent, Intent::Card(_)))
        .map(|image| image.raw_weight(params))
        .sum()
}

/// Bounds new-tier weight by `γ/(1−γ) × trusted` using the fixed pre-merge new total.
fn enforce_look_group(
    params: &TrustParams,
    working: &[Working<'_>],
    new_scale: &mut BTreeMap<Region, f64>,
    new_raw: &BTreeMap<Region, f64>,
    anchored: &BTreeMap<Region, f64>,
    caps: &mut CapReport,
) -> bool {
    let ratio = gamma_ratio(params);
    let mut trusted: BTreeMap<Region, f64> = anchored.clone();
    for entry in working {
        if entry.image.standing == Standing::New {
            continue;
        }
        let weight = entry.raw * entry.merge_factor * entry.contributor_factor;
        for (region, share) in entry.image.coverage.iter() {
            *trusted.entry(region).or_insert(0.0) += weight * share;
        }
    }

    let mut changed = false;
    for (region, new_pre) in new_raw {
        if *new_pre <= 0.0 {
            continue;
        }
        let trusted_total = trusted.get(region).copied().unwrap_or(0.0);
        if trusted_total <= 0.0 {
            continue;
        }
        let bound = (ratio * trusted_total / new_pre).min(1.0);
        let current = new_scale.get(region).copied().unwrap_or(1.0);
        if bound >= current * (1.0 - CAP_TOLERANCE) {
            continue;
        }
        let factor = bound / current;
        new_scale.insert(*region, bound);
        for entry in working {
            if entry.image.standing != Standing::New {
                continue;
            }
            let share = entry.image.coverage.share(*region);
            if share <= 0.0 {
                continue;
            }
            let base = entry.raw * share * entry.merge_factor * entry.contributor_factor;
            caps.reductions.push(cap_reduction(
                entry.image,
                CapReason::NewTierGroupCap,
                Some(*region),
                factor,
                base * current,
                base * bound,
            ));
        }
        changed = true;
    }
    changed
}

fn enforce_body_group(
    params: &TrustParams,
    working: &[Working<'_>],
    group_scale: &mut f64,
    new_pre: f64,
    anchored: f64,
    caps: &mut CapReport,
) -> bool {
    if new_pre <= 0.0 {
        return false;
    }
    let ratio = gamma_ratio(params);
    let mut trusted = anchored;
    for entry in working {
        if entry.image.standing != Standing::New {
            trusted += body_weight(entry, *group_scale);
        }
    }
    if trusted <= 0.0 {
        return false;
    }
    let bound = (ratio * trusted / new_pre).min(1.0);
    let current = *group_scale;
    if bound >= current * (1.0 - CAP_TOLERANCE) {
        return false;
    }
    let factor = bound / current;
    *group_scale = bound;
    for entry in working {
        if entry.image.standing != Standing::New {
            continue;
        }
        let base = entry.raw * entry.merge_factor * entry.contributor_factor;
        caps.reductions.push(cap_reduction(
            entry.image,
            CapReason::NewTierGroupCap,
            None,
            factor,
            base * current,
            base * bound,
        ));
    }
    true
}

struct LookCapContext<'a> {
    params: &'a TrustParams,
    share: f64,
    new_raw: &'a BTreeMap<Region, f64>,
    anchored: &'a BTreeMap<Region, f64>,
    anchored_total: f64,
}

fn apply_look_caps(
    context: &LookCapContext<'_>,
    working: &mut [Working<'_>],
    new_scale: &mut BTreeMap<Region, f64>,
    caps: &mut CapReport,
) {
    for _ in 0..MAX_CAP_ITERATIONS {
        let mut changed = water_fill_look(
            working,
            new_scale,
            context.anchored_total,
            context.share,
            caps,
        );
        changed |= enforce_look_group(
            context.params,
            working,
            new_scale,
            context.new_raw,
            context.anchored,
            caps,
        );
        if !changed {
            return;
        }
    }
}

fn apply_body_caps(
    params: &TrustParams,
    share: f64,
    working: &mut [Working<'_>],
    group_scale: &mut f64,
    new_raw: f64,
    anchored: f64,
    caps: &mut CapReport,
) {
    for _ in 0..MAX_CAP_ITERATIONS {
        let mut changed = water_fill_body(working, *group_scale, anchored, share, caps);
        changed |= enforce_body_group(params, working, group_scale, new_raw, anchored, caps);
        if !changed {
            return;
        }
    }
}

fn water_fill_look(
    working: &mut [Working<'_>],
    new_scale: &BTreeMap<Region, f64>,
    anchored_total: f64,
    share: f64,
    caps: &mut CapReport,
) -> bool {
    let mut by_contributor: BTreeMap<EffectiveContributorId, Vec<usize>> = BTreeMap::new();
    for (index, entry) in working.iter().enumerate() {
        by_contributor
            .entry(entry.image.contributor)
            .or_default()
            .push(index);
    }
    let list: Vec<(EffectiveContributorId, f64)> = by_contributor
        .iter()
        .map(|(contributor, indices)| {
            (
                *contributor,
                indices
                    .iter()
                    .map(|&index| look_total(&working[index], new_scale))
                    .sum(),
            )
        })
        .collect();
    let finals = water_fill(&list, anchored_total, share);
    let targets: BTreeMap<EffectiveContributorId, f64> = list
        .iter()
        .zip(&finals)
        .map(|((contributor, _), final_total)| (*contributor, *final_total))
        .collect();

    let mut changed = false;
    for (contributor, mut indices) in by_contributor {
        changed |= reduce_contributor(
            working,
            &mut indices,
            targets[&contributor],
            |entry| look_total(entry, new_scale),
            caps,
        );
    }
    changed
}

fn water_fill_body(
    working: &mut [Working<'_>],
    group_scale: f64,
    anchored: f64,
    share: f64,
    caps: &mut CapReport,
) -> bool {
    let mut by_contributor: BTreeMap<EffectiveContributorId, Vec<usize>> = BTreeMap::new();
    for (index, entry) in working.iter().enumerate() {
        by_contributor
            .entry(entry.image.contributor)
            .or_default()
            .push(index);
    }
    let list: Vec<(EffectiveContributorId, f64)> = by_contributor
        .iter()
        .map(|(contributor, indices)| {
            (
                *contributor,
                indices
                    .iter()
                    .map(|&index| body_weight(&working[index], group_scale))
                    .sum(),
            )
        })
        .collect();
    let finals = water_fill(&list, anchored, share);
    let targets: BTreeMap<EffectiveContributorId, f64> = list
        .iter()
        .zip(&finals)
        .map(|((contributor, _), final_total)| (*contributor, *final_total))
        .collect();

    let mut changed = false;
    for (contributor, mut indices) in by_contributor {
        changed |= reduce_contributor(
            working,
            &mut indices,
            targets[&contributor],
            |entry| body_weight(entry, group_scale),
            caps,
        );
    }
    changed
}

/// Brings one contributor to `target`, taking weight from lower-standing credentials'
/// images first.
fn reduce_contributor(
    working: &mut [Working<'_>],
    indices: &mut [usize],
    target: f64,
    weight: impl Fn(&Working<'_>) -> f64,
    caps: &mut CapReport,
) -> bool {
    let current: f64 = indices.iter().map(|&index| weight(&working[index])).sum();
    if current <= target * (1.0 + CAP_TOLERANCE) {
        return false;
    }
    indices.sort_by(|&a, &b| {
        let left = &working[a].image;
        let right = &working[b].image;
        left.standing
            .cmp(&right.standing)
            .then_with(|| left.credential.cmp(&right.credential))
            .then_with(|| left.capture.cmp(&right.capture))
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut excess = current - target;
    for &index in indices.iter() {
        if excess <= current * CAP_TOLERANCE {
            break;
        }
        let before = weight(&working[index]);
        if before <= 0.0 {
            continue;
        }
        let take = if excess < before { excess } else { before };
        let factor = (before - take) / before;
        working[index].contributor_factor *= factor;
        excess -= take;
        caps.reductions.push(cap_reduction(
            working[index].image,
            CapReason::ContributorCap,
            None,
            factor,
            before,
            before - take,
        ));
    }
    true
}

/// Solves `v_c = min(raw_c, share × (anchored + Σ v))` for every contributor.
///
/// The top `k` contributors are capped at `value` and the rest keep their weight, with
/// `total = (anchored + Σ uncapped) / (1 − k·share)`. Returns all zeros when no such
/// allocation exists.
fn water_fill(totals: &[(EffectiveContributorId, f64)], anchored: f64, share: f64) -> Vec<f64> {
    let n = totals.len();
    let mut result = vec![0.0; n];
    if share <= 0.0 {
        return result;
    }
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        totals[a]
            .1
            .total_cmp(&totals[b].1)
            .then_with(|| totals[a].0.cmp(&totals[b].0))
    });

    let mut suffix: f64 = totals.iter().map(|(_, total)| *total).sum();
    for k in 0..=n {
        if k > 0 {
            suffix -= totals[order[n - k]].1;
        }
        let denominator = 1.0 - k as f64 * share;
        if denominator <= 0.0 {
            continue;
        }
        let total = (anchored + suffix) / denominator;
        let value = share * total;
        let uncapped_ok = k == n
            || totals[order[n - k - 1]].1 <= value * (1.0 + CAP_TOLERANCE) + f64::MIN_POSITIVE;
        let capped_ok = k == 0 || totals[order[n - k]].1 > value * (1.0 - CAP_TOLERANCE);
        if uncapped_ok && capped_ok {
            for (rank, &index) in order.iter().enumerate() {
                result[index] = if rank >= n - k {
                    value
                } else {
                    totals[index].1
                };
            }
            return result;
        }
    }
    result
}

fn cap_reduction(
    image: &WeightImage,
    reason: CapReason,
    region: Option<Region>,
    factor: f64,
    before: f64,
    after: f64,
) -> CapReduction {
    CapReduction {
        reason,
        image: Some(image.id.clone()),
        credential: Some(image.credential),
        contributor: Some(image.contributor),
        region,
        factor,
        before,
        after,
    }
}
