// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap, HashSet};

use looklab_color::ucs::delta_e_ucs;
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_model::solve::ucs_of_xyz;
use looklab_model::{Look, LookDocument, LookInput};
use looklab_samples::OutputSpace;
use looklab_trust::{
    BountyStatus, BountyTarget, BountyVerification, EpochOrdinal, JacobianDirections,
    ModelVersions, RecipeKey, RecipePrediction, Region, ResidualObservation, Tier, TrustParams,
    VerifiedRecipe, VersionId,
};
use looklab_upload::{BodyId, LookSlug, SimKey};
use sqlx::{Row, SqlitePool};

use crate::body::epoch_for;
use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db::{self, ContributionIntent, ContributionRow};
use crate::error::{Result, WorkerError};
use crate::pool::{self, PoolImage};
use crate::refit::LOOK_ARTIFACT;

const SIGMA_FLOOR: f64 = 1e-3;
/// The upload path records no match prediction; the recipe baseline is the
/// target itself.
const RECIPE_BASELINE_DE: f64 = 0.0;

pub async fn run(db_pool: &SqlitePool, config: &WorkerConfig) -> Result<()> {
    let params = TrustParams::default();
    prune_stale(db_pool).await?;

    let mut scores = Vec::new();
    for (body, look) in targets(db_pool).await? {
        scores.extend(analyze_target(db_pool, config, &params, &body, &look).await?);
    }
    write_scores(db_pool, scores).await?;
    Ok(())
}

async fn targets(db_pool: &SqlitePool) -> Result<Vec<(BodyId, LookSlug)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT body_id, target_look_id FROM contributions
         WHERE intent = 'recipe' AND status = 'accepted' AND target_look_id IS NOT NULL
           AND render_class = 'colour'
         ORDER BY body_id, target_look_id",
    )
    .fetch_all(db_pool)
    .await?;
    rows.into_iter()
        .map(|(body, look)| {
            Ok((
                BodyId::new(&body)
                    .map_err(|_| WorkerError::Data(format!("invalid body id `{body}`")))?,
                LookSlug::new(&look)
                    .map_err(|_| WorkerError::Data(format!("invalid look slug `{look}`")))?,
            ))
        })
        .collect()
}

async fn prune_stale(db_pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "DELETE FROM recipe_observations
         WHERE contribution_id IN (
             SELECT id FROM contributions
             WHERE status IN ('withdrawn', 'quarantined') OR render_class = 'monochrome'
         ) OR image_id IN (
             SELECT id FROM contribution_images WHERE status = 'quarantined'
         )",
    )
    .execute(db_pool)
    .await?;
    Ok(())
}

async fn analyze_target(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    params: &TrustParams,
    body: &BodyId,
    look: &LookSlug,
) -> Result<Vec<(RecipeKey, RecipePrediction, Vec<VerifiedRecipe>)>> {
    let Some((version, target_look)) = latest_look(db_pool, config, look).await? else {
        return Ok(Vec::new());
    };
    let pool = pool::build(db_pool, config, body, look).await?;
    let mut observations = Vec::new();
    let mut groups: HashMap<RecipeKey, (RecipePrediction, Vec<VerifiedRecipe>)> = HashMap::new();
    let mut root_admissions: HashMap<looklab_server::ContributionId, bool> = HashMap::new();
    let now = now_unix();

    for image in &pool.images {
        if image.contribution.intent != ContributionIntent::Recipe
            || image.contribution.target_look.as_ref() != Some(look)
        {
            continue;
        }
        let Some(measured) = measure_recipe(&target_look, image)? else {
            continue;
        };
        let key = recipe_key(&pool.resources.schema, look, image)?;
        let prediction = RecipePrediction {
            delta_e: RECIPE_BASELINE_DE,
            sigma: params.bounty_image_sigma,
            versions: ModelVersions {
                input_transform: 0,
                control_response: 0,
                look: VersionId::new(version),
            },
        };
        let entry = groups.entry(key).or_insert((prediction, Vec::new()));
        entry.1.push(VerifiedRecipe {
            contributor: image.contributor,
            measured_de: measured.mean,
            sigma: measured.sigma,
            weight: 1.0,
        });

        let root = match root_admissions.get(&image.contribution.id) {
            Some(value) => *value,
            None => {
                let admitted = root_admitted(db_pool, &image.contribution.id).await?;
                root_admissions.insert(image.contribution.id.clone(), admitted);
                admitted
            }
        };
        let epoch = epoch_for(&pool.resources.schema, &image.contribution.firmware)?.id;
        for (region, region_measured) in measured.regions {
            let observation = ResidualObservation {
                contributor: image.contributor,
                region,
                residual_de: region_measured.mean,
                sigma: region_measured.sigma,
                reachable: true,
                jacobian: JacobianDirections {
                    input_transform: 0.0,
                    control_model: 0.0,
                    look: 0.0,
                },
            };
            upsert_observation(
                db_pool,
                &image.contribution,
                image,
                epoch,
                region,
                &region_measured,
                root,
                now,
            )
            .await?;
            observations.push(observation);
        }
    }

    let residuals = looklab_trust::analyze_residuals(params, &observations)?;
    write_residuals(db_pool, look, body, &residuals).await?;
    write_bounties(db_pool, params, look, body, version, &residuals).await?;
    Ok(groups
        .into_iter()
        .map(|(key, (prediction, verifications))| (key, prediction, verifications))
        .collect())
}

struct Measured {
    mean: f64,
    sigma: f64,
    regions: BTreeMap<Region, RegionMeasured>,
}

struct RegionMeasured {
    mean: f64,
    sigma: f64,
}

fn measure_recipe(look: &Look, image: &PoolImage) -> Result<Option<Measured>> {
    let count = image.samples.n();
    if count == 0 {
        return Ok(None);
    }
    let space = match image.samples.header().output_space {
        OutputSpace::Srgb => RgbSpace::Srgb,
        OutputSpace::AdobeRgb => RgbSpace::AdobeRgb,
    };
    let mut total = 0.0;
    let mut total_squared = 0.0;
    let mut total_count = 0_usize;
    let mut regions: BTreeMap<Region, (f64, f64, usize)> = BTreeMap::new();
    for index in 0..count {
        let in_rgb = image.samples.in_rgb()[index].map(f64::from);
        let xyz = image.transform.mul_vec(in_rgb);
        if !xyz.iter().all(|value| value.is_finite()) {
            continue;
        }
        let input = LookInput::new(Xyz::from_array(xyz));
        let expected = look.eval(Xyz::from_array(xyz));
        let out = image.samples.out_rgb()[index].map(f64::from);
        let observed = ucs_of_xyz(space.rgb_linear_to_xyz(Rgb::new(out[0], out[1], out[2])));
        let delta = delta_e_ucs(expected, observed);
        if !delta.is_finite() {
            continue;
        }
        let region = Region::from_probe(
            input
                .s
                .clamp(looklab_trust::EXPOSURE_MIN, looklab_trust::EXPOSURE_MAX),
            input.h_in,
        )?;
        total += delta;
        total_squared += delta * delta;
        total_count += 1;
        let entry = regions.entry(region).or_insert((0.0, 0.0, 0));
        entry.0 += delta;
        entry.1 += delta * delta;
        entry.2 += 1;
    }
    if total_count == 0 {
        return Ok(None);
    }
    let mean = total / total_count as f64;
    let sigma = standard_error(total, total_squared, total_count);
    let regions = regions
        .into_iter()
        .map(|(region, (sum, squared, count))| {
            (
                region,
                RegionMeasured {
                    mean: sum / count as f64,
                    sigma: standard_error(sum, squared, count),
                },
            )
        })
        .collect();
    Ok(Some(Measured {
        mean,
        sigma,
        regions,
    }))
}

fn standard_error(sum: f64, squared: f64, count: usize) -> f64 {
    let n = count as f64;
    let mean = sum / n;
    let variance = if count > 1 {
        ((squared - n * mean * mean) / (n - 1.0)).max(0.0)
    } else {
        mean * mean
    };
    (variance / n).sqrt().max(SIGMA_FLOOR)
}

fn recipe_key(
    schema: &looklab_schema::ControlSchema,
    look: &LookSlug,
    image: &PoolImage,
) -> Result<RecipeKey> {
    Ok(RecipeKey {
        body: image.contribution.body.clone(),
        epoch: EpochOrdinal::new(epoch_for(schema, &image.contribution.firmware)?.id),
        base_sim: SimKey::new(&image.contribution.sim_key)
            .map_err(|_| WorkerError::Data("invalid sim key".to_owned()))?,
        settings: image.contribution.settings.clone(),
        target_look: look.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn upsert_observation(
    db_pool: &SqlitePool,
    contribution: &ContributionRow,
    image: &PoolImage,
    epoch: u32,
    region: Region,
    measured: &RegionMeasured,
    root: bool,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recipe_observations (
             id, contribution_id, image_id, body_id, epoch, base_sim, settings,
             target_look, region, contributor_id, tier, root, residual_de, sigma,
             measured_de, input_transform_version, control_response_version,
             look_version, created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 0, ?)
         ON CONFLICT (image_id, region) DO UPDATE SET
             body_id = excluded.body_id,
             epoch = excluded.epoch,
             base_sim = excluded.base_sim,
             settings = excluded.settings,
             target_look = excluded.target_look,
             contributor_id = excluded.contributor_id,
             tier = excluded.tier,
             root = excluded.root,
             residual_de = excluded.residual_de,
             sigma = excluded.sigma,
             measured_de = excluded.measured_de",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(contribution.id.as_str())
    .bind(image.image.id.as_str())
    .bind(contribution.body.as_str())
    .bind(i64::from(epoch))
    .bind(&contribution.sim_key)
    .bind(contribution.settings_json()?)
    .bind(
        contribution
            .target_look
            .as_ref()
            .map(LookSlug::as_str)
            .unwrap_or_default(),
    )
    .bind(region.to_string())
    .bind(image.contributor.to_string())
    .bind(tier_name(image.credential.tier))
    .bind(i64::from(u8::from(root)))
    .bind(measured.mean)
    .bind(measured.sigma)
    .bind(measured.mean)
    .bind(now)
    .execute(db_pool)
    .await?;
    Ok(())
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::New => "new",
        Tier::Endorsed => "endorsed",
        Tier::Seeder => "seeder",
    }
}

async fn root_admitted(
    db_pool: &SqlitePool,
    contribution: &looklab_server::ContributionId,
) -> Result<bool> {
    let admitted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM root_admissions WHERE contribution_id = ? AND revoked_at IS NULL",
    )
    .bind(contribution.as_str())
    .fetch_one(db_pool)
    .await?;
    Ok(admitted > 0)
}

async fn latest_look(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
) -> Result<Option<(u32, Look)>> {
    let versions = db::look_versions(db_pool, look.as_str()).await?;
    let Some(latest) = versions
        .iter()
        .filter(|version| {
            matches!(
                version.status,
                db::LookVersionStatus::Published | db::LookVersionStatus::Rooted
            )
        })
        .max_by_key(|version| version.version)
    else {
        return Ok(None);
    };
    let Some(artifacts) = db::manifest_artifacts(db_pool, look.as_str(), latest.version).await?
    else {
        return Ok(None);
    };
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == LOOK_ARTIFACT)
        .ok_or_else(|| WorkerError::Data("manifest has no look artifact".to_owned()))?;
    let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
    let document = LookDocument::decode(&bytes)?;
    Ok(Some((latest.version, document.look)))
}

async fn write_residuals(
    db_pool: &SqlitePool,
    look: &LookSlug,
    body: &BodyId,
    residuals: &[looklab_trust::RegionResidual],
) -> Result<()> {
    sqlx::query("DELETE FROM recipe_residuals WHERE target_look = ? AND body_id = ?")
        .bind(look.as_str())
        .bind(body.as_str())
        .execute(db_pool)
        .await?;
    let now = now_unix();
    for residual in residuals {
        sqlx::query(
            "INSERT INTO recipe_residuals (
                 id, target_look, body_id, region, contributors, mean_residual, sigma,
                 reachable, persistent, attribution, alignment, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(look.as_str())
        .bind(body.as_str())
        .bind(residual.region.to_string())
        .bind(i64::try_from(residual.contributors).unwrap_or(i64::MAX))
        .bind(residual.mean_residual)
        .bind(residual.sigma)
        .bind(i64::from(u8::from(residual.reachable)))
        .bind(i64::from(u8::from(residual.persistent)))
        .bind(serde_json::to_string(&residual.attribution)?)
        .bind(serde_json::to_string(&residual.alignment)?)
        .bind(now)
        .execute(db_pool)
        .await?;
    }
    Ok(())
}

async fn write_bounties(
    db_pool: &SqlitePool,
    params: &TrustParams,
    look: &LookSlug,
    body: &BodyId,
    version: u32,
    residuals: &[looklab_trust::RegionResidual],
) -> Result<()> {
    let mut resolved = resolved_regions(db_pool, params, look, body).await?;
    let previous: Vec<String> = sqlx::query_scalar(
        "SELECT region FROM bounties
         WHERE target_look = ? AND body_id = ? AND status = 'resolved'",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .fetch_all(db_pool)
    .await?;
    for text in previous {
        if let Ok(region) = text.parse() {
            resolved.insert(region);
        }
    }
    let now = now_unix();
    for region in &resolved {
        sqlx::query(
            "UPDATE bounties SET status = 'resolved', resolved_at = ?
             WHERE target_look = ? AND body_id = ? AND region = ? AND status = 'open'",
        )
        .bind(now)
        .bind(look.as_str())
        .bind(body.as_str())
        .bind(region.to_string())
        .execute(db_pool)
        .await?;
    }
    let uncertainties: Vec<_> = looklab_trust::region_uncertainties(residuals)
        .into_iter()
        .filter(|region| !resolved.contains(&region.region))
        .collect();
    let target = BountyTarget {
        look: look.clone(),
        look_version: VersionId::new(version),
        body: Some(body.clone()),
    };
    let bounties =
        looklab_trust::rank_bounties(params, &target, &uncertainties, params.bounty_top_n)?;
    sqlx::query("DELETE FROM bounties WHERE target_look = ? AND body_id = ? AND status = 'open'")
        .bind(look.as_str())
        .bind(body.as_str())
        .execute(db_pool)
        .await?;
    for bounty in bounties {
        sqlx::query(
            "INSERT INTO bounties (
                 id, target_look, look_version, body_id, region,
                 expected_variance_reduction, reason, status, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 'open', ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(bounty.look.as_str())
        .bind(i64::from(bounty.look_version.get()))
        .bind(bounty.body.as_ref().map(BodyId::as_str))
        .bind(bounty.region.to_string())
        .bind(bounty.expected_variance_reduction)
        .bind(serde_json::to_string(&bounty.reason)?)
        .bind(now)
        .execute(db_pool)
        .await?;
    }
    Ok(())
}

async fn resolved_regions(
    db_pool: &SqlitePool,
    params: &TrustParams,
    look: &LookSlug,
    body: &BodyId,
) -> Result<HashSet<Region>> {
    let rows = sqlx::query(
        "SELECT region, sigma, tier, root, contributor_id FROM recipe_observations
         WHERE target_look = ? AND body_id = ?",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .fetch_all(db_pool)
    .await?;
    let mut by_region: BTreeMap<Region, (f64, Vec<BountyVerification>)> = BTreeMap::new();
    for row in &rows {
        let region_text: String = row.get("region");
        let Ok(region) = region_text.parse() else {
            continue;
        };
        let sigma: f64 = row.get("sigma");
        if !sigma.is_finite() || sigma <= 0.0 {
            continue;
        }
        let tier_text: String = row.get("tier");
        let Some(tier) = parse_tier(&tier_text) else {
            continue;
        };
        let root: i64 = row.get("root");
        let contributor_text: String = row.get("contributor_id");
        let contributor: looklab_trust::CredentialId = contributor_text.parse()?;
        let entry = by_region.entry(region).or_insert((0.0, Vec::new()));
        entry.0 += 1.0 / (sigma * sigma);
        entry.1.push(BountyVerification {
            contributor: contributor.into(),
            tier,
            root: root != 0,
            sigma,
        });
    }
    let mut resolved = HashSet::new();
    for (region, (precision, verifications)) in by_region {
        let current_sigma = (1.0 / precision).sqrt();
        if looklab_trust::resolution_status(params, current_sigma, &verifications)?
            == BountyStatus::Resolved
        {
            resolved.insert(region);
        }
    }
    Ok(resolved)
}

fn parse_tier(value: &str) -> Option<Tier> {
    match value {
        "new" => Some(Tier::New),
        "endorsed" => Some(Tier::Endorsed),
        "seeder" => Some(Tier::Seeder),
        _ => None,
    }
}

async fn write_scores(
    db_pool: &SqlitePool,
    groups: Vec<(RecipeKey, RecipePrediction, Vec<VerifiedRecipe>)>,
) -> Result<()> {
    let mut scores = Vec::with_capacity(groups.len());
    for (key, prediction, verifications) in groups {
        scores.push(looklab_trust::score_recipe(
            key,
            prediction,
            &verifications,
        )?);
    }
    sqlx::query("DELETE FROM recipe_scores")
        .execute(db_pool)
        .await?;
    let now = now_unix();
    for score in &scores {
        sqlx::query(
            "INSERT INTO recipe_scores (
                 id, body_id, epoch, base_sim, settings, target_look, predicted_de,
                 predicted_sigma, measured_de, measured_sigma, n_verifications,
                 input_transform_version, control_response_version, look_version, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(score.recipe.body.as_str())
        .bind(i64::from(score.recipe.epoch.get()))
        .bind(score.recipe.base_sim.as_str())
        .bind(serde_json::to_string(&score.recipe.settings)?)
        .bind(score.recipe.target_look.as_str())
        .bind(score.predicted_de)
        .bind(score.predicted_sigma)
        .bind(score.measured_de)
        .bind(score.measured_sigma)
        .bind(i64::try_from(score.n_verifications).unwrap_or(i64::MAX))
        .bind(i64::from(score.model_versions.input_transform))
        .bind(i64::from(score.model_versions.control_response))
        .bind(i64::from(score.model_versions.look.get()))
        .bind(now)
        .execute(db_pool)
        .await?;
    }
    Ok(())
}
