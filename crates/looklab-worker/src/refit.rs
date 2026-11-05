// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::RgbSpace;
use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_fit::{
    ExposureGauge, FitOptions, FitResult, ImageIntent, PreviousSupport,
    fit_controlled_with_history, post_clip, refit_responses,
};
use looklab_model::realization::{ControlResponseModel, decode_crm, encode_crm};
use looklab_model::{
    Density, Look, LookDocument, ProbeCovariance, ProbeId, ProbeScalars, UncertaintyData,
};
use looklab_publish::{
    BodyUse, CARD_ARTIFACT, GateCheck, GateSummary, PREVIEW_LUT_ARTIFACT, ProvenanceSummary,
    bake_preview_lut,
};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_trust::{
    GateInput, GateReport, GatedUnion, ProbeEstimate, ProbeSupport, ReputationUpdate, TrustParams,
    VersionId, VersionProbes, gate_release,
};
use looklab_upload::{BodyId, LookSlug};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::body::epoch_for;
use crate::clock::{now_unix, rfc3339};
use crate::config::WorkerConfig;
use crate::db::{self, BodyStage, ContributionStatus, ImageOutcome, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::jobs::JobOwnership;
use crate::pool::{self, LookPool};
use crate::roots::{self, RootData};
use crate::weights::{self, WeightedPool};

pub const LOOK_ARTIFACT: &str = "look.looklab";
pub const RELEASE_ARTIFACT: &str = "release.json";
pub const CRM_ARTIFACT: &str = "crm.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProbe {
    pub id: ProbeId,
    /// Pre-clip `σ_ΔE`.
    pub sigma: f64,
    /// Post-clip `σ_ΔE`.
    #[serde(default)]
    pub sigma_post: f64,
    /// `|b_p|`, ΔE.
    #[serde(default)]
    pub bias_de: f64,
    /// The pre-clip value was identified in the fit that published this version.
    #[serde(default)]
    pub pre_clip_identified: bool,
    /// The four support tests as the fit decided them.
    #[serde(default)]
    pub support: ProbeSupport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub version: u32,
    pub gauge: f64,
    pub anchor: u32,
    /// Soft clip of the published realization, for re-evaluating the Look.
    #[serde(default)]
    pub clip_k: f64,
    /// Output encoding of the published probe predictions.
    #[serde(default = "default_output_space")]
    pub output_space: RgbSpace,
    pub probes: Vec<StoredProbe>,
    /// Probes supported in any version published since [`Self::anchor`].
    #[serde(default)]
    pub gated_since_anchor: GatedUnion,
    /// The native stage reached its objective and step tolerances.
    #[serde(default)]
    pub converged: bool,
    pub gate: GateReport,
    pub provenance: ProvenanceSummary,
    pub created_at: i64,
}

fn default_output_space() -> RgbSpace {
    RgbSpace::Srgb
}

/// Body parameters published for the recipe stage (docs/02 §4.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RecipeStageParams {
    /// Hex of `encode_crm` for the recipe-stage control response.
    control_response: String,
    /// `body_pool_fingerprint` of the images the response was fitted on.
    #[serde(default)]
    recipe_pool: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefitOutcome {
    Published(u32),
    Held(u32),
    Skipped,
}

/// Single-body native stage: each body's Looks are refitted independently. A hierarchical
/// fit replaces the outer body loop once bodies share a Look.
pub async fn run(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    bodies: &[BodyId],
    ownership: &JobOwnership,
) -> Result<Vec<(BodyId, LookSlug, RefitOutcome)>> {
    let explicit = !bodies.is_empty();
    let targets: Vec<BodyId> = if explicit {
        bodies.to_vec()
    } else {
        dirty_bodies(db_pool, config).await?
    };

    let mut outcomes = Vec::new();
    for body in targets {
        for look in pool::looks_for_body(db_pool, config, &body).await? {
            let outcome = refit_look(db_pool, config, &body, &look, ownership).await?;
            if outcome != RefitOutcome::Skipped {
                outcomes.push((body.clone(), look, outcome));
            }
        }
    }
    Ok(outcomes)
}

async fn dirty_bodies(db_pool: &SqlitePool, config: &WorkerConfig) -> Result<Vec<BodyId>> {
    let contributions = db::all_contributions(db_pool).await?;
    let mut bodies: BTreeSet<BodyId> = contributions
        .iter()
        .filter(|contribution| contribution.status != ContributionStatus::Withdrawn)
        .map(|contribution| contribution.body.clone())
        .collect();
    let mut dirty = Vec::new();
    for body in std::mem::take(&mut bodies) {
        for look in pool::looks_for_body(db_pool, config, &body).await? {
            if is_dirty(db_pool, config, &body, &look).await? {
                dirty.push(body);
                break;
            }
        }
    }
    Ok(dirty)
}

async fn is_dirty(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
) -> Result<bool> {
    let pool = pool::build(db_pool, config, body, look).await?;
    if pool.images.is_empty() {
        return Ok(false);
    }
    let fingerprint = pool_fingerprint(&pool)?;
    let stored = db::pool_fingerprint(db_pool, look.as_str(), body.as_str()).await?;
    Ok(stored.as_deref() != Some(fingerprint.as_str()))
}

/// Covers every input that changes the fit: pool membership, statuses, standing,
/// reputation, settings and the resolved effective contributors.
fn pool_fingerprint(pool: &LookPool) -> Result<String> {
    let mut hasher = Sha256::new();
    for image in &pool.images {
        hash_image(&mut hasher, image)?;
    }
    Ok(db::hex(&hasher.finalize()))
}

fn hash_image(hasher: &mut Sha256, image: &crate::pool::PoolImage) -> Result<()> {
    hasher.update(image.image.id.as_str().as_bytes());
    hasher.update(db::hex(&image.image.samples_hash).as_bytes());
    hasher.update(image.contribution.id.as_str().as_bytes());
    hasher.update(image.contribution.status.as_str().as_bytes());
    hasher.update(image.contribution.intent.as_str().as_bytes());
    hasher.update(
        image
            .image
            .status
            .map_or("pending", ImageOutcome::as_str)
            .as_bytes(),
    );
    hasher.update(image.credential.id.as_str().as_bytes());
    hasher.update(format!("{:?}", image.credential.tier).as_bytes());
    hasher.update(image.contributor.to_string().as_bytes());
    hasher.update(serde_json::to_vec(&image.credential.scope)?);
    hasher.update(serde_json::to_vec(&image.settings)?);
    hasher.update(image.epoch.to_le_bytes());
    hasher.update(image.exposure_mode.0.to_le_bytes());
    Ok(())
}

/// The body-stage pool and the weights the stage saw. A refit with the same hash
/// can reuse the published recipe-stage response.
fn body_pool_fingerprint(pool: &LookPool, weighted: &WeightedPool) -> Result<String> {
    let mut hasher = Sha256::new();
    for entry in &weighted.images {
        let image = &pool.images[entry.pool_index];
        hash_image(&mut hasher, image)?;
        hasher.update(entry.look_weight.to_le_bytes());
        hasher.update(entry.body_weight.to_le_bytes());
        hasher.update(entry.duplicates.to_le_bytes());
        hasher.update(serde_json::to_vec(&entry.per_region)?);
    }
    Ok(db::hex(&hasher.finalize()))
}

fn lap(at: &mut std::time::Instant, label: &str) {
    if std::env::var_os("LOOKLAB_TIMING").is_some() {
        eprintln!("refit timing: {label} {:?}", at.elapsed());
        *at = std::time::Instant::now();
    }
}

async fn refit_look(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
    ownership: &JobOwnership,
) -> Result<RefitOutcome> {
    let mut at = std::time::Instant::now();
    let pool = pool::build(db_pool, config, body, look).await?;
    if pool.images.is_empty() {
        return Ok(RefitOutcome::Skipped);
    }
    lap(&mut at, "pool");
    let roots = roots::load(db_pool, config, &pool).await?;
    let options = FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        ..FitOptions::default()
    };
    let params = TrustParams {
        support_sigma_exit: options.support_sigma_exit,
        ..TrustParams::default()
    };
    let weighted = weights::weigh(&pool, &params, &roots.weights)?;
    lap(&mut at, "weigh");
    let contributors = weights::contributor_index(&pool);
    let native_images = weights::fit_images(&pool, &weighted, &contributors);
    let recipe_images = weights::recipe_fit_images(&pool, &weighted, &contributors);
    lap(&mut at, "fit images");

    let versions = db::look_versions(db_pool, look.as_str()).await?;
    let next_version = versions
        .iter()
        .map(|version| version.version)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| WorkerError::Data("version overflow".to_owned()))?;
    let previous_version = latest_release(&versions);
    let previous = match &previous_version {
        Some(version) => Some(load_snapshot(db_pool, config, look, version.version).await?),
        None => None,
    };
    lap(&mut at, "snapshot");

    let fingerprint = pool_fingerprint(&pool)?;
    let stored = db::pool_fingerprint(db_pool, look.as_str(), body.as_str()).await?;
    lap(&mut at, "fingerprint");
    if stored.as_deref() == Some(fingerprint.as_str()) && previous_version.is_some() {
        return Ok(RefitOutcome::Skipped);
    }
    if stored.is_none()
        && let Some(latest) = previous_version
    {
        let newest = pool
            .images
            .iter()
            .map(|image| image.contribution.created_at)
            .max()
            .unwrap_or(0);
        if newest <= latest.created_at {
            update_reputation(db_pool, look, latest.version, &pool, &previous).await?;
            db::record_pool_fingerprint(
                db_pool,
                look.as_str(),
                body.as_str(),
                &fingerprint,
                now_unix(),
            )
            .await?;
            return Ok(RefitOutcome::Published(latest.version));
        }
    }
    if db::manifest_exists(db_pool, look.as_str(), next_version).await? {
        update_reputation(db_pool, look, next_version, &pool, &previous).await?;
        return Ok(RefitOutcome::Published(next_version));
    }
    if db::gate_failure_pending(db_pool, look.as_str(), next_version).await? {
        return Ok(RefitOutcome::Held(next_version));
    }

    let body_images = recipe_images;
    let has_recipe = body_images
        .iter()
        .any(|image| image.intent == ImageIntent::Recipe);
    let recipe_pool = if has_recipe {
        Some(body_pool_fingerprint(&pool, &weighted)?)
    } else {
        None
    };
    if native_images.is_empty() {
        return run_recipe_only(
            db_pool,
            &pool,
            &body_images,
            recipe_pool.as_deref(),
            &previous,
            &options,
            look,
            body,
        )
        .await;
    }
    let (epoch, sim) = roots::base_sim(&pool)?;
    let support_history = previous_support(&previous);
    let native_result = fit_controlled_with_history(
        &native_images,
        &pool.resources.schema,
        epoch,
        &sim,
        &options,
        support_history.as_ref(),
    )?;
    let converged = native_result.diagnostics.converged;
    if !converged {
        tracing::warn!(
            look = look.as_str(),
            body = body.as_str(),
            sweeps = native_result.diagnostics.sweeps,
            evaluations = native_result.diagnostics.evaluations,
            "native fit exhausted its sweep budget"
        );
    }
    if std::env::var_os("LOOKLAB_TIMING").is_some() {
        eprintln!(
            "native fit diagnostics: converged={} sweeps={} evaluations={} cost={} samples={}",
            native_result.diagnostics.converged,
            native_result.diagnostics.sweeps,
            native_result.diagnostics.evaluations,
            native_result.diagnostics.final_cost,
            native_result.diagnostics.objective_samples
        );
    }
    lap(&mut at, "native fit");
    let native_model = native_result
        .control_response
        .as_ref()
        .map(|fit| fit.model.clone())
        .ok_or_else(|| WorkerError::Data("native stage produced no control response".to_owned()))?;
    // Without recipe data the native optimum already solves the response block;
    // an unchanged body pool reuses the response the previous release published.
    let recipe_model = match &recipe_pool {
        None => native_model.clone(),
        Some(recipe_pool) => {
            let reused = match &previous_version {
                Some(version) => db::release_body_params(
                    db_pool,
                    look.as_str(),
                    version.version,
                    body.as_str(),
                    BodyStage::Recipe,
                )
                .await?
                .map(|stored| stored_recipe_model(&stored, recipe_pool))
                .transpose()?
                .flatten(),
                None => None,
            };
            match reused {
                Some(model) => model,
                None => {
                    let mut recipe_options = options;
                    recipe_options.recipe_sweeps = recipe_sweeps(body_images.len(), options.n_eff);
                    refit_responses(
                        &native_result.look,
                        &body_images,
                        &pool.resources.schema,
                        epoch,
                        &sim,
                        &native_model,
                        &recipe_options,
                    )?
                    .model
                }
            }
        }
    };
    lap(&mut at, "recipe fit");

    crate::split::detect_and_propose(db_pool, config, &pool, &native_result).await?;
    lap(&mut at, "split");
    let gauge = gauge(&pool, &weighted, &native_result);
    let candidate_probes = stored_probes(&native_result);
    let candidate_versions = candidate_estimates(&native_result);

    let anchor_version = select_anchor(db_pool, look, &versions).await?;
    let anchor = match &anchor_version {
        Some(version) => Some(load_snapshot(db_pool, config, look, *version).await?),
        None => None,
    };
    let anchor_id = anchor_version.unwrap_or(next_version);

    let previous_probes = match &previous {
        Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge)?,
        None => candidate_versions.clone(),
    };
    let anchor_probes = match &anchor {
        Some(snapshot) => stored_estimates(&snapshot.look, &snapshot.record, gauge)?,
        None => previous_probes.clone(),
    };

    let gated = gated_since_anchor(&previous, anchor_id, &anchor_probes);
    let published_gated = if anchor_id == next_version {
        GatedUnion::at_anchor(&candidate_versions)
    } else {
        let mut union = gated.clone();
        union.include(&candidate_versions);
        union
    };
    let gate = gate_release(
        &params,
        &GateInput {
            candidate: candidate_versions,
            previous: previous_probes,
            anchor: anchor_probes,
            root_observations: roots.observations.clone(),
            gated_since_anchor: gated,
        },
    );
    lap(&mut at, "gate");

    let now = now_unix();
    let provenance = provenance(
        &pool,
        &weighted,
        anchor_version.unwrap_or(next_version),
        &roots,
        now,
    )?;
    if !gate.passed {
        db::insert_gate_failure(
            db_pool,
            look.as_str(),
            next_version,
            &serde_json::to_string(&gate)?,
            now,
        )
        .await?;
        db::record_pool_fingerprint(
            db_pool,
            look.as_str(),
            body.as_str(),
            &pool_fingerprint(&pool)?,
            now,
        )
        .await?;
        return Ok(RefitOutcome::Held(next_version));
    }

    ownership.check(db_pool).await?;
    let (document, document_bytes) = look_document(look, next_version, &native_result)?;
    lap(&mut at, "document");
    let release = ReleaseRecord {
        version: next_version,
        gauge,
        anchor: anchor_id,
        clip_k: native_result.realization.clip_k,
        output_space: native_result.output_space,
        probes: candidate_probes,
        gated_since_anchor: published_gated,
        converged,
        gate: gate.clone(),
        provenance: provenance.clone(),
        created_at: now,
    };
    let artifacts = vec![
        store_artifact(config, LOOK_ARTIFACT, &document_bytes)?,
        store_artifact(
            config,
            CARD_ARTIFACT,
            &write_card(&document, &card_meta(look), &CardOptions::default())?,
        )?,
        store_artifact(config, PREVIEW_LUT_ARTIFACT, &bake_preview_lut(&document))?,
        store_artifact(config, CRM_ARTIFACT, &encode_crm(&recipe_model)?)?,
        store_artifact(config, RELEASE_ARTIFACT, &serde_json::to_vec(&release)?)?,
    ];
    lap(&mut at, "artifacts");
    store_stage_params(
        db_pool,
        look,
        next_version,
        body,
        &native_result,
        &native_model,
        &recipe_model,
        recipe_pool.as_deref(),
        now,
    )
    .await?;
    lap(&mut at, "stage params");
    admin::write_version_manifest(
        db_pool,
        look,
        VersionId::new(next_version),
        &artifacts,
        &roots.admissions,
    )
    .await?;
    lap(&mut at, "manifest");
    update_reputation(db_pool, look, next_version, &pool, &previous).await?;
    lap(&mut at, "reputation");
    db::record_pool_fingerprint(
        db_pool,
        look.as_str(),
        body.as_str(),
        &pool_fingerprint(&pool)?,
        now,
    )
    .await?;
    Ok(RefitOutcome::Published(next_version))
}

#[allow(clippy::too_many_arguments)]
async fn run_recipe_only(
    db_pool: &SqlitePool,
    pool: &LookPool,
    recipe_images: &[looklab_fit::FitImage],
    recipe_pool: Option<&str>,
    previous: &Option<Snapshot>,
    options: &FitOptions,
    look: &LookSlug,
    body: &BodyId,
) -> Result<RefitOutcome> {
    let Some(previous) = previous else {
        return Ok(RefitOutcome::Skipped);
    };
    if recipe_images.is_empty() {
        return Ok(RefitOutcome::Skipped);
    }
    let (epoch, sim) = roots::base_sim(pool)?;
    let model = previous
        .control
        .clone()
        .unwrap_or(ControlResponseModel::zeroed(
            &pool.resources.schema,
            epoch,
            &sim,
            1,
        )?);
    let mut recipe_options = *options;
    recipe_options.recipe_sweeps = recipe_sweeps(recipe_images.len(), options.n_eff);
    let control = refit_responses(
        &previous.look,
        recipe_images,
        &pool.resources.schema,
        epoch,
        &sim,
        &model,
        &recipe_options,
    )?;
    let now = now_unix();
    db::store_release_body_params(
        db_pool,
        look.as_str(),
        previous.record.version,
        body.as_str(),
        BodyStage::Recipe,
        &recipe_params(&control.model, recipe_pool)?,
        now,
    )
    .await?;
    db::record_pool_fingerprint(
        db_pool,
        look.as_str(),
        body.as_str(),
        &pool_fingerprint(pool)?,
        now,
    )
    .await?;
    Ok(RefitOutcome::Skipped)
}

fn latest_release(versions: &[db::LookVersionRow]) -> Option<&db::LookVersionRow> {
    versions
        .iter()
        .filter(|version| {
            matches!(
                version.status,
                LookVersionStatus::Published | LookVersionStatus::Rooted
            )
        })
        .max_by_key(|version| version.version)
}

/// The anchor is the first rooted version, or the last version a moderator
/// re-anchored (docs/05 §8).
async fn select_anchor(
    db_pool: &SqlitePool,
    look: &LookSlug,
    versions: &[db::LookVersionRow],
) -> Result<Option<u32>> {
    let mut candidates: Vec<u32> = Vec::new();
    if let Some(first_rooted) = versions
        .iter()
        .filter(|version| version.status == LookVersionStatus::Rooted)
        .min_by_key(|version| version.version)
    {
        candidates.push(first_rooted.version);
    }
    candidates.extend(db::re_anchor_versions(db_pool, look.as_str()).await?);
    Ok(candidates.into_iter().max())
}

struct Snapshot {
    look: Look,
    record: ReleaseRecord,
    control: Option<ControlResponseModel>,
}

async fn load_snapshot(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
    version: u32,
) -> Result<Snapshot> {
    let artifacts = db::manifest_artifacts(db_pool, look.as_str(), version)
        .await?
        .ok_or_else(|| WorkerError::Data(format!("{} {version} has no manifest", look)))?;
    let document = read_artifact(config, &artifacts, LOOK_ARTIFACT)?;
    let record = read_artifact(config, &artifacts, RELEASE_ARTIFACT)?;
    let control = match artifacts
        .iter()
        .find(|artifact| artifact.name == CRM_ARTIFACT)
    {
        Some(_) => Some(decode_crm(&read_artifact(
            config,
            &artifacts,
            CRM_ARTIFACT,
        )?)?),
        None => None,
    };
    Ok(Snapshot {
        look: LookDocument::decode(&document)?.look,
        record: serde_json::from_slice(&record)?,
        control,
    })
}

fn read_artifact(
    config: &WorkerConfig,
    artifacts: &[ManifestArtifact],
    name: &str,
) -> Result<Vec<u8>> {
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .ok_or_else(|| WorkerError::Data(format!("manifest has no `{name}` artifact")))?;
    Ok(std::fs::read(config.blobs.join(&artifact.sha256))?)
}

fn store_artifact(config: &WorkerConfig, name: &str, bytes: &[u8]) -> Result<ManifestArtifact> {
    let digest = Sha256::digest(bytes);
    let sha256 = db::hex(&digest);
    let path = config.blobs.join(&sha256);
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(ManifestArtifact {
        name: name.to_owned(),
        sha256,
    })
}

fn look_document(
    look: &LookSlug,
    version: u32,
    result: &FitResult,
) -> Result<(LookDocument, Vec<u8>)> {
    let identity = looklab_model::file::identity(look.as_str(), &format!("{version}.0.0"))?;
    let mut document = LookDocument::from_look(identity, result.look.clone())?;
    let count = result.probes.probes.len();
    let mut covariance = Vec::with_capacity(count * 9);
    let mut covariance_post = Vec::with_capacity(count * 9);
    let mut bias = Vec::with_capacity(count);
    let mut sigma = Vec::with_capacity(count);
    let mut sigma_post = Vec::with_capacity(count);
    let mut pre_reliable = Vec::with_capacity(count);
    let mut support = Vec::with_capacity(count);
    let mut density = Vec::with_capacity(count);
    for probe in &result.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        for row in &probe.post_covariance {
            covariance_post.extend_from_slice(row);
        }
        bias.push(probe.bias_de);
        sigma.push(probe.sigma_de);
        sigma_post.push(probe.post_sigma_de);
        pre_reliable.push(if probe.pre_clip_reliable { 1.0 } else { 0.0 });
        support.push(if probe.supported { 1.0 } else { 0.0 });
        density.push(probe.density);
    }
    document = document.with_uncertainty(UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(count, covariance)?),
        probe_cov_post: Some(ProbeCovariance::new(count, covariance_post)?),
        probe_bias: Some(ProbeScalars::new(bias)?),
        probe_sigma: Some(ProbeScalars::new(sigma)?),
        probe_sigma_post: Some(ProbeScalars::new(sigma_post)?),
        probe_pre_reliable: Some(ProbeScalars::new(pre_reliable)?),
        support: Some(ProbeScalars::new(support)?),
        density: Some(Density::new(density)?),
    })?;
    let bytes = document.encode()?;
    Ok((document, bytes))
}

fn card_meta(look: &LookSlug) -> CardMeta {
    let short: String = look.as_str().chars().take(27).collect();
    CardMeta::new(look.as_str(), short)
}

fn stored_probes(result: &FitResult) -> Vec<StoredProbe> {
    result
        .probes
        .ids
        .iter()
        .zip(&result.probes.probes)
        .map(|(id, probe)| StoredProbe {
            id: id.clone(),
            sigma: probe.sigma_de,
            sigma_post: probe.post_sigma_de,
            bias_de: probe.bias_de,
            pre_clip_identified: probe.pre_clip_reliable,
            support: ProbeSupport {
                sigma: probe.supported_sigma,
                density: probe.supported_density,
                clusters: probe.supported_clusters,
                bias: probe.supported_bias,
            },
        })
        .collect()
}

fn candidate_estimates(result: &FitResult) -> VersionProbes {
    VersionProbes {
        probes: result
            .probes
            .ids
            .iter()
            .zip(&result.probes.probes)
            .map(|(id, probe)| ProbeEstimate {
                id: id.clone(),
                pre_clip: probe.ucs.to_array(),
                sigma_pre_clip: probe.sigma_de,
                post_clip: probe.post_ucs.to_array(),
                sigma_post_clip: probe.post_sigma_de,
                bias_de: probe.bias_de,
                pre_clip_identified: probe.pre_clip_reliable,
                support: ProbeSupport {
                    sigma: probe.supported_sigma,
                    density: probe.supported_density,
                    clusters: probe.supported_clusters,
                    bias: probe.supported_bias,
                },
            })
            .collect(),
    }
}

/// Re-evaluates a published Look in the candidate's exposure gauge.
fn stored_estimates(look: &Look, record: &ReleaseRecord, gauge: f64) -> Result<VersionProbes> {
    let by_id: BTreeMap<&ProbeId, &StoredProbe> = record
        .probes
        .iter()
        .map(|probe| (&probe.id, probe))
        .collect();
    let exposure = ExposureGauge::between(record.gauge, gauge);
    let mut probes = Vec::with_capacity(by_id.len());
    for probe in looklab_model::probes::v1().probes {
        let stored = by_id.get(&probe.id).ok_or_else(|| {
            WorkerError::Data(format!(
                "release {} lacks probe {}",
                record.version,
                probe.id.as_str()
            ))
        })?;
        let input = exposure.apply(probe.xyz);
        probes.push(ProbeEstimate {
            id: probe.id,
            pre_clip: look.eval(input).to_array(),
            sigma_pre_clip: stored.sigma,
            post_clip: post_clip(look, input, record.output_space, record.clip_k).to_array(),
            sigma_post_clip: stored.sigma_post,
            bias_de: stored.bias_de,
            pre_clip_identified: stored.pre_clip_identified,
            support: stored.support,
        });
    }
    Ok(VersionProbes { probes })
}

/// The union as of the previous release; a re-anchor restarts it from the new anchor.
fn gated_since_anchor(
    previous: &Option<Snapshot>,
    anchor: u32,
    anchor_probes: &VersionProbes,
) -> GatedUnion {
    match previous {
        Some(snapshot) if snapshot.record.anchor == anchor => {
            snapshot.record.gated_since_anchor.clone()
        }
        _ => GatedUnion::at_anchor(anchor_probes),
    }
}

fn previous_support(previous: &Option<Snapshot>) -> Option<PreviousSupport> {
    previous.as_ref().map(|snapshot| PreviousSupport {
        sigma: snapshot
            .record
            .probes
            .iter()
            .map(|probe| probe.support.sigma)
            .collect(),
        bias: snapshot
            .record
            .probes
            .iter()
            .map(|probe| probe.support.bias)
            .collect(),
    })
}

fn gauge(pool: &LookPool, weighted: &WeightedPool, result: &FitResult) -> f64 {
    let excluded: BTreeSet<usize> = result.excluded.iter().map(|image| image.index).collect();
    let entries: Vec<(usize, f64)> = weighted
        .fit_entries(pool)
        .into_iter()
        .enumerate()
        .filter(|(index, _)| !excluded.contains(index))
        .map(|(_, entry)| entry)
        .collect();
    let mut offsets: Vec<f64> = entries
        .into_iter()
        .zip(&result.images)
        .map(|((index, _weight), nuisances)| {
            nuisances.exposure - f64::from(pool.images[index].samples.header().exposure_hint_ev)
        })
        .collect();
    if offsets.is_empty() {
        return 0.0;
    }
    offsets.sort_by(f64::total_cmp);
    offsets[offsets.len() / 2]
}

fn provenance(
    pool: &LookPool,
    weighted: &WeightedPool,
    anchor_version: u32,
    roots: &RootData,
    now: i64,
) -> Result<ProvenanceSummary> {
    let mut prep_versions: Vec<u32> = pool
        .images
        .iter()
        .map(|image| image.samples.header().prep_version.get())
        .collect();
    prep_versions.sort_unstable();
    prep_versions.dedup();
    let credentials: BTreeSet<String> = pool
        .images
        .iter()
        .map(|image| image.credential.id.as_str().to_owned())
        .collect();
    let mut epochs: Vec<u32> = pool
        .images
        .iter()
        .map(|image| {
            epoch_for(&pool.resources.schema, &image.contribution.firmware).map(|epoch| epoch.id)
        })
        .collect::<Result<Vec<_>>>()?;
    epochs.sort_unstable();
    epochs.dedup();
    Ok(ProvenanceSummary {
        bodies: vec![BodyUse {
            body: pool.body.as_str().to_owned(),
            epochs,
            images: u32::try_from(weighted.fit_entries(pool).len()).unwrap_or(u32::MAX),
            credentials: u32::try_from(credentials.len()).unwrap_or(u32::MAX),
        }],
        root_admissions: roots
            .admissions
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect(),
        anchor_version: format!("{anchor_version}.0.0"),
        community_snapshot_version: 0,
        prep_versions,
        created: rfc3339(now),
        software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
    })
}

pub fn gate_summary(report: &GateReport, converged: bool) -> GateSummary {
    GateSummary {
        checks: vec![
            GateCheck {
                name: "root_previous".to_owned(),
                passed: report.root_previous.pass,
                detail: None,
            },
            GateCheck {
                name: "root_anchor".to_owned(),
                passed: report.root_anchor.pass,
                detail: None,
            },
            GateCheck {
                name: "drift_previous".to_owned(),
                passed: report.drift_previous.pass,
                detail: None,
            },
            GateCheck {
                name: "drift_anchor".to_owned(),
                passed: report.drift_anchor.pass,
                detail: None,
            },
            GateCheck {
                name: "supported_regions".to_owned(),
                passed: report.supported_regressions.is_empty(),
                detail: None,
            },
            GateCheck {
                name: "converged".to_owned(),
                passed: converged,
                detail: (!converged)
                    .then(|| "the native stage exhausted its sweep budget".to_owned()),
            },
        ],
    }
}

fn recipe_params(model: &ControlResponseModel, recipe_pool: Option<&str>) -> Result<String> {
    Ok(serde_json::to_string(&RecipeStageParams {
        control_response: db::hex(&encode_crm(model)?),
        recipe_pool: recipe_pool.map(str::to_owned),
    })?)
}

/// The recipe-stage response the record holds, when it was fitted on `recipe_pool`.
fn stored_recipe_model(params: &str, recipe_pool: &str) -> Result<Option<ControlResponseModel>> {
    let stored: RecipeStageParams = serde_json::from_str(params)?;
    if stored.recipe_pool.as_deref() != Some(recipe_pool) {
        return Ok(None);
    }
    let bytes = unhex(&stored.control_response)
        .ok_or_else(|| WorkerError::Data("recipe control response is not hex".to_owned()))?;
    Ok(Some(decode_crm(&bytes)?))
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let (pairs, remainder) = text.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

/// Upper bound on the tempered sample evaluations the stage processes.
const RECIPE_SWEEP_BUDGET: f64 = 1_000_000.0;

/// The recipe stage solves only body parameters, so its cost is bounded by the
/// tempered effective sample count rather than the raw sample count.
fn recipe_sweeps(images: usize, n_eff: f64) -> usize {
    let effective = n_eff * images as f64;
    (RECIPE_SWEEP_BUDGET / effective.max(1.0))
        .ceil()
        .clamp(2.0, 4.0) as usize
}

#[allow(clippy::too_many_arguments)]
async fn store_stage_params(
    db_pool: &SqlitePool,
    look: &LookSlug,
    version: u32,
    body: &BodyId,
    result: &FitResult,
    native_model: &ControlResponseModel,
    recipe_model: &ControlResponseModel,
    recipe_pool: Option<&str>,
    now: i64,
) -> Result<()> {
    let native = serde_json::json!({
        "clip_k": result.realization.clip_k,
        "exposure_offsets": result
            .realization
            .exposure_offsets
            .iter()
            .map(|offset| serde_json::json!({
                "mode": offset.mode.0,
                "offset": offset.offset,
            }))
            .collect::<Vec<_>>(),
        "control_response": db::hex(&encode_crm(native_model)?),
    });
    db::store_release_body_params(
        db_pool,
        look.as_str(),
        version,
        body.as_str(),
        BodyStage::Native,
        &native.to_string(),
        now,
    )
    .await?;
    db::store_release_body_params(
        db_pool,
        look.as_str(),
        version,
        body.as_str(),
        BodyStage::Recipe,
        &recipe_params(recipe_model, recipe_pool)?,
        now,
    )
    .await
}

#[derive(Debug, Default)]
struct ReputationCounts {
    total: u32,
    accepted: u32,
    quarantined: u32,
    passing_charts: u32,
}

async fn update_reputation(
    db_pool: &SqlitePool,
    look: &LookSlug,
    version: u32,
    pool: &LookPool,
    previous: &Option<Snapshot>,
) -> Result<()> {
    let params = TrustParams::default();
    let now = now_unix();
    let days = previous.as_ref().map_or(0.0, |snapshot| {
        (now - snapshot.record.created_at).max(0) as f64 / 86_400.0
    });
    let mut counts: BTreeMap<String, ReputationCounts> = BTreeMap::new();
    for id in pool.contribution_ids() {
        let contribution = db::contribution(db_pool, &id).await?;
        let statuses: Vec<String> =
            sqlx::query_scalar("SELECT status FROM contribution_images WHERE contribution_id = ?")
                .bind(id.as_str())
                .fetch_all(db_pool)
                .await?;
        let entry = counts
            .entry(contribution.credential.as_str().to_owned())
            .or_default();
        let mut all_accepted = !statuses.is_empty();
        for status in &statuses {
            entry.total += 1;
            match db::ImageOutcome::parse(status) {
                Some(db::ImageOutcome::Accepted) => entry.accepted += 1,
                Some(db::ImageOutcome::DownWeighted) => all_accepted = false,
                Some(db::ImageOutcome::Quarantined) => {
                    entry.quarantined += 1;
                    all_accepted = false;
                }
                None => all_accepted = false,
            }
        }
        if contribution.kind == looklab_upload::ContributionKind::Chart && all_accepted {
            entry.passing_charts += 1;
        }
    }
    for (credential, counts) in counts {
        if counts.total == 0 {
            continue;
        }
        let id = looklab_server::CredentialId::try_from(credential.clone())
            .map_err(|_| WorkerError::Data("credential id is not a UUID".to_owned()))?;
        let row: Option<f64> =
            sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
                .bind(id.as_str())
                .fetch_optional(db_pool)
                .await?;
        let current = looklab_trust::Reputation::saturating(row.unwrap_or(0.5));
        let updated = current.updated(
            &params,
            ReputationUpdate {
                accepted_fraction: f64::from(counts.accepted) / f64::from(counts.total),
                deliberate_quarantines: 0,
                automatic_quarantines: counts.quarantined,
                passing_chart_sessions: counts.passing_charts,
                days_elapsed: days,
            },
        );
        let mut tx = db_pool.begin().await?;
        db::apply_reputation_once(&mut tx, look.as_str(), version, &id, updated.value(), now)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}
