// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet, HashMap};

use looklab_fit::{ContributorId, FitImage, ImageIntent, ImageWeights, StageImageWeights};
use looklab_trust::{
    EffectiveContributorId, EvidenceGrid, ImageRef, LookWeightInput, Region, Standing, TrustParams,
    WeightImage, coverage_gain, evidence, weight_body, weight_look,
};
use looklab_upload::{BodyId, Intent};

use crate::db::{ContributionIntent, ImageId, ImageOutcome};
use crate::error::Result;
use crate::pool::{EvidenceRole, EvidenceStage, LookPool, PoolImage};
pub use crate::roots::RootWeights;

/// Image verdicts from one qualified subject/candidate/scope.
#[derive(Debug, Clone, Default)]
pub struct ConsistencyScores {
    verdicts: HashMap<ImageId, ImageOutcome>,
}

impl ConsistencyScores {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, image: ImageId, outcome: ImageOutcome) {
        self.verdicts.insert(image, outcome);
    }

    pub fn get(&self, image: &ImageId) -> Option<ImageOutcome> {
        self.verdicts.get(image).copied()
    }

    /// Legacy acceptance carries no scoped evidence.
    pub fn effective(&self, image: &ImageId, legacy: Option<ImageOutcome>) -> Option<ImageOutcome> {
        match (self.get(image), legacy) {
            (Some(ImageOutcome::Quarantined), _) | (_, Some(ImageOutcome::Quarantined)) => {
                Some(ImageOutcome::Quarantined)
            }
            (Some(ImageOutcome::DownWeighted), _) | (_, Some(ImageOutcome::DownWeighted)) => {
                Some(ImageOutcome::DownWeighted)
            }
            (Some(ImageOutcome::Accepted), _) => Some(ImageOutcome::Accepted),
            (None, None | Some(ImageOutcome::Accepted)) => None,
        }
    }
}

/// An image the consistency pass down-weighted keeps half its weight (docs/02 §4.4).
pub const DOWN_WEIGHT_FACTOR: f64 = 0.5;

#[derive(Debug, Clone)]
pub struct WeightedImage {
    pub image: ImageId,
    pub body: BodyId,
    pub look_weight: f64,
    pub body_weight: f64,
    /// Native-only body-stage weight, without recipe evidence.
    pub native_body_weight: f64,
    pub per_region: BTreeMap<Region, f64>,
    pub duplicates: usize,
    pub native_body_duplicates: usize,
    pub outcome: Option<ImageOutcome>,
}

impl WeightedImage {
    pub fn consistency_factor(&self) -> f64 {
        match self.outcome {
            Some(ImageOutcome::DownWeighted) => DOWN_WEIGHT_FACTOR,
            _ => 1.0,
        }
    }

    pub fn effective_look_weight(&self) -> f64 {
        self.look_weight * self.consistency_factor()
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
    let pools = crate::pool::build_all(
        db_pool,
        config,
        body,
        look,
        &crate::pool::EvidenceScope::Experimental,
    )
    .await?;
    let scores = ConsistencyScores::default();
    let root = crate::roots::load_family(&pools, &scores)?;
    let weighted = weigh_family(&pools, &TrustParams::default(), &root.weights, &scores)?;
    let mut images = Vec::new();
    for pool in &pools {
        for image in &pool.images {
            let Some(entry) = weighted.get(&image.image.id) else {
                continue;
            };
            images.push(ImageInspection {
                image: ImageRef::new(image.image.id.as_str()),
                look_weight: entry.look_weight,
                effective_look_weight: entry.effective_look_weight(),
                body_weight: entry.body_weight,
                per_region: entry.per_region.clone(),
                sample_trust: region_shares(image, entry),
                sample_regions: sample_regions(image),
            });
        }
    }
    Ok(Inspection { images })
}

/// Weights over one family's eligible evidence across bodies.
#[derive(Debug)]
pub struct WeightedFamily {
    pub images: Vec<WeightedImage>,
}

impl WeightedFamily {
    pub fn get(&self, id: &ImageId) -> Option<&WeightedImage> {
        self.images.iter().find(|entry| &entry.image == id)
    }

    /// Native representatives in fit order, with the weight the Look fit uses.
    pub fn fit_entries(&self, pool: &LookPool) -> Vec<(usize, f64)> {
        pool.images
            .iter()
            .enumerate()
            .filter_map(|(index, image)| {
                if image.role != EvidenceRole::Native || !image.is_eligible() {
                    return None;
                }
                let entry = self.get(&image.image.id)?;
                let weight = entry.effective_look_weight();
                (weight > 0.0).then_some((index, weight))
            })
            .collect()
    }

    /// Native and recipe images for the body stage, weighted by the body report.
    pub fn body_entries(&self, pool: &LookPool) -> Vec<(usize, f64)> {
        pool.images
            .iter()
            .enumerate()
            .filter_map(|(index, image)| {
                if !image.is_eligible() {
                    return None;
                }
                let entry = self.get(&image.image.id)?;
                (entry.body_weight > 0.0).then_some((index, entry.body_weight))
            })
            .collect()
    }

    pub fn body_entries_native(&self, pool: &LookPool) -> Vec<(usize, f64)> {
        pool.images
            .iter()
            .enumerate()
            .filter_map(|(index, image)| {
                if image.role != EvidenceRole::Native || !image.is_eligible() {
                    return None;
                }
                let entry = self.get(&image.image.id)?;
                (entry.native_body_weight > 0.0).then_some((index, entry.native_body_weight))
            })
            .collect()
    }
}

/// Weights over one realization's eligible evidence.
pub fn weigh(
    pool: &LookPool,
    params: &TrustParams,
    root: &RootWeights,
    scores: &ConsistencyScores,
) -> Result<WeightedFamily> {
    weigh_family(std::slice::from_ref(pool), params, root, scores)
}

/// Weights over the actual combined evidence entering a family's shared blocks.
pub fn weigh_family(
    pools: &[LookPool],
    params: &TrustParams,
    root: &RootWeights,
    scores: &ConsistencyScores,
) -> Result<WeightedFamily> {
    let mut entries: Vec<(&LookPool, &PoolImage)> = Vec::new();
    let mut seen: BTreeSet<ImageId> = BTreeSet::new();
    for pool in pools {
        for image in &pool.images {
            if image.contribution.intent == ContributionIntent::Card {
                return Err(crate::error::WorkerError::Data(
                    "card renders cannot enter a weighting pool".to_owned(),
                ));
            }
            if scores.effective(&image.image.id, image.outcome) == Some(ImageOutcome::Quarantined) {
                continue;
            }
            if image.is_eligible() && seen.insert(image.image.id) {
                entries.push((pool, image));
            }
        }
    }
    entries.sort_by_key(|left| left.1.image.id);
    if entries.is_empty() {
        return Ok(WeightedFamily { images: Vec::new() });
    }

    let evidence = evidence_values(&entries, params, EvidenceUnion::Pass)?;
    let look_evidence = evidence_values(&entries, params, EvidenceUnion::Family)?;
    let published_inputs: Vec<WeightImage> = entries
        .iter()
        .zip(&evidence)
        .map(|((pool, image), evidence)| {
            weight_image(pool, image, *evidence, EvidenceStage::Published)
        })
        .collect();
    let native_inputs: Vec<Option<WeightImage>> = entries
        .iter()
        .zip(&evidence)
        .map(|((pool, image), evidence)| {
            (image.role == EvidenceRole::Native)
                .then(|| weight_image(pool, image, *evidence, EvidenceStage::Native))
        })
        .collect();

    let native: Vec<WeightImage> = entries
        .iter()
        .zip(&look_evidence)
        .filter(|((_, image), _)| image.role == EvidenceRole::Native)
        .map(|((pool, image), evidence)| {
            weight_image(pool, image, *evidence, EvidenceStage::Native)
        })
        .collect();
    let look = weight_look(
        params,
        &LookWeightInput {
            images: native,
            anchored_weight: root.anchored_weight.clone(),
            rooted: root.rooted,
        },
    );
    let look_weights: HashMap<ImageRef, (f64, BTreeMap<Region, f64>, usize)> = look
        .images
        .iter()
        .map(|image| {
            (
                image.id.clone(),
                (image.total, image.per_region.clone(), image.duplicates),
            )
        })
        .collect();

    let mut by_body: BTreeMap<&BodyId, Vec<usize>> = BTreeMap::new();
    for (index, (pool, _)) in entries.iter().enumerate() {
        by_body.entry(&pool.body).or_default().push(index);
    }
    let mut body_weights: HashMap<ImageRef, f64> = HashMap::new();
    for positions in by_body.values() {
        let images: Vec<WeightImage> = positions
            .iter()
            .map(|index| published_inputs[*index].clone())
            .collect();
        let body = weight_body(
            params,
            &looklab_trust::BodyWeightInput {
                images,
                anchored_weight: 0.0,
                rooted: false,
            },
        );
        for image in body.images {
            body_weights.insert(image.id, image.final_weight);
        }
    }

    let mut native_body_weights: HashMap<ImageRef, (f64, usize)> = HashMap::new();
    for positions in by_body.values() {
        let images: Vec<WeightImage> = positions
            .iter()
            .filter_map(|index| native_inputs[*index].clone())
            .collect();
        if images.is_empty() {
            continue;
        }
        let body = weight_body(
            params,
            &looklab_trust::BodyWeightInput {
                images,
                anchored_weight: 0.0,
                rooted: false,
            },
        );
        for image in body.images {
            native_body_weights.insert(image.id, (image.final_weight, image.duplicates));
        }
    }

    let mut images = Vec::with_capacity(entries.len());
    for ((pool, image), input) in entries.iter().zip(published_inputs) {
        let id = input.id;
        let look_weight = look_weights.get(&id).map(|entry| entry.0).unwrap_or(0.0);
        let (per_region, duplicates) = look_weights
            .get(&id)
            .map(|entry| (entry.1.clone(), entry.2))
            .unwrap_or_default();
        let body_weight = body_weights.get(&id).copied().unwrap_or(0.0);
        let (native_body_weight, native_body_duplicates) =
            native_body_weights.get(&id).copied().unwrap_or((0.0, 0));
        if look_weight <= 0.0 && body_weight <= 0.0 && native_body_weight <= 0.0 {
            continue;
        }
        images.push(WeightedImage {
            image: image.image.id,
            body: pool.body.clone(),
            look_weight,
            body_weight,
            native_body_weight,
            per_region,
            duplicates,
            native_body_duplicates,
            outcome: scores.effective(&image.image.id, image.outcome),
        });
    }

    Ok(WeightedFamily { images })
}

fn weight_image(
    pool: &LookPool,
    image: &PoolImage,
    evidence: f64,
    stage: EvidenceStage,
) -> WeightImage {
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
        contributor: pool.observation.contributor(stage, image.credential.trust),
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
        coverage: image
            .coverage
            .clone()
            .expect("eligible images admit at least one sample"),
    }
}

/// The images one leave-one-out union spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EvidenceUnion {
    /// Every image of the weighting pass: the body stage, whose parameters all of them move.
    Pass,
    /// The images of one family: the Look stage. A render of the same RAW in another
    /// film simulation says nothing about this family's Look.
    Family,
}

/// One image as the evidence computation sees it.
struct EvidenceEntry<'a> {
    /// Images with equal scope share one union.
    scope: usize,
    native: bool,
    contributor: EffectiveContributorId,
    capture: &'a looklab_trust::CaptureTuple,
    axes: &'a [(f64, f64, f64)],
    clip_fraction: f64,
}

/// Leave-one-out coverage over native images; a recipe image is scored against the
/// native pool alone (docs/05 §5, §12).
fn evidence_values(
    entries: &[(&LookPool, &PoolImage)],
    params: &TrustParams,
    union: EvidenceUnion,
) -> Result<Vec<f64>> {
    let mut families = Vec::new();
    let views: Vec<EvidenceEntry<'_>> = entries
        .iter()
        .map(|(pool, image)| {
            let scope = match union {
                EvidenceUnion::Pass => 0,
                EvidenceUnion::Family => {
                    let known = families.iter().position(|family| *family == &pool.look);
                    known.unwrap_or_else(|| {
                        families.push(&pool.look);
                        families.len() - 1
                    })
                }
            };
            EvidenceEntry {
                scope,
                native: image.role == EvidenceRole::Native,
                contributor: image.contributor,
                capture: &image.capture,
                axes: &image.axes,
                clip_fraction: image.clip_fraction,
            }
        })
        .collect();
    scoped_evidence(&views, params)
}

fn scoped_evidence(entries: &[EvidenceEntry<'_>], params: &TrustParams) -> Result<Vec<f64>> {
    let mut grids: Vec<EvidenceGrid> = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut grid = EvidenceGrid::default();
        for (s, hue, chroma) in entry.axes {
            grid.add(*s, *hue, *chroma, 1.0)?;
        }
        grids.push(grid);
    }
    type GroupKey<'a> = (
        usize,
        EffectiveContributorId,
        &'a looklab_trust::CaptureTuple,
    );
    let mut groups: BTreeMap<GroupKey<'_>, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.native {
            groups
                .entry((entry.scope, entry.contributor, entry.capture))
                .or_default()
                .push(index);
        }
    }
    let mut representative_of: HashMap<usize, usize> = HashMap::new();
    let mut unions: BTreeMap<usize, EvidenceGrid> = BTreeMap::new();
    let mut representatives: Vec<usize> = Vec::with_capacity(groups.len());
    for ((scope, _, _), members) in &groups {
        let representative = members.iter().copied().min().unwrap_or_default();
        for member in members {
            representative_of.insert(*member, representative);
        }
        let native_union = unions.entry(*scope).or_default();
        for (s, hue, chroma) in entries[representative].axes {
            native_union.add(*s, *hue, *chroma, 1.0)?;
        }
        representatives.push(representative);
    }

    let empty = EvidenceGrid::default();
    let mut representative_values: HashMap<usize, f64> = HashMap::new();
    for representative in &representatives {
        let entry = &entries[*representative];
        let value = representative_evidence(
            unions.get(&entry.scope).unwrap_or(&empty),
            &grids[*representative],
            entry.clip_fraction,
            params,
        )?;
        representative_values.insert(*representative, value);
    }

    let mut values = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let value = if entry.native {
            representative_of
                .get(&index)
                .and_then(|representative| representative_values.get(representative))
                .copied()
                .unwrap_or(0.0)
        } else {
            let gain = coverage_gain(unions.get(&entry.scope).unwrap_or(&empty), &grids[index])?;
            evidence(gain, entry.clip_fraction, params)
        };
        values.push(value);
    }
    Ok(values)
}

/// Evidence of one representative against the union without itself.
fn representative_evidence(
    union: &EvidenceGrid,
    own: &EvidenceGrid,
    clip_fraction: f64,
    params: &TrustParams,
) -> Result<f64> {
    let mut rest = union.clone();
    rest.subtract(own)?;
    Ok(evidence(coverage_gain(&rest, own)?, clip_fraction, params))
}

/// `σ_v` groups mean the same thing on every body in the invocation.
pub fn family_contributor_index(
    pools: &[LookPool],
    scores: &ConsistencyScores,
) -> HashMap<EffectiveContributorId, u32> {
    let mut native: BTreeSet<EffectiveContributorId> = BTreeSet::new();
    let mut recipe: BTreeSet<EffectiveContributorId> = BTreeSet::new();
    for pool in pools {
        for image in &pool.images {
            if !image.is_eligible()
                || scores.effective(&image.image.id, image.outcome)
                    == Some(ImageOutcome::Quarantined)
            {
                continue;
            }
            match image.role {
                EvidenceRole::Native => {
                    native.insert(image.contributor);
                }
                EvidenceRole::Recipe => {
                    recipe.insert(image.contributor);
                }
            }
        }
    }
    let mut ordered: Vec<EffectiveContributorId> = native.iter().copied().collect();
    ordered.extend(
        recipe
            .iter()
            .copied()
            .filter(|entry| !native.contains(entry)),
    );
    ordered
        .into_iter()
        .enumerate()
        .filter_map(|(index, contributor)| {
            u32::try_from(index).ok().map(|index| (contributor, index))
        })
        .collect()
}

pub fn contributor_index(
    pool: &LookPool,
    scores: &ConsistencyScores,
) -> HashMap<EffectiveContributorId, u32> {
    family_contributor_index(std::slice::from_ref(pool), scores)
}

pub fn fit_images(
    pool: &LookPool,
    weighted: &WeightedFamily,
    contributors: &HashMap<EffectiveContributorId, u32>,
) -> Vec<FitImage> {
    weighted
        .fit_entries(pool)
        .into_iter()
        .map(|(index, weight)| {
            let image = &pool.images[index];
            let entry = weighted.get(&image.image.id);
            let mut fit = base_image(pool, index, contributors);
            let shares = entry.map(|entry| region_shares(image, entry));
            fit.trust_weight = weight;
            fit.sample_trust = shares.clone();
            fit.stage_weights.native_look = Some(ImageWeights {
                trust_weight: weight,
                sample_trust: shares,
                duplicates: entry.map_or(0, |entry| entry.duplicates),
            });
            fit.intent = ImageIntent::Native;
            fit
        })
        .collect()
}

/// One image with native-look, native-body and published-body weights attached.
pub fn stage_fit_image(
    pool: &LookPool,
    weighted: &WeightedFamily,
    contributors: &HashMap<EffectiveContributorId, u32>,
    index: usize,
) -> FitImage {
    let image = &pool.images[index];
    let mut fit = base_image(pool, index, contributors);
    if let Some(entry) = weighted.get(&image.image.id) {
        let shares = (image.contribution.intent == ContributionIntent::Native
            && entry.look_weight > 0.0
            && !entry.per_region.is_empty())
        .then(|| region_shares(image, entry));
        fit.stage_weights = StageImageWeights {
            native_look: Some(ImageWeights {
                trust_weight: entry.look_weight,
                sample_trust: shares.clone(),
                duplicates: entry.duplicates,
            }),
            native_body: Some(ImageWeights {
                trust_weight: entry.native_body_weight,
                sample_trust: shares.clone(),
                duplicates: entry.native_body_duplicates,
            }),
            published_body: Some(ImageWeights {
                trust_weight: entry.body_weight,
                sample_trust: shares,
                duplicates: entry.duplicates,
            }),
        };
        fit.trust_weight = entry.body_weight;
        fit.sample_trust = fit
            .stage_weights
            .published_body
            .as_ref()
            .and_then(|weights| weights.sample_trust.clone());
    }
    fit.intent = match image.contribution.intent {
        ContributionIntent::Native => ImageIntent::Native,
        ContributionIntent::Recipe => ImageIntent::Recipe,
        ContributionIntent::Card => unreachable!("card renders carry no fit weight"),
    };
    fit
}

fn base_image(
    pool: &LookPool,
    index: usize,
    contributors: &HashMap<EffectiveContributorId, u32>,
) -> FitImage {
    let image = &pool.images[index];
    // `exposure_hint` is the relative prior mean `x_meta` (docs/02 §2); the
    // header's absolute EV at ISO 100 is not one.
    let mut fit = FitImage::new(image.samples.clone(), image.transform);
    fit.exposure_mode = image.exposure_mode;
    fit.contributor = ContributorId(
        contributors
            .get(&image.contributor)
            .copied()
            .unwrap_or_default(),
    );
    fit.settings = image.settings.clone();
    fit.setting_source = image.setting_source.clone();
    fit
}

fn sample_regions(image: &PoolImage) -> Vec<Option<Region>> {
    image
        .samples
        .in_rgb()
        .iter()
        .zip(&image.admitted)
        .map(|(rgb, admitted)| {
            if !admitted {
                return None;
            }
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
/// the mean over admitted samples is 1.
fn region_shares(image: &PoolImage, entry: &WeightedImage) -> Vec<f32> {
    let total = entry.look_weight;
    let admitted = image.admitted.iter().filter(|admitted| **admitted).count();
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
            (per_region * admitted as f64 / (total * count as f64)) as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(samples: &[(f64, f64, f64)]) -> EvidenceGrid {
        let mut grid = EvidenceGrid::default();
        grid.add_samples(samples.iter().copied())
            .expect("samples in range");
        grid
    }

    #[test]
    fn leave_one_out_matches_rebuilding_the_rest() {
        let params = TrustParams::default();
        let pool: Vec<Vec<(f64, f64, f64)>> = vec![
            vec![(0.0, 10.0, 0.2), (0.5, 20.0, 0.4), (1.0, 30.0, 0.6)],
            vec![(0.5, 20.0, 0.4), (1.5, 40.0, 0.8)],
            vec![(-1.0, 200.0, 0.3), (0.0, 210.0, 0.3), (2.0, 300.0, 0.9)],
        ];
        let grids: Vec<EvidenceGrid> = pool.iter().map(|samples| grid(samples)).collect();
        let union = grid(
            &pool
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<(f64, f64, f64)>>(),
        );
        for (index, own) in grids.iter().enumerate() {
            let rebuilt = grid(
                &pool
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index)
                    .flat_map(|(_, samples)| samples.iter().copied())
                    .collect::<Vec<(f64, f64, f64)>>(),
            );
            let mut rest = union.clone();
            rest.subtract(own).expect("union contains the image");
            assert_eq!(rest, rebuilt);
            assert_eq!(
                representative_evidence(&union, own, 0.2, &params).expect("subtracted evidence"),
                evidence(coverage_gain(&rebuilt, own).expect("gain"), 0.2, &params),
            );
        }
    }

    fn render(film_mode: &str, raw: i64) -> looklab_trust::CaptureTuple {
        crate::meta::capture_tuple(&format!(
            r#"{{"model":"X-T4","iso":160,"look":{{"film_mode":{{"raw":{raw},"value":"{film_mode}"}}}}}}"#
        ))
        .expect("capture tuple")
    }

    #[test]
    fn a_render_in_another_family_does_not_lower_look_evidence() {
        let params = TrustParams::default();
        let contributor = EffectiveContributorId::from(looklab_trust::CredentialId::from_u128(7));
        let axes = vec![(0.0, 10.0, 0.2), (0.5, 20.0, 0.4), (1.0, 30.0, 0.6)];
        let (astia, velvia) = (render("Astia", 288), render("Velvia", 512));
        let entry = |scope, capture| EvidenceEntry {
            scope,
            native: true,
            contributor,
            capture,
            axes: &axes,
            clip_fraction: 0.1,
        };
        let alone = scoped_evidence(&[entry(0, &astia)], &params).expect("standalone");
        let per_family =
            scoped_evidence(&[entry(0, &astia), entry(1, &velvia)], &params).expect("families");
        let pooled =
            scoped_evidence(&[entry(0, &astia), entry(0, &velvia)], &params).expect("pooled");
        assert_eq!(per_family, vec![alone[0], alone[0]]);
        assert!(pooled[0] < alone[0], "{} vs {}", pooled[0], alone[0]);
    }
}
