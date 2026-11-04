// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;

use looklab_fit::{FitOptions, OutlierReason, Verdict, fit};
use looklab_trust::TrustParams;
use looklab_upload::LookSlug;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::body::{DataDir, resolve_look};
use crate::config::WorkerConfig;
use crate::db::{
    self, ContributionIntent, ContributionOutcome, ImageId, ImageOutcome, ImageStatsWrite,
};
use crate::error::Result;
use crate::pool;
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
    if contribution.status == db::ContributionStatus::Withdrawn {
        return Ok(());
    }
    let resources = DataDir::new(&config.data).load(&contribution.body)?;
    let look: LookSlug = resolve_look(
        &resources,
        &contribution.firmware,
        &contribution.sim_key,
        contribution.intent,
        contribution.target_look.as_ref(),
    )?;

    crate::duplicates::run(db_pool).await?;
    let pool = pool::build(db_pool, config, &contribution.body, &look).await?;
    let params = TrustParams::default();
    let weighted = weights::weigh(&pool, &params)?;
    let contributors = weights::contributor_index(&pool);
    let fit_images = weights::fit_images(&pool, &weighted, &contributors);
    if fit_images.is_empty() {
        return Ok(());
    }
    let options = FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        ..FitOptions::default()
    };
    let result = fit(&fit_images, &options)?;

    let entries = weighted.fit_entries(&pool);
    let mut written: HashSet<ImageId> = HashSet::new();
    for ((pool_index, _weight), stats) in entries.iter().zip(&result.image_stats) {
        let image = &pool.images[*pool_index];
        let entry = weighted
            .images
            .iter()
            .find(|entry| entry.pool_index == *pool_index);
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
        db::write_image_stats(
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
        written.insert(image.image.id);
    }

    let contribution_images = db::images(db_pool, contribution_id).await?;
    for image in &contribution_images {
        if written.contains(&image.id) {
            continue;
        }
        let status_reason = if image.duplicate_of.is_some() {
            "honest duplicate".to_owned()
        } else if contribution.intent == ContributionIntent::Recipe {
            "recipe channel".to_owned()
        } else {
            "not in the fitted pool".to_owned()
        };
        db::write_image_stats(
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
