// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeSet, HashMap, HashSet};

use looklab_fit::{FitOptions, OutlierReason, Verdict, score_controlled};
use looklab_schema::SimKey;
use looklab_trust::TrustParams;
use looklab_upload::LookSlug;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::body::{epoch_for, resolve_look};
use crate::config::WorkerConfig;
use crate::db::{
    self, ContributionIntent, ContributionOutcome, ImageId, ImageOutcome, ImageStatsWrite,
};
use crate::error::Result;
use crate::pool;
use crate::roots;
use crate::weights;

#[derive(Serialize)]
struct WeightComponents {
    look: f64,
    body: f64,
    contributor: String,
    duplicates: usize,
}

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution_id: &looklab_server::ContributionId,
) -> Result<()> {
    let contribution = db::contribution(db_pool, contribution_id).await?;
    if contribution.status == db::ContributionStatus::Withdrawn
        || contribution.intent == ContributionIntent::Card
        || contribution.render_class == looklab_upload::RenderClass::Monochrome
    {
        return Ok(());
    }
    let resources = crate::body::data_dir(config)?.load(&contribution.body)?;
    let look: LookSlug = resolve_look(
        &resources,
        &contribution.firmware,
        &contribution.sim_key,
        contribution.intent,
        contribution.target_look.as_ref(),
    )?;
    let epoch = epoch_for(&resources.schema, &contribution.firmware)?;

    crate::duplicates::run(db_pool).await?;
    let pool = pool::build(db_pool, config, &contribution.body, &look).await?;
    let roots = roots::load(db_pool, config, &pool).await?;
    let params = TrustParams::default();
    let weighted = weights::weigh(&pool, &params, &roots.weights)?;
    let contributors = weights::contributor_index(&pool);
    let entries = weighted.fit_entries(&pool);
    let images = weights::fit_images(&pool, &weighted, &contributors);
    let (indices, fit_images): (Vec<usize>, Vec<looklab_fit::FitImage>) = entries
        .iter()
        .map(|(index, _)| *index)
        .zip(images)
        .filter(|(index, _)| pool.images[*index].epoch == epoch.id)
        .unzip();
    if indices.is_empty() {
        return Ok(());
    }
    let sim = SimKey::new(&contribution.sim_key)
        .map_err(|_| crate::error::WorkerError::Data("invalid sim key".to_owned()))?;
    let options = FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        ..FitOptions::default()
    };
    let result = score_controlled(&fit_images, &resources.schema, epoch, &sim, &options)?;

    let excluded: BTreeSet<usize> = result.excluded.iter().map(|image| image.index).collect();
    let mut written: HashSet<ImageId> = HashSet::new();
    for ((index, stats), position) in indices
        .iter()
        .copied()
        .zip(&result.image_stats)
        .zip(0_usize..)
    {
        if excluded.contains(&position) {
            continue;
        }
        let image = &pool.images[index];
        let entry = weighted
            .images
            .iter()
            .find(|entry| entry.pool_index == index);
        let outcome = match stats.verdict {
            Verdict::Accept => ImageOutcome::Accepted,
            Verdict::DownWeight => ImageOutcome::DownWeighted,
            Verdict::Quarantine => ImageOutcome::Quarantined,
        };
        let components = entry.map(|entry| WeightComponents {
            look: entry.look_weight,
            body: entry.body_weight,
            contributor: image.contributor.to_string(),
            duplicates: entry.duplicates,
        });
        let applied = db::write_image_stats(
            db_pool,
            &image.image.id,
            &ImageStatsWrite {
                rms_de: Some(stats.rms_de),
                z: Some(stats.z),
                reason: stats.reason.map(reason_name),
                weight_components: components.as_ref().map(serde_json::to_string).transpose()?,
                status: outcome,
                status_reason: verdict_reason(outcome, stats.reason),
            },
        )
        .await?;
        if !applied {
            return Ok(());
        }
        written.insert(image.image.id);
    }

    let epochs: HashMap<ImageId, u32> = pool
        .images
        .iter()
        .map(|image| (image.image.id, image.epoch))
        .collect();
    let contribution_images = db::images(db_pool, contribution_id).await?;
    for image in &contribution_images {
        if written.contains(&image.id) {
            continue;
        }
        if image.status == Some(ImageOutcome::Quarantined) {
            continue;
        }
        if epochs
            .get(&image.id)
            .is_some_and(|image_epoch| *image_epoch != epoch.id)
        {
            continue;
        }
        let status_reason = if image.duplicate_of.is_some() {
            "honest duplicate".to_owned()
        } else if contribution.intent == ContributionIntent::Recipe {
            "recipe channel".to_owned()
        } else {
            "not in the fitted pool".to_owned()
        };
        let applied = db::write_image_stats(
            db_pool,
            &image.id,
            &ImageStatsWrite {
                rms_de: None,
                z: None,
                reason: None,
                weight_components: None,
                status: ImageOutcome::Accepted,
                status_reason: Some(status_reason),
            },
        )
        .await?;
        if !applied {
            return Ok(());
        }
    }

    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM contribution_images WHERE contribution_id = ?")
            .bind(contribution_id.as_str())
            .fetch_all(db_pool)
            .await?;
    let quarantined = statuses
        .iter()
        .filter(|status| ImageOutcome::parse(status) == Some(ImageOutcome::Quarantined))
        .count();
    let outcome = if !statuses.is_empty() && quarantined == statuses.len() {
        ContributionOutcome::Quarantined
    } else {
        ContributionOutcome::Accepted
    };
    db::set_contribution_status(db_pool, contribution_id, outcome).await?;
    Ok(())
}

fn verdict_reason(outcome: ImageOutcome, reason: Option<OutlierReason>) -> Option<String> {
    let label = match outcome {
        ImageOutcome::Accepted => return None,
        ImageOutcome::DownWeighted => "down-weighted",
        ImageOutcome::Quarantined => "quarantined",
    };
    Some(match reason {
        Some(reason) => format!("{label}: {}", reason_name(reason)),
        None => label.to_owned(),
    })
}

fn reason_name(reason: OutlierReason) -> String {
    match reason {
        OutlierReason::ExposureLike => "exposure-like".to_owned(),
        OutlierReason::HueUniformCast => "hue-uniform cast".to_owned(),
        OutlierReason::ShoulderOnly => "shoulder-only".to_owned(),
        OutlierReason::ContentCorrelated => "content-correlated".to_owned(),
    }
}
