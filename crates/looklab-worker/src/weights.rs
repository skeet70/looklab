// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};

use looklab_fit::{ContributorId, FitImage, ImageIntent};
use looklab_trust::{
    EffectiveContributorId, EvidenceGrid, ImageRef, LookWeightInput, Region, Standing, TrustParams,
    WeightImage, coverage_gain, evidence, weight_body, weight_look,
};
use looklab_upload::Intent;

use crate::db::{ContributionIntent, ImageOutcome};
use crate::error::Result;
use crate::pool::{LookPool, PoolImage};
use crate::roots::RootWeights;

/// An image the consistency pass down-weighted keeps half its weight (docs/02 §4.4).
pub const DOWN_WEIGHT_FACTOR: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct WeightedImage {
    pub pool_index: usize,
    pub look_weight: f64,
    pub body_weight: f64,
    pub per_region: BTreeMap<Region, f64>,
    pub duplicates: usize,
}

impl WeightedImage {
    pub fn consistency_factor(&self, pool: &LookPool) -> f64 {
        match pool.images[self.pool_index].outcome {
            Some(ImageOutcome::DownWeighted) => DOWN_WEIGHT_FACTOR,
            _ => 1.0,
        }
    }

    pub fn effective_look_weight(&self, pool: &LookPool) -> f64 {
        self.look_weight * self.consistency_factor(pool)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageInspection {
    pub image: ImageRef,
    pub look_weight: f64,
    pub effective_look_weight: f64,
    pub body_weight: f64,
    pub per_region: BTreeMap<Region, f64>,
    pub sample_trust: Vec<f32>,
    pub sample_regions: Vec<Option<Region>>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Inspection {
    pub images: Vec<ImageInspection>,
}

/// The weights a fit would use, without fitting.
pub async fn inspect(
    db_pool: &sqlx::SqlitePool,
    config: &crate::config::WorkerConfig,
    body: &looklab_upload::BodyId,
    look: &looklab_upload::LookSlug,
) -> Result<Inspection> {
    let pool = crate::pool::build(db_pool, config, body, look).await?;
    let root = crate::roots::load(db_pool, config, &pool).await?;
    let weighted = weigh(&pool, &TrustParams::default(), &root.weights)?;
    Ok(Inspection {
        images: weighted
            .images
            .iter()
            .map(|entry| ImageInspection {
                image: ImageRef::new(pool.images[entry.pool_index].image.id.as_str()),
                look_weight: entry.look_weight,
                effective_look_weight: entry.effective_look_weight(&pool),
                body_weight: entry.body_weight,
                per_region: entry.per_region.clone(),
                sample_trust: region_shares(&pool.images[entry.pool_index], entry),
                sample_regions: sample_regions(&pool.images[entry.pool_index]),
            })
            .collect(),
    })
}

#[derive(Debug)]
pub struct WeightedPool {
    pub images: Vec<WeightedImage>,
}

impl WeightedPool {
    /// Native representatives in fit order, with the weight the Look fit uses.
    pub fn fit_entries(&self, pool: &LookPool) -> Vec<(usize, f64)> {
        self.images
            .iter()
            .filter(|entry| {
                pool.images[entry.pool_index].contribution.intent == ContributionIntent::Native
                    && entry.effective_look_weight(pool) > 0.0
            })
            .map(|entry| (entry.pool_index, entry.effective_look_weight(pool)))
            .collect()
    }
}

pub fn weigh(pool: &LookPool, params: &TrustParams, root: &RootWeights) -> Result<WeightedPool> {
    if pool
        .images
        .iter()
        .any(|image| image.contribution.intent == ContributionIntent::Card)
    {
        return Err(crate::error::WorkerError::Data(
            "card renders cannot enter a weighting pool".to_owned(),
        ));
    }
    let evidence_values = evidence_values(pool, params)?;
    let inputs: Vec<WeightImage> = pool
        .images
        .iter()
        .enumerate()
        .map(|(index, image)| weight_image(pool, image, evidence_values[index]))
        .collect();
    let look_inputs: Vec<WeightImage> = inputs
        .iter()
        .filter(|image| matches!(image.intent, Intent::Native))
        .cloned()
        .collect();
    let look = weight_look(
        params,
        &LookWeightInput {
            images: look_inputs,
            anchored_weight: root.anchored_weight.clone(),
            rooted: root.rooted,
        },
    );
    let body = weight_body(
        params,
        &looklab_trust::BodyWeightInput {
            images: inputs,
            anchored_weight: 0.0,
            rooted: false,
        },
    );

    type LookWeights = (f64, BTreeMap<Region, f64>, usize);
    let look_weights: HashMap<String, LookWeights> = look
        .images
        .iter()
        .map(|image| {
            (
                image.id.as_str().to_owned(),
                (image.total, image.per_region.clone(), image.duplicates),
            )
        })
        .collect();
    let body_weights: HashMap<String, f64> = body
        .images
        .iter()
        .map(|image| (image.id.as_str().to_owned(), image.final_weight))
        .collect();

    let images = pool
        .images
        .iter()
        .enumerate()
        .filter_map(|(pool_index, image)| {
            let id = image.image.id.as_str();
            let body_weight = body_weights.get(&id).copied().unwrap_or(0.0);
            match look_weights.get(&id) {
                Some((look_weight, per_region, duplicates)) => Some(WeightedImage {
                    pool_index,
                    look_weight: *look_weight,
                    body_weight,
                    per_region: per_region.clone(),
                    duplicates: *duplicates,
                }),
                None if body_weight > 0.0 => Some(WeightedImage {
                    pool_index,
                    look_weight: 0.0,
                    body_weight,
                    per_region: BTreeMap::new(),
                    duplicates: 0,
                }),
                None => None,
            }
        })
        .collect();

    Ok(WeightedPool { images })
}

fn weight_image(pool: &LookPool, image: &PoolImage, evidence: f64) -> WeightImage {
    let intent = match image.contribution.intent {
        ContributionIntent::Native => Intent::Native,
        ContributionIntent::Recipe => Intent::Recipe {
            target_look: pool.look.clone(),
        },
        ContributionIntent::Card => unreachable!("card renders never reach a weighting pool"),
    };
    WeightImage {
        id: ImageRef::new(image.image.id.as_str()),
        credential: image.credential.trust,
        contributor: image.contributor,
        kind: image.contribution.kind,
        intent,
        evidence,
        reputation: image.credential.reputation,
        standing: Standing::resolve(
            image.credential.tier,
            image.credential.scope.as_ref(),
            &pool.look,
            &pool.body,
        ),
        capture: image.capture.clone(),
        coverage: image.coverage.clone(),
    }
}

/// Leave-one-out coverage over native images; a recipe image is scored against the
/// native pool alone (docs/05 §12).
fn evidence_values(pool: &LookPool, params: &TrustParams) -> Result<Vec<f64>> {
    let mut grids: Vec<EvidenceGrid> = Vec::with_capacity(pool.images.len());
    for image in &pool.images {
        let mut grid = EvidenceGrid::default();
        for (s, hue, chroma) in &image.axes {
            grid.add(*s, *hue, *chroma, 1.0)?;
        }
        grids.push(grid);
    }
    let native: Vec<usize> = pool
        .images
        .iter()
        .enumerate()
        .filter(|(_, image)| image.contribution.intent == ContributionIntent::Native)
        .map(|(index, _)| index)
        .collect();
    let mut groups: BTreeMap<(EffectiveContributorId, looklab_trust::CaptureTuple), Vec<usize>> =
        BTreeMap::new();
    for index in &native {
        let image = &pool.images[*index];
        groups
            .entry((image.contributor, image.capture.clone()))
            .or_default()
            .push(*index);
    }
    let representatives: Vec<usize> = groups
        .values()
        .map(|members| members.iter().copied().min().unwrap_or_default())
        .collect();
    let mut representative_of: HashMap<usize, usize> = HashMap::new();
    for members in groups.values() {
        let representative = members.iter().copied().min().unwrap_or_default();
        for member in members {
            representative_of.insert(*member, representative);
        }
    }
    let mut native_union = EvidenceGrid::default();
    for index in &representatives {
        for (s, hue, chroma) in &pool.images[*index].axes {
            native_union.add(*s, *hue, *chroma, 1.0)?;
        }
    }

    let mut representative_values: HashMap<usize, f64> = HashMap::new();
    for representative in &representatives {
        let mut rest = EvidenceGrid::default();
        for other in &representatives {
            if other == representative {
                continue;
            }
            for (s, hue, chroma) in &pool.images[*other].axes {
                rest.add(*s, *hue, *chroma, 1.0)?;
            }
        }
        let gain = coverage_gain(&rest, &grids[*representative])?;
        representative_values.insert(
            *representative,
            evidence(gain, pool.images[*representative].clip_fraction, params),
        );
    }

    let mut values = Vec::with_capacity(pool.images.len());
    for (index, image) in pool.images.iter().enumerate() {
        let value = if image.contribution.intent == ContributionIntent::Native {
            representative_of
                .get(&index)
                .and_then(|representative| representative_values.get(representative))
                .copied()
                .unwrap_or(0.0)
        } else {
            let gain = coverage_gain(&native_union, &grids[index])?;
            evidence(gain, image.clip_fraction, params)
        };
        values.push(value);
    }
    Ok(values)
}

/// Native contributors take the first ids in sorted order, so adding a recipe
/// contributor leaves every existing `σ_v` group unchanged.
pub fn contributor_index(pool: &LookPool) -> HashMap<EffectiveContributorId, u32> {
    let mut contributors: Vec<EffectiveContributorId> = pool
        .images
        .iter()
        .filter(|image| image.contribution.intent == ContributionIntent::Native)
        .map(|image| image.contributor)
        .collect();
    contributors.sort();
    contributors.dedup();
    let mut recipe: Vec<EffectiveContributorId> = pool
        .images
        .iter()
        .filter(|image| image.contribution.intent != ContributionIntent::Native)
        .map(|image| image.contributor)
        .filter(|contributor| !contributors.contains(contributor))
        .collect();
    recipe.sort();
    recipe.dedup();
    contributors.extend(recipe);
    contributors
        .into_iter()
        .enumerate()
        .filter_map(|(index, contributor)| {
            u32::try_from(index).ok().map(|index| (contributor, index))
        })
        .collect()
}

pub fn fit_images(
    pool: &LookPool,
    weighted: &WeightedPool,
    contributors: &HashMap<EffectiveContributorId, u32>,
) -> Vec<FitImage> {
    weighted
        .fit_entries(pool)
        .into_iter()
        .map(|(index, weight)| {
            let image = &pool.images[index];
            let entry = weighted
                .images
                .iter()
                .find(|entry| entry.pool_index == index);
            let mut fit = base_image(pool, index, contributors);
            fit.trust_weight = weight;
            fit.sample_trust = entry.map(|entry| region_shares(image, entry));
            fit.intent = ImageIntent::Native;
            fit
        })
        .collect()
}

/// Native and recipe images for the body stage, weighted by the body report.
pub fn recipe_fit_images(
    pool: &LookPool,
    weighted: &WeightedPool,
    contributors: &HashMap<EffectiveContributorId, u32>,
) -> Vec<FitImage> {
    weighted
        .images
        .iter()
        .filter(|entry| entry.body_weight > 0.0)
        .map(|entry| {
            let image = &pool.images[entry.pool_index];
            let mut fit = base_image(pool, entry.pool_index, contributors);
            fit.trust_weight = entry.body_weight;
            fit.intent = match image.contribution.intent {
                ContributionIntent::Native => ImageIntent::Native,
                ContributionIntent::Recipe => ImageIntent::Recipe,
                ContributionIntent::Card => unreachable!("card renders carry no fit weight"),
            };
            fit
        })
        .collect()
}

fn base_image(
    pool: &LookPool,
    index: usize,
    contributors: &HashMap<EffectiveContributorId, u32>,
) -> FitImage {
    let image = &pool.images[index];
    let mut fit = FitImage::new(image.samples.clone(), image.transform);
    fit.exposure_hint = f64::from(image.samples.header().exposure_hint_ev);
    fit.exposure_mode = image.exposure_mode;
    fit.contributor = ContributorId(
        contributors
            .get(&image.contributor)
            .copied()
            .unwrap_or_default(),
    );
    fit.settings = image.settings.clone();
    fit
}

fn sample_regions(image: &PoolImage) -> Vec<Option<Region>> {
    image
        .samples
        .in_rgb()
        .iter()
        .map(|rgb| {
            let xyz = image.transform.mul_vec(rgb.map(f64::from));
            if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
                return None;
            }
            let input = looklab_model::LookInput::new(looklab_color::Xyz::from_array(xyz));
            let s = input
                .s
                .clamp(looklab_trust::EXPOSURE_MIN, looklab_trust::EXPOSURE_MAX);
            Region::from_probe(s, input.h_in).ok()
        })
        .collect()
}

/// Per-sample multipliers of `trust_weight`; regional totals follow `per_region` and
/// the mean multiplier is 1.
fn region_shares(image: &PoolImage, entry: &WeightedImage) -> Vec<f32> {
    let total = entry.look_weight;
    let n = image.samples.n();
    let mut counts: HashMap<Region, usize> = HashMap::new();
    let regions = sample_regions(image);
    for region in regions.iter().flatten() {
        *counts.entry(*region).or_insert(0) += 1;
    }
    regions
        .into_iter()
        .map(|region| {
            let Some(region) = region else {
                return 0.0;
            };
            let count = counts.get(&region).copied().unwrap_or(0);
            if total <= 0.0 || count == 0 {
                return 0.0;
            }
            let per_region = entry.per_region.get(&region).copied().unwrap_or(0.0);
            (per_region * n as f64 / (total * count as f64)) as f32
        })
        .collect()
}
