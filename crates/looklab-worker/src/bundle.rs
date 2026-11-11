// SPDX-License-Identifier: AGPL-3.0-or-later
//! Camera bundle assembly (docs/03 §5, docs/04 §2).
//!
//! Bundles publish independently of Look versions. A revision pins every
//! selected realization's family target, parameters and artifact hashes.

use looklab_publish::{ArtifactBlob, ArtifactName};
use looklab_schema::{
    ArtifactBinding, ArtifactRole, BodyId, BodyRealizationId, BodyStateVersion, BundleExclusion,
    BundleExclusionReason, BundleSimEntry, ControlEpochId, MatchBundleRevision, ModelSubject,
    ModelVersionId, ProjectionUse, PublishedTargetId, QualificationCapability,
    RealizationRevisionId, RealizationStatus, SelectionScope, Sha256Hex, SourceRef,
    TargetEligibilityStatus,
};
use looklab_server::model_store::{self, StoredRealizationRevision};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::body::DataDir;
use crate::config::WorkerConfig;
use crate::db;
use crate::error::{Result, WorkerError};

/// A revision plus its artifact bytes, ready for the bundle registry.
pub(crate) struct AssembledBundle {
    pub manifest: MatchBundleRevision,
    pub blobs: Vec<ArtifactBlob>,
    pub scope: SelectionScope,
}

impl AssembledBundle {
    fn artifact_bindings(&self) -> Vec<ArtifactBinding> {
        let mut bindings = Vec::new();
        for blob in &self.blobs {
            let role = match blob.name.as_str() {
                "body.yaml" => Some(ArtifactRole::Manifest),
                "transform.yaml" => Some(ArtifactRole::InputTransform),
                "profile.yaml" => Some(ArtifactRole::CalibrationProfile),
                "body_params.json" => Some(ArtifactRole::BodyRealizationParams),
                "gauge.json" => Some(ArtifactRole::Gauge),
                _ => None,
            };
            if let Some(role) = role
                && let Ok(hash) = Sha256Hex::parse(&db::hex(&Sha256::digest(&blob.bytes)))
            {
                bindings.push(ArtifactBinding { sha256: hash, role });
            }
        }
        for sim in &self.manifest.sims {
            bindings.push(ArtifactBinding {
                sha256: sim.base_artifact,
                role: ArtifactRole::Look,
            });
            bindings.push(ArtifactBinding {
                sha256: sim.control_response,
                role: ArtifactRole::ControlResponse,
            });
            if let Some(covariance) = sim.covariance {
                bindings.push(ArtifactBinding {
                    sha256: covariance,
                    role: ArtifactRole::Covariance,
                });
            }
            if let Some(params) = sim.realization_params {
                bindings.push(ArtifactBinding {
                    sha256: params,
                    role: ArtifactRole::RealizationParams,
                });
            }
        }
        bindings
    }
}

/// # Errors
/// Propagates assembly, storage or serialization failures.
pub(crate) async fn record_epoch_revision(
    pool: &SqlitePool,
    config: &WorkerConfig,
    snapshot_version: u64,
    snapshot_root: &std::path::Path,
    body: &BodyId,
    epoch: u32,
    body_state_version: BodyStateVersion,
) -> Result<Option<(looklab_schema::MatchBundleRevisionId, SelectionScope)>> {
    let Some(assembled) = assemble_epoch(
        pool,
        config,
        snapshot_version,
        snapshot_root,
        body,
        epoch,
        body_state_version,
    )
    .await?
    else {
        return Ok(None);
    };
    persist_blobs(config, &assembled.blobs)?;
    let manifest = assembled.manifest.manifest();
    let artifacts = assembled.artifact_bindings();
    let scope = assembled.scope;
    let mut tx = model_store::begin_immediate(pool).await?;
    let id = model_store::insert_bundle_revision(
        &mut tx,
        &model_store::NewBundleRevision {
            body,
            epoch,
            manifest: &manifest,
            artifacts: &artifacts,
            dependencies: &[],
        },
        crate::clock::now_unix(),
    )
    .await?;
    tx.commit().await?;
    Ok(Some((id, scope)))
}

struct Selected {
    revision: StoredRealizationRevision,
    scope: SelectionScope,
}

/// # Errors
/// Propagates storage, blob or transform failures.
pub(crate) async fn assemble_epoch(
    pool: &SqlitePool,
    config: &WorkerConfig,
    snapshot_version: u64,
    snapshot_root: &std::path::Path,
    body: &BodyId,
    epoch: u32,
    body_state_version: BodyStateVersion,
) -> Result<Option<AssembledBundle>> {
    let resources = DataDir::new(snapshot_root).load(body)?;
    let Some(control_epoch) = resources
        .schema
        .epochs
        .iter()
        .find(|candidate| candidate.id == epoch)
    else {
        return Ok(None);
    };
    if control_epoch.native_sims.is_empty() {
        return Ok(None);
    }
    let mut conn = pool.acquire().await?;
    let Some(state) = model_store::body_state(&mut conn, body, body_state_version).await? else {
        return Ok(None);
    };
    drop(conn);
    if state.stage != looklab_schema::BodyStateStage::Published {
        return Err(WorkerError::Data(format!(
            "{body} body state {} is not the published stage",
            body_state_version.get()
        )));
    }
    let schema_bytes = resources.schema.source().as_bytes().to_vec();
    let mut blobs = vec![ArtifactBlob::new(
        ArtifactName::new("body.yaml")?,
        schema_bytes,
    )];
    read_pin(
        config,
        &state.pins.input_transform,
        "transform.yaml",
        &mut blobs,
    )?;
    if let Some(profile) = &state.pins.profile {
        read_pin(config, profile, "profile.yaml", &mut blobs)?;
    }
    read_pin(
        config,
        &state.pins.body_params,
        "body_params.json",
        &mut blobs,
    )?;
    read_pin(config, &state.pins.gauge, "gauge.json", &mut blobs)?;
    let body_params = state.pins.body_params.sha256;
    let mut sims = Vec::new();
    let mut excluded = Vec::new();
    let mut transform_version = None;
    let mut params_version = None;
    let mut qualified = true;
    for native in &control_epoch.native_sims {
        let selected = match select(pool, body, epoch, &native.key).await? {
            Ok(selected) => selected,
            Err(reason) => {
                excluded.push(BundleExclusion {
                    native_sim: native.key.clone(),
                    reason,
                });
                continue;
            }
        };
        if selected.revision.subject.body != *body
            || selected.revision.subject.epoch.get() != epoch
            || selected.revision.subject.native_sim != native.key
            || selected.revision.target.family.as_str() != native.look.as_str()
        {
            excluded.push(BundleExclusion {
                native_sim: native.key.clone(),
                reason: BundleExclusionReason::IncompatibleRevision,
            });
            continue;
        }
        let compatible = transform_version
            .is_none_or(|version| version == selected.revision.input_transform_version)
            && params_version
                .is_none_or(|version| version == selected.revision.realization_params_version);
        if !compatible {
            excluded.push(BundleExclusion {
                native_sim: native.key.clone(),
                reason: BundleExclusionReason::IncompatibleRevision,
            });
            continue;
        }
        match bundle_sim(
            config,
            native.key.clone(),
            selected,
            body_params,
            &mut blobs,
        ) {
            Ok((sim, transform, params, scope)) => {
                transform_version.get_or_insert(transform);
                params_version.get_or_insert(params);
                qualified &= scope == SelectionScope::Qualified;
                sims.push(sim);
            }
            Err(reason) => excluded.push(BundleExclusion {
                native_sim: native.key.clone(),
                reason,
            }),
        }
    }
    if sims.is_empty() {
        return Ok(None);
    }
    let (Some(transform_version), Some(params_version)) = (transform_version, params_version)
    else {
        return Ok(None);
    };
    let manifest = MatchBundleRevision {
        id: looklab_schema::MatchBundleRevisionId::from_digest([0; 32]),
        body: body.clone(),
        epoch: ControlEpochId::new(epoch),
        schema_snapshot_version: snapshot_version,
        input_transform_version: transform_version,
        body_state_version,
        body_realization_params_version: params_version,
        sims,
        excluded,
        manifest_hash: looklab_schema::MatchBundleRevisionId::from_digest([0; 32]),
        producer: SourceRef::new(format!("looklab-worker/{}", env!("CARGO_PKG_VERSION")))
            .map_err(|error| WorkerError::Data(error.to_string()))?,
        created_at: crate::clock::now_unix(),
    }
    .sealed();
    Ok(Some(AssembledBundle {
        manifest,
        blobs,
        scope: if qualified {
            SelectionScope::Qualified
        } else {
            SelectionScope::Experimental
        },
    }))
}

fn digest(bytes: &[u8]) -> Result<Sha256Hex> {
    Ok(Sha256Hex::from_bytes(Sha256::digest(bytes).into()))
}

fn persist_blobs(config: &WorkerConfig, blobs: &[ArtifactBlob]) -> Result<()> {
    use looklab_server::{BlobHash, BlobStore as _, FsBlobStore};

    let store =
        FsBlobStore::new(&config.blobs).map_err(|error| WorkerError::Data(error.to_string()))?;
    for blob in blobs {
        let hash = BlobHash::of(&blob.bytes);
        let path = config.blobs.join(hash.to_hex());
        match std::fs::read(&path) {
            Ok(existing) if existing == blob.bytes => continue,
            Ok(_) => {
                return Err(WorkerError::Data(format!(
                    "{}: stored bytes do not match their content address",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                store
                    .put(hash, &blob.bytes)
                    .map_err(|error| WorkerError::Data(error.to_string()))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_pin(
    config: &WorkerConfig,
    binding: &ArtifactBinding,
    name: &str,
    blobs: &mut Vec<ArtifactBlob>,
) -> Result<()> {
    let hash = binding.sha256.to_hex();
    let path = config.blobs.join(&hash);
    let bytes = std::fs::read(&path)
        .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
    let actual = digest(&bytes)?.to_hex();
    if actual != hash {
        return Err(WorkerError::Data(format!(
            "{}: bytes hash to {actual}, the body state pins {hash}",
            path.display()
        )));
    }
    blobs.push(ArtifactBlob::new(ArtifactName::new(name)?, bytes));
    Ok(())
}

fn bundle_sim(
    config: &WorkerConfig,
    sim: looklab_schema::SimKey,
    selected: Selected,
    body_params: Sha256Hex,
    blobs: &mut Vec<ArtifactBlob>,
) -> std::result::Result<(BundleSimEntry, u32, u32, SelectionScope), BundleExclusionReason> {
    let Selected { revision, scope } = selected;
    let base = binding(&revision, ArtifactRole::Look)
        .ok_or(BundleExclusionReason::NoPublishedRealization)?;
    let crm = binding(&revision, ArtifactRole::ControlResponse)
        .ok_or(BundleExclusionReason::NoControlResponse)?;
    let covariance = binding(&revision, ArtifactRole::Covariance);
    let realization_params = binding(&revision, ArtifactRole::RealizationParams)
        .ok_or(BundleExclusionReason::IncompatibleRevision)?;
    for binding in [Some(base), Some(crm), covariance, Some(realization_params)]
        .into_iter()
        .flatten()
    {
        read(config, binding, blobs)?;
    }
    Ok((
        BundleSimEntry {
            native_sim: sim,
            realization: revision.id.clone(),
            family_version: revision.target.clone(),
            base_artifact: base.sha256,
            control_response: crm.sha256,
            covariance: covariance.map(|binding| binding.sha256),
            control_response_version: revision.control_response_version,
            clip_k: revision.clip_k,
            realization_params_version: revision.realization_params_version,
            body_params: Some(body_params),
            realization_params: Some(realization_params.sha256),
        },
        revision.input_transform_version,
        revision.realization_params_version,
        scope,
    ))
}

fn binding(revision: &StoredRealizationRevision, role: ArtifactRole) -> Option<&ArtifactBinding> {
    revision
        .artifacts
        .iter()
        .find(|binding| binding.role == role)
}

fn read(
    config: &WorkerConfig,
    binding: &ArtifactBinding,
    blobs: &mut Vec<ArtifactBlob>,
) -> std::result::Result<(), BundleExclusionReason> {
    let hash = binding.sha256.to_hex();
    if blobs.iter().any(|blob| blob.name.as_str() == hash.as_str()) {
        return Ok(());
    }
    let path = config.blobs.join(&hash);
    let bytes = std::fs::read(&path).map_err(|_| BundleExclusionReason::IncompatibleRevision)?;
    if db::hex(&Sha256::digest(&bytes)) != hash {
        return Err(BundleExclusionReason::IncompatibleRevision);
    }
    let name =
        ArtifactName::new(hash.clone()).map_err(|_| BundleExclusionReason::IncompatibleRevision)?;
    blobs.push(ArtifactBlob::new(name, bytes));
    Ok(())
}

async fn select(
    pool: &SqlitePool,
    body: &BodyId,
    epoch: u32,
    sim: &looklab_schema::SimKey,
) -> Result<std::result::Result<Selected, BundleExclusionReason>> {
    let subject = ModelSubject::BodyRealization {
        realization: BodyRealizationId {
            body: body.clone(),
            epoch: ControlEpochId::new(epoch),
            native_sim: sim.clone(),
        },
    }
    .key();
    let mut conn = pool.acquire().await?;
    let projections =
        model_store::current_projections(&mut conn, &subject, ProjectionUse::QualifiedMatch)
            .await?;
    let qualified = projections.iter().find_map(|projection| {
        (projection.capability == QualificationCapability::ControlsMatching
            && projection.applicability.is_current())
        .then(|| projection.selected_candidate.clone())
        .flatten()
    });
    let mut revisions = model_store::realization_revisions(&mut conn, Some(&subject)).await?;
    revisions.sort_by_key(|revision| revision.version);
    let had_revisions = !revisions.is_empty();
    let qualified_position = match qualified {
        Some(ModelVersionId::BodyRealization { realization }) => {
            revisions.iter().position(|revision| {
                RealizationRevisionId::for_version(&realization) == revision.id
                    && revision.status == RealizationStatus::Candidate
                    && revision.safety.accepted()
            })
        }
        _ => None,
    };
    if let Some(position) = qualified_position {
        let revision = revisions.remove(position);
        return Ok(Ok(Selected {
            revision,
            scope: SelectionScope::Qualified,
        }));
    }
    for revision in revisions.into_iter().rev() {
        if revision.status != RealizationStatus::Candidate || !revision.safety.accepted() {
            continue;
        }
        let published = PublishedTargetId::from(&revision.target);
        let eligible = model_store::target_eligibility(&mut conn, &published)
            .await?
            .is_some_and(|eligibility| eligibility.status == TargetEligibilityStatus::Eligible);
        if !eligible {
            continue;
        }
        return Ok(Ok(Selected {
            revision,
            scope: SelectionScope::Experimental,
        }));
    }
    Ok(Err(if had_revisions {
        BundleExclusionReason::IncompatibleRevision
    } else {
        BundleExclusionReason::NoPublishedRealization
    }))
}
