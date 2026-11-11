// SPDX-License-Identifier: AGPL-3.0-or-later
//! Imports an approved custom-look LUT into a published Look version.

use looklab_color::RgbSpace;
use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_export::{
    CubeFile, LutLookOptions, approximation_publishable, import_lut_look, lut_data_from_cube,
    read_cube,
};
use looklab_model::baseline::is_neutral_baseline;
use looklab_model::file::{
    BaseReference, BaseStatus, DeclaredEncoding, Gamut, LicenseRecord, LogTransfer, LutDefinition,
    LutEmulation, LutRecord, OutputEncoding,
};
use looklab_model::{LookDocument, LookIdentity, LookKind, WbPolicy};
use looklab_publish::{
    ArtifactEntry, ArtifactHash, ArtifactName, CARD_ARTIFACT, LookArtifact, PREVIEW_LUT_ARTIFACT,
    PreviewBaseline, ProvenanceSummary, RELEASE_ARTIFACT, SOURCE_CUBE_ARTIFACT,
    SOURCE_LUT_ARTIFACT, SourceLutFormat, SourceLutPreview, SourceLutPreviewContext,
    bake_lut_preview, bake_preview_lut,
};
use looklab_schema::{
    ArtifactBinding, ArtifactRole, BasePin, DependencyGeneration, DependencyKind, DependencyRef,
    DependencyTarget, EligibilitySnapshotId, LookId, LutEmulationDecl, LutEncoding,
    LutLookProposal, ProposalKind, ProposalStatus, PublishedTargetId, TargetBasis,
    TargetEligibility, TargetEligibilityReason, TargetEligibilityStatus, TargetVersion,
};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_server::{BlobHash, BlobStore, FsBlobStore, ProposalId, model_store};
use looklab_trust::{GatedUnion, VersionId};
use looklab_upload::LookSlug;
use sha2::Digest;
use sqlx::{SqliteConnection, SqlitePool};

use crate::clock::{now_unix, rfc3339};
use crate::config::WorkerConfig;
use crate::db;
use crate::error::{Result, WorkerError};
use crate::jobs::JobOwnership;
use crate::lut_rebase::{definition_name, document_probes, load_document};
use crate::refit::{GaugeConvention, LOOK_ARTIFACT, ReleaseRecord, StoredProbe};

pub async fn run(
    pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    proposal: &ProposalId,
) -> Result<()> {
    ownership.check(pool).await?;
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT type, status, payload FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(pool)
            .await?;
    let Some((kind, status, payload)) = row else {
        return Err(WorkerError::Job("unknown proposal".to_owned()));
    };
    let kind = kind
        .parse::<ProposalKind>()
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    if kind != ProposalKind::LutLook {
        return Ok(());
    }
    let status = ProposalStatus::parse(&status)
        .ok_or_else(|| WorkerError::Data("unknown proposal status".to_owned()))?;
    if status != ProposalStatus::Approved {
        return Ok(());
    }
    let imported: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports WHERE proposal_id = ?")
            .bind(proposal.as_str())
            .fetch_one(pool)
            .await?;
    if imported > 0 {
        return Ok(());
    }
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures WHERE proposal_id = ?")
            .bind(proposal.as_str())
            .fetch_one(pool)
            .await?;
    if failed > 0 {
        return Ok(());
    }
    let proposal_payload: LutLookProposal =
        serde_norway::from_str(&payload).map_err(|error| WorkerError::Data(error.to_string()))?;
    let look = LookSlug::new(proposal_payload.look.as_str())
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let sha256 = *proposal_payload.lut.sha256.as_bytes();
    match import(
        pool,
        config,
        ownership,
        proposal,
        &proposal_payload,
        &look,
        &sha256,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error @ WorkerError::Job(_)) => Err(error),
        Err(error) if error.permanent() => {
            let reason = error.to_string();
            fail(pool, ownership, proposal, &sha256, &reason).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn import(
    pool: &SqlitePool,
    config: &WorkerConfig,
    ownership: &JobOwnership,
    proposal: &ProposalId,
    payload: &LutLookProposal,
    look: &LookSlug,
    sha256: &[u8; 32],
) -> Result<()> {
    if look.as_str() == looklab_schema::RESERVED_LUT_LOOK_SLUG {
        return Err(WorkerError::Data(
            "the neutral baseline link is reserved".to_owned(),
        ));
    }
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
        .bind(look.as_str())
        .fetch_one(pool)
        .await?;
    if existing > 0 {
        return Err(WorkerError::Data(format!(
            "{look} already has published versions"
        )));
    }
    let lut_file = &payload.lut;
    let path = db::blob_path(&config.blobs, sha256);
    let bytes = std::fs::read(&path)?;
    if bytes.len() as u64 != lut_file.size
        || db::hex(&sha2::Sha256::digest(&bytes)) != lut_file.sha256.to_hex()
    {
        return Err(WorkerError::Data(
            "the uploaded LUT does not match its recorded size and digest".to_owned(),
        ));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| WorkerError::Data("the LUT is not text".to_owned()))?;
    let cube = read_cube(text).map_err(|error| WorkerError::Data(error.to_string()))?;
    let record = lut_data_from_cube(&cube)
        .map_err(|error| WorkerError::Data(error.to_string()))?
        .record();

    let (base, base_status, base_document, base_pin) =
        base_reference(pool, config, &payload.encoding).await?;
    let preview_only = base_status
        .as_ref()
        .is_some_and(|status| !status.is_pinned());
    let base_dependency_snapshot = if preview_only {
        Vec::new()
    } else {
        base_snapshot(pool, base.as_ref()).await?
    };
    let encoding = match &base {
        None => DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        },
        Some(reference) => DeclaredEncoding::DisplayReferred {
            base: reference.clone(),
            status: base_status
                .ok_or_else(|| WorkerError::Data("the display base has no status".to_owned()))?,
        },
    };
    let license = LicenseRecord {
        own_work: payload.license.own_work,
        source_license: payload.license.source_license.clone(),
        source_url: payload.license.source_url.clone(),
        attribution: payload.attribution.clone(),
    };
    let emulates = payload.emulates.as_ref().map(emulation);
    let identity = LookIdentity::new(
        look.as_str(),
        "1.0.0",
        LookKind::LutDefined,
        WbPolicy::Adaptive,
    )?;
    let mut preview_base_record = None;
    let mut commit_base = base.clone();
    let mut commit_status = base_status;
    let mut commit_pin = base_pin;
    let mut commit_eligibility_base = base.clone();
    let mut commit_eligibility_snapshot = base_dependency_snapshot;
    let mut commit_preview = preview_only;
    let mut manifest_status = admin::VersionStatus::Published;

    let approximation = if preview_only {
        match base.as_ref() {
            Some(base) => crate::lut_rebase::resolve_preview_base(pool, config, base).await?,
            None => None,
        }
    } else {
        None
    };

    let mut fitted_rendered = None;
    if let Some(target) = &approximation {
        let exact = match base.as_ref() {
            Some(declared) => crate::lut_rebase::preview_target_is_exact(declared, target)?,
            None => false,
        };
        let definition = LutDefinition {
            name: payload.name.clone(),
            license: license.clone(),
            encoding: DeclaredEncoding::DisplayReferred {
                base: target.base.clone(),
                status: BaseStatus::Pinned,
            },
            declared_base: if exact { None } else { base.clone() },
            output: OutputEncoding::Srgb,
            lut: record.clone(),
            emulates: emulates.clone(),
            superseded_by: None,
            notes: Vec::new(),
        };
        if let Ok(imported) = import_lut_look(
            identity.clone(),
            definition,
            &cube,
            Some(&target.document),
            LutLookOptions {
                seed: config.seed,
                ..LutLookOptions::default()
            },
        ) && imported.converged
            && approximation_publishable(&imported.approximation)
        {
            let approximation_p95 = imported.approximation.p95;
            let document = imported.document;
            if document.is_matchable() {
                let candidate = document_probes(&document);
                if candidate.probes.iter().any(|probe| probe.supported()) {
                    let now = now_unix();
                    let (snapshot_version, _) =
                        crate::body::published_snapshot(pool, config).await?;
                    let snapshot_version = u32::try_from(snapshot_version).map_err(|_| {
                        WorkerError::Data("the community snapshot version exceeds u32".to_owned())
                    })?;
                    let provenance = ProvenanceSummary {
                        bodies: Vec::new(),
                        root_admissions: Vec::new(),
                        anchor_version: "1.0.0".to_owned(),
                        community_snapshot_version: u64::from(snapshot_version),
                        prep_versions: Vec::new(),
                        created: rfc3339(now),
                        software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
                    };
                    let release = ReleaseRecord {
                        version: 1,
                        gauge: 1.0,
                        gauge_convention: GaugeConvention::Relative,
                        anchor: 1,
                        clip_k: document.look.clip.k,
                        output_space: RgbSpace::Srgb,
                        probes: candidate
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
                        gated_since_anchor: GatedUnion::at_anchor(&candidate),
                        converged: true,
                        gate: None,
                        approximation_p95: Some(approximation_p95),
                        provenance,
                        created_at: now,
                    };
                    let short: String = look.as_str().chars().take(27).collect();
                    fitted_rendered = Some(vec![
                        (LOOK_ARTIFACT, document.encode()?),
                        (PREVIEW_LUT_ARTIFACT, bake_preview_lut(&document)),
                        (RELEASE_ARTIFACT, serde_json::to_vec(&release)?),
                        (
                            CARD_ARTIFACT,
                            write_card(
                                &document,
                                &CardMeta::new(definition_name(&document, look.as_str()), short),
                                &CardOptions::default(),
                            )
                            .map_err(|error| WorkerError::Data(error.to_string()))?,
                        ),
                    ]);
                    preview_base_record = Some(target.base.clone());
                    commit_eligibility_base = Some(target.base.clone());
                    commit_eligibility_snapshot =
                        crate::lut_import::base_snapshot(pool, commit_eligibility_base.as_ref())
                            .await?;
                    commit_preview = false;
                    manifest_status = admin::VersionStatus::Published;
                    if exact {
                        let BaseReference::Look { slug, .. } = &target.base else {
                            return Err(WorkerError::Data(
                                "the approximation is not a Look reference".to_owned(),
                            ));
                        };
                        commit_base = Some(target.base.clone());
                        commit_status = Some(BaseStatus::Pinned);
                        commit_pin = Some((
                            LookSlug::new(slug.as_str())
                                .map_err(|error| WorkerError::Data(error.to_string()))?,
                            target.version,
                        ));
                    }
                }
            }
        }
    }

    let preview_identity = identity.clone();
    let preview_license = license.clone();
    let preview_encoding = encoding.clone();
    let preview_emulates = emulates.clone();
    let fallback_identity = identity.clone();
    let fallback_license = license.clone();
    let fallback_encoding = encoding.clone();
    let fallback_emulates = emulates.clone();
    let approximate_fit = fitted_rendered.is_some();

    let rendered: Vec<(&str, Vec<u8>)> = if let Some(fitted) = fitted_rendered {
        fitted
    } else if preview_only {
        let preview = render_source_preview(
            preview_identity,
            &payload.name,
            preview_license,
            preview_encoding,
            preview_emulates,
            &bytes,
            &cube,
            &record,
            approximation.as_ref(),
        )?;
        preview_base_record = preview.preview_base;
        preview.rendered
    } else {
        let definition = LutDefinition {
            name: payload.name.clone(),
            license,
            encoding,
            declared_base: None,
            output: OutputEncoding::Srgb,
            lut: record.clone(),
            emulates,
            superseded_by: None,
            notes: Vec::new(),
        };
        let imported = import_lut_look(
            identity,
            definition,
            &cube,
            base_document.as_ref(),
            LutLookOptions {
                seed: config.seed,
                ..LutLookOptions::default()
            },
        )?;
        if !imported.converged {
            return Err(WorkerError::Data(
                "the LUT fit did not converge; no version was written".to_owned(),
            ));
        }
        if !approximation_publishable(&imported.approximation) {
            return Err(WorkerError::Data(
                "the LUT fit exceeds the LUT publication accuracy gate; no version was written"
                    .to_owned(),
            ));
        }
        let approximation_p95 = imported.approximation.p95;
        let document = imported.document;
        if !document.is_matchable() {
            return Err(WorkerError::Data(
                "the LUT import is not matchable; no version was written".to_owned(),
            ));
        }
        let candidate = document_probes(&document);
        if !candidate.probes.iter().any(|probe| probe.supported()) {
            return Err(WorkerError::Data(
                "the LUT import has no supported probes; no version was written".to_owned(),
            ));
        }
        let gated = GatedUnion::at_anchor(&candidate);
        let now = now_unix();
        let (snapshot_version, _) = crate::body::published_snapshot(pool, config).await?;
        let snapshot_version = u32::try_from(snapshot_version).map_err(|_| {
            WorkerError::Data("the community snapshot version exceeds u32".to_owned())
        })?;
        let provenance = ProvenanceSummary {
            bodies: Vec::new(),
            root_admissions: Vec::new(),
            anchor_version: "1.0.0".to_owned(),
            community_snapshot_version: u64::from(snapshot_version),
            prep_versions: Vec::new(),
            created: rfc3339(now),
            software: format!("looklab-worker {}", env!("CARGO_PKG_VERSION")),
        };
        let release = ReleaseRecord {
            version: 1,
            gauge: 1.0,
            gauge_convention: GaugeConvention::Relative,
            anchor: 1,
            clip_k: document.look.clip.k,
            output_space: RgbSpace::Srgb,
            probes: candidate
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
            gated_since_anchor: gated,
            converged: true,
            gate: None,
            approximation_p95: Some(approximation_p95),
            provenance,
            created_at: now,
        };
        let short: String = look.as_str().chars().take(27).collect();
        vec![
            (LOOK_ARTIFACT, document.encode()?),
            (PREVIEW_LUT_ARTIFACT, bake_preview_lut(&document)),
            (RELEASE_ARTIFACT, serde_json::to_vec(&release)?),
            (
                CARD_ARTIFACT,
                write_card(
                    &document,
                    &CardMeta::new(definition_name(&document, look.as_str()), short),
                    &CardOptions::default(),
                )
                .map_err(|error| WorkerError::Data(error.to_string()))?,
            ),
        ]
    };
    let hashes: Vec<[u8; 32]> = rendered
        .iter()
        .map(|(_, bytes)| sha2::Sha256::digest(bytes).into())
        .collect();
    stage_artifacts(pool, ownership, &hashes).await?;
    let mut artifacts = Vec::with_capacity(rendered.len());
    for (name, bytes) in &rendered {
        match put_artifact(config, name, bytes) {
            Ok(artifact) => artifacts.push(artifact),
            Err(error) => {
                sweep_artifacts(pool, ownership, &hashes).await?;
                return Err(error);
            }
        }
    }
    if let Err(error) = commit(
        pool,
        ownership,
        look,
        proposal,
        sha256,
        &artifacts,
        commit_base.as_ref(),
        commit_status.as_ref(),
        commit_pin.as_ref(),
        commit_eligibility_base.as_ref(),
        &commit_eligibility_snapshot,
        commit_preview,
        preview_base_record.as_ref(),
        manifest_status,
    )
    .await
    {
        if approximate_fit && matches!(&error, WorkerError::Data(_)) {
            let preview = match render_source_preview(
                fallback_identity,
                &payload.name,
                fallback_license,
                fallback_encoding,
                fallback_emulates,
                &bytes,
                &cube,
                &record,
                approximation.as_ref(),
            ) {
                Ok(preview) => preview,
                Err(_) => {
                    sweep_artifacts(pool, ownership, &hashes).await?;
                    return Err(error);
                }
            };
            sweep_artifacts(pool, ownership, &hashes).await?;
            let preview_hashes: Vec<[u8; 32]> = preview
                .rendered
                .iter()
                .map(|(_, bytes)| sha2::Sha256::digest(bytes).into())
                .collect();
            stage_artifacts(pool, ownership, &preview_hashes).await?;
            let mut artifacts = Vec::with_capacity(preview.rendered.len());
            for (name, bytes) in &preview.rendered {
                match put_artifact(config, name, bytes) {
                    Ok(artifact) => artifacts.push(artifact),
                    Err(error) => {
                        sweep_artifacts(pool, ownership, &preview_hashes).await?;
                        return Err(error);
                    }
                }
            }
            commit(
                pool,
                ownership,
                look,
                proposal,
                sha256,
                &artifacts,
                base.as_ref(),
                base_status.as_ref(),
                None,
                None,
                &[],
                true,
                preview.preview_base.as_ref(),
                admin::VersionStatus::Published,
            )
            .await?;
            return Ok(());
        }
        sweep_artifacts(pool, ownership, &hashes).await?;
        return Err(error);
    }
    Ok(())
}

async fn base_reference(
    pool: &SqlitePool,
    config: &WorkerConfig,
    encoding: &LutEncoding,
) -> Result<(
    Option<BaseReference>,
    Option<BaseStatus>,
    Option<LookDocument>,
    Option<(LookSlug, u32)>,
)> {
    let LutEncoding::Display { base } = encoding else {
        return Ok((None, None, None, None));
    };
    let reference =
        BaseReference::try_from(base).map_err(|error| WorkerError::Data(error.to_string()))?;
    if is_neutral_baseline(&reference) {
        return Ok((
            Some(reference.clone()),
            Some(BaseStatus::Pinned),
            None,
            None,
        ));
    }
    match &reference {
        BaseReference::BodyRealization { body, .. } => {
            let schema = crate::body::DataDir::new(&config.data)
                .load(body)
                .ok()
                .map(|resources| resources.schema);
            let status = reference.resolve_status(schema.as_ref());
            Ok((Some(reference.clone()), Some(status), None, None))
        }
        BaseReference::FamilyRendering { .. } => {
            let status = reference.resolve_status(None);
            Ok((Some(reference.clone()), Some(status), None, None))
        }
        BaseReference::Look { slug, version } => {
            let major = version
                .as_str()
                .split('.')
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| WorkerError::Data("invalid base Look version".to_owned()))?;
            let look_slug = LookSlug::new(slug.as_str())
                .map_err(|error| WorkerError::Data(error.to_string()))?;
            let versions = db::look_versions(pool, look_slug.as_str()).await?;
            let eligible = versions.iter().any(|candidate| {
                candidate.version == major
                    && matches!(
                        candidate.status,
                        db::LookVersionStatus::Published | db::LookVersionStatus::Rooted
                    )
            });
            let artifacts = if eligible {
                db::manifest_artifacts(pool, look_slug.as_str(), major).await?
            } else {
                None
            };
            let invalidated = {
                let mut conn = pool.acquire().await?;
                admin::is_version_invalidated_conn(&mut conn, &look_slug, major).await?
            };
            let Some(artifacts) = artifacts.filter(|_| !invalidated) else {
                return Ok((
                    Some(reference.clone()),
                    Some(BaseStatus::BaseIdentifiedUnmeasured),
                    None,
                    None,
                ));
            };
            let layered = artifacts
                .iter()
                .any(|artifact| artifact.name == LOOK_ARTIFACT);
            let source_only = artifacts
                .iter()
                .any(|artifact| artifact.name == SOURCE_LUT_ARTIFACT);
            if layered && source_only {
                return Err(WorkerError::Data(
                    "the selected base Look version carries both look artifacts".to_owned(),
                ));
            }
            if layered {
                let document = load_document(pool, config, &look_slug, major).await?;
                if document.file.look.slug.as_str() != slug.as_str()
                    || document.file.look.version.as_str() != version.as_str()
                {
                    return Err(WorkerError::Data(
                        "the published base does not match the declared reference".to_owned(),
                    ));
                }
                if document.is_matchable() {
                    return Ok((
                        Some(reference.clone()),
                        Some(BaseStatus::Pinned),
                        Some(document),
                        Some((look_slug, major)),
                    ));
                }
                let status = preview_status(&document).ok_or_else(|| {
                    WorkerError::Data(
                        "the selected base Look version is not a usable rendering".to_owned(),
                    )
                })?;
                return Ok((Some(reference.clone()), Some(status), None, None));
            }
            if source_only {
                let bytes =
                    crate::lut_rebase::read_artifact(config, &artifacts, SOURCE_LUT_ARTIFACT)?;
                let descriptor = SourceLutPreview::decode(&bytes)
                    .map_err(|error| WorkerError::Data(error.to_string()))?;
                if descriptor.look.slug.as_str() != slug.as_str()
                    || descriptor.look.version.as_str() != version.as_str()
                {
                    return Err(WorkerError::Data(
                        "the published base does not match the declared reference".to_owned(),
                    ));
                }
                let status = source_preview_status(&descriptor).ok_or_else(|| {
                    WorkerError::Data(
                        "the selected base Look version is not a usable rendering".to_owned(),
                    )
                })?;
                return Ok((Some(reference.clone()), Some(status), None, None));
            }
            Ok((
                Some(reference.clone()),
                Some(BaseStatus::BaseIdentifiedUnmeasured),
                None,
                None,
            ))
        }
    }
}

fn preview_status(document: &LookDocument) -> Option<BaseStatus> {
    match document
        .file
        .definition
        .as_ref()
        .map(|definition| &definition.encoding)
    {
        Some(DeclaredEncoding::DisplayReferred { status, .. }) if !status.is_pinned() => {
            Some(*status)
        }
        _ => None,
    }
}

fn source_preview_status(descriptor: &SourceLutPreview) -> Option<BaseStatus> {
    match &descriptor.encoding {
        DeclaredEncoding::DisplayReferred { status, .. } if !status.is_pinned() => Some(*status),
        _ => None,
    }
}

struct PreviewRender {
    rendered: Vec<(&'static str, Vec<u8>)>,
    preview_base: Option<BaseReference>,
}

#[allow(clippy::too_many_arguments)]
fn render_source_preview(
    identity: LookIdentity,
    name: &str,
    license: LicenseRecord,
    encoding: DeclaredEncoding,
    emulates: Option<LutEmulation>,
    source_bytes: &[u8],
    cube: &CubeFile,
    record: &LutRecord,
    approximation: Option<&crate::lut_rebase::PreviewTarget>,
) -> Result<PreviewRender> {
    let (baseline, preview_base, base_look) = match approximation {
        Some(target) => (
            PreviewBaseline::Approximated {
                base: target.base.clone(),
            },
            target.base.clone(),
            Some(&target.document),
        ),
        None => (
            PreviewBaseline::NeutralFallback,
            BaseReference::neutral_baseline(),
            None,
        ),
    };
    let preview = bake_lut_preview(
        &cube.lut,
        &encoding,
        OutputEncoding::Srgb,
        record,
        &preview_base,
        base_look,
    )
    .map_err(|error| WorkerError::Data(error.to_string()))?;
    let descriptor = SourceLutPreview {
        format: SourceLutFormat::V1,
        look: identity,
        name: name.to_owned(),
        license,
        encoding,
        output: OutputEncoding::Srgb,
        emulates,
        source: ArtifactEntry {
            name: ArtifactName::new(SOURCE_CUBE_ARTIFACT)
                .map_err(|error| WorkerError::Data(error.to_string()))?,
            sha256: ArtifactHash::of(source_bytes),
            bytes: source_bytes.len() as u64,
        },
        preview: SourceLutPreviewContext {
            baseline,
            lut: ArtifactEntry {
                name: ArtifactName::new(PREVIEW_LUT_ARTIFACT)
                    .map_err(|error| WorkerError::Data(error.to_string()))?,
                sha256: ArtifactHash::of(&preview),
                bytes: preview.len() as u64,
            },
        },
    };
    descriptor
        .validate()
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(PreviewRender {
        rendered: vec![
            (
                SOURCE_LUT_ARTIFACT,
                descriptor
                    .to_json()
                    .map_err(|error| WorkerError::Data(error.to_string()))?,
            ),
            (PREVIEW_LUT_ARTIFACT, preview),
            (SOURCE_CUBE_ARTIFACT, source_bytes.to_vec()),
        ],
        preview_base: descriptor.preview.baseline.approximation().cloned(),
    })
}

fn emulation(value: &LutEmulationDecl) -> LutEmulation {
    match value {
        LutEmulationDecl::CameraRendering { maker, rendering } => LutEmulation::CameraRendering {
            maker: maker.clone(),
            rendering: rendering.clone(),
        },
        LutEmulationDecl::FilmStock { name } => LutEmulation::FilmStock { name: name.clone() },
    }
}

fn artifact_attempt(ownership: &JobOwnership) -> String {
    format!(
        "lut-import:{}:{}",
        ownership.id.as_str(),
        ownership.attempts
    )
}

pub(crate) fn put_artifact(
    config: &WorkerConfig,
    name: &str,
    bytes: &[u8],
) -> Result<ManifestArtifact> {
    let blob_error = |error| match error {
        looklab_server::BlobError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            WorkerError::Data(error.to_string())
        }
        looklab_server::BlobError::Io(error) => WorkerError::Io(error),
    };
    let store = FsBlobStore::new(&config.blobs).map_err(blob_error)?;
    let hash = BlobHash::of(bytes);
    store.put(hash, bytes).map_err(blob_error)?;
    Ok(ManifestArtifact {
        name: name.to_owned(),
        sha256: hash.to_hex(),
    })
}

fn base_target(base: &BaseReference) -> Option<PublishedTargetId> {
    let BaseReference::Look { slug, version } = base else {
        return None;
    };
    let major = version
        .as_str()
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())?;
    Some(PublishedTargetId {
        look: LookId::new(slug.as_str()).ok()?,
        version: TargetVersion::new(major)?,
    })
}

fn base_dependency(base: &BaseReference) -> Option<DependencyRef> {
    base_target(base).map(|target| {
        DependencyRef::required(
            DependencyKind::PublishedTarget,
            DependencyTarget::published_target(&target),
        )
    })
}

pub(crate) async fn base_snapshot(
    pool: &SqlitePool,
    base: Option<&BaseReference>,
) -> Result<Vec<DependencyGeneration>> {
    let Some(base) = base else {
        return Ok(Vec::new());
    };
    let Some(dependency) = base_dependency(base) else {
        return Ok(Vec::new());
    };
    let mut conn = pool.acquire().await?;
    model_store::observe_dependency_snapshot(&mut conn, &[dependency], now_unix())
        .await
        .map_err(|error| WorkerError::Data(error.to_string()))
}

const ARTIFACT_STAGING_SECONDS: i64 = 86_400;

async fn stage_artifacts(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    hashes: &[[u8; 32]],
) -> Result<()> {
    let now = now_unix();
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        for hash in hashes {
            sqlx::query(
                "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at)
                 VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
            )
            .bind(artifact_attempt(ownership))
            .bind(hash.as_slice())
            .bind(now)
            .bind(now + ARTIFACT_STAGING_SECONDS)
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

async fn sweep_artifacts(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    hashes: &[[u8; 32]],
) -> Result<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let result = async {
        sqlx::query("DELETE FROM blob_staging WHERE attempt_id = ?")
            .bind(artifact_attempt(ownership))
            .execute(&mut *conn)
            .await?;
        for hash in hashes {
            sqlx::query(
                "INSERT INTO lut_blob_deletions (hash, queued_at) VALUES (?, ?)
                 ON CONFLICT (hash) DO NOTHING",
            )
            .bind(hash.as_slice())
            .bind(now_unix())
            .execute(&mut *conn)
            .await?;
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

pub(crate) async fn record_eligibility(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
    artifacts: &[ManifestArtifact],
    base: Option<&BaseReference>,
    base_snapshot: &[DependencyGeneration],
    now: i64,
) -> Result<()> {
    if !model_store::numerical_sources_current(conn, &crate::source_identity::sources().storage())
        .await
        .map_err(|error| WorkerError::Data(error.to_string()))?
    {
        return Err(WorkerError::Job(
            "the numerical sources changed during import".to_owned(),
        ));
    }
    if let Some(base) = base {
        let base_target =
            base_target(base).ok_or_else(|| WorkerError::Data("invalid base Look".to_owned()))?;
        if !is_neutral_baseline(base) {
            let eligibility = model_store::target_eligibility(conn, &base_target)
                .await
                .map_err(|error| WorkerError::Data(error.to_string()))?;
            if !eligibility
                .is_some_and(|eligibility| eligibility.status == TargetEligibilityStatus::Eligible)
            {
                return Err(WorkerError::Data(
                    "the selected base is not currently eligible".to_owned(),
                ));
            }
        }
        let dependency = base_dependency(base)
            .ok_or_else(|| WorkerError::Data("invalid base Look".to_owned()))?;
        let current = model_store::dependency_snapshot(conn, &[dependency])
            .await
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        if current != base_snapshot {
            return Err(WorkerError::Data(
                "the selected base changed during import".to_owned(),
            ));
        }
    }
    let target = PublishedTargetId {
        look: LookId::new(look.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?,
        version: TargetVersion::new(version)
            .ok_or_else(|| WorkerError::Data("LUT target version is zero".to_owned()))?,
    };
    let artifacts = artifacts
        .iter()
        .filter_map(|artifact| {
            let role = match artifact.name.as_str() {
                looklab_publish::DOCUMENT_ARTIFACT => ArtifactRole::Look,
                looklab_publish::CARD_ARTIFACT => ArtifactRole::Card,
                looklab_publish::PREVIEW_LUT_ARTIFACT => ArtifactRole::Preview,
                RELEASE_ARTIFACT => ArtifactRole::Release,
                _ => return None,
            };
            Some(
                looklab_schema::Sha256Hex::parse(&artifact.sha256)
                    .map(|sha256| ArtifactBinding { sha256, role }),
            )
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let base_pin = match base {
        Some(BaseReference::Look { slug, version }) => {
            let major = version
                .as_str()
                .split('.')
                .next()
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| WorkerError::Data("invalid base Look version".to_owned()))?;
            Some(BasePin::LookVersion(PublishedTargetId {
                look: LookId::new(slug.as_str())
                    .map_err(|error| WorkerError::Data(error.to_string()))?,
                version: TargetVersion::new(major)
                    .ok_or_else(|| WorkerError::Data("base Look version is zero".to_owned()))?,
            }))
        }
        Some(_) | None => None,
    };
    let mut dependencies = crate::source_identity::sources().dependencies();
    if let Some(BasePin::LookVersion(base)) = &base_pin {
        dependencies.push(DependencyRef::required(
            DependencyKind::PublishedTarget,
            DependencyTarget::published_target(base),
        ));
    }
    let eligibility = TargetEligibility {
        id: EligibilitySnapshotId::from_digest([0; 32]),
        target,
        basis: TargetBasis::LutComposition,
        artifacts,
        base_pin,
        support_uncertainty_source: Some(crate::source_identity::sources().evaluator.clone()),
        status: TargetEligibilityStatus::Eligible,
        reasons: Vec::new(),
        dependencies,
    };
    model_store::record_target_eligibility(conn, &eligibility, now)
        .await
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(())
}

pub(crate) async fn record_preview_eligibility(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
    artifacts: &[ManifestArtifact],
    now: i64,
) -> Result<()> {
    let target = PublishedTargetId {
        look: LookId::new(look.as_str()).map_err(|error| WorkerError::Data(error.to_string()))?,
        version: TargetVersion::new(version)
            .ok_or_else(|| WorkerError::Data("LUT target version is zero".to_owned()))?,
    };
    let artifacts = artifacts
        .iter()
        .filter_map(|artifact| {
            let name = ArtifactName::new(artifact.name.as_str()).ok()?;
            let role = match LookArtifact::of(&name) {
                LookArtifact::Document => ArtifactRole::Look,
                LookArtifact::PreviewLut => ArtifactRole::Preview,
                LookArtifact::Card | LookArtifact::Other => return None,
            };
            Some(
                looklab_schema::Sha256Hex::parse(&artifact.sha256)
                    .map(|sha256| ArtifactBinding { sha256, role }),
            )
        })
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    let eligibility = TargetEligibility {
        id: EligibilitySnapshotId::from_digest([0; 32]),
        target,
        basis: TargetBasis::LutComposition,
        artifacts,
        base_pin: None,
        support_uncertainty_source: None,
        status: TargetEligibilityStatus::Ineligible,
        reasons: vec![
            TargetEligibilityReason::LutApproximationUnfrozen,
            TargetEligibilityReason::BasePinMissing,
            TargetEligibilityReason::SupportUncertaintyMissing,
        ],
        dependencies: Vec::new(),
    };
    model_store::record_target_eligibility(conn, &eligibility, now)
        .await
        .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn commit(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    look: &LookSlug,
    proposal: &ProposalId,
    sha256: &[u8; 32],
    artifacts: &[ManifestArtifact],
    base: Option<&BaseReference>,
    base_status: Option<&BaseStatus>,
    base_pin: Option<&(LookSlug, u32)>,
    eligibility_base: Option<&BaseReference>,
    base_snapshot: &[DependencyGeneration],
    preview: bool,
    preview_base: Option<&BaseReference>,
    manifest_status: admin::VersionStatus,
) -> Result<()> {
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
            return Err(WorkerError::Job("lut import lost its lease".to_owned()));
        }
        let failed: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures WHERE proposal_id = ?")
                .bind(proposal.as_str())
                .fetch_one(&mut *conn)
                .await?;
        if failed > 0 {
            return Err(WorkerError::Data(
                "the import was expired before publication".to_owned(),
            ));
        }
        let current: Option<String> =
            sqlx::query_scalar("SELECT status FROM proposals WHERE id = ?")
                .bind(proposal.as_str())
                .fetch_optional(&mut *conn)
                .await?;
        if current.and_then(|value| ProposalStatus::parse(&value)) != Some(ProposalStatus::Approved)
        {
            return Err(WorkerError::Data(
                "the proposal is no longer approved".to_owned(),
            ));
        }
        let uploads: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads WHERE proposal_id = ?")
                .bind(proposal.as_str())
                .fetch_one(&mut *conn)
                .await?;
        if uploads == 0 {
            return Err(WorkerError::Data(
                "the LUT upload was released before publication".to_owned(),
            ));
        }
        if let Some((base_look, base_version)) = base_pin {
            let status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM look_versions WHERE look = ? AND version = ?",
            )
            .bind(base_look.as_str())
            .bind(i64::from(*base_version))
            .fetch_optional(&mut *conn)
            .await?;
            let status = status.and_then(|value| db::LookVersionStatus::parse(&value).ok());
            if !matches!(
                status,
                Some(db::LookVersionStatus::Published | db::LookVersionStatus::Rooted)
            ) {
                return Err(WorkerError::Data(
                    "the selected base Look version is no longer published".to_owned(),
                ));
            }
            if admin::is_version_invalidated_conn(&mut conn, base_look, *base_version).await? {
                return Err(WorkerError::Data(
                    "the selected base Look version was invalidated".to_owned(),
                ));
            }
        }
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
            .bind(look.as_str())
            .fetch_one(&mut *conn)
            .await?;
        if existing > 0 {
            return Err(WorkerError::Data(format!(
                "{look} already has published versions"
            )));
        }
        admin::write_version_manifest_conn(
            &mut conn,
            look,
            VersionId::new(1),
            manifest_status,
            artifacts,
            &[],
        )
        .await?;
        if let (Some(base), Some(status)) = (base, base_status) {
            admin::record_lut_base_conn(&mut conn, look, 1, base, status).await?;
        }
        if let Some(preview_base) = preview_base {
            admin::record_lut_preview_base_conn(&mut conn, look, 1, preview_base).await?;
        }
        if preview {
            record_preview_eligibility(&mut conn, look, 1, artifacts, now_unix()).await?;
        } else {
            record_eligibility(
                &mut conn,
                look,
                1,
                artifacts,
                eligibility_base,
                base_snapshot,
                now_unix(),
            )
            .await?;
        }
        sqlx::query(
            "INSERT INTO lut_imports (proposal_id, look, version, created_at)
             VALUES (?, ?, 1, ?)",
        )
        .bind(proposal.as_str())
        .bind(look.as_str())
        .bind(now_unix())
        .execute(&mut *conn)
        .await?;
        release_upload(
            &mut conn,
            proposal,
            sha256,
            if preview {
                UploadBlob::Keep
            } else {
                UploadBlob::Delete
            },
        )
        .await?;
        sqlx::query("DELETE FROM blob_staging WHERE attempt_id = ?")
            .bind(artifact_attempt(ownership))
            .execute(&mut *conn)
            .await?;
        looklab_server::request_publication(&mut conn).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

async fn fail(
    pool: &SqlitePool,
    ownership: &JobOwnership,
    proposal: &ProposalId,
    sha256: &[u8; 32],
    reason: &str,
) -> Result<()> {
    let now = now_unix();
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
            return Err(WorkerError::Job("lut import lost its lease".to_owned()));
        }
        sqlx::query(
            "INSERT INTO lut_import_failures (proposal_id, reason, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT (proposal_id) DO NOTHING",
        )
        .bind(proposal.as_str())
        .bind(reason)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        release_upload(&mut conn, proposal, sha256, UploadBlob::Delete).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(())
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UploadBlob {
    Delete,
    /// The published manifest keeps the uploaded bytes as an artifact.
    Keep,
}

async fn release_upload(
    conn: &mut sqlx::SqliteConnection,
    proposal: &ProposalId,
    sha256: &[u8; 32],
    blob: UploadBlob,
) -> Result<()> {
    sqlx::query("DELETE FROM lut_uploads WHERE proposal_id = ?")
        .bind(proposal.as_str())
        .execute(&mut *conn)
        .await?;
    if blob == UploadBlob::Keep {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO lut_blob_deletions (hash, queued_at) VALUES (?, ?)
         ON CONFLICT (hash) DO NOTHING",
    )
    .bind(sha256.as_slice())
    .bind(now_unix())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobId;
    use looklab_server::open_database;
    use tempfile::TempDir;

    const DIGEST: &str = "abababababababababababababababababababababababababababababababab";

    async fn fixture(
        status: ProposalStatus,
    ) -> (TempDir, SqlitePool, WorkerConfig, ProposalId, JobOwnership) {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("db.sqlite");
        let pool = open_database(&db_path).await.expect("database");
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).expect("blobs");
        sqlx::query(
            "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
             VALUES ('cred', x'01', '{}', 0)",
        )
        .execute(&pool)
        .await
        .expect("credential");
        let proposal = ProposalId::new();
        let payload = format!(
            "look: test-lut\nname: Test LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut:\n  file: test-lut.cube\n  sha256: {DIGEST}\n  \
             size: 4\nencoding: flog2_fgamut\n"
        );
        sqlx::query(
            "INSERT INTO proposals
             (id, credential_id, type, payload, human_summary, evidence, status, created_at)
             VALUES (?, 'cred', 'lut_look', ?, 'x', '[]', ?, 0)",
        )
        .bind(proposal.as_str())
        .bind(payload)
        .bind(status.as_str())
        .execute(&pool)
        .await
        .expect("proposal");
        let job = JobId::new();
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, 'lut_import', ?, 'running', 1, 0, 0)",
        )
        .bind(job.as_str())
        .bind(serde_json::json!({ "proposal_id": proposal.as_str() }).to_string())
        .execute(&pool)
        .await
        .expect("job");
        let ownership = JobOwnership {
            id: job,
            attempts: 1,
        };
        (
            dir,
            pool,
            WorkerConfig::new(db_path, blobs),
            proposal,
            ownership,
        )
    }

    #[tokio::test]
    async fn an_imported_proposal_is_not_imported_again() {
        let (_dir, pool, config, proposal, ownership) = fixture(ProposalStatus::Approved).await;
        sqlx::query(
            "INSERT INTO lut_imports (proposal_id, look, version, created_at)
             VALUES (?, 'test-lut', 1, 0)",
        )
        .bind(proposal.as_str())
        .execute(&pool)
        .await
        .expect("import row");
        run(&pool, &config, &ownership, &proposal)
            .await
            .expect("idempotent");
        let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures")
            .fetch_one(&pool)
            .await
            .expect("failures");
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn a_pending_proposal_is_skipped() {
        let (_dir, pool, config, proposal, ownership) = fixture(ProposalStatus::Pending).await;
        run(&pool, &config, &ownership, &proposal)
            .await
            .expect("skipped");
        let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures")
            .fetch_one(&pool)
            .await
            .expect("failures");
        assert_eq!(failures, 0);
    }

    #[tokio::test]
    async fn a_stale_attempt_cannot_record_a_failure_or_release_the_upload() {
        let (_dir, pool, _config, proposal, ownership) = fixture(ProposalStatus::Approved).await;
        let sha256 = [0xabu8; 32];
        sqlx::query(
            "INSERT INTO lut_uploads (proposal_id, sha256, credential_id, file, size, created_at)
             VALUES (?, ?, 'cred', 'test-lut.cube', 4, 0)",
        )
        .bind(proposal.as_str())
        .bind(sha256.as_slice())
        .execute(&pool)
        .await
        .expect("upload");
        let stale = JobOwnership {
            id: ownership.id.clone(),
            attempts: 2,
        };
        let error = super::fail(&pool, &stale, &proposal, &sha256, "stale")
            .await
            .expect_err("stale failure");
        assert!(matches!(error, WorkerError::Job(_)));
        let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures")
            .fetch_one(&pool)
            .await
            .expect("failures");
        assert_eq!(failures, 0);
        let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads")
            .fetch_one(&pool)
            .await
            .expect("uploads");
        assert_eq!(uploads, 1);
    }

    #[tokio::test]
    async fn a_stale_attempt_cannot_commit_a_version() {
        let (_dir, pool, _, proposal, ownership) = fixture(ProposalStatus::Approved).await;
        let sha256 = [0xabu8; 32];
        sqlx::query(
            "INSERT INTO lut_uploads (proposal_id, sha256, credential_id, file, size, created_at)
             VALUES (?, ?, 'cred', 'test-lut.cube', 4, 0)",
        )
        .bind(proposal.as_str())
        .bind(sha256.as_slice())
        .execute(&pool)
        .await
        .expect("upload");
        let stale = JobOwnership {
            id: ownership.id.clone(),
            attempts: 2,
        };
        let look = LookSlug::new("test-lut").expect("look");
        let error = super::commit(
            &pool,
            &stale,
            &look,
            &proposal,
            &sha256,
            &[],
            None,
            None,
            None,
            None,
            &[],
            false,
            None,
            admin::VersionStatus::Published,
        )
        .await
        .expect_err("stale commit");
        assert!(matches!(error, WorkerError::Job(_)));
        let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions")
            .fetch_one(&pool)
            .await
            .expect("versions");
        assert_eq!(versions, 0);
        let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads")
            .fetch_one(&pool)
            .await
            .expect("uploads");
        assert_eq!(uploads, 1);
    }

    #[tokio::test]
    async fn an_upload_that_does_not_match_its_digest_records_a_failure() {
        let (_dir, pool, config, proposal, ownership) = fixture(ProposalStatus::Approved).await;
        let sha256 = [0xabu8; 32];
        sqlx::query(
            "INSERT INTO lut_uploads (proposal_id, sha256, credential_id, file, size, created_at)
             VALUES (?, ?, 'cred', 'test-lut.cube', 4, 0)",
        )
        .bind(proposal.as_str())
        .bind(sha256.as_slice())
        .execute(&pool)
        .await
        .expect("upload");
        std::fs::write(db::blob_path(&config.blobs, &sha256), b"bad!").expect("blob");
        run(&pool, &config, &ownership, &proposal)
            .await
            .expect("failure recorded");
        crate::gc::run(&pool, &config, None).await.expect("gc");
        let reason: Option<String> =
            sqlx::query_scalar("SELECT reason FROM lut_import_failures WHERE proposal_id = ?")
                .bind(proposal.as_str())
                .fetch_optional(&pool)
                .await
                .expect("failure");
        assert!(reason.expect("failure row").contains("digest"));
        let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads")
            .fetch_one(&pool)
            .await
            .expect("uploads");
        assert_eq!(uploads, 0);
        assert!(!db::blob_path(&config.blobs, &sha256).exists());
    }

    /// A 2³ cube whose output is the complement of its input.
    const INVERTING_CUBE: &str = "TITLE \"Invert\"\nLUT_3D_SIZE 2\nDOMAIN_MIN 0 0 0\n\
        DOMAIN_MAX 1 1 1\n1 1 1\n0 1 1\n1 0 1\n0 0 1\n1 1 0\n0 1 0\n1 0 0\n0 0 0\n";

    async fn published_preview_base(pool: &SqlitePool, blobs: &std::path::Path) {
        let cube_bytes = INVERTING_CUBE.as_bytes().to_vec();
        let cube = read_cube(INVERTING_CUBE).expect("cube");
        let encoding = DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status: BaseStatus::BaseIncomplete {
                missing: looklab_model::file::BaseField::FilmSimulation,
            },
        };
        let record = lut_data_from_cube(&cube).expect("lut").record();
        let preview = bake_lut_preview(
            &cube.lut,
            &encoding,
            OutputEncoding::Srgb,
            &record,
            &BaseReference::neutral_baseline(),
            None,
        )
        .expect("preview");
        let descriptor = SourceLutPreview {
            format: SourceLutFormat::V1,
            look: LookIdentity::new(
                "base-look",
                "1.0.0",
                LookKind::LutDefined,
                WbPolicy::Adaptive,
            )
            .expect("identity"),
            name: "Preview base".to_owned(),
            license: LicenseRecord {
                own_work: true,
                source_license: None,
                source_url: None,
                attribution: "a donor".to_owned(),
            },
            encoding,
            output: OutputEncoding::Srgb,
            emulates: None,
            source: ArtifactEntry {
                name: ArtifactName::new(SOURCE_CUBE_ARTIFACT).expect("name"),
                sha256: ArtifactHash::of(&cube_bytes),
                bytes: cube_bytes.len() as u64,
            },
            preview: SourceLutPreviewContext {
                baseline: PreviewBaseline::NeutralFallback,
                lut: ArtifactEntry {
                    name: ArtifactName::new(PREVIEW_LUT_ARTIFACT).expect("name"),
                    sha256: ArtifactHash::of(&preview),
                    bytes: preview.len() as u64,
                },
            },
        };
        descriptor.validate().expect("descriptor");
        let mut artifacts = Vec::new();
        for (name, bytes) in [
            (SOURCE_LUT_ARTIFACT, descriptor.to_json().expect("json")),
            (PREVIEW_LUT_ARTIFACT, preview),
            (SOURCE_CUBE_ARTIFACT, cube_bytes),
        ] {
            let sha256: [u8; 32] = sha2::Sha256::digest(&bytes).into();
            std::fs::write(db::blob_path(blobs, &sha256), &bytes).expect("base blob");
            artifacts.push(ManifestArtifact {
                name: name.to_owned(),
                sha256: db::hex(&sha256),
            });
        }
        admin::write_version_manifest(
            pool,
            &LookSlug::new("base-look").expect("slug"),
            looklab_trust::VersionId::new(1),
            admin::VersionStatus::Published,
            &artifacts,
            &[],
        )
        .await
        .expect("base manifest");
    }

    #[tokio::test]
    async fn an_unpinned_base_publishes_the_lut_preview_without_a_fit() {
        let dir = TempDir::new().expect("tempdir");
        let db_path = dir.path().join("db.sqlite");
        let pool = open_database(&db_path).await.expect("database");
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).expect("blobs");
        sqlx::query(
            "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
             VALUES ('cred', x'01', '{}', 0)",
        )
        .execute(&pool)
        .await
        .expect("credential");
        published_preview_base(&pool, &blobs).await;

        let bytes = INVERTING_CUBE.as_bytes().to_vec();
        let sha256: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        let proposal = ProposalId::new();
        let proposal_payload = LutLookProposal {
            look: LookId::new("test-lut").expect("look"),
            name: "Test LUT".to_owned(),
            license: looklab_schema::LicenseDeclaration {
                own_work: true,
                source_license: None,
                source_url: None,
            },
            attribution: "A contributor".to_owned(),
            lut: looklab_schema::LutFile {
                file: "test-lut.cube".to_owned(),
                sha256: looklab_schema::Sha256Hex::parse(&db::hex(&sha256)).expect("digest"),
                size: bytes.len() as u64,
            },
            encoding: looklab_schema::LutEncoding::Display {
                base: looklab_schema::LutBase::Look {
                    look: LookId::new("base-look").expect("base look"),
                    version: looklab_schema::LutLookVersion::new("1.0.0").expect("version"),
                },
            },
            emulates: None,
        };
        let payload = serde_norway::to_string(&proposal_payload).expect("proposal yaml");
        sqlx::query(
            "INSERT INTO proposals
             (id, credential_id, type, payload, human_summary, evidence, status, created_at)
             VALUES (?, 'cred', 'lut_look', ?, 'x', '[]', 'approved', 0)",
        )
        .bind(proposal.as_str())
        .bind(payload)
        .execute(&pool)
        .await
        .expect("proposal");
        sqlx::query(
            "INSERT INTO lut_uploads (proposal_id, sha256, credential_id, file, size, created_at)
             VALUES (?, ?, 'cred', 'test-lut.cube', ?, 0)",
        )
        .bind(proposal.as_str())
        .bind(sha256.as_slice())
        .bind(bytes.len() as i64)
        .execute(&pool)
        .await
        .expect("upload");
        std::fs::write(db::blob_path(&blobs, &sha256), &bytes).expect("blob");
        let job = JobId::new();
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, 'lut_import', ?, 'running', 1, 0, 0)",
        )
        .bind(job.as_str())
        .bind(serde_json::json!({ "proposal_id": proposal.as_str() }).to_string())
        .execute(&pool)
        .await
        .expect("job");
        let config = WorkerConfig::new(&db_path, &blobs);
        let ownership = JobOwnership {
            id: job,
            attempts: 1,
        };
        run(&pool, &config, &ownership, &proposal)
            .await
            .expect("import");
        let failures: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_import_failures")
            .fetch_one(&pool)
            .await
            .expect("failures");
        assert_eq!(failures, 0);

        let artifacts = db::manifest_artifacts(&pool, "test-lut", 1)
            .await
            .expect("manifest")
            .expect("version");
        assert!(
            !artifacts
                .iter()
                .any(|artifact| artifact.name == LOOK_ARTIFACT
                    || artifact.name == CARD_ARTIFACT
                    || artifact.name == RELEASE_ARTIFACT)
        );

        let read = |name: &str| {
            let found = artifacts
                .iter()
                .find(|artifact| artifact.name == name)
                .expect("artifact");
            std::fs::read(blobs.join(&found.sha256)).expect("artifact bytes")
        };
        let descriptor = SourceLutPreview::decode(&read(SOURCE_LUT_ARTIFACT)).expect("descriptor");
        assert!(matches!(
            &descriptor.encoding,
            DeclaredEncoding::DisplayReferred {
                status: BaseStatus::BaseIncomplete { .. },
                ..
            }
        ));
        assert_eq!(
            descriptor.preview.baseline,
            PreviewBaseline::NeutralFallback
        );
        assert_eq!(read(SOURCE_CUBE_ARTIFACT), bytes);

        let manifest_blobs: Vec<looklab_publish::ArtifactBlob> = artifacts
            .iter()
            .map(|entry| {
                looklab_publish::ArtifactBlob::new(
                    ArtifactName::new(entry.name.as_str()).expect("name"),
                    read(&entry.name),
                )
            })
            .collect();
        let manifest = looklab_publish::VersionManifest::from_files(
            looklab_model::file::Slug::new("test-lut").expect("slug"),
            looklab_model::file::LookVersion::new("1.0.0").expect("version"),
            looklab_publish::LookStatus::Published,
            looklab_publish::GateSummary::default(),
            looklab_publish::ProvenanceSummary::default(),
            &manifest_blobs,
        )
        .expect("manifest");
        assert_eq!(
            looklab_publish::look_shape(&manifest).expect("shape"),
            looklab_publish::VersionLookShape::SourcePreview
        );
        descriptor
            .verify_artifacts(&manifest)
            .expect("descriptor artifacts");

        let preview = read(PREVIEW_LUT_ARTIFACT);
        assert_eq!(preview.len(), looklab_publish::LUT_BYTES);
        let index = 19;
        let coordinate = index as f64 / (looklab_publish::LUT_SIZE - 1) as f64;
        let linear = looklab_publish::preview_unshaper(coordinate);
        let xyz = looklab_color::RgbSpace::Rec2020
            .rgb_linear_to_xyz(looklab_color::Rgb::new(linear, linear, linear));
        let baseline = looklab_model::baseline::neutral_baseline_srgb_v1(
            looklab_color::RgbSpace::Srgb.xyz_to_rgb_linear(xyz),
        );
        let offset =
            ((index * looklab_publish::LUT_SIZE + index) * looklab_publish::LUT_SIZE + index) * 12;
        for (channel, expected) in [1.0 - baseline.r, 1.0 - baseline.g, 1.0 - baseline.b]
            .into_iter()
            .enumerate()
        {
            let start = offset + channel * 4;
            let actual =
                f32::from_le_bytes(preview[start..start + 4].try_into().expect("channel")) as f64;
            assert!(
                (actual - expected).abs() < 1.0e-6,
                "channel {channel}: {actual} != {expected}"
            );
        }

        let (base_status,): (String,) = sqlx::query_as(
            "SELECT base_status FROM lut_base_pins WHERE look = 'test-lut' AND version = 1",
        )
        .fetch_one(&pool)
        .await
        .expect("base pin");
        assert!(matches!(
            serde_json::from_str::<BaseStatus>(&base_status).expect("status"),
            BaseStatus::BaseIncomplete { .. }
        ));

        let target = PublishedTargetId {
            look: LookId::new("test-lut").expect("look"),
            version: TargetVersion::new(1).expect("version"),
        };
        let mut conn = pool.acquire().await.expect("connection");
        let eligibility = model_store::target_eligibility(&mut conn, &target)
            .await
            .expect("eligibility")
            .expect("snapshot");
        assert_eq!(eligibility.status, TargetEligibilityStatus::Ineligible);
        assert!(
            eligibility
                .reasons
                .contains(&TargetEligibilityReason::LutApproximationUnfrozen)
        );
        assert!(eligibility.base_pin.is_none());
        assert!(
            eligibility
                .artifacts
                .iter()
                .any(|binding| binding.role == ArtifactRole::Preview)
        );
        assert!(
            !eligibility
                .artifacts
                .iter()
                .any(|binding| binding.role == ArtifactRole::Look)
        );
        drop(conn);

        let uploads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_uploads")
            .fetch_one(&pool)
            .await
            .expect("uploads");
        assert_eq!(uploads, 0);
        let imports: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM lut_imports")
            .fetch_one(&pool)
            .await
            .expect("imports");
        assert_eq!(imports, 1);

        crate::gc::run(&pool, &config, None).await.expect("gc");
        assert!(db::blob_path(&config.blobs, &sha256).exists());
        assert_eq!(read(SOURCE_CUBE_ARTIFACT), bytes);
    }
}
