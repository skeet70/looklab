// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rebases published `lut_defined` Looks when their display base changes (docs/05 §8).

use std::collections::HashSet;

use looklab_color::RgbSpace;
use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_export::{
    CubeFile, Lut3d, LutLookOptions, approximation_publishable, checked_appearance,
    import_lut_look, lut_data_from_cube, read_cube,
};
use looklab_fit::post_clip;
use looklab_model::file::{
    BaseReference, BaseStatus, DeclaredEncoding, LookVersion, LutData, LutDefinition, LutEmulation,
    Provenance, RootProvenance,
};
use looklab_model::{LookDocument, LookIdentity, LookKind, ProbeCovariance, WbPolicy, probes};
use looklab_publish::{
    ArtifactEntry, ArtifactHash, ArtifactName, BodyUse, PREVIEW_LUT_ARTIFACT, PreviewBaseline,
    ProvenanceSummary, RELEASE_ARTIFACT, SOURCE_CUBE_ARTIFACT, SOURCE_LUT_ARTIFACT,
    SourceLutPreview, SourceLutPreviewContext, VersionLookShape, bake_lut_preview,
    bake_preview_lut, look_shape_names,
};
use looklab_schema::DependencyGeneration;
use looklab_server::admin::{self, ManifestArtifact};
use looklab_trust::{
    GateInput, GatedUnion, ProbeEstimate, ProbeSupport, TrustParams, VersionId, VersionProbes,
    gate_release,
};
use looklab_upload::LookSlug;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};

use crate::clock::{now_unix, rfc3339};
use crate::config::WorkerConfig;
use crate::db::{self, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::jobs::JobOwnership;
use crate::refit::{GaugeConvention, LOOK_ARTIFACT, ReleaseRecord, StoredProbe};

/// Records a published version's base or native camera rendering.
pub async fn record_lut_pin(
    db: &SqlitePool,
    look: &LookSlug,
    version: u32,
    document: &LookDocument,
) -> Result<()> {
    if let Some(definition) = &document.file.definition {
        let DeclaredEncoding::DisplayReferred { base, status } = &definition.encoding else {
            return Ok(());
        };
        admin::record_lut_base(db, look, version, base, status).await?;
    } else if let Some(rendering) = &document.file.rendering {
        admin::record_native_rendering(db, look, version, rendering).await?;
    }
    Ok(())
}

pub async fn run(pool: &SqlitePool, config: &WorkerConfig, ownership: &JobOwnership) -> Result<()> {
    refresh_published_documents(pool, config).await?;
    admin::mark_lut_dependents(pool, now_unix()).await?;
    for work in admin::pending_lut_rebases(pool).await? {
        rebase_one(pool, config, ownership, &work).await?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RebaseOutcome {
    Published,
    Held,
    Blocked,
    Skipped,
}

enum PendingLook {
    Layered(Box<LookDocument>),
    SourceOnly(Box<SourceLutPreview>),
}

struct BaseTarget {
    look: LookSlug,
    version: u32,
    document: LookDocument,
    provenance: Provenance,
}

/// The published state the base selection read, rechecked before mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TargetState {
    looks: Vec<(String, Vec<(u32, String)>)>,
    renderings: Vec<(String, i64, String, String)>,
}

async fn target_state_conn(
    conn: &mut SqliteConnection,
    looks: &[LookSlug],
    family: bool,
) -> Result<TargetState> {
    let mut state_looks = Vec::with_capacity(looks.len());
    for look in looks {
        let versions = versions_conn(&mut *conn, look).await?;
        let mut rows = Vec::with_capacity(versions.len());
        for (version, status) in versions {
            rows.push((version, status.as_str().to_owned()));
        }
        state_looks.push((look.as_str().to_owned(), rows));
    }
    state_looks.sort_by(|left, right| left.0.cmp(&right.0));
    state_looks.dedup_by(|left, right| left.0 == right.0);
    let renderings: Vec<(String, i64, String, String)> = if family {
        sqlx::query_as(
            "SELECT look, version, maker, rendering FROM lut_native_renderings
             ORDER BY look, version",
        )
        .fetch_all(&mut *conn)
        .await?
    } else {
        Vec::new()
    };
    Ok(TargetState {
        looks: state_looks,
        renderings,
    })
}

async fn target_state_matches_conn(
    conn: &mut SqliteConnection,
    fitted: &TargetState,
    family: bool,
) -> Result<bool> {
    let mut looks = Vec::with_capacity(fitted.looks.len());
    for (look, _) in &fitted.looks {
        looks.push(
            LookSlug::try_from(look.clone())
                .map_err(|_| WorkerError::Data("base slug".to_owned()))?,
        );
    }
    Ok(target_state_conn(&mut *conn, &looks, family).await? == *fitted)
}

async fn refresh_published_documents(pool: &SqlitePool, config: &WorkerConfig) -> Result<()> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT v.look, v.version FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.status IN ('published', 'rooted')
         ORDER BY v.look, v.version",
    )
    .fetch_all(pool)
    .await?;
    for (look, version) in rows {
        let look =
            LookSlug::try_from(look).map_err(|_| WorkerError::Data("look slug".to_owned()))?;
        let version =
            u32::try_from(version).map_err(|_| WorkerError::Data("look version".to_owned()))?;
        let Some(artifacts) = db::manifest_artifacts(pool, look.as_str(), version).await? else {
            continue;
        };
        let shape = look_shape_names(artifacts.iter().map(|artifact| artifact.name.as_str()))
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        match shape {
            VersionLookShape::Layered => {
                let document = load_document(pool, config, &look, version).await?;
                record_lut_pin(pool, &look, version, &document).await?;
            }
            VersionLookShape::SourcePreview => {
                let bytes = read_artifact(config, &artifacts, SOURCE_LUT_ARTIFACT)?;
                let descriptor = SourceLutPreview::decode(&bytes)
                    .map_err(|error| WorkerError::Data(error.to_string()))?;
                let DeclaredEncoding::DisplayReferred { base, status } = &descriptor.encoding
                else {
                    return Err(WorkerError::Data(
                        "source-only LUT preview is not display-referred".to_owned(),
                    ));
                };
                admin::record_lut_base(pool, &look, version, base, status).await?;
            }
        }
    }
    Ok(())
}

async fn rebase_one(
    pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    work: &admin::LutRebaseWork,
) -> Result<RebaseOutcome> {
    ownership.check(pool).await?;
    let Some(pin) = admin::lut_base_pin(pool, &work.look, work.version).await? else {
        return Ok(RebaseOutcome::Blocked);
    };
    let artifacts = db::manifest_artifacts(pool, work.look.as_str(), work.version)
        .await?
        .ok_or_else(|| WorkerError::Data("pending rebase has no version manifest".to_owned()))?;
    let shape = look_shape_names(artifacts.iter().map(|artifact| artifact.name.as_str()))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let pending = match shape {
        VersionLookShape::Layered => PendingLook::Layered(Box::new(
            load_document(pool, config, &work.look, work.version).await?,
        )),
        VersionLookShape::SourcePreview => {
            let bytes = read_artifact(config, &artifacts, SOURCE_LUT_ARTIFACT)?;
            PendingLook::SourceOnly(Box::new(
                SourceLutPreview::decode(&bytes)
                    .map_err(|error| WorkerError::Data(error.to_string()))?,
            ))
        }
    };
    let source_only = matches!(pending, PendingLook::SourceOnly(_));
    let Some((target, target_state)) = resolve_base(pool, config, &pin.base, source_only).await?
    else {
        if source_only && work.reason != admin::LutRebaseReason::BaseRevoked {
            return clear_pending_work(pool, ownership, &work.look, work.version).await;
        }
        return Ok(RebaseOutcome::Blocked);
    };
    let document = match pending {
        PendingLook::SourceOnly(descriptor) => {
            return rebase_source_only(
                pool,
                config,
                ownership,
                work,
                &descriptor,
                &artifacts,
                &pin.base,
                &target,
                &target_state,
            )
            .await;
        }
        PendingLook::Layered(document) => document,
    };
    let family = matches!(pin.base, BaseReference::FamilyRendering { .. });
    let pinned_version = match &pin.base {
        BaseReference::Look { version, .. } => Some(pin_version(version)?),
        _ => None,
    };
    if pinned_version == Some(target.version)
        && work.reason != admin::LutRebaseReason::FamilyResolved
    {
        return clear_stale_work(
            pool,
            ownership,
            &work.look,
            work.version,
            &target.look,
            target.version,
            &target_state,
            family,
        )
        .await;
    }

    let versions = db::look_versions(pool, work.look.as_str()).await?;
    let next = versions
        .iter()
        .map(|version| version.version)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| WorkerError::Data("version overflow".to_owned()))?;
    if db::gate_failure_pending(pool, work.look.as_str(), next).await? {
        return Ok(RebaseOutcome::Held);
    }
    let now = now_unix();

    let mut anchor = anchor_version(pool, &work.look, &versions, work.version).await?;
    let previous = versions
        .iter()
        .filter(|version| {
            matches!(
                version.status,
                LookVersionStatus::Published | LookVersionStatus::Rooted
            )
        })
        .map(|version| version.version)
        .max()
        .unwrap_or(work.version);
    let mut anchor_document = load_optional_document(pool, config, &work.look, anchor).await?;
    if anchor_document.is_none() {
        for version in versions.iter().filter(|version| {
            version.version > anchor
                && version.version <= previous
                && matches!(
                    version.status,
                    LookVersionStatus::Published | LookVersionStatus::Rooted
                )
        }) {
            if let Some(document) =
                load_optional_document(pool, config, &work.look, version.version).await?
            {
                anchor = version.version;
                anchor_document = Some(document);
                break;
            }
        }
    }
    let Some(anchor_document) = anchor_document else {
        return Ok(RebaseOutcome::Blocked);
    };
    let anchor_probes = document_probes(&anchor_document);
    let previous_probes = match load_optional_document(pool, config, &work.look, previous).await? {
        Some(document) => document_probes(&document),
        None => anchor_probes.clone(),
    };
    let mut gated = GatedUnion::at_anchor(&anchor_probes);
    for version in versions.iter().filter(|version| {
        version.version > anchor
            && version.version <= previous
            && matches!(
                version.status,
                LookVersionStatus::Published | LookVersionStatus::Rooted
            )
    }) {
        if let Some(document) =
            load_optional_document(pool, config, &work.look, version.version).await?
        {
            gated.include(&document_probes(&document));
        }
    }

    let base = look_reference(&target.look, target.version)?;
    let base_dependency_snapshot = crate::lut_import::base_snapshot(pool, Some(&base)).await?;
    let (mut candidate_definition, cube): (LutDefinition, CubeFile) = {
        let definition = document.file.definition.clone().ok_or_else(|| {
            WorkerError::Data("pending rebase is not a lut_defined Look".to_owned())
        })?;
        let cube =
            cube_from_lut(document.lut.as_ref().ok_or_else(|| {
                WorkerError::Data("lut_defined Look has no stored LUT".to_owned())
            })?)?;
        (definition, cube)
    };
    let declared = candidate_definition
        .declared_base
        .clone()
        .or_else(|| candidate_definition.encoding.base().cloned());
    let exact_new = match &declared {
        Some(declared) => declared_is_exact(declared, &target.look, target.version)?,
        None => true,
    };
    let (pin_base, pin_status) = match &declared {
        Some(declared) if !exact_new => (declared.clone(), BaseStatus::BaseIdentifiedUnmeasured),
        _ => (base.clone(), BaseStatus::Pinned),
    };
    candidate_definition.declared_base = if exact_new { None } else { declared };
    candidate_definition.encoding = DeclaredEncoding::DisplayReferred {
        base: base.clone(),
        status: BaseStatus::Pinned,
    };
    let identity = LookIdentity::new(
        work.look.as_str(),
        &format!("{next}.0.0"),
        LookKind::LutDefined,
        WbPolicy::Adaptive,
    )?;
    let imported = import_lut_look(
        identity,
        candidate_definition,
        &cube,
        Some(&target.document),
        LutLookOptions {
            seed: config.seed,
            ..LutLookOptions::default()
        },
    )?;
    if !imported.converged {
        let report = serde_json::json!({
            "converged": false,
            "reason": "the rebased LUT fit did not converge",
        })
        .to_string();
        return hold_failed_candidate(
            pool,
            ownership,
            &work.look,
            work.version,
            next,
            &target.look,
            target.version,
            &target_state,
            family,
            &report,
            now,
        )
        .await;
    }
    let mut document = imported.document;
    if !document.is_matchable() {
        return Ok(RebaseOutcome::Blocked);
    }

    let candidate_probes = document_probes(&document);
    let mut published_gated = gated.clone();
    published_gated.include(&candidate_probes);
    let gate_input = GateInput {
        candidate: candidate_probes.clone(),
        previous: previous_probes,
        anchor: anchor_probes,
        root_observations: Vec::new(),
        gated_since_anchor: gated,
    };
    let report = gate_release(&TrustParams::default(), &gate_input);
    if !report.passed {
        return hold_failed_candidate(
            pool,
            ownership,
            &work.look,
            work.version,
            next,
            &target.look,
            target.version,
            &target_state,
            family,
            &serde_json::to_string(&report)?,
            now,
        )
        .await;
    }

    document.file.provenance = lut_provenance(&document, &target.provenance, anchor, now);
    let provenance = ProvenanceSummary {
        bodies: target
            .provenance
            .bodies
            .iter()
            .map(|body| BodyUse {
                body: body.body.clone(),
                epochs: body.epochs.clone(),
                images: body.images,
                credentials: body.credentials,
            })
            .collect(),
        root_admissions: target.provenance.root.admissions.clone(),
        anchor_version: format!("{anchor}.0.0"),
        community_snapshot_version: target.provenance.community_snapshot_version,
        prep_versions: target.provenance.prep_versions.clone(),
        created: rfc3339(now),
        software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
    };

    let mut release_probes = Vec::with_capacity(candidate_probes.probes.len());
    for probe in &candidate_probes.probes {
        release_probes.push(StoredProbe {
            id: probe.id.clone(),
            sigma: probe.sigma_pre_clip,
            sigma_post: probe.sigma_post_clip,
            bias_de: probe.bias_de,
            pre_clip_identified: probe.pre_clip_identified,
            support: probe.support,
        });
    }
    let release = ReleaseRecord {
        version: next,
        gauge: 1.0,
        gauge_convention: GaugeConvention::Relative,
        anchor,
        clip_k: document.look.clip.k,
        output_space: RgbSpace::Srgb,
        probes: release_probes,
        gated_since_anchor: published_gated,
        converged: true,
        gate: Some(report),
        approximation_p95: None,
        provenance,
        created_at: now,
    };
    let short: String = work.look.as_str().chars().take(27).collect();
    let artifacts = vec![
        crate::lut_import::put_artifact(config, LOOK_ARTIFACT, &document.encode()?)?,
        crate::lut_import::put_artifact(
            config,
            looklab_publish::CARD_ARTIFACT,
            &write_card(
                &document,
                &CardMeta::new(definition_name(&document, work.look.as_str()), short),
                &CardOptions::default(),
            )?,
        )?,
        crate::lut_import::put_artifact(
            config,
            PREVIEW_LUT_ARTIFACT,
            &bake_preview_lut(&document),
        )?,
        crate::lut_import::put_artifact(config, RELEASE_ARTIFACT, &serde_json::to_vec(&release)?)?,
    ];
    commit_candidate(
        pool,
        ownership,
        &work.look,
        work.version,
        next,
        &artifacts,
        &pin_base,
        &pin_status,
        &base,
        &target.look,
        target.version,
        &base_dependency_snapshot,
        admin::VersionStatus::Published,
    )
    .await
}

fn look_reference(look: &LookSlug, version: u32) -> Result<BaseReference> {
    Ok(BaseReference::Look {
        slug: looklab_model::file::Slug::new(look.as_str())
            .map_err(|_| WorkerError::Data("base slug".to_owned()))?,
        version: LookVersion::new(format!("{version}.0.0"))
            .map_err(|_| WorkerError::Data("base version".to_owned()))?,
    })
}

pub(crate) struct PreviewTarget {
    pub base: BaseReference,
    pub version: u32,
    pub document: LookDocument,
}

pub(crate) fn preview_target_is_exact(
    declared: &BaseReference,
    target: &PreviewTarget,
) -> Result<bool> {
    let BaseReference::Look { slug, .. } = &target.base else {
        return Ok(false);
    };
    let target_look =
        LookSlug::new(slug.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?;
    declared_is_exact(declared, &target_look, target.version)
}

fn declared_is_exact(
    declared: &BaseReference,
    target_look: &LookSlug,
    target_version: u32,
) -> Result<bool> {
    Ok(match declared {
        BaseReference::Look { slug, version } => {
            target_look.as_str() == slug.as_str() && pin_version(version)? == target_version
        }
        BaseReference::FamilyRendering { .. } => true,
        BaseReference::BodyRealization { .. } => false,
    })
}

pub(crate) async fn resolve_preview_base(
    pool: &SqlitePool,
    config: &WorkerConfig,
    base: &BaseReference,
) -> Result<Option<PreviewTarget>> {
    let Some((target, _)) = resolve_base(pool, config, base, false).await? else {
        return Ok(None);
    };
    Ok(Some(PreviewTarget {
        base: look_reference(&target.look, target.version)?,
        version: target.version,
        document: target.document,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn rebase_source_only(
    pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    work: &admin::LutRebaseWork,
    descriptor: &SourceLutPreview,
    artifacts: &[ManifestArtifact],
    declared_base: &BaseReference,
    target: &BaseTarget,
    target_state: &TargetState,
) -> Result<RebaseOutcome> {
    let exact = declared_is_exact(declared_base, &target.look, target.version)?;
    let declared_status = match &descriptor.encoding {
        DeclaredEncoding::DisplayReferred { status, .. } => *status,
        DeclaredEncoding::SceneReferred { .. } => {
            return Err(WorkerError::Data(
                "source-only preview is not display-referred".to_owned(),
            ));
        }
    };
    let family = matches!(declared_base, BaseReference::FamilyRendering { .. });
    let versions = db::look_versions(pool, work.look.as_str()).await?;
    let next = versions
        .iter()
        .map(|version| version.version)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| WorkerError::Data("version overflow".to_owned()))?;

    let source = read_artifact(config, artifacts, SOURCE_CUBE_ARTIFACT)?;
    if ArtifactHash::of(&source) != descriptor.source.sha256
        || source.len() as u64 != descriptor.source.bytes
    {
        return Err(WorkerError::Data(
            "the source cube does not match the descriptor".to_owned(),
        ));
    }
    let text = std::str::from_utf8(&source)
        .map_err(|_| WorkerError::Data("the source LUT is not text".to_owned()))?;
    let cube = read_cube(text).map_err(|error| WorkerError::Data(error.to_string()))?;
    let record = lut_data_from_cube(&cube)
        .map_err(|error| WorkerError::Data(error.to_string()))?
        .record();

    let target_base = look_reference(&target.look, target.version)?;
    if !exact && descriptor.preview.baseline.approximation() == Some(&target_base) {
        return clear_stale_work(
            pool,
            ownership,
            &work.look,
            work.version,
            &target.look,
            target.version,
            target_state,
            family,
        )
        .await;
    }

    let mut fitted = None;
    'fit: {
        if exact && db::gate_failure_pending(pool, work.look.as_str(), next).await? {
            return Ok(RebaseOutcome::Held);
        }
        let definition = LutDefinition {
            name: descriptor.name.clone(),
            license: descriptor.license.clone(),
            encoding: DeclaredEncoding::DisplayReferred {
                base: target_base.clone(),
                status: BaseStatus::Pinned,
            },
            declared_base: if exact {
                None
            } else {
                Some(declared_base.clone())
            },
            output: descriptor.output,
            lut: record.clone(),
            emulates: descriptor.emulates.clone(),
            superseded_by: None,
            notes: Vec::new(),
        };
        let identity = LookIdentity::new(
            work.look.as_str(),
            &format!("{next}.0.0"),
            descriptor.look.kind,
            descriptor.look.wb_policy,
        )?;
        let imported = match import_lut_look(
            identity,
            definition,
            &cube,
            Some(&target.document),
            LutLookOptions {
                seed: config.seed,
                ..LutLookOptions::default()
            },
        ) {
            Ok(imported) => imported,
            Err(error) if exact => return Err(error.into()),
            Err(_) => break 'fit,
        };
        let now = now_unix();
        if !imported.converged {
            if exact {
                let report = serde_json::json!({
                    "converged": false,
                    "reason": "the first fitted LUT representation did not converge",
                })
                .to_string();
                db::insert_gate_failure(pool, work.look.as_str(), next, &report, now).await?;
                return Ok(RebaseOutcome::Held);
            }
            break 'fit;
        }
        if !approximation_publishable(&imported.approximation) {
            if exact {
                let report = serde_json::json!({
                    "converged": true,
                    "reason": "the first fitted LUT representation exceeds the LUT publication accuracy gate",
                })
                .to_string();
                db::insert_gate_failure(pool, work.look.as_str(), next, &report, now).await?;
                return Ok(RebaseOutcome::Held);
            }
            break 'fit;
        }
        let approximation_p95 = imported.approximation.p95;
        let mut document = imported.document;
        if !document.is_matchable() {
            if exact {
                let report = serde_json::json!({
                    "converged": true,
                    "reason": "the first fitted LUT representation is not matchable",
                })
                .to_string();
                db::insert_gate_failure(pool, work.look.as_str(), next, &report, now).await?;
                return Ok(RebaseOutcome::Held);
            }
            break 'fit;
        }
        let candidate_probes = document_probes(&document);
        if !candidate_probes
            .probes
            .iter()
            .any(|probe| probe.supported())
        {
            if exact {
                let report = serde_json::json!({
                    "converged": true,
                    "reason": "the first fitted LUT representation has no supported probes",
                })
                .to_string();
                db::insert_gate_failure(pool, work.look.as_str(), next, &report, now).await?;
                return Ok(RebaseOutcome::Held);
            }
            break 'fit;
        }
        document.file.provenance = lut_provenance(&document, &target.provenance, next, now);
        let provenance = ProvenanceSummary {
            bodies: target
                .provenance
                .bodies
                .iter()
                .map(|body| BodyUse {
                    body: body.body.clone(),
                    epochs: body.epochs.clone(),
                    images: body.images,
                    credentials: body.credentials,
                })
                .collect(),
            root_admissions: target.provenance.root.admissions.clone(),
            anchor_version: format!("{next}.0.0"),
            community_snapshot_version: target.provenance.community_snapshot_version,
            prep_versions: target.provenance.prep_versions.clone(),
            created: rfc3339(now),
            software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
        };
        let release = ReleaseRecord {
            version: next,
            gauge: 1.0,
            gauge_convention: GaugeConvention::Relative,
            anchor: next,
            clip_k: document.look.clip.k,
            output_space: RgbSpace::Srgb,
            probes: candidate_probes
                .probes
                .iter()
                .map(|probe| StoredProbe {
                    id: probe.id.clone(),
                    sigma: probe.sigma_pre_clip,
                    sigma_post: probe.sigma_post_clip,
                    bias_de: probe.bias_de,
                    pre_clip_identified: probe.pre_clip_identified,
                    support: probe.support,
                })
                .collect(),
            gated_since_anchor: GatedUnion::at_anchor(&candidate_probes),
            converged: true,
            gate: None,
            approximation_p95: Some(approximation_p95),
            provenance,
            created_at: now,
        };
        let short: String = work.look.as_str().chars().take(27).collect();
        let artifacts = vec![
            crate::lut_import::put_artifact(config, LOOK_ARTIFACT, &document.encode()?)?,
            crate::lut_import::put_artifact(
                config,
                looklab_publish::CARD_ARTIFACT,
                &write_card(
                    &document,
                    &CardMeta::new(definition_name(&document, work.look.as_str()), short),
                    &CardOptions::default(),
                )?,
            )?,
            crate::lut_import::put_artifact(
                config,
                PREVIEW_LUT_ARTIFACT,
                &bake_preview_lut(&document),
            )?,
            crate::lut_import::put_artifact(
                config,
                RELEASE_ARTIFACT,
                &serde_json::to_vec(&release)?,
            )?,
        ];
        let base_dependency_snapshot =
            crate::lut_import::base_snapshot(pool, Some(&target_base)).await?;
        let manifest_status = admin::VersionStatus::Published;
        let (pin_base, pin_status) = if exact {
            (&target_base, BaseStatus::Pinned)
        } else {
            (declared_base, declared_status)
        };
        match commit_candidate(
            pool,
            ownership,
            &work.look,
            work.version,
            next,
            &artifacts,
            pin_base,
            &pin_status,
            &target_base,
            &target.look,
            target.version,
            &base_dependency_snapshot,
            manifest_status,
        )
        .await
        {
            Ok(outcome) => fitted = Some(outcome),
            Err(WorkerError::Data(_)) if !exact => break 'fit,
            Err(error) => return Err(error),
        }
    }
    if let Some(outcome) = fitted {
        return Ok(outcome);
    }
    if exact {
        return Ok(RebaseOutcome::Blocked);
    }

    let preview = bake_lut_preview(
        &cube.lut,
        &descriptor.encoding,
        descriptor.output,
        &record,
        &target_base,
        Some(&target.document),
    )?;
    let identity = LookIdentity::new(
        work.look.as_str(),
        &format!("{next}.0.0"),
        descriptor.look.kind,
        descriptor.look.wb_policy,
    )?;
    let updated = SourceLutPreview {
        format: descriptor.format,
        look: identity,
        name: descriptor.name.clone(),
        license: descriptor.license.clone(),
        encoding: descriptor.encoding.clone(),
        output: descriptor.output,
        emulates: descriptor.emulates.clone(),
        source: descriptor.source.clone(),
        preview: SourceLutPreviewContext {
            baseline: PreviewBaseline::Approximated {
                base: target_base.clone(),
            },
            lut: ArtifactEntry {
                name: ArtifactName::new(PREVIEW_LUT_ARTIFACT)
                    .map_err(|error| WorkerError::Data(error.to_string()))?,
                sha256: ArtifactHash::of(&preview),
                bytes: preview.len() as u64,
            },
        },
    };
    updated.validate()?;
    let rendered = [
        (SOURCE_LUT_ARTIFACT, updated.to_json()?),
        (PREVIEW_LUT_ARTIFACT, preview),
        (SOURCE_CUBE_ARTIFACT, source),
    ];
    let mut manifest = Vec::with_capacity(rendered.len());
    for (name, bytes) in &rendered {
        manifest.push(crate::lut_import::put_artifact(config, name, bytes)?);
    }
    let status = match &updated.encoding {
        DeclaredEncoding::DisplayReferred { status, .. } => *status,
        DeclaredEncoding::SceneReferred { .. } => {
            return Err(WorkerError::Data(
                "source-only preview is not display-referred".to_owned(),
            ));
        }
    };
    commit_preview_candidate(
        pool,
        ownership,
        &work.look,
        work.version,
        next,
        &manifest,
        declared_base,
        &status,
        &target_base,
        &target.look,
        target.version,
        target_state,
        family,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn commit_preview_candidate(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    source_version: u32,
    next: u32,
    artifacts: &[ManifestArtifact],
    base: &BaseReference,
    status: &BaseStatus,
    preview_base: &BaseReference,
    target_look: &LookSlug,
    target_version: u32,
    fitted_state: &TargetState,
    family: bool,
) -> Result<RebaseOutcome> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lut_rebase_pending WHERE look = ? AND version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(source_version))
        .fetch_one(&mut *conn)
        .await?;
        if pending == 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        let existing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM look_versions v
             JOIN version_manifests m ON m.look_version_id = v.id
             WHERE v.look = ? AND v.version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(next))
        .fetch_one(&mut *conn)
        .await?;
        if existing > 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        if admin::is_version_invalidated_conn(&mut conn, target_look, target_version).await?
            || !target_state_matches_conn(&mut conn, fitted_state, family).await?
        {
            return Ok(RebaseOutcome::Blocked);
        }
        admin::write_version_manifest_conn(
            &mut conn,
            look,
            VersionId::new(next),
            admin::VersionStatus::Published,
            artifacts,
            &[],
        )
        .await?;
        admin::record_lut_base_conn(&mut conn, look, next, base, status).await?;
        admin::record_lut_preview_base_conn(&mut conn, look, next, preview_base).await?;
        crate::lut_import::record_preview_eligibility(&mut conn, look, next, artifacts, now_unix())
            .await?;
        looklab_server::request_publication(&mut conn).await?;
        admin::clear_lut_rebase_pending_conn(&mut conn, look, source_version).await?;
        Ok(RebaseOutcome::Published)
    }
    .await;
    match result {
        Ok(RebaseOutcome::Published) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RebaseOutcome::Published)
        }
        Ok(outcome) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Ok(outcome)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

async fn clear_pending_work(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    source_version: u32,
) -> Result<RebaseOutcome> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lut_rebase_pending WHERE look = ? AND version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(source_version))
        .fetch_one(&mut *conn)
        .await?;
        if pending == 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        admin::clear_lut_rebase_pending_conn(&mut conn, look, source_version).await?;
        Ok(RebaseOutcome::Skipped)
    }
    .await;
    match result {
        Ok(RebaseOutcome::Skipped) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RebaseOutcome::Skipped)
        }
        Ok(outcome) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Ok(outcome)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_candidate(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    source_version: u32,
    next: u32,
    artifacts: &[ManifestArtifact],
    base: &BaseReference,
    base_status: &BaseStatus,
    eligibility_base: &BaseReference,
    target_look: &LookSlug,
    target_version: u32,
    base_snapshot: &[DependencyGeneration],
    manifest_status: admin::VersionStatus,
) -> Result<RebaseOutcome> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lut_rebase_pending WHERE look = ? AND version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(source_version))
        .fetch_one(&mut *conn)
        .await?;
        if pending == 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        let existing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM look_versions v
             JOIN version_manifests m ON m.look_version_id = v.id
             WHERE v.look = ? AND v.version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(next))
        .fetch_one(&mut *conn)
        .await?;
        if existing > 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        if admin::is_version_invalidated_conn(&mut conn, target_look, target_version).await? {
            return Ok(RebaseOutcome::Blocked);
        }
        admin::write_version_manifest_conn(
            &mut conn,
            look,
            VersionId::new(next),
            manifest_status,
            artifacts,
            &[],
        )
        .await?;
        admin::record_lut_base_conn(&mut conn, look, next, base, base_status).await?;
        admin::record_lut_preview_base_conn(&mut conn, look, next, eligibility_base).await?;
        if admin::is_version_invalidated_conn(&mut conn, target_look, target_version).await? {
            return Ok(RebaseOutcome::Blocked);
        }
        crate::lut_import::record_eligibility(
            &mut conn,
            look,
            next,
            artifacts,
            Some(eligibility_base),
            base_snapshot,
            crate::clock::now_unix(),
        )
        .await?;
        looklab_server::request_publication(&mut conn).await?;
        admin::clear_lut_rebase_pending_conn(&mut conn, look, source_version).await?;
        Ok(RebaseOutcome::Published)
    }
    .await;
    match result {
        Ok(RebaseOutcome::Published) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RebaseOutcome::Published)
        }
        Ok(outcome) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Ok(outcome)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn hold_failed_candidate(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    source_version: u32,
    next: u32,
    target_look: &LookSlug,
    target_version: u32,
    fitted_state: &TargetState,
    family: bool,
    report: &str,
    now: i64,
) -> Result<RebaseOutcome> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lut_rebase_pending WHERE look = ? AND version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(source_version))
        .fetch_one(&mut *conn)
        .await?;
        if pending == 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        admin::insert_gate_failure_conn(&mut conn, look, next, report, now).await?;
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        if admin::is_version_invalidated_conn(&mut conn, target_look, target_version).await? {
            return Ok(RebaseOutcome::Blocked);
        }
        if !target_state_matches_conn(&mut conn, fitted_state, family).await? {
            return Ok(RebaseOutcome::Blocked);
        }
        admin::clear_lut_rebase_pending_conn(&mut conn, look, source_version).await?;
        Ok(RebaseOutcome::Held)
    }
    .await;
    match result {
        Ok(RebaseOutcome::Held) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RebaseOutcome::Held)
        }
        Ok(outcome) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Ok(outcome)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn clear_stale_work(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    source_version: u32,
    target_look: &LookSlug,
    target_version: u32,
    fitted_state: &TargetState,
    family: bool,
) -> Result<RebaseOutcome> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
        )
        .bind(ownership.id.as_str())
        .bind(ownership.attempts)
        .fetch_one(&mut *conn)
        .await?;
        if owned == 0 {
            return Err(WorkerError::Job("lut rebase lost its lease".to_owned()));
        }
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM lut_rebase_pending WHERE look = ? AND version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(source_version))
        .fetch_one(&mut *conn)
        .await?;
        if pending == 0 {
            return Ok(RebaseOutcome::Skipped);
        }
        if admin::is_version_invalidated_conn(&mut conn, look, source_version).await?
            || admin::is_version_invalidated_conn(&mut conn, target_look, target_version).await?
            || !target_state_matches_conn(&mut conn, fitted_state, family).await?
        {
            return Ok(RebaseOutcome::Blocked);
        }
        admin::clear_lut_rebase_pending_conn(&mut conn, look, source_version).await?;
        Ok(RebaseOutcome::Skipped)
    }
    .await;
    match result {
        Ok(RebaseOutcome::Skipped) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(RebaseOutcome::Skipped)
        }
        Ok(outcome) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Ok(outcome)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

pub(crate) fn definition_name(document: &LookDocument, fallback: &str) -> String {
    document
        .file
        .definition
        .as_ref()
        .map(|definition| definition.name.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

async fn anchor_version(
    pool: &SqlitePool,
    look: &LookSlug,
    versions: &[db::LookVersionRow],
    fallback: u32,
) -> Result<u32> {
    let mut candidates: Vec<u32> = versions
        .iter()
        .filter(|version| {
            matches!(
                version.status,
                LookVersionStatus::Published | LookVersionStatus::Rooted
            )
        })
        .map(|version| version.version)
        .min()
        .into_iter()
        .collect();
    for re_anchor in db::re_anchor_versions(pool, look.as_str()).await? {
        if versions.iter().any(|version| {
            version.version == re_anchor
                && matches!(
                    version.status,
                    LookVersionStatus::Published | LookVersionStatus::Rooted
                )
        }) {
            candidates.push(re_anchor);
        }
    }
    Ok(candidates.into_iter().max().unwrap_or(fallback))
}

fn pin_version(version: &LookVersion) -> Result<u32> {
    version
        .as_str()
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .ok_or_else(|| WorkerError::Data("base version".to_owned()))
}

async fn resolve_base(
    pool: &SqlitePool,
    config: &WorkerConfig,
    base: &BaseReference,
    prefer_pinned: bool,
) -> Result<Option<(BaseTarget, TargetState)>> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    let result = async {
        let invalid: HashSet<(String, u32)> = admin::invalidated_versions_conn(&mut conn)
            .await?
            .into_iter()
            .map(|version| (version.look.as_str().to_owned(), version.version))
            .collect();
        let (looks, pinned) = match base {
            BaseReference::Look { slug, version } => (
                vec![
                    LookSlug::try_from(slug.as_str().to_owned())
                        .map_err(|_| WorkerError::Data("base slug".to_owned()))?,
                ],
                Some(pin_version(version)?),
            ),
            BaseReference::FamilyRendering {
                maker,
                rendering,
                dynamic_range,
                ..
            } => {
                if dynamic_range.is_some() {
                    return Ok(None);
                }
                let rows: Vec<(String, String, String)> = sqlx::query_as(
                    "SELECT look, maker, rendering FROM lut_native_renderings
                     GROUP BY look, maker, rendering
                     ORDER BY MAX(version) DESC, look",
                )
                .fetch_all(&mut *conn)
                .await?;
                let family = LutEmulation::CameraRendering {
                    maker: maker.clone(),
                    rendering: rendering.clone(),
                };
                let mut looks = Vec::new();
                for (look, native_maker, native_rendering) in rows {
                    if !looklab_model::file::emulation_matches_camera_rendering(
                        &family,
                        &native_maker,
                        &native_rendering,
                    ) {
                        continue;
                    }
                    let look = LookSlug::try_from(look)
                        .map_err(|_| WorkerError::Data("base slug".to_owned()))?;
                    if !looks.contains(&look) {
                        looks.push(look);
                    }
                }
                (looks, None)
            }
            BaseReference::BodyRealization { .. } => return Ok(None),
        };
        let family = matches!(base, BaseReference::FamilyRendering { .. });
        for look in &looks {
            if let Some(target) =
                resolve_from_look_conn(&mut conn, config, look, pinned, &invalid, prefer_pinned)
                    .await?
            {
                let state = target_state_conn(&mut conn, &looks, family).await?;
                return Ok(Some((target, state)));
            }
        }
        Ok(None)
    }
    .await;
    match result {
        Ok(value) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(value)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

async fn resolve_from_look_conn(
    conn: &mut SqliteConnection,
    config: &WorkerConfig,
    look: &LookSlug,
    pinned: Option<u32>,
    invalid: &HashSet<(String, u32)>,
    prefer_pinned: bool,
) -> Result<Option<BaseTarget>> {
    let versions = versions_conn(conn, look).await?;
    let mut candidates: Vec<u32> = versions.iter().map(|(version, _)| *version).collect();
    candidates.sort_by(|left, right| {
        if prefer_pinned {
            let left_exact = Some(*left) == pinned;
            let right_exact = Some(*right) == pinned;
            right_exact.cmp(&left_exact).then(right.cmp(left))
        } else {
            right.cmp(left)
        }
    });
    for version in candidates {
        if invalid.contains(&(look.as_str().to_owned(), version)) {
            continue;
        }
        if pinned.is_some_and(|pinned| version < pinned) {
            continue;
        }
        if !has_layered_artifact_conn(&mut *conn, look, version).await? {
            continue;
        }
        let document = load_document_conn(conn, config, look, version).await?;
        if !document.is_matchable() {
            continue;
        }
        let provenance = validated_provenance(&document)
            .ok_or_else(|| WorkerError::MissingProvenance {
                look: look.as_str().to_owned(),
                version,
            })?
            .clone();
        return Ok(Some(BaseTarget {
            look: look.clone(),
            version,
            document,
            provenance,
        }));
    }
    Ok(None)
}

async fn versions_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
) -> Result<Vec<(u32, LookVersionStatus)>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT version, status FROM look_versions
         WHERE look = ? AND status IN ('published', 'rooted') ORDER BY version",
    )
    .bind(look.as_str())
    .fetch_all(&mut *conn)
    .await?;
    let mut versions = Vec::with_capacity(rows.len());
    for (version, status) in rows {
        versions.push((
            u32::try_from(version).map_err(|_| WorkerError::Data("look version".to_owned()))?,
            LookVersionStatus::parse(&status)?,
        ));
    }
    Ok(versions)
}

async fn load_document_conn(
    conn: &mut SqliteConnection,
    config: &WorkerConfig,
    look: &LookSlug,
    version: u32,
) -> Result<LookDocument> {
    let Some(artifacts) = manifest_artifacts_conn(&mut *conn, look, version).await? else {
        return Err(WorkerError::Data(format!(
            "{look} {version} has no version manifest"
        )));
    };
    let bytes = read_artifact(config, &artifacts, LOOK_ARTIFACT)?;
    Ok(LookDocument::decode(&bytes)?)
}

async fn manifest_artifacts_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
) -> Result<Option<Vec<ManifestArtifact>>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT m.artifacts FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.version = ?",
    )
    .bind(look.as_str())
    .bind(i64::from(version))
    .fetch_optional(&mut *conn)
    .await?;
    let parsed: Option<Result<Vec<ManifestArtifact>, WorkerError>> = row.map(|(artifacts,)| {
        serde_json::from_str::<Vec<ManifestArtifact>>(&artifacts).map_err(WorkerError::from)
    });
    parsed.transpose()
}

async fn has_layered_artifact_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
) -> Result<bool> {
    let Some(artifacts) = manifest_artifacts_conn(&mut *conn, look, version).await? else {
        return Ok(false);
    };
    let shape = look_shape_names(artifacts.iter().map(|artifact| artifact.name.as_str()))
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(shape == VersionLookShape::Layered)
}

fn validated_provenance(document: &LookDocument) -> Option<&Provenance> {
    let provenance = &document.file.provenance;
    if provenance.bodies.is_empty() || provenance.prep_versions.is_empty() {
        return None;
    }
    if provenance
        .bodies
        .iter()
        .any(|body| body.body.is_empty() || body.images == 0 || body.credentials == 0)
    {
        return None;
    }
    Some(provenance)
}

fn lut_provenance(
    document: &LookDocument,
    base_provenance: &Provenance,
    anchor: u32,
    now: i64,
) -> Provenance {
    let mut provenance = base_provenance.clone();
    provenance.base = match &document.file.definition {
        Some(definition) => match &definition.encoding {
            DeclaredEncoding::DisplayReferred { base, .. } => Some(base.clone()),
            _ => None,
        },
        None => None,
    };
    provenance.root = RootProvenance {
        admissions: base_provenance.root.admissions.clone(),
        anchor_version: format!("{anchor}.0.0"),
    };
    provenance.needs_rebase = false;
    provenance.created = rfc3339(now);
    provenance.software = format!("looklab-worker {}", env!("CARGO_PKG_VERSION"));
    provenance
}

pub(crate) async fn load_document(
    pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
    version: u32,
) -> Result<LookDocument> {
    let artifacts = db::manifest_artifacts(pool, look.as_str(), version)
        .await?
        .ok_or_else(|| WorkerError::Data(format!("{look} {version} has no version manifest")))?;
    let bytes = read_artifact(config, &artifacts, LOOK_ARTIFACT)?;
    Ok(LookDocument::decode(&bytes)?)
}

async fn load_optional_document(
    pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
    version: u32,
) -> Result<Option<LookDocument>> {
    let Some(artifacts) = db::manifest_artifacts(pool, look.as_str(), version).await? else {
        return Ok(None);
    };
    if !artifacts
        .iter()
        .any(|artifact| artifact.name == LOOK_ARTIFACT)
    {
        return Ok(None);
    }
    Ok(Some(load_document(pool, config, look, version).await?))
}

pub(crate) fn read_artifact(
    config: &WorkerConfig,
    artifacts: &[ManifestArtifact],
    name: &str,
) -> Result<Vec<u8>> {
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .ok_or_else(|| WorkerError::Data(format!("manifest has no `{name}` artifact")))?;
    let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
    if db::hex(&Sha256::digest(&bytes)) != artifact.sha256 {
        return Err(WorkerError::Data(format!(
            "artifact `{name}` does not match its manifest digest"
        )));
    }
    Ok(bytes)
}

fn cube_from_lut(lut: &LutData) -> Result<CubeFile> {
    let uniform_min = lut
        .domain_min
        .iter()
        .all(|value| *value == lut.domain_min[0]);
    let uniform_max = lut
        .domain_max
        .iter()
        .all(|value| *value == lut.domain_max[0]);
    if !(uniform_min && uniform_max) || lut.domain_min[0] >= lut.domain_max[0] {
        return Err(WorkerError::Data(
            "stored LUT has a per-channel domain".to_owned(),
        ));
    }
    let (chunks, _) = lut.values.as_chunks::<3>();
    let data = chunks
        .iter()
        .map(|chunk| [chunk[0] as f32, chunk[1] as f32, chunk[2] as f32])
        .collect();
    Ok(CubeFile {
        lut: Lut3d {
            size: lut.size,
            data,
            shaper: None,
        },
        domain: [lut.domain_min[0], lut.domain_max[0]],
    })
}

pub(crate) fn document_probes(document: &LookDocument) -> VersionProbes {
    let uncertainty = &document.uncertainty;
    let support = uncertainty.support.as_ref().map(|values| &values.values);
    let bias = uncertainty.probe_bias.as_ref().map(|values| &values.values);
    let sigma = uncertainty
        .probe_sigma
        .as_ref()
        .map(|values| &values.values);
    let sigma_post = uncertainty
        .probe_sigma_post
        .as_ref()
        .map(|values| &values.values);
    let identified = uncertainty
        .probe_pre_reliable
        .as_ref()
        .map(|values| &values.values);
    let probes = probes::v1()
        .probes
        .into_iter()
        .enumerate()
        .map(|(index, probe)| {
            let render_feasible = checked_appearance(&document.look, probe.xyz).is_ok();
            let (pre_clip, post_clip) = if render_feasible {
                (
                    document.look.eval(probe.xyz).to_array(),
                    post_clip(
                        &document.look,
                        probe.xyz,
                        RgbSpace::Srgb,
                        document.look.clip.k,
                    )
                    .to_array(),
                )
            } else {
                ([f64::NAN; 3], [f64::NAN; 3])
            };
            let sigma_post_clip = if render_feasible {
                sigma_post
                    .and_then(|values| values.get(index).copied())
                    .unwrap_or_else(|| covariance_sigma(uncertainty.probe_cov_post.as_ref(), index))
            } else {
                f64::MAX
            };
            let supported = render_feasible
                && support
                    .and_then(|values| values.get(index).copied())
                    .is_some_and(|value| value > 0.5);
            ProbeEstimate {
                id: probe.id,
                pre_clip,
                sigma_pre_clip: if render_feasible {
                    sigma
                        .and_then(|values| values.get(index).copied())
                        .unwrap_or_else(|| covariance_sigma(uncertainty.probe_cov.as_ref(), index))
                } else {
                    f64::MAX
                },
                post_clip,
                sigma_post_clip,
                bias_de: bias
                    .and_then(|values| values.get(index).copied())
                    .unwrap_or(0.0),
                support: ProbeSupport {
                    sigma: supported,
                    density: supported,
                    clusters: supported,
                    bias: supported,
                },
                pre_clip_identified: render_feasible
                    && identified
                        .and_then(|values| values.get(index).copied())
                        .is_some_and(|value| value > 0.5),
            }
        })
        .collect();
    VersionProbes { probes }
}

fn covariance_sigma(covariance: Option<&ProbeCovariance>, index: usize) -> f64 {
    let Some(covariance) = covariance else {
        return 0.0;
    };
    let offset = index * 9;
    let Some(block) = covariance.values.get(offset..offset + 9) else {
        return 0.0;
    };
    (block[0].max(0.0) + block[4].max(0.0) + block[8].max(0.0)).sqrt()
}
