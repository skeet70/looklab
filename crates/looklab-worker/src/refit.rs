// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};

use looklab_fit::{FitOptions, FitResult, fit_controlled, refit_responses};
use looklab_model::realization::{ControlResponseModel, decode_crm, encode_crm};
use looklab_model::{Density, Look, LookDocument, ProbeCovariance, ProbeId, UncertaintyData};
use looklab_publish::{
    BodyUse, GateCheck, GateSummary, PREVIEW_LUT_ARTIFACT, ProvenanceSummary, bake_preview_lut,
};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_trust::{
    GateInput, GateReport, ReputationUpdate, TrustParams, VersionId, VersionProbes, gate_release,
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
    pub sigma: f64,
    /// The probe's density reached the fit's floor.
    #[serde(default)]
    pub density_ok: bool,
    pub supported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRecord {
    pub version: u32,
    pub gauge: f64,
    pub anchor: u32,
    pub probes: Vec<StoredProbe>,
    pub gate: GateReport,
    pub provenance: ProvenanceSummary,
    pub created_at: i64,
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
    }
    Ok(db::hex(&hasher.finalize()))
}

async fn refit_look(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
    ownership: &JobOwnership,
) -> Result<RefitOutcome> {
    let pool = pool::build(db_pool, config, body, look).await?;
    if pool.images.is_empty() {
        return Ok(RefitOutcome::Skipped);
    }
    let roots = roots::load(db_pool, config, &pool).await?;
    let params = TrustParams::default();
    let weighted = weights::weigh(&pool, &params, &roots.weights)?;
    let contributors = weights::contributor_index(&pool);
    let native_images = weights::fit_images(&pool, &weighted, &contributors);
    let recipe_images = weights::recipe_fit_images(&pool, &weighted, &contributors);

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

    let fingerprint = pool_fingerprint(&pool)?;
    let stored = db::pool_fingerprint(db_pool, look.as_str(), body.as_str()).await?;
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

    let options = FitOptions {
        seed: config.seed,
        max_samples_per_image: config.max_samples_per_image,
        ..FitOptions::default()
    };
    if native_images.is_empty() {
        return run_recipe_only(
            db_pool,
            &pool,
            &recipe_images,
            &previous,
            &options,
            look,
            body,
        )
        .await;
    }
    let (epoch, sim) = roots::base_sim(&pool)?;
    let native_result = fit_controlled(
        &native_images,
        &pool.resources.schema,
        epoch,
        &sim,
        &options,
    )?;
    let native_model = native_result
        .control_response
        .as_ref()
        .map(|fit| fit.model.clone())
        .ok_or_else(|| WorkerError::Data("native stage produced no control response".to_owned()))?;
    let control = refit_responses(
        &native_result.look,
        &recipe_images,
        &pool.resources.schema,
        epoch,
        &sim,
        &native_model,
        &options,
    )?;

    crate::split::detect_and_propose(db_pool, config, &pool, &native_result).await?;
    let gauge = gauge(&pool, &weighted, &native_result);
    let candidate_probes = candidate_probes(&native_result);

    let anchor_version = select_anchor(db_pool, look, &versions).await?;
    let anchor = match &anchor_version {
        Some(version) => Some(load_snapshot(db_pool, config, look, *version).await?),
        None => None,
    };
    let anchor_gauge = anchor.as_ref().map(|snapshot| snapshot.record.gauge);

    let candidate = canonical_probes(
        &native_result.look,
        gauge,
        anchor_gauge.unwrap_or(gauge),
        options.support_sigma,
        &candidate_probes,
    );
    let previous_probes = match &previous {
        Some(snapshot) => canonical_probes(
            &snapshot.look,
            snapshot.record.gauge,
            anchor_gauge.unwrap_or(gauge),
            options.support_sigma,
            &snapshot.record.probes,
        ),
        None => candidate.clone(),
    };
    let anchor_probes = match &anchor {
        Some(snapshot) => canonical_probes(
            &snapshot.look,
            snapshot.record.gauge,
            anchor_gauge.unwrap_or(gauge),
            options.support_sigma,
            &snapshot.record.probes,
        ),
        None => previous_probes.clone(),
    };

    let gate = gate_release(
        &params,
        &GateInput {
            candidate,
            previous: previous_probes,
            anchor: anchor_probes,
            root_observations: roots.observations.clone(),
        },
    );

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
    let release = ReleaseRecord {
        version: next_version,
        gauge,
        anchor: anchor_version.unwrap_or(next_version),
        probes: candidate_probes,
        gate: gate.clone(),
        provenance: provenance.clone(),
        created_at: now,
    };
    let artifacts = vec![
        store_artifact(config, LOOK_ARTIFACT, &document_bytes)?,
        store_artifact(config, PREVIEW_LUT_ARTIFACT, &bake_preview_lut(&document))?,
        store_artifact(config, CRM_ARTIFACT, &encode_crm(&control.model)?)?,
        store_artifact(config, RELEASE_ARTIFACT, &serde_json::to_vec(&release)?)?,
    ];
    store_stage_params(
        db_pool,
        look,
        next_version,
        body,
        &native_result,
        &native_model,
        &control.model,
        now,
    )
    .await?;
    admin::write_version_manifest(
        db_pool,
        look,
        VersionId::new(next_version),
        &artifacts,
        &roots.admissions,
    )
    .await?;
    update_reputation(db_pool, look, next_version, &pool, &previous).await?;
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

async fn run_recipe_only(
    db_pool: &SqlitePool,
    pool: &LookPool,
    recipe_images: &[looklab_fit::FitImage],
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
    let control = refit_responses(
        &previous.look,
        recipe_images,
        &pool.resources.schema,
        epoch,
        &sim,
        &model,
        options,
    )?;
    let now = now_unix();
    db::store_release_body_params(
        db_pool,
        look.as_str(),
        previous.record.version,
        body.as_str(),
        BodyStage::Recipe,
        &crm_params(&control.model)?,
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
    let mut covariance = Vec::with_capacity(result.probes.probes.len() * 9);
    let mut density = Vec::with_capacity(result.probes.probes.len());
    for probe in &result.probes.probes {
        for row in &probe.covariance {
            covariance.extend_from_slice(row);
        }
        density.push(probe.density);
    }
    document = document.with_uncertainty(UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(
            result.probes.probes.len(),
            covariance,
        )?),
        density: Some(Density::new(density)?),
    })?;
    let bytes = document.encode()?;
    Ok((document, bytes))
}

fn candidate_probes(result: &FitResult) -> Vec<StoredProbe> {
    result
        .probes
        .ids
        .iter()
        .zip(&result.probes.probes)
        .map(|(id, probe)| StoredProbe {
            id: id.clone(),
            sigma: probe.sigma_de,
            density_ok: probe.density >= result.density.floor,
            supported: probe.supported,
        })
        .collect()
}

fn canonical_probes(
    look: &Look,
    gauge: f64,
    anchor_gauge: f64,
    support_sigma: f64,
    stored: &[StoredProbe],
) -> VersionProbes {
    let by_id: BTreeMap<&ProbeId, &StoredProbe> =
        stored.iter().map(|probe| (&probe.id, probe)).collect();
    let scale = 2.0_f64.powf(gauge - anchor_gauge);
    let probes = looklab_model::probes::v1()
        .probes
        .into_iter()
        .map(|probe| {
            let stored = by_id.get(&probe.id);
            // A record written before `density_ok` existed keeps its support flag.
            let (sigma, density_ok, supported) = stored.map_or((1.0, true, true), |probe| {
                (
                    probe.sigma,
                    probe.density_ok || probe.supported,
                    probe.supported,
                )
            });
            looklab_trust::ProbeEstimate::from_fit(
                probe.id.clone(),
                look.eval(probe.xyz.scale(scale)).to_array(),
                sigma,
                density_ok,
                supported,
                support_sigma,
            )
        })
        .collect();
    VersionProbes { probes }
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

pub fn gate_summary(report: &GateReport) -> GateSummary {
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
        ],
    }
}

fn crm_params(model: &ControlResponseModel) -> Result<String> {
    Ok(serde_json::json!({
        "control_response": db::hex(&encode_crm(model)?),
    })
    .to_string())
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
        &crm_params(recipe_model)?,
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
