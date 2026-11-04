// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};

use looklab_fit::{ContributorId, ExposureModeId, FitImage, ImageIntent, Settings};
use looklab_trust::{
    EffectiveContributorId, EvidenceGrid, ImageRef, LookWeightInput, Standing, TrustParams,
    WeightImage, coverage_gain, evidence, weight_body, weight_look,
};
use looklab_upload::Intent;

use crate::db::ContributionIntent;
use crate::error::Result;
use crate::pool::{LookPool, PoolImage};

#[derive(Debug, Clone)]
pub struct WeightedImage {
    pub pool_index: usize,
    pub look_weight: f64,
    pub body_weight: f64,
    pub duplicates: usize,
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
                    && entry.look_weight > 0.0
            })
            .map(|entry| (entry.pool_index, entry.look_weight))
            .collect()
    }
}

pub fn weigh(pool: &LookPool, params: &TrustParams) -> Result<WeightedPool> {
    let evidence_values = evidence_values(pool, params)?;
    let inputs: Vec<WeightImage> = pool
        .images
        .iter()
        .enumerate()
        .map(|(index, image)| weight_image(pool, image, evidence_values[index]))
        .collect();
    let look = weight_look(
        params,
        &LookWeightInput {
            images: inputs.clone(),
            anchored_weight: BTreeMap::new(),
            rooted: false,
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

    let look_weights: HashMap<String, (f64, usize)> = look
        .images
        .iter()
        .map(|image| {
            (
                image.id.as_str().to_owned(),
                (image.total, image.duplicates),
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
            let (look_weight, duplicates) = look_weights.get(&id)?;
            Some(WeightedImage {
                pool_index,
                look_weight: *look_weight,
                body_weight: body_weights.get(&id).copied().unwrap_or(0.0),
                duplicates: *duplicates,
            })
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

/// Leave-one-out coverage, so evidence does not depend on upload order.
fn evidence_values(pool: &LookPool, params: &TrustParams) -> Result<Vec<f64>> {
    let grids: Vec<EvidenceGrid> = pool
        .images
        .iter()
        .map(|image| {
            let mut grid = EvidenceGrid::default();
            for (s, hue, chroma) in &image.axes {
                grid.add(*s, *hue, *chroma, 1.0)?;
            }
            Ok(grid)
        })
        .collect::<Result<Vec<_>>>()?;

    let mut values = Vec::with_capacity(pool.images.len());
    for (index, image) in pool.images.iter().enumerate() {
        let mut rest = EvidenceGrid::default();
        for (other, source) in pool.images.iter().enumerate() {
            if other == index {
                continue;
            }
            for (s, hue, chroma) in &source.axes {
                rest.add(*s, *hue, *chroma, 1.0)?;
            }
        }
        let gain = coverage_gain(&rest, &grids[index])?;
        values.push(evidence(gain, image.clip_fraction, params));
    }
    Ok(values)
}

pub fn contributor_index(pool: &LookPool) -> HashMap<EffectiveContributorId, u32> {
    let mut contributors: Vec<EffectiveContributorId> =
        pool.images.iter().map(|image| image.contributor).collect();
    contributors.sort();
    contributors.dedup();
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
            let mut fit = FitImage::new(image.samples.clone(), image.transform);
            fit.exposure_hint = f64::from(image.samples.header().exposure_hint_ev);
            fit.trust_weight = weight;
            fit.exposure_mode = ExposureModeId(0);
            fit.contributor = ContributorId(
                contributors
                    .get(&image.contributor)
                    .copied()
                    .unwrap_or_default(),
            );
            fit.settings = Settings::default();
            fit.intent = ImageIntent::Native;
            fit
        })
        .collect()
}
