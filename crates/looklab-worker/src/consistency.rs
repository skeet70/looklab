// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeSet;

use looklab_fit::{FitOptions, OutlierReason, Verdict, score_controlled};
use looklab_schema::{
    ContributorRef, ImageRef, ModelSubject, ModelVersionId, QualificationCapability,
    RealizationRevisionId, ScopeKey,
};
use looklab_trust::TrustParams;
use sqlx::SqlitePool;

use crate::assessment::PinnedModes;
use crate::config::WorkerConfig;
use crate::db::{self, ContributionIntent, ImageId, ImageOutcome};
use crate::error::Result;
use crate::pool::{self, EvidenceScope};
use crate::roots;
use crate::weights;
use looklab_server::model_store::{
    self, DiagnosticImageStats, DiagnosticVerdict, DiagnosticWeightComponents, NumericalSources,
    QualifiedImageOutcome, QualifiedImageStats, QualifiedImageWrite,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsistencyOutcome {
    /// Scores were written to the qualified image statistics.
    Qualified,
    /// Scores were written to the subject's diagnostic store.
    Experimental,
    /// The selected qualification changed while scoring; nothing was written.
    Superseded,
    /// The contribution has no scorable evidence in the requested scope.
    Skipped,
}

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    contribution_id: &looklab_server::ContributionId,
    scope: &ScopeKey,
    job: &crate::jobs::JobId,
    attempts: i64,
) -> Result<ConsistencyOutcome> {
    let contribution = db::contribution(db_pool, contribution_id).await?;
    if contribution.status == db::ContributionStatus::Withdrawn
        || contribution.intent == ContributionIntent::Card
        || contribution.render_class == looklab_upload::RenderClass::Monochrome
    {
        return Ok(ConsistencyOutcome::Skipped);
    }
    let data = crate::body::published_data_dir(db_pool, config).await?;
    let resources = data.load(&contribution.body)?;
    let Some((realization, look)) = pool::realization_scope(&resources, &contribution)? else {
        return Ok(ConsistencyOutcome::Skipped);
    };
    let subject = ModelSubject::BodyRealization {
        realization: realization.clone(),
    }
    .key();

    let mut conn = db_pool.acquire().await?;
    let selection = model_store::qualified_selection(
        &mut conn,
        &subject,
        QualificationCapability::NativePrediction,
        scope,
    )
    .await?;
    drop(conn);

    let now = crate::clock::now_unix();
    let mut qualified_write = None;
    if let Some(selection) = &selection {
        let Some(candidate) = selection.projection.selected_candidate.clone() else {
            return Ok(ConsistencyOutcome::Superseded);
        };
        let Some(report_id) = selection.projection.selected_report else {
            return Ok(ConsistencyOutcome::Superseded);
        };
        let server_job = looklab_server::JobId::try_from(job.as_str())
            .map_err(|error| crate::error::WorkerError::Data(error.to_string()))?;
        let mut conn = db_pool.acquire().await?;
        let mut dependencies =
            model_store::dependencies(&mut conn, "report", &report_id.to_hex()).await?;
        if let ModelVersionId::BodyRealization { realization } = &candidate {
            let revision = RealizationRevisionId::for_version(realization);
            dependencies.extend(
                model_store::dependencies(&mut conn, "realization_revision", revision.as_str())
                    .await?,
            );
        }
        let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies).await?;
        drop(conn);
        qualified_write = Some((candidate, report_id, server_job, snapshot));
    }

    let pinned = match &selection {
        Some(selection) => {
            let Some(candidate) = &selection.projection.selected_candidate else {
                return Ok(ConsistencyOutcome::Skipped);
            };
            let mut conn = db_pool.acquire().await?;
            match crate::model_fit::pinned_modes_for_candidate(&mut conn, config, candidate).await?
            {
                Some(pinned) => pinned,
                None => return Ok(ConsistencyOutcome::Skipped),
            }
        }
        None => PinnedModes::default(),
    };
    let (evidence_scope, qualified) = match &selection {
        Some(selection) => (
            EvidenceScope::Qualified {
                report: selection.report.scope.clone(),
                mask: selection.report.supported_mask.clone(),
                pinned,
            },
            true,
        ),
        None => (EvidenceScope::Experimental, false),
    };

    let scores = if qualified {
        let candidate = selection
            .as_ref()
            .and_then(|selection| selection.projection.selected_candidate.clone());
        match candidate {
            Some(candidate) => {
                let mut conn = db_pool.acquire().await?;
                let stats =
                    model_store::qualified_image_stats(&mut conn, &subject, &candidate, scope)
                        .await?;
                drop(conn);
                let mut scores = weights::ConsistencyScores::new();
                for stat in stats {
                    let Some(image) = ImageId::parse(stat.image.as_str()) else {
                        continue;
                    };
                    scores.insert(
                        image,
                        match stat.verdict {
                            DiagnosticVerdict::Accepted => ImageOutcome::Accepted,
                            DiagnosticVerdict::DownWeighted => ImageOutcome::DownWeighted,
                            DiagnosticVerdict::Quarantined => ImageOutcome::Quarantined,
                        },
                    );
                }
                scores
            }
            None => weights::ConsistencyScores::new(),
        }
    } else {
        weights::ConsistencyScores::new()
    };

    crate::duplicates::run(db_pool).await?;
    let mut pool = pool::build(
        db_pool,
        config,
        &contribution.body,
        &look,
        &realization,
        &evidence_scope,
    )
    .await?;
    pool::resolve_contributors(std::slice::from_mut(&mut pool))?;
    let roots = roots::load(&pool, &scores)?;
    let params = TrustParams::default();
    let weighted = weights::weigh(&pool, &params, &roots.weights, &scores)?;
    let contributors = weights::contributor_index(&pool, &scores);
    let entries = weighted.fit_entries(&pool);
    let images = weights::fit_images(&pool, &weighted, &contributors);
    let (indices, fit_images): (Vec<usize>, Vec<looklab_fit::FitImage>) =
        entries.iter().map(|(index, _)| *index).zip(images).unzip();
    if indices.is_empty() {
        return Ok(ConsistencyOutcome::Skipped);
    }
    let epoch = resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == realization.epoch.get())
        .ok_or_else(|| {
            crate::error::WorkerError::Data(format!(
                "epoch {} is not in the schema of {}",
                realization.epoch.get(),
                contribution.body
            ))
        })?;
    let options = FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        ..FitOptions::default()
    };
    let result = score_controlled(
        &fit_images,
        &resources.schema,
        epoch,
        &realization.native_sim,
        &options,
    )?;

    let excluded: BTreeSet<usize> = result.excluded.iter().map(|image| image.index).collect();
    if qualified {
        let mut conn = db_pool.acquire().await?;
        let current = model_store::qualified_selection(
            &mut conn,
            &subject,
            QualificationCapability::NativePrediction,
            scope,
        )
        .await?;
        drop(conn);
        let Some(current) = current else {
            return Ok(ConsistencyOutcome::Superseded);
        };
        if !current.projection.applicability.is_current()
            || current.report != selection.as_ref().expect("qualified selection").report
        {
            return Ok(ConsistencyOutcome::Superseded);
        }
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
            let entry = weighted.get(&image.image.id);
            let Some((candidate, report_id, server_job, snapshot)) = qualified_write.as_ref()
            else {
                return Ok(ConsistencyOutcome::Superseded);
            };
            let qualified_stats = QualifiedImageStats {
                image: ImageRef::new(image.image.id.as_str())
                    .map_err(|error| crate::error::WorkerError::Data(error.to_string()))?,
                rms_de: stats.rms_de,
                z: stats.z,
                verdict: match stats.verdict {
                    Verdict::Accept => DiagnosticVerdict::Accepted,
                    Verdict::DownWeight => DiagnosticVerdict::DownWeighted,
                    Verdict::Quarantine => DiagnosticVerdict::Quarantined,
                },
                reason: stats.reason.map(reason_name),
                weight_components: DiagnosticWeightComponents {
                    look: entry.map(|entry| entry.look_weight).unwrap_or(0.0),
                    body: entry.map(|entry| entry.body_weight).unwrap_or(0.0),
                    contributor: ContributorRef::new(image.contributor.to_string())
                        .map_err(|error| crate::error::WorkerError::Data(error.to_string()))?,
                },
            };
            let sources = NumericalSources {
                fitter: selection
                    .as_ref()
                    .expect("qualified selection")
                    .report
                    .provenance
                    .fitter
                    .clone(),
                evaluator: selection
                    .as_ref()
                    .expect("qualified selection")
                    .report
                    .provenance
                    .evaluator
                    .clone(),
                model: selection
                    .as_ref()
                    .expect("qualified selection")
                    .report
                    .provenance
                    .model
                    .clone(),
            };
            let write = QualifiedImageWrite {
                subject: &subject,
                sources: &sources,
                candidate,
                scope_key: scope,
                report: report_id,
                stats: &qualified_stats,
                lease: model_store::JobLease {
                    job: server_job,
                    attempts,
                },
                snapshot,
            };
            if let QualifiedImageOutcome::Rejected(_) =
                model_store::write_qualified_image_stats_fenced(db_pool, &write, now).await?
            {
                return Ok(ConsistencyOutcome::Superseded);
            }
        }
        return Ok(ConsistencyOutcome::Qualified);
    }

    let mut stats = Vec::new();
    for ((index, image_stats), position) in indices
        .iter()
        .copied()
        .zip(&result.image_stats)
        .zip(0_usize..)
    {
        if excluded.contains(&position) {
            continue;
        }
        let image = &pool.images[index];
        let entry = weighted.get(&image.image.id);
        stats.push(DiagnosticImageStats {
            image: ImageRef::new(image.image.id.as_str())
                .map_err(|error| crate::error::WorkerError::Data(error.to_string()))?,
            rms_de: image_stats.rms_de,
            z: image_stats.z,
            verdict: match image_stats.verdict {
                Verdict::Accept => DiagnosticVerdict::Accepted,
                Verdict::DownWeight => DiagnosticVerdict::DownWeighted,
                Verdict::Quarantine => DiagnosticVerdict::Quarantined,
            },
            reason: image_stats.reason.map(reason_name),
            weight_components: DiagnosticWeightComponents {
                look: entry.map(|entry| entry.look_weight).unwrap_or(0.0),
                body: entry.map(|entry| entry.body_weight).unwrap_or(0.0),
                contributor: ContributorRef::new(image.contributor.to_string())
                    .map_err(|error| crate::error::WorkerError::Data(error.to_string()))?,
            },
        });
    }
    let mut tx = model_store::begin_immediate(db_pool).await?;
    // Rows that predate the model store reach scoring before any refit registered them.
    model_store::register_subject(&mut tx, &ModelSubject::BodyRealization { realization }, now)
        .await?;
    model_store::clear_diagnostic_image_stats(&mut tx, &subject).await?;
    for stat in &stats {
        model_store::write_diagnostic_image_stats(&mut tx, &subject, stat, now).await?;
    }
    tx.commit().await?;
    Ok(ConsistencyOutcome::Experimental)
}

fn reason_name(reason: OutlierReason) -> String {
    match reason {
        OutlierReason::ExposureLike => "exposure-like".to_owned(),
        OutlierReason::HueUniformCast => "hue-uniform cast".to_owned(),
        OutlierReason::ShoulderOnly => "shoulder-only".to_owned(),
        OutlierReason::ContentCorrelated => "content-correlated".to_owned(),
    }
}
