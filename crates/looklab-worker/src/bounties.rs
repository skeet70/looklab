// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use looklab_color::Xyz;
use looklab_color::ucs::delta_e_ucs;
use looklab_match::{ControlCovariance, MatchModel, realized_trace};
use looklab_model::{Look, LookDocument, LookInput};
use looklab_schema::{
    ArtifactRole, BodyRealizationVersionId, DependencyGeneration, DependencyRef, LookId,
    ModelReport, ModelReportId, ModelSubject, ModelVersionId, ProjectionUse,
    QualificationCapability, SelectionScope, SubjectKey, TargetBasis, TargetEligibilityStatus,
};
use looklab_server::model_store;
use looklab_trust::{
    AssessmentStatus, BountyStatus, BountyTarget, BountyVerification, EpochOrdinal, ModelVersions,
    OutputContext, OutputPin, Reachability, RecipeKey, RecipePrediction, RecipeSource, Region,
    ResidualObservation, SceneCell, ScoringMetric, Tier, TrustParams, VerifiedRecipe, VersionId,
};
use looklab_upload::{BodyId, LookSlug, SimKey};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::assessment::{PinnedModes, matching_scope_covers};
use crate::body::{TransformDir, epoch_for};
use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db::{self, ContributionIntent};
use crate::error::{Result, WorkerError};
use crate::model_assessment::artifacts::{
    LoadedRealization, load_realization, output_encoding, read_binding,
};
use crate::model_assessment::evidence::cell_for_scene;
use crate::model_assessment::predict::{observed_ucs, output_space};
use crate::pool::{self, EvidenceStage, LookPool, PhysicalContext, PoolImage};
use crate::refit::LOOK_ARTIFACT;

const SIGMA_FLOOR: f64 = 1e-3;

pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &crate::jobs::JobOwnership,
) -> Result<()> {
    let params = TrustParams::default();
    prune_stale(db_pool, ownership).await?;

    for (body, look) in targets(db_pool).await? {
        analyze_target(db_pool, config, &params, ownership, &body, &look).await?;
    }
    Ok(())
}

async fn targets(db_pool: &SqlitePool) -> Result<Vec<(BodyId, LookSlug)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT body_id, target_look_id FROM contributions
         WHERE intent = 'recipe' AND status IN ('pending', 'accepted')
           AND target_look_id IS NOT NULL AND render_class = 'colour'
           AND deferred_until IS NULL
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

async fn prune_stale(
    db_pool: &SqlitePool,
    ownership: &crate::jobs::JobOwnership,
) -> Result<Vec<(BodyId, LookSlug)>> {
    let mut tx = model_store::begin_immediate(db_pool).await?;
    if !lease_owned(&mut tx, ownership).await? {
        return Ok(Vec::new());
    }
    let sources = crate::source_identity::sources().storage();
    if !model_store::numerical_sources_current(&mut tx, &sources)
        .await
        .map_err(|error| WorkerError::Data(error.to_string()))?
    {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT body_id, target_look FROM recipe_observations
         WHERE contribution_id IN (
             SELECT id FROM contributions
             WHERE status IN ('withdrawn', 'quarantined') OR render_class = 'monochrome'
                OR deferred_until IS NOT NULL
         ) OR image_id IN (
             SELECT id FROM contribution_images WHERE status = 'quarantined'
         )",
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM recipe_observations
         WHERE contribution_id IN (
             SELECT id FROM contributions
             WHERE status IN ('withdrawn', 'quarantined') OR render_class = 'monochrome'
                OR deferred_until IS NOT NULL
         ) OR image_id IN (
             SELECT id FROM contribution_images WHERE status = 'quarantined'
         )",
    )
    .execute(&mut *tx)
    .await?;
    let mut pruned = Vec::with_capacity(rows.len());
    for (body, look) in rows {
        let body = BodyId::new(&body)
            .map_err(|_| WorkerError::Data(format!("invalid body id `{body}`")))?;
        let look = LookSlug::new(&look)
            .map_err(|_| WorkerError::Data(format!("invalid look slug `{look}`")))?;
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM contributions
             WHERE body_id = ? AND target_look_id = ? AND intent = 'recipe'
               AND status IN ('pending', 'accepted') AND render_class = 'colour'
               AND deferred_until IS NULL",
        )
        .bind(body.as_str())
        .bind(look.as_str())
        .fetch_one(&mut *tx)
        .await?;
        if remaining == 0 {
            for table in ["recipe_observations", "recipe_residuals"] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE body_id = ? AND target_look = ?"
                ))
                .bind(body.as_str())
                .bind(look.as_str())
                .execute(&mut *tx)
                .await?;
            }
            for table in ["bounties", "recipe_scores"] {
                sqlx::query(&format!(
                    "DELETE FROM {table} WHERE body_id = ? AND target_look = ? AND metric = ?"
                ))
                .bind(body.as_str())
                .bind(look.as_str())
                .bind(looklab_trust::scoring_metric_name(
                    ScoringMetric::RenderedPostClipV1,
                ))
                .execute(&mut *tx)
                .await?;
            }
        }
        pruned.push((body, look));
    }
    tx.commit().await?;
    Ok(pruned)
}

type RecipeGroupKey = (RecipeKey, RecipeSource, String);

type RecipeGroupValue = (RecipePrediction, Vec<VerifiedRecipe>, Vec<PredictionSample>);

async fn analyze_target(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    params: &TrustParams,
    ownership: &crate::jobs::JobOwnership,
    body: &BodyId,
    look: &LookSlug,
) -> Result<()> {
    let Some(target) = selected_target(db_pool, config, look).await? else {
        return Ok(());
    };
    let version = target.version;
    if TransformDir::new(&config.transforms)
        .load_optional(body)?
        .is_none()
    {
        tracing::warn!(%body, "skipping bounty analysis: no input transform");
        return Ok(());
    }
    let mut pools = Vec::new();
    for realization in pool::realizations_for_body(db_pool, config, body).await? {
        for family in pool::looks_for_realization(db_pool, config, body, &realization).await? {
            pools.push(
                pool::build(
                    db_pool,
                    config,
                    body,
                    &family,
                    &realization,
                    &pool::EvidenceScope::Experimental,
                )
                .await?,
            );
        }
    }
    pools.retain(|pool| {
        pool.images.iter().any(|image| {
            image.contribution.intent == ContributionIntent::Recipe
                && image.contribution.target_look.as_ref() == Some(look)
        })
    });
    pool::resolve_contributors(&mut pools)?;
    let mut observations = Vec::new();
    let mut writes = Vec::new();
    let mut groups: HashMap<RecipeGroupKey, RecipeGroupValue> = HashMap::new();
    let mut root_admissions: HashMap<looklab_server::ContributionId, bool> = HashMap::new();
    let mut dependencies = Vec::new();
    let mut sources = Vec::new();

    for pool in &pools {
        let Some(mut set) = source_set(db_pool, pool).await? else {
            tracing::warn!(
                %body,
                realization = %pool.realization.native_sim,
                "skipping recipe analysis: no selected source realization candidates"
            );
            continue;
        };
        dependencies.extend(pool.observation.dependencies.iter().cloned());
        for image in &pool.images {
            if image.contribution.intent != ContributionIntent::Recipe
                || image.contribution.target_look.as_ref() != Some(look)
                || !image.is_eligible()
            {
                continue;
            }
            let Some(context) = image.physical.as_ref() else {
                continue;
            };
            let Some(source) =
                select_source(db_pool, config, &mut set, pool, image, context).await?
            else {
                continue;
            };
            sources.push(source.pin.clone());
            let source_binding = RecipeSource {
                realization: source.loaded.revision.id.clone(),
                body_state: source.loaded.revision.body_state_version,
                report: source.pin.report,
                scope: match source.pin.use_ {
                    ProjectionUse::QualifiedMatch => SelectionScope::Qualified,
                    _ => SelectionScope::Experimental,
                },
            };
            let effective = image
                .transform
                .mul(&source.loaded.transform.delta().transform());
            let model = match source
                .loaded
                .model
                .match_model_with_transform(source.covariance.clone(), effective)
            {
                Ok(model) => model.with_modes(
                    Some(context.wb_mode.clone()),
                    Some(context.firmware.clone()),
                ),
                Err(error) => {
                    tracing::warn!(
                        image = %image.image.id.as_str(),
                        %error,
                        "skipping recipe image: the source model is not usable"
                    );
                    continue;
                }
            };
            if source.loaded.release.output_space
                != output_space(image.samples.header().output_space)
            {
                continue;
            }
            let Some(measured) = measure_recipe(&target.document.look, &target.pin, &model, image)?
            else {
                continue;
            };
            let key = (
                recipe_key(&pool.resources.schema, look, image)?,
                source_binding.clone(),
                measured.output.canonical(),
            );
            let prediction = RecipePrediction {
                metric: measured.metric,
                output: Some(measured.output),
                delta_e: measured.predicted_mean,
                sigma: measured.predicted_sigma,
                versions: ModelVersions {
                    input_transform: source.loaded.revision.input_transform_version,
                    control_response: source.loaded.revision.control_response_version,
                    look: VersionId::new(version),
                },
                source: Some(source_binding.clone()),
            };
            let contributor = pool
                .observation
                .contributor(EvidenceStage::Published, image.credential.trust);
            let entry = groups
                .entry(key)
                .or_insert((prediction, Vec::new(), Vec::new()));
            entry.1.push(VerifiedRecipe {
                contributor,
                measured_de: measured.measured_mean,
                raw_gap: measured.raw_gap_mean,
                signed_residual: Some(measured.signed_mean),
                sigma: measured.measured_sigma,
                weight: 1.0,
            });
            entry.2.push(PredictionSample {
                contributor,
                predicted: measured.predicted_mean,
                sigma: measured.predicted_sigma,
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
            for (cell, region_measured) in measured.regions {
                observations.push(ResidualObservation {
                    contributor,
                    cell,
                    colour: None,
                    settings: image.contribution.settings.clone(),
                    output: Some(measured.output),
                    raw_gap: region_measured.raw_gap_mean,
                    signed_residual: Some(region_measured.signed_mean),
                    prediction_residual: Some(region_measured.prediction_mean),
                    observation_sigma: Some(region_measured.observation_sigma),
                    joint_covariance: None,
                    assessment: AssessmentStatus::Unassessed,
                    reachable: Reachability::Unassessed,
                    attribution: None,
                });
                writes.push(ObservationWrite {
                    contribution_id: image.contribution.id.clone(),
                    body_id: image.contribution.body.clone(),
                    sim_key: SimKey::new(&image.contribution.sim_key)
                        .map_err(|_| WorkerError::Data("invalid sim key".to_owned()))?,
                    settings: image.contribution.settings.clone(),
                    target_look: image.contribution.target_look.clone().ok_or_else(|| {
                        WorkerError::Data("recipe contribution has no target look".to_owned())
                    })?,
                    image_id: image.image.id,
                    epoch,
                    tier: image.credential.tier,
                    cell,
                    metric: measured.metric,
                    output: measured.output,
                    measured: region_measured,
                    contributor,
                    root,
                    source: source_binding.clone(),
                    input_transform: source.loaded.revision.input_transform_version,
                    control_response: source.loaded.revision.control_response_version,
                    look_version: version,
                });
            }
        }
    }

    let residuals = looklab_trust::analyze_residuals(params, &observations, &[])?;
    let mut scores = Vec::with_capacity(groups.len());
    for ((key, _, _), (mut prediction, verifications, samples)) in groups {
        aggregate_prediction(&mut prediction, &samples);
        scores.push((key, prediction, verifications));
    }
    commit_target(
        db_pool,
        ownership,
        params,
        look,
        body,
        version,
        &writes,
        &residuals,
        scores,
        &dependencies,
        &sources,
    )
    .await
}

struct PredictionSample {
    contributor: looklab_trust::EffectiveContributorId,
    predicted: f64,
    sigma: Option<f64>,
    weight: f64,
}

fn aggregate_prediction(prediction: &mut RecipePrediction, samples: &[PredictionSample]) {
    struct Contributor {
        weight: f64,
        weighted: f64,
        sigma: Option<f64>,
        count: usize,
    }
    let mut contributors: BTreeMap<looklab_trust::EffectiveContributorId, Contributor> =
        BTreeMap::new();
    for sample in samples {
        let entry = contributors
            .entry(sample.contributor)
            .or_insert(Contributor {
                weight: 0.0,
                weighted: 0.0,
                sigma: Some(0.0),
                count: 0,
            });
        entry.weight += sample.weight;
        entry.weighted += sample.weight * sample.predicted;
        entry.sigma = match (entry.sigma, sample.sigma) {
            (Some(total), Some(value)) => Some(total + value),
            _ => None,
        };
        entry.count += 1;
    }
    let mut weight_total = 0.0;
    let mut weighted = 0.0;
    let mut sigma_weighted = 0.0;
    let mut all_sigma = true;
    for contributor in contributors.values() {
        weight_total += contributor.weight;
        weighted += contributor.weighted;
        match contributor.sigma {
            Some(total) => sigma_weighted += contributor.weight * total / contributor.count as f64,
            None => all_sigma = false,
        }
    }
    if weight_total > 0.0 {
        prediction.delta_e = weighted / weight_total;
        prediction.sigma = all_sigma.then(|| sigma_weighted / weight_total);
    }
}

#[derive(Clone)]
struct SourcePin {
    subject: SubjectKey,
    use_: ProjectionUse,
    candidate: ModelVersionId,
    report: Option<ModelReportId>,
}

struct ObservationWrite {
    contribution_id: looklab_server::ContributionId,
    body_id: BodyId,
    sim_key: SimKey,
    settings: looklab_upload::ControlSettings,
    target_look: LookSlug,
    image_id: db::ImageId,
    epoch: u32,
    tier: Tier,
    cell: SceneCell,
    metric: ScoringMetric,
    output: OutputContext,
    measured: RegionMeasured,
    contributor: looklab_trust::EffectiveContributorId,
    root: bool,
    source: RecipeSource,
    input_transform: u32,
    control_response: u32,
    look_version: u32,
}

#[allow(clippy::too_many_arguments)]
async fn commit_target(
    db_pool: &SqlitePool,
    ownership: &crate::jobs::JobOwnership,
    params: &TrustParams,
    look: &LookSlug,
    body: &BodyId,
    version: u32,
    writes: &[ObservationWrite],
    residuals: &[looklab_trust::RegionResidual],
    scores: Vec<(RecipeKey, RecipePrediction, Vec<VerifiedRecipe>)>,
    dependencies: &[DependencyGeneration],
    sources: &[SourcePin],
) -> Result<()> {
    let now = now_unix();
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let server_job = ownership.server_id();
    let lease = model_store::JobLease {
        job: &server_job,
        attempts: ownership.attempts,
    };
    let sources_identity = crate::source_identity::sources().storage();
    let dependency_refs: Vec<DependencyRef> = dependencies
        .iter()
        .map(|generation| generation.dependency.clone())
        .collect();
    if !model_store::execution_current(
        &mut tx,
        lease,
        &sources_identity,
        &dependency_refs,
        dependencies,
    )
    .await?
    {
        return Ok(());
    }
    if selected_version_conn(&mut tx, look).await? != Some(version) {
        return Ok(());
    }
    for source in sources {
        if !source_pin_current(&mut tx, source).await? {
            return Ok(());
        }
    }
    for write in writes {
        upsert_observation(&mut tx, write, now).await?;
    }
    write_residuals(&mut tx, look, body, residuals, now).await?;
    write_bounties(&mut tx, params, look, body, version, residuals, now).await?;
    write_scores(&mut tx, scores, look, body, now).await?;
    tx.commit().await?;
    Ok(())
}

async fn lease_owned(
    conn: &mut SqliteConnection,
    ownership: &crate::jobs::JobOwnership,
) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(ownership.id.as_str())
    .bind(ownership.attempts)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count == 1)
}

async fn source_pin_current(conn: &mut SqliteConnection, pin: &SourcePin) -> Result<bool> {
    let projections = model_store::current_projections(conn, &pin.subject, pin.use_).await?;
    Ok(projections.iter().any(|projection| {
        projection.applicability.is_current()
            && projection.selected_candidate.as_ref() == Some(&pin.candidate)
            && projection.selected_report == pin.report
    }))
}

struct Measured {
    metric: ScoringMetric,
    output: OutputContext,
    predicted_mean: f64,
    predicted_sigma: Option<f64>,
    measured_mean: f64,
    measured_sigma: f64,
    raw_gap_mean: f64,
    signed_mean: [f64; 3],
    regions: BTreeMap<SceneCell, RegionMeasured>,
}

struct RegionMeasured {
    predicted_mean: f64,
    predicted_sigma: Option<f64>,
    measured_mean: f64,
    raw_gap_mean: f64,
    observation_sigma: f64,
    signed_mean: [f64; 3],
    prediction_mean: [f64; 3],
}

#[derive(Default)]
struct RegionAccum {
    predicted_sum: f64,
    measured_sum: f64,
    raw_gap_sum: f64,
    signed_sum: [f64; 3],
    signed_squared: [f64; 3],
    prediction_sum: [f64; 3],
    count: usize,
}

fn measure_recipe(
    target: &Look,
    target_pin: &OutputPin,
    model: &MatchModel,
    image: &PoolImage,
) -> Result<Option<Measured>> {
    let count = image.samples.n();
    if count == 0 {
        return Ok(None);
    }
    let space = output_space(image.samples.header().output_space);
    let realization = match model.realize(&image.settings) {
        Ok(realization) => realization,
        Err(error) => {
            tracing::warn!(
                image = %image.image.id.as_str(),
                %error,
                "skipping recipe image: the source realization cannot realize its settings"
            );
            return Ok(None);
        }
    };
    let metric = ScoringMetric::RenderedPostClipV1;
    let output = OutputContext {
        target: *target_pin,
        camera: model.output_pin(&realization),
    };
    let mut predicted_total = 0.0;
    let mut measured_total = 0.0;
    let mut measured_squared = 0.0;
    let mut raw_gap_total = 0.0;
    let mut signed_total = [0.0; 3];
    let mut signed_squared = [0.0; 3];
    let mut prediction_total = [0.0; 3];
    let mut total_count = 0_usize;
    let mut band_total = 0.0;
    let mut band_count = 0_usize;
    let stride = (count / 32).max(1);
    let mut regions: BTreeMap<SceneCell, RegionAccum> = BTreeMap::new();
    for index in 0..count {
        if !image.admitted[index] {
            continue;
        }
        let rgb = image.samples.in_rgb()[index].map(f64::from);
        let scene = model.scene_from_camera(rgb);
        let input = LookInput::new(scene);
        let expected = target_pin.render(target.eval(scene));
        let predicted = model.scored_ucs(metric, &realization, scene);
        let observed = observed_ucs(&image.samples, index, space);
        let predicted_de = delta_e_ucs(expected, predicted);
        let measured_de = delta_e_ucs(expected, observed);
        let raw_gap = measured_de - predicted_de;
        if !predicted_de.is_finite() || !measured_de.is_finite() || !raw_gap.is_finite() {
            continue;
        }
        let target_ucs = expected.to_array();
        let predicted_ucs = predicted.to_array();
        let observed_values = observed.to_array();
        let prediction_residual = [
            predicted_ucs[0] - target_ucs[0],
            predicted_ucs[1] - target_ucs[1],
            predicted_ucs[2] - target_ucs[2],
        ];
        let signed_residual = [
            observed_values[0] - predicted_ucs[0],
            observed_values[1] - predicted_ucs[1],
            observed_values[2] - predicted_ucs[2],
        ];
        let cell = SceneCell::from_input(input.s, input.c_in, input.h_in);
        predicted_total += predicted_de;
        measured_total += measured_de;
        measured_squared += measured_de * measured_de;
        raw_gap_total += raw_gap;
        total_count += 1;
        for axis in 0..3 {
            signed_total[axis] += signed_residual[axis];
            signed_squared[axis] += signed_residual[axis] * signed_residual[axis];
            prediction_total[axis] += prediction_residual[axis];
        }
        if index % stride == 0
            && let Ok(Some(trace)) =
                realized_trace(model, &realization, &image.settings, metric, scene)
            && trace.is_finite()
        {
            band_total += trace.max(0.0).sqrt();
            band_count += 1;
        }
        let entry = regions.entry(cell).or_default();
        entry.predicted_sum += predicted_de;
        entry.measured_sum += measured_de;
        entry.raw_gap_sum += raw_gap;
        for axis in 0..3 {
            entry.signed_sum[axis] += signed_residual[axis];
            entry.signed_squared[axis] += signed_residual[axis] * signed_residual[axis];
            entry.prediction_sum[axis] += prediction_residual[axis];
        }
        entry.count += 1;
    }
    if total_count == 0 {
        return Ok(None);
    }
    let predicted_sigma = if band_count > 0 && model.covariance.is_some() {
        Some(band_total / band_count as f64)
    } else {
        None
    };
    let regions = regions
        .into_iter()
        .map(|(cell, accum)| {
            let n = accum.count as f64;
            (
                cell,
                RegionMeasured {
                    predicted_mean: accum.predicted_sum / n,
                    predicted_sigma,
                    measured_mean: accum.measured_sum / n,
                    raw_gap_mean: accum.raw_gap_sum / n,
                    observation_sigma: vector_scatter(
                        accum.signed_sum,
                        accum.signed_squared,
                        accum.count,
                    ),
                    signed_mean: accum.signed_sum.map(|value| value / n),
                    prediction_mean: accum.prediction_sum.map(|value| value / n),
                },
            )
        })
        .collect();
    Ok(Some(Measured {
        metric,
        output,
        predicted_mean: predicted_total / total_count as f64,
        predicted_sigma,
        measured_mean: measured_total / total_count as f64,
        measured_sigma: scatter(measured_total, measured_squared, total_count),
        raw_gap_mean: raw_gap_total / total_count as f64,
        signed_mean: signed_total.map(|value| value / total_count as f64),
        regions,
    }))
}

fn scatter(sum: f64, squared: f64, count: usize) -> f64 {
    let n = count as f64;
    let mean = sum / n;
    let variance = if count > 1 {
        ((squared - n * mean * mean) / (n - 1.0)).max(0.0)
    } else {
        0.0
    };
    variance.sqrt().max(SIGMA_FLOOR)
}

fn vector_scatter(sum: [f64; 3], squared: [f64; 3], count: usize) -> f64 {
    let n = count as f64;
    let mut variance = 0.0;
    for axis in 0..3 {
        let mean = sum[axis] / n;
        let component = if count > 1 {
            ((squared[axis] - n * mean * mean) / (n - 1.0)).max(0.0)
        } else {
            0.0
        };
        variance += component;
    }
    variance.sqrt().max(SIGMA_FLOOR)
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

async fn upsert_observation(
    conn: &mut SqliteConnection,
    write: &ObservationWrite,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recipe_observations (
             id, contribution_id, image_id, body_id, epoch, base_sim, settings,
             target_look, region, metric, target_encoding, target_clip, output_encoding,
             realization_clip, contributor_id, tier, root, residual_de, raw_gap, sigma,
             observation_sigma, measured_de, predicted_de, predicted_sigma,
             signed_residual, prediction_residual, assessment, systematic_sigma,
             input_transform_version, control_response_version, look_version,
             source_realization, source_body_state, source_report, source_scope,
             created_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (image_id, region, metric) DO UPDATE SET
             body_id = excluded.body_id,
             epoch = excluded.epoch,
             base_sim = excluded.base_sim,
             settings = excluded.settings,
             target_look = excluded.target_look,
             target_encoding = excluded.target_encoding,
             target_clip = excluded.target_clip,
             output_encoding = excluded.output_encoding,
             realization_clip = excluded.realization_clip,
             contributor_id = excluded.contributor_id,
             tier = excluded.tier,
             root = excluded.root,
             residual_de = excluded.residual_de,
             raw_gap = excluded.raw_gap,
             sigma = excluded.sigma,
             observation_sigma = excluded.observation_sigma,
             measured_de = excluded.measured_de,
             predicted_de = excluded.predicted_de,
             predicted_sigma = excluded.predicted_sigma,
             signed_residual = excluded.signed_residual,
             prediction_residual = excluded.prediction_residual,
             assessment = excluded.assessment,
             systematic_sigma = excluded.systematic_sigma,
             input_transform_version = excluded.input_transform_version,
             control_response_version = excluded.control_response_version,
             look_version = excluded.look_version,
             source_realization = excluded.source_realization,
             source_body_state = excluded.source_body_state,
             source_report = excluded.source_report,
             source_scope = excluded.source_scope",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(write.contribution_id.as_str())
    .bind(write.image_id.as_str())
    .bind(write.body_id.as_str())
    .bind(i64::from(write.epoch))
    .bind(write.sim_key.as_str())
    .bind(serde_json::to_string(&write.settings)?)
    .bind(write.target_look.as_str())
    .bind(write.cell.to_string())
    .bind(looklab_trust::scoring_metric_name(write.metric))
    .bind(write.output.target.encoding.as_str())
    .bind(write.output.target.clip_k)
    .bind(write.output.camera.encoding.as_str())
    .bind(write.output.camera.clip_k)
    .bind(write.contributor.to_string())
    .bind(tier_name(write.tier))
    .bind(i64::from(u8::from(write.root)))
    .bind(write.measured.raw_gap_mean)
    .bind(write.measured.raw_gap_mean)
    .bind(write.measured.observation_sigma)
    .bind(write.measured.observation_sigma)
    .bind(write.measured.measured_mean)
    .bind(write.measured.predicted_mean)
    .bind(write.measured.predicted_sigma)
    .bind(serde_json::to_string(&write.measured.signed_mean)?)
    .bind(serde_json::to_string(&write.measured.prediction_mean)?)
    .bind("unassessed")
    .bind(Option::<f64>::None)
    .bind(i64::from(write.input_transform))
    .bind(i64::from(write.control_response))
    .bind(i64::from(write.look_version))
    .bind(write.source.realization.as_str())
    .bind(i64::from(write.source.body_state.get()))
    .bind(write.source.report.map(ModelReportId::to_hex))
    .bind(write.source.scope.as_str())
    .bind(now)
    .execute(&mut *conn)
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

struct SelectedTarget {
    version: u32,
    document: LookDocument,
    pin: OutputPin,
}

async fn selected_version_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
) -> Result<Option<u32>> {
    let look_id =
        LookId::new(look.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?;
    match model_store::family_head(&mut *conn, &look_id).await? {
        Some(head) => Ok(head
            .applicability
            .is_current()
            .then_some(head.target.version.get())),
        None => Ok(model_store::target_eligibilities(&mut *conn)
            .await?
            .into_iter()
            .filter(|eligibility| {
                eligibility.basis == TargetBasis::LutComposition
                    && eligibility.status == TargetEligibilityStatus::Eligible
                    && eligibility.target.look == look_id
            })
            .max_by_key(|eligibility| eligibility.target.version)
            .map(|eligibility| eligibility.target.version.get())),
    }
}

/// A native family selects its head; a LUT-defined target selects its current eligible composition.
async fn selected_target(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
) -> Result<Option<SelectedTarget>> {
    let version = {
        let mut conn = db_pool.acquire().await?;
        let version = selected_version_conn(&mut conn, look).await?;
        drop(conn);
        version
    };
    let Some(version) = version else {
        return Ok(None);
    };
    let Some(artifacts) = db::manifest_artifacts(db_pool, look.as_str(), version).await? else {
        return Ok(None);
    };
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == LOOK_ARTIFACT)
        .ok_or_else(|| WorkerError::Data("manifest has no look artifact".to_owned()))?;
    let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
    let document = LookDocument::decode(&bytes)?;
    let pin = OutputPin::from_document(&document)
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(Some(SelectedTarget {
        version,
        document,
        pin,
    }))
}

struct SourceRealization {
    loaded: Arc<LoadedRealization>,
    covariance: Option<ControlCovariance>,
    pin: SourcePin,
}

#[derive(Clone)]
struct SourceCandidate {
    version: BodyRealizationVersionId,
    use_: ProjectionUse,
    report: Option<ModelReportId>,
    capability: QualificationCapability,
}

struct SourceSet {
    subject: SubjectKey,
    candidates: Vec<SourceCandidate>,
    loaded: HashMap<BodyRealizationVersionId, Option<Arc<LoadedRealization>>>,
    covariance: HashMap<BodyRealizationVersionId, Option<ControlCovariance>>,
    reports: HashMap<ModelReportId, Option<ModelReport>>,
}

async fn source_set(db_pool: &SqlitePool, pool: &LookPool) -> Result<Option<SourceSet>> {
    let subject = ModelSubject::BodyRealization {
        realization: pool.realization.clone(),
    }
    .key();
    let mut candidates = Vec::new();
    {
        let mut conn = db_pool.acquire().await?;
        for use_ in [
            ProjectionUse::QualifiedMatch,
            ProjectionUse::ExperimentalMatch,
        ] {
            for projection in model_store::current_projections(&mut conn, &subject, use_).await? {
                if !projection.applicability.is_current() {
                    continue;
                }
                if let Some(ModelVersionId::BodyRealization { realization }) =
                    projection.selected_candidate
                {
                    candidates.push(SourceCandidate {
                        version: realization,
                        use_,
                        report: projection.selected_report,
                        capability: projection.capability,
                    });
                }
            }
        }
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    Ok(Some(SourceSet {
        subject,
        candidates,
        loaded: HashMap::new(),
        covariance: HashMap::new(),
        reports: HashMap::new(),
    }))
}

async fn select_source(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    set: &mut SourceSet,
    pool: &LookPool,
    image: &PoolImage,
    context: &PhysicalContext,
) -> Result<Option<SourceRealization>> {
    let space = output_space(image.samples.header().output_space);
    for index in 0..set.candidates.len() {
        let candidate = set.candidates[index].clone();
        if candidate.use_ == ProjectionUse::QualifiedMatch {
            if candidate.capability != QualificationCapability::ControlsMatching {
                continue;
            }
            let Some(report_id) = candidate.report else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = set.reports.entry(report_id) {
                let mut conn = db_pool.acquire().await?;
                let report = model_store::report(&mut conn, &report_id)
                    .await?
                    .map(|stored| stored.report);
                entry.insert(report);
            }
            let Some(Some(report)) = set.reports.get(&report_id) else {
                continue;
            };
            if report.scope.output_encoding != output_encoding(space) {
                continue;
            }
        }
        if !set.loaded.contains_key(&candidate.version) {
            let loaded = load_realization(db_pool, config, &candidate.version)
                .await
                .ok()
                .map(Arc::new);
            set.loaded.insert(candidate.version.clone(), loaded);
        }
        let Some(loaded) = set.loaded.get(&candidate.version).cloned().flatten() else {
            continue;
        };
        if loaded.release.output_space != space {
            continue;
        }
        if candidate.use_ == ProjectionUse::QualifiedMatch {
            let Some(report_id) = candidate.report else {
                continue;
            };
            let Some(Some(report)) = set.reports.get(&report_id) else {
                continue;
            };
            let Some(epoch) = pool
                .resources
                .schema
                .epochs
                .iter()
                .find(|epoch| epoch.id == image.epoch)
            else {
                continue;
            };
            let effective = image.transform.mul(&loaded.transform.delta().transform());
            let pinned = PinnedModes {
                firmware: report
                    .scope
                    .points
                    .iter()
                    .filter_map(|point| point.firmware.clone())
                    .collect(),
                wb_modes: report
                    .scope
                    .points
                    .iter()
                    .filter_map(|point| point.wb_mode.clone())
                    .collect(),
            };
            let mut covered = false;
            let mut all_covered = true;
            for (sample, admitted) in image.admitted.iter().enumerate() {
                if !admitted {
                    continue;
                }
                covered = true;
                let scene = Xyz::from_array(
                    effective.mul_vec(image.samples.in_rgb()[sample].map(f64::from)),
                );
                let Some(cell) = cell_for_scene(scene) else {
                    all_covered = false;
                    break;
                };
                if !matching_scope_covers(
                    report,
                    epoch,
                    &image.settings,
                    Some(&context.firmware),
                    Some(&context.wb_mode),
                    &pinned,
                    &cell,
                ) {
                    all_covered = false;
                    break;
                }
            }
            if !covered || !all_covered {
                continue;
            }
        }
        if !set.covariance.contains_key(&candidate.version) {
            let covariance = loaded
                .revision
                .artifacts
                .iter()
                .find(|binding| binding.role == ArtifactRole::Covariance)
                .and_then(|binding| read_binding(config, binding, ArtifactRole::Covariance).ok())
                .and_then(|bytes| ControlCovariance::from_json(&bytes).ok());
            set.covariance.insert(candidate.version.clone(), covariance);
        }
        let covariance = set.covariance.get(&candidate.version).cloned().flatten();
        return Ok(Some(SourceRealization {
            loaded,
            covariance,
            pin: SourcePin {
                subject: set.subject.clone(),
                use_: candidate.use_,
                candidate: ModelVersionId::BodyRealization {
                    realization: candidate.version.clone(),
                },
                report: candidate.report,
            },
        }));
    }
    Ok(None)
}

async fn write_residuals(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    body: &BodyId,
    residuals: &[looklab_trust::RegionResidual],
    now: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM recipe_residuals WHERE target_look = ? AND body_id = ?")
        .bind(look.as_str())
        .bind(body.as_str())
        .execute(&mut *conn)
        .await?;
    for residual in residuals {
        sqlx::query(
            "INSERT INTO recipe_residuals (
                 id, target_look, body_id, region, cell, target_encoding, target_clip,
                 output_encoding, realization_clip, contributors, settings_cells,
                 mean_residual, raw_gap, observation_sigma, sigma, systematic_excess,
                 systematic_sigma, assessment, reachable, persistent, attribution,
                 alignment, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(look.as_str())
        .bind(body.as_str())
        .bind(residual.cell.to_string())
        .bind(residual.cell.to_string())
        .bind(
            residual
                .output
                .map(|output| output.target.encoding.as_str()),
        )
        .bind(residual.output.map(|output| output.target.clip_k))
        .bind(
            residual
                .output
                .map(|output| output.camera.encoding.as_str()),
        )
        .bind(residual.output.map(|output| output.camera.clip_k))
        .bind(i64::try_from(residual.contributors).unwrap_or(i64::MAX))
        .bind(i64::try_from(residual.settings_cells).unwrap_or(i64::MAX))
        .bind(residual.raw_gap)
        .bind(residual.raw_gap)
        .bind(residual.observation_sigma)
        .bind(
            residual
                .systematic_sigma
                .filter(|sigma| *sigma > 0.0)
                .or(residual.observation_sigma)
                .unwrap_or(SIGMA_FLOOR),
        )
        .bind(residual.systematic_excess)
        .bind(residual.systematic_sigma)
        .bind(assessment_name(residual.assessment))
        .bind(match residual.reachable {
            Reachability::Reachable => Some(1_i64),
            Reachability::Unreachable => Some(0_i64),
            Reachability::Unassessed => None,
        })
        .bind(i64::from(u8::from(residual.persistent)))
        .bind(serde_json::to_string(&residual.attribution)?)
        .bind(serde_json::to_string(&residual.alignment)?)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

fn assessment_name(assessment: AssessmentStatus) -> &'static str {
    match assessment {
        AssessmentStatus::Assessed => "assessed",
        AssessmentStatus::Partial => "partial",
        AssessmentStatus::Unassessed => "unassessed",
    }
}

async fn write_bounties(
    conn: &mut SqliteConnection,
    params: &TrustParams,
    look: &LookSlug,
    body: &BodyId,
    version: u32,
    residuals: &[looklab_trust::RegionResidual],
    now: i64,
) -> Result<()> {
    let mut resolved = resolved_regions(&mut *conn, params, look, body, version).await?;
    let previous = sqlx::query(
        "SELECT region, target_encoding, target_clip, output_encoding, realization_clip
         FROM bounties
         WHERE target_look = ? AND body_id = ? AND look_version = ? AND status = 'resolved'
           AND metric = ?",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .bind(i64::from(version))
    .bind(looklab_trust::scoring_metric_name(
        ScoringMetric::RenderedPostClipV1,
    ))
    .fetch_all(&mut *conn)
    .await?;
    for row in &previous {
        let region_text: String = row.get("region");
        if let Ok(region) = region_text.parse() {
            resolved.insert((region, output_key(row)));
        }
    }
    for (region, key) in &resolved {
        let (target_encoding, target_clip, output_encoding, realization_clip) = output_parts(key);
        sqlx::query(
            "UPDATE bounties SET status = 'resolved', resolved_at = ?
             WHERE target_look = ? AND body_id = ? AND look_version = ? AND region = ?
               AND target_encoding IS ? AND target_clip IS ? AND output_encoding IS ?
               AND realization_clip IS ? AND status = 'open' AND metric = ?",
        )
        .bind(now)
        .bind(look.as_str())
        .bind(body.as_str())
        .bind(i64::from(version))
        .bind(region.to_string())
        .bind(target_encoding)
        .bind(target_clip)
        .bind(output_encoding)
        .bind(realization_clip)
        .bind(looklab_trust::scoring_metric_name(
            ScoringMetric::RenderedPostClipV1,
        ))
        .execute(&mut *conn)
        .await?;
    }
    let uncertainties: Vec<_> = looklab_trust::region_uncertainties(residuals)
        .into_iter()
        .filter(|region| {
            !resolved.contains(&(
                region.region,
                region.output.map(|output| output.canonical()),
            ))
        })
        .collect();
    let target = BountyTarget {
        look: look.clone(),
        look_version: VersionId::new(version),
        body: Some(body.clone()),
    };
    let bounties =
        looklab_trust::rank_bounties(params, &target, &uncertainties, params.bounty_top_n)?;
    sqlx::query(
        "DELETE FROM bounties WHERE target_look = ? AND body_id = ? AND status = 'open'
           AND metric = ?",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .bind(looklab_trust::scoring_metric_name(
        ScoringMetric::RenderedPostClipV1,
    ))
    .execute(&mut *conn)
    .await?;
    for bounty in bounties {
        sqlx::query(
            "INSERT INTO bounties (
                 id, target_look, look_version, body_id, region, metric, target_encoding,
                 target_clip, output_encoding, realization_clip,
                 expected_variance_reduction, reason, status, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(bounty.look.as_str())
        .bind(i64::from(bounty.look_version.get()))
        .bind(bounty.body.as_ref().map(BodyId::as_str))
        .bind(bounty.region.to_string())
        .bind(looklab_trust::scoring_metric_name(
            ScoringMetric::RenderedPostClipV1,
        ))
        .bind(
            bounty
                .output
                .map(|output| output.target.encoding.as_str().to_owned()),
        )
        .bind(bounty.output.map(|output| output.target.clip_k))
        .bind(
            bounty
                .output
                .map(|output| output.camera.encoding.as_str().to_owned()),
        )
        .bind(bounty.output.map(|output| output.camera.clip_k))
        .bind(bounty.expected_variance_reduction)
        .bind(serde_json::to_string(&bounty.reason)?)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

type OutputKey = Option<String>;

fn output_key(row: &sqlx::sqlite::SqliteRow) -> OutputKey {
    let target_encoding: Option<String> = row.get("target_encoding");
    let target_clip: Option<f64> = row.get("target_clip");
    let output_encoding: Option<String> = row.get("output_encoding");
    let realization_clip: Option<f64> = row.get("realization_clip");
    Some(format!(
        "target={}@{};camera={}@{}",
        target_encoding?, target_clip?, output_encoding?, realization_clip?
    ))
}

fn output_parts(key: &OutputKey) -> (Option<String>, Option<f64>, Option<String>, Option<f64>) {
    match key
        .as_deref()
        .and_then(|value| OutputContext::parse(value).ok())
    {
        Some(output) => (
            Some(output.target.encoding.as_str().to_owned()),
            Some(output.target.clip_k),
            Some(output.camera.encoding.as_str().to_owned()),
            Some(output.camera.clip_k),
        ),
        None => (None, None, None, None),
    }
}

async fn resolved_regions(
    conn: &mut SqliteConnection,
    params: &TrustParams,
    look: &LookSlug,
    body: &BodyId,
    version: u32,
) -> Result<HashSet<(Region, OutputKey)>> {
    let residual_rows = sqlx::query(
        "SELECT region, target_encoding, target_clip, output_encoding, realization_clip,
                systematic_sigma, assessment
         FROM recipe_residuals WHERE target_look = ? AND body_id = ?",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .fetch_all(&mut *conn)
    .await?;
    let mut assessed: BTreeMap<(Region, OutputKey), f64> = BTreeMap::new();
    for row in &residual_rows {
        let region_text: String = row.get("region");
        let Ok(region) = region_text.parse() else {
            continue;
        };
        let assessment: String = row.get("assessment");
        if assessment != "assessed" {
            continue;
        }
        let Some(sigma): Option<f64> = row.get("systematic_sigma") else {
            continue;
        };
        if sigma.is_finite() && sigma > 0.0 {
            assessed.insert((region, output_key(row)), sigma);
        }
    }
    let rows = sqlx::query(
        "SELECT region, target_encoding, target_clip, output_encoding, realization_clip,
                sigma, tier, root, contributor_id
         FROM recipe_observations
         WHERE target_look = ? AND body_id = ? AND look_version = ? AND metric = ?",
    )
    .bind(look.as_str())
    .bind(body.as_str())
    .bind(i64::from(version))
    .bind(looklab_trust::scoring_metric_name(
        ScoringMetric::RenderedPostClipV1,
    ))
    .fetch_all(&mut *conn)
    .await?;
    let mut verifications: BTreeMap<(Region, OutputKey), Vec<BountyVerification>> = BTreeMap::new();
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
        verifications
            .entry((region, output_key(row)))
            .or_default()
            .push(BountyVerification {
                contributor: contributor.into(),
                tier,
                root: root != 0,
                sigma,
            });
    }
    let mut resolved = HashSet::new();
    for (key, sigma) in assessed {
        let Some(region_verifications) = verifications.get(&key) else {
            continue;
        };
        if looklab_trust::resolution_status(params, sigma, region_verifications)?
            == BountyStatus::Resolved
        {
            resolved.insert(key);
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
    conn: &mut SqliteConnection,
    groups: Vec<(RecipeKey, RecipePrediction, Vec<VerifiedRecipe>)>,
    look: &LookSlug,
    body: &BodyId,
    now: i64,
) -> Result<()> {
    let mut scores = Vec::with_capacity(groups.len());
    for (key, prediction, verifications) in groups {
        scores.push(looklab_trust::score_recipe(
            key,
            prediction,
            &verifications,
        )?);
    }
    let metric = looklab_trust::scoring_metric_name(ScoringMetric::RenderedPostClipV1);
    sqlx::query("DELETE FROM recipe_scores WHERE target_look = ? AND body_id = ? AND metric = ?")
        .bind(look.as_str())
        .bind(body.as_str())
        .bind(metric)
        .execute(&mut *conn)
        .await?;
    for score in &scores {
        sqlx::query(
            "INSERT INTO recipe_scores (
                 id, body_id, epoch, base_sim, settings, target_look, metric,
                 predicted_de, predicted_sigma, measured_de, measured_sigma, raw_gap,
                 signed_residual, n_verifications, input_transform_version,
                 control_response_version, look_version, target_encoding, target_clip,
                 output_encoding, realization_clip, source_realization, source_body_state,
                 source_report, source_scope, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(score.recipe.body.as_str())
        .bind(i64::from(score.recipe.epoch.get()))
        .bind(score.recipe.base_sim.as_str())
        .bind(serde_json::to_string(&score.recipe.settings)?)
        .bind(score.recipe.target_look.as_str())
        .bind(looklab_trust::scoring_metric_name(score.metric))
        .bind(score.predicted_de)
        .bind(score.predicted_sigma)
        .bind(score.measured_de)
        .bind(score.measured_sigma)
        .bind(score.raw_gap)
        .bind(
            score
                .signed_residual
                .map(|value| serde_json::to_string(&value))
                .transpose()?,
        )
        .bind(i64::try_from(score.n_verifications).unwrap_or(i64::MAX))
        .bind(i64::from(score.model_versions.input_transform))
        .bind(i64::from(score.model_versions.control_response))
        .bind(i64::from(score.model_versions.look.get()))
        .bind(score.output.as_ref().map(|output| output.target.encoding.as_str()))
        .bind(score.output.as_ref().map(|output| output.target.clip_k))
        .bind(score.output.as_ref().map(|output| output.camera.encoding.as_str()))
        .bind(score.output.as_ref().map(|output| output.camera.clip_k))
        .bind(
            score
                .source
                .as_ref()
                .map(|source| source.realization.as_str()),
        )
        .bind(
            score
                .source
                .as_ref()
                .map(|source| i64::from(source.body_state.get())),
        )
        .bind(
            score
                .source
                .as_ref()
                .and_then(|source| source.report)
                .map(ModelReportId::to_hex),
        )
        .bind(score.source.as_ref().map(|source| source.scope.as_str()))
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
