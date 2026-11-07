// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};

use looklab_color::Mat3;
use looklab_fit::ExposureModeId;
use looklab_model::LookInput;
use looklab_model::realization::{ControlValue, Settings};
use looklab_samples::{SampleFlags, SampleSet};
use looklab_schema::{Affects, ControlDefault, ControlKey, ControlSchema, ControlType};
use looklab_trust::{
    CredentialId as TrustCredentialId, EffectiveContributorId, Region, RegionCoverage, SerialLink,
    resolve_effective_contributors,
};
use looklab_upload::{BodyId, LookSlug};
use looklab_upload::{ControlSettings, SettingSource};
use sqlx::SqlitePool;

use crate::body::{BodyResources, BodyTransforms, DataDir, TransformDir, epoch_for, resolve_look};
use crate::config::WorkerConfig;
use crate::db::{self, ContributionRow, CredentialInfo, ImageId, ImageOutcome, ImageRow};
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
    pub epoch: u32,
    pub settings: Settings,
    /// Which controls came from the file and which the contributor entered.
    pub setting_source: BTreeMap<ControlKey, SettingSource>,
    pub exposure_mode: ExposureModeId,
    pub outcome: Option<ImageOutcome>,
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
    pub transforms: BodyTransforms,
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
    let transforms = TransformDir::new(&config.transforms).load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let exclusions = db::look_credential_exclusions(db_pool, look.as_str()).await?;

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
        if &resolved == look && !exclusions.contains(&contribution.credential) {
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
        let epoch_schema = epoch_for(&resources.schema, &contribution.firmware)?;
        let settings = model_settings(&contribution.settings, epoch_schema)?;
        let epoch = epoch_schema.id;
        let setting_source: BTreeMap<ControlKey, SettingSource> = contribution
            .settings
            .entries()
            .iter()
            .map(|entry| (entry.key.clone(), entry.source))
            .collect();
        let exposure_mode = exposure_mode(&resources.schema, epoch, &settings)?;
        for row in rows {
            let path = db::blob_path(&config.blobs, &row.samples_hash);
            let bytes = std::fs::read(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let samples = looklab_samples::decode(&bytes)?;
            let transform = transforms.matrix_for(None, samples.header().wb_multipliers)?;
            let axes = axes_of(&samples, &transform);
            let coverage = coverage_of(&axes)?;
            let clip_fraction = clip_fraction(&samples);
            let capture = meta::capture_tuple(&row.meta_json)?;
            let outcome = row.status;
            images.push(PoolImage {
                image: row,
                contribution: contribution.clone(),
                credential: credential.clone(),
                contributor,
                samples,
                transform,
                epoch,
                settings: settings.clone(),
                setting_source: setting_source.clone(),
                exposure_mode,
                outcome,
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
        transforms,
        images,
    })
}

fn model_settings(wire: &ControlSettings, epoch: &looklab_schema::Epoch) -> Result<Settings> {
    let mut settings = Settings::new();
    for entry in wire.entries() {
        let control = epoch
            .controls
            .iter()
            .find(|control| control.key == entry.key)
            .ok_or_else(|| {
                WorkerError::Data(format!(
                    "control {} is not in epoch {}",
                    entry.key, epoch.id
                ))
            })?;
        let step = entry.step.get();
        let value = match control.kind {
            ControlType::Int | ControlType::Decimal => ControlValue::Step(step),
            ControlType::Enum => {
                let value = usize::try_from(step)
                    .ok()
                    .and_then(|index| control.values.as_ref()?.get(index))
                    .ok_or_else(|| {
                        WorkerError::Data(format!(
                            "control {} has no value at step {step}",
                            entry.key
                        ))
                    })?;
                ControlValue::Enum(value.clone())
            }
            ControlType::Bool => match step {
                0 => ControlValue::Bool(false),
                1 => ControlValue::Bool(true),
                _ => {
                    return Err(WorkerError::Data(format!(
                        "control {} has no boolean value at step {step}",
                        entry.key
                    )));
                }
            },
        };
        settings.set(entry.key.clone(), value);
    }
    Ok(settings)
}

/// The exposure mode a settings map selects: the first enum control whose value
/// names an exposure mode key; mode 0 when none does (docs/03 §1).
fn exposure_mode(
    schema: &ControlSchema,
    epoch: u32,
    settings: &Settings,
) -> Result<ExposureModeId> {
    let epoch = schema
        .epochs
        .iter()
        .find(|candidate| candidate.id == epoch)
        .ok_or_else(|| WorkerError::Data(format!("body schema has no epoch {epoch}")))?;
    for control in &epoch.controls {
        if control.kind != ControlType::Enum || !control.affects.contains(&Affects::Exposure) {
            continue;
        }
        let selected = match settings.get(&control.key) {
            Some(ControlValue::Enum(value)) => Some(value),
            None => match &control.default {
                ControlDefault::Enum(value) => Some(value),
                _ => None,
            },
            _ => None,
        };
        let Some(selected) = selected else {
            continue;
        };
        if let Some(index) = epoch
            .exposure_modes
            .iter()
            .position(|mode| mode.key.as_str() == selected.as_str())
        {
            return u32::try_from(index)
                .map(ExposureModeId)
                .map_err(|_| WorkerError::Data("too many exposure modes".to_owned()));
        }
    }
    Ok(ExposureModeId(0))
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
