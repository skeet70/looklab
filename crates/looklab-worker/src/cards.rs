// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-firmware verification of card renders against the Look version their
//! render tag names ([docs/04 §8.4]).

use std::collections::{BTreeMap, HashMap, HashSet};

use looklab_color::ucs::delta_e_ucs;
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_export::card::read_card;
use looklab_meta::{FirmwareId, FirmwareVersion, RenderLookSlug, RenderLookVersion};
use looklab_model::solve::ucs_of_xyz;
use looklab_model::{Look, LookDocument};
use looklab_publish::CARD_ARTIFACT;
use looklab_samples::{OutputSpace, SampleSet};
use looklab_upload::{BodyId, CardDeclaration, RenderTag};
use sqlx::{Row, SqlitePool};

use crate::body::TransformDir;
use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db::{self, ContributionOutcome, ImageOutcome, ImageStatsWrite, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::jobs::{self, JobPayload};
use crate::refit::LOOK_ARTIFACT;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct VerdictKey {
    firmware: FirmwareId,
    firmware_version: FirmwareVersion,
    look: RenderLookSlug,
    look_version: RenderLookVersion,
}

pub async fn run(db_pool: &SqlitePool, config: &WorkerConfig) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, card, body_id FROM contributions
         WHERE intent = 'card' AND status = 'accepted' AND render_class = 'colour' ORDER BY id",
    )
    .fetch_all(db_pool)
    .await?;

    let mut transforms: HashMap<BodyId, crate::body::BodyTransforms> = HashMap::new();
    let mut unavailable: HashSet<BodyId> = HashSet::new();
    let mut looks: HashMap<(String, String), Look> = HashMap::new();
    let mut groups: BTreeMap<VerdictKey, Vec<f64>> = BTreeMap::new();

    for row in &rows {
        let contribution: String = row.get("id");
        let body_text: String = row.get("body_id");
        let body = BodyId::new(&body_text)
            .map_err(|_| WorkerError::Data(format!("invalid body id `{body_text}`")))?;
        let card: Option<String> = row.get("card");
        let declaration = card
            .as_deref()
            .map(serde_json::from_str::<CardDeclaration>)
            .transpose()?;
        if unavailable.contains(&body) {
            continue;
        }
        if !transforms.contains_key(&body) {
            match TransformDir::new(&config.transforms).load_optional(&body)? {
                Some(loaded) => {
                    transforms.insert(body.clone(), loaded);
                }
                None => {
                    tracing::warn!(%body, "skipping card verification: no input transform");
                    unavailable.insert(body.clone());
                    continue;
                }
            }
        }
        let images = sqlx::query(
            "SELECT samples_hash, meta FROM contribution_images
             WHERE contribution_id = ? AND status != 'quarantined' ORDER BY id",
        )
        .bind(&contribution)
        .fetch_all(db_pool)
        .await?;
        for image in &images {
            let tag = match &declaration {
                Some(CardDeclaration::Tagged(tag)) => Some(tag.clone()),
                Some(CardDeclaration::UserDeclared { .. }) | None => per_image_tag(image)?,
            };
            let Some(tag) = tag else {
                continue;
            };
            let Some(look) = published_look(db_pool, config, &tag, &mut looks).await? else {
                continue;
            };
            let hash: Vec<u8> = image.get("samples_hash");
            let hash: [u8; 32] = hash
                .try_into()
                .map_err(|_| WorkerError::Data("samples hash is not 32 bytes".to_owned()))?;
            let path = db::blob_path(&config.blobs, &hash);
            let bytes = std::fs::read(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let samples = looklab_samples::decode(&bytes)?;
            let transform = transforms
                .get(&body)
                .ok_or_else(|| WorkerError::Data("missing transform directory".to_owned()))?
                .matrix_for(None, samples.header().wb_multipliers)?;
            let Some(mean) = mean_delta_e(&look, &samples, &transform)? else {
                continue;
            };
            groups
                .entry(VerdictKey {
                    firmware: tag.firmware_id,
                    firmware_version: tag.firmware_version,
                    look: tag.look_slug,
                    look_version: tag.look_version,
                })
                .or_default()
                .push(mean);
        }
    }

    sqlx::query("DELETE FROM card_verifications")
        .execute(db_pool)
        .await?;
    let now = now_unix();
    for (key, mut values) in groups {
        values.sort_by(f64::total_cmp);
        let images = values.len();
        sqlx::query(
            "INSERT INTO card_verifications (
                 firmware_id, firmware_version, look_slug, look_version,
                 images, median_de, p95_de, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key.firmware.as_str())
        .bind(key.firmware_version.as_str())
        .bind(key.look.as_str())
        .bind(key.look_version.as_str())
        .bind(i64::try_from(images).unwrap_or(i64::MAX))
        .bind(median(&values))
        .bind(percentile(&values, 0.95))
        .bind(now)
        .execute(db_pool)
        .await?;
    }
    Ok(())
}

/// Validates a card contribution without fitting it (docs/07 §4), records the
/// outcomes and queues verification.
pub async fn validate(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution_id: &looklab_server::ContributionId,
) -> Result<()> {
    let contribution = db::contribution(db_pool, contribution_id).await?;
    if contribution.status == db::ContributionStatus::Withdrawn
        || contribution.render_class == looklab_upload::RenderClass::Monochrome
    {
        return Ok(());
    }
    let declaration = card_declaration(db_pool, contribution_id).await?;
    let verdict = match &declaration {
        Some(CardDeclaration::Tagged(tag)) => tagged_verdict(db_pool, config, tag).await?,
        Some(CardDeclaration::UserDeclared { .. }) => Ok("user-declared firmware".to_owned()),
        None => Err("no card declaration".to_owned()),
    };
    let (outcome, reason) = match verdict {
        Ok(reason) => (ImageOutcome::Accepted, reason),
        Err(reason) => (ImageOutcome::Quarantined, reason),
    };
    let images = db::images(db_pool, contribution_id).await?;
    for image in &images {
        if image.status == Some(ImageOutcome::Quarantined) {
            continue;
        }
        let applied = db::write_image_stats(
            db_pool,
            &image.id,
            &ImageStatsWrite {
                rms_de: None,
                z: None,
                reason: None,
                weight_components: None,
                status: outcome,
                status_reason: Some(reason.clone()),
            },
        )
        .await?;
        if !applied {
            return Ok(());
        }
    }
    let accepted = outcome == ImageOutcome::Accepted;
    let status = if accepted {
        ContributionOutcome::Accepted
    } else {
        ContributionOutcome::Quarantined
    };
    if !db::set_contribution_status(db_pool, contribution_id, status).await? {
        return Ok(());
    }
    if accepted {
        jobs::enqueue_if_idle(db_pool, &JobPayload::CardVerification, now_unix()).await?;
    }
    Ok(())
}

async fn card_declaration(
    db_pool: &SqlitePool,
    contribution_id: &looklab_server::ContributionId,
) -> Result<Option<CardDeclaration>> {
    let card: Option<String> = sqlx::query_scalar("SELECT card FROM contributions WHERE id = ?")
        .bind(contribution_id.as_str())
        .fetch_one(db_pool)
        .await?;
    card.as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(Into::into)
}

/// `Ok(reason)` accepts the batch; `Err(reason)` rejects it with that reason.
async fn tagged_verdict(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    tag: &RenderTag,
) -> Result<std::result::Result<String, String>> {
    let Some(major) = tag
        .look_version
        .as_str()
        .split('.')
        .next()
        .and_then(|text| text.parse::<u32>().ok())
    else {
        return Ok(Err("look version is not numeric".to_owned()));
    };
    let look = tag.look_slug.as_str();
    let versions = db::look_versions(db_pool, look).await?;
    let Some(version) = versions.iter().find(|version| version.version == major) else {
        return Ok(Err(format!("{look} {major} does not exist")));
    };
    if !matches!(
        version.status,
        LookVersionStatus::Published | LookVersionStatus::Rooted
    ) {
        return Ok(Err(format!("{look} {major} is not published")));
    }
    let Some(artifacts) = db::manifest_artifacts(db_pool, look, major).await? else {
        return Ok(Err(format!("{look} {major} has no manifest")));
    };
    let Some(artifact) = artifacts
        .iter()
        .find(|artifact| artifact.name == CARD_ARTIFACT)
    else {
        return Ok(Err(format!("{look} {major} has no card")));
    };
    let path = config.blobs.join(&artifact.sha256);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return Ok(Err(format!("card {} unreadable: {error}", artifact.sha256)));
        }
    };
    let card = match read_card(&bytes) {
        Ok(card) => card,
        Err(error) => return Ok(Err(format!("card does not parse: {error}"))),
    };
    if card.cid_prefix() != tag.cid_prefix {
        return Ok(Err(format!(
            "card content id {} does not match the tag",
            card.content_id_hex()
        )));
    }
    Ok(Ok(format!(
        "card render of {look} {}",
        tag.look_version.as_str()
    )))
}

fn per_image_tag(image: &sqlx::sqlite::SqliteRow) -> Result<Option<RenderTag>> {
    let meta: String = image.get("meta");
    let meta: looklab_upload::ManifestMetadata = serde_json::from_str(&meta)?;
    Ok(meta.render_tag)
}

async fn published_look(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    tag: &RenderTag,
    cache: &mut HashMap<(String, String), Look>,
) -> Result<Option<Look>> {
    let key = (
        tag.look_slug.as_str().to_owned(),
        tag.look_version.as_str().to_owned(),
    );
    if let Some(look) = cache.get(&key) {
        return Ok(Some(look.clone()));
    }
    let Some(major) = tag
        .look_version
        .as_str()
        .split('.')
        .next()
        .and_then(|text| text.parse::<u32>().ok())
    else {
        return Ok(None);
    };
    let Some(artifacts) = db::manifest_artifacts(db_pool, tag.look_slug.as_str(), major).await?
    else {
        return Ok(None);
    };
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == LOOK_ARTIFACT)
        .ok_or_else(|| WorkerError::Data("manifest has no look artifact".to_owned()))?;
    let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
    let look = LookDocument::decode(&bytes)?.look;
    cache.insert(key, look.clone());
    Ok(Some(look))
}

/// Mean CAM16-UCS ΔE of the render against the Look.
fn mean_delta_e(look: &Look, samples: &SampleSet, transform: &Mat3) -> Result<Option<f64>> {
    let space = match samples.header().output_space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let mut total = 0.0;
    let mut count = 0usize;
    for index in 0..samples.n() {
        let xyz = transform.mul_vec(samples.in_rgb()[index].map(f64::from));
        if !xyz.iter().all(|value| value.is_finite()) {
            continue;
        }
        let expected = look.eval(Xyz::from_array(xyz));
        let out = samples.out_rgb()[index].map(f64::from);
        let observed = ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::new(out[0], out[1], out[2])));
        let delta = delta_e_ucs(expected, observed);
        if !delta.is_finite() {
            continue;
        }
        total += delta;
        count += 1;
    }
    Ok((count > 0).then(|| total / count as f64))
}

fn median(values: &[f64]) -> f64 {
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

/// Nearest-rank percentile over sorted `values`.
fn percentile(values: &[f64], fraction: f64) -> f64 {
    let rank = ((fraction * values.len() as f64).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[rank]
}
