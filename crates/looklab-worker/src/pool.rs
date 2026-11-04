// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};

use looklab_color::Mat3;
use looklab_model::LookInput;
use looklab_samples::{SampleFlags, SampleSet};
use looklab_trust::{
    CredentialId as TrustCredentialId, EffectiveContributorId, Region, RegionCoverage, SerialLink,
    resolve_effective_contributors,
};
use looklab_upload::{BodyId, LookSlug};
use sqlx::SqlitePool;

use crate::body::{BodyResources, DataDir, TransformDir, resolve_look};
use crate::config::WorkerConfig;
use crate::db::{self, ContributionRow, CredentialInfo, ImageId, ImageRow};
use crate::error::{Result, WorkerError};
use crate::meta;

const EXPOSURE_MIN: f64 = looklab_trust::EXPOSURE_MIN;
const EXPOSURE_MAX: f64 = looklab_trust::EXPOSURE_MAX;

#[derive(Debug, Clone)]
pub struct PoolImage {
    pub image: ImageRow,
    pub contribution: ContributionRow,
    pub credential: CredentialInfo,
    pub contributor: EffectiveContributorId,
    pub samples: SampleSet,
    pub transform: Mat3,
    pub capture: looklab_trust::CaptureTuple,
    pub coverage: RegionCoverage,
    /// `(s, hue, √C)` per sample.
    pub axes: Vec<(f64, f64, f64)>,
    pub clip_fraction: f64,
}

#[derive(Debug)]
pub struct LookPool {
    pub body: BodyId,
    pub look: LookSlug,
    pub resources: BodyResources,
    pub images: Vec<PoolImage>,
}

impl LookPool {
    pub fn image_index(&self, id: &ImageId) -> Option<usize> {
        self.images.iter().position(|image| &image.image.id == id)
    }

    pub fn contribution_ids(&self) -> Vec<looklab_server::ContributionId> {
        let mut ids: Vec<_> = self
            .images
            .iter()
            .map(|image| image.contribution.id.clone())
            .collect();
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        ids.dedup();
        ids
    }
}

pub async fn build(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
) -> Result<LookPool> {
    let resources = DataDir::new(&config.data).load(body)?;
    let transform = TransformDir::new(&config.transforms).load(body)?;
    let rows = db::body_pool(db_pool, body).await?;

    let mut contributions = Vec::new();
    for (contribution, images) in rows {
        if contribution.body != *body {
            continue;
        }
        let resolved = resolve_look(
            &resources,
            &contribution.firmware,
            &contribution.sim_key,
            contribution.intent,
            contribution.target_look.as_ref(),
        )?;
        if &resolved == look {
            contributions.push((contribution, images));
        }
    }

    let mut credentials: HashMap<looklab_server::CredentialId, CredentialInfo> = HashMap::new();
    for (contribution, _) in &contributions {
        if !credentials.contains_key(&contribution.credential) {
            let info = db::credential(db_pool, &contribution.credential).await?;
            credentials.insert(contribution.credential.clone(), info);
        }
    }

    let links: Vec<SerialLink> = db::serial_links(db_pool)
        .await?
        .into_iter()
        .map(|(credential, hmac)| {
            let credential =
                looklab_server::admin::trust_id(&credential).map_err(WorkerError::Admin)?;
            Ok(SerialLink {
                credential,
                hmac: looklab_meta::SerialHmac::from_bytes(hmac),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let trust_credentials: Vec<TrustCredentialId> =
        credentials.values().map(|info| info.trust).collect();
    let effective = resolve_effective_contributors(trust_credentials, &links);

    let mut images = Vec::new();
    for (contribution, rows) in contributions {
        let credential = credentials
            .get(&contribution.credential)
            .cloned()
            .ok_or_else(|| WorkerError::Data("missing credential row".to_owned()))?;
        let contributor = effective
            .get(&credential.trust)
            .copied()
            .unwrap_or_else(|| EffectiveContributorId::from(credential.trust));
        for row in rows {
            let path = db::blob_path(&config.blobs, &row.samples_hash);
            let bytes = std::fs::read(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let samples = looklab_samples::decode(&bytes)?;
            let axes = axes_of(&samples, &transform);
            let coverage = coverage_of(&axes)?;
            let clip_fraction = clip_fraction(&samples);
            let capture = meta::capture_tuple(&row.meta_json)?;
            images.push(PoolImage {
                image: row,
                contribution: contribution.clone(),
                credential: credential.clone(),
                contributor,
                samples,
                transform,
                capture,
                coverage,
                axes,
                clip_fraction,
            });
        }
    }
    images.sort_by_key(|left| left.image.id.as_str());

    Ok(LookPool {
        body: body.clone(),
        look: look.clone(),
        resources,
        images,
    })
}

pub fn axes_of(samples: &SampleSet, transform: &Mat3) -> Vec<(f64, f64, f64)> {
    samples
        .in_rgb()
        .iter()
        .filter_map(|rgb| {
            let xyz = transform.mul_vec(rgb.map(f64::from));
            if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
                return None;
            }
            let input = LookInput::new(looklab_color::Xyz::from_array(xyz));
            Some((
                input.s.clamp(EXPOSURE_MIN, EXPOSURE_MAX),
                input.h_in,
                input.c_in,
            ))
        })
        .collect()
}

fn coverage_of(axes: &[(f64, f64, f64)]) -> Result<RegionCoverage> {
    let mut counts: BTreeMap<Region, f64> = BTreeMap::new();
    for (s, hue, _chroma) in axes {
        let region = Region::from_probe(*s, *hue)?;
        *counts.entry(region).or_insert(0.0) += 1.0;
    }
    Ok(RegionCoverage::from_shares(counts)?)
}

fn clip_fraction(samples: &SampleSet) -> f64 {
    if samples.n() == 0 {
        return 0.0;
    }
    let clipped = samples
        .flags()
        .iter()
        .filter(|flags| flags.contains(SampleFlags::NEAR_CLIP_OUTPUT))
        .count();
    clipped as f64 / samples.n() as f64
}

pub async fn looks_for_body(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
) -> Result<Vec<LookSlug>> {
    let resources = DataDir::new(&config.data).load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let mut looks = Vec::new();
    for (contribution, images) in rows {
        if images.is_empty() || contribution.status == db::ContributionStatus::Withdrawn {
            continue;
        }
        let look = resolve_look(
            &resources,
            &contribution.firmware,
            &contribution.sim_key,
            contribution.intent,
            contribution.target_look.as_ref(),
        )?;
        looks.push(look);
    }
    looks.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    looks.dedup();
    Ok(looks)
}
