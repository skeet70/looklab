// SPDX-License-Identifier: AGPL-3.0-or-later
//! Resolving a settings card from published, hash-checked artifacts.

use looklab_match::{
    Calibration, Permalink, QualificationRef, ReportPin, ReportSubject, ScoringMetric,
};
use looklab_model::file::LookVersion;
use looklab_publish::{
    ArtifactEntry, ArtifactHash, CameraModelProvenance, ExcludedSim, MatchBundle, SelectionScope,
    SimExclusion, body_artifact, bundle_artifact, bundle_dir, transform_artifact,
};
use looklab_schema::{
    BodyId, MatchBundleRevision, MatchBundleRevisionId, OutputEncoding, Sha256Hex,
};
use looklab_wasm::MatchInputs;
use looklab_wasm::SimInputs;
use looklab_wasm::SimSources;
use looklab_wasm::match_api::{ControlsReport, MatchSelection, SourceNames};

use crate::data::{self, LoadedVersion};

const NO_MODEL: &str = "We do not have a settings model for this camera yet. Try a different camera or help by sharing photos.";
const BODY_PARAMS: &str = "body_params.json";

/// Match inputs plus the selection scope, blockers and target camera-model provenance.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopedInputs {
    pub inputs: MatchInputs,
    pub scope: SelectionScope,
    pub blockers: Vec<String>,
    /// The target version these inputs were resolved from.
    pub target_version: LookVersion,
    /// What the verified target manifest declares about its camera models.
    pub camera_model_provenance: CameraModelProvenance,
}

/// The published target and camera selection a match resolves against.
pub struct MatchRequest<'a> {
    pub version: &'a LoadedVersion,
    pub page: &'a looklab_publish::LookPage,
    pub body_page: &'a looklab_publish::BodyPage,
    pub camera: &'a BodyId,
    pub emphasis: &'a looklab_match::Emphasis,
    pub epoch: u32,
    pub seed: u64,
    pub mode: SelectionScope,
    pub wb_mode: Option<looklab_schema::WbModeKey>,
    pub firmware: Option<looklab_schema::FirmwareId>,
    pub output_encoding: OutputEncoding,
}

/// Resolves a camera match from the published target and bundle selection.
pub async fn inputs_for_camera(request: MatchRequest<'_>) -> Result<ScopedInputs, String> {
    let MatchRequest {
        version,
        page,
        body_page,
        camera,
        emphasis,
        epoch,
        seed,
        mode,
        wb_mode,
        firmware,
        output_encoding,
    } = request;
    let index = data::index().await?;
    let target = data::published_target(&version.manifest.look, &version.manifest.version)
        .ok_or_else(|| "This look version has no published target identity.".to_owned())?;
    let version_page = page
        .versions
        .iter()
        .find(|entry| entry.version() == &version.manifest.version)
        .ok_or_else(|| format!("{} is not in the look page", version.manifest.version))?;
    let mut blockers = Vec::new();
    let mut reports = Vec::new();
    let approximated = version_page.approximated;
    let effective_mode = if approximated {
        blockers.push(
            "This target approximates its declared base, so it is experimental and not checked for these settings."
                .to_owned(),
        );
        SelectionScope::Experimental
    } else {
        mode
    };

    if !version_page.applicability.is_current() {
        blockers.push(format!(
            "This target version is {}.",
            data::applicability_label(&version_page.applicability)
        ));
    }
    if let Some(id) = &version_page.eligibility {
        let snapshot = data::eligibility(&index, id).await?;
        if snapshot.status != looklab_schema::TargetEligibilityStatus::Eligible {
            blockers.push("This version cannot be used for new matches or downloads.".to_owned());
        }
        reports.push(ReportPin {
            subject: ReportSubject::Target(target.clone()),
            reference: QualificationRef::Eligibility(*id),
        });
    } else if let Some(head) = page.head.as_ref()
        && head.target.family.as_str() == target.look.as_str()
        && head.target.version == target.version
        && head.applicability.is_current()
        && let Some(report_id) = head.report
    {
        let report = data::report(&index, &report_id).await?;
        if report.applicability.is_current() {
            reports.push(ReportPin {
                subject: ReportSubject::Target(target.clone()),
                reference: QualificationRef::Report(report_id),
            });
        } else {
            blockers.push(format!(
                "The target's assessment is {}.",
                data::applicability_label(&report.applicability)
            ));
        }
    } else {
        blockers.push("This target has not been checked for these settings.".to_owned());
    }

    let Some(selection) = data::selected_bundle(body_page, epoch) else {
        return legacy_inputs(version, camera, emphasis, epoch, seed, blockers).await;
    };
    if !selection.applicability.is_current() {
        blockers.push(format!(
            "The camera bundle is {}.",
            data::applicability_label(&selection.applicability)
        ));
    }
    let entry = data::bundle_sim_entry(&index, camera, epoch, &selection.bundle.to_hex())
        .ok_or_else(|| "This camera's settings model is not published.".to_owned())?;
    let manifest = data::bundle_revision(&index, entry).await?;
    let artifacts = data::bundle_artifacts(&index, entry).await?;
    if &manifest.body != camera || manifest.epoch.get() != epoch {
        return Err(
            "This settings model belongs to a different camera or firmware version.".to_owned(),
        );
    }
    let body_params = shared_body_params(camera, epoch, &manifest, &artifacts).await?;
    let body_model: looklab_schema::MatchParams = serde_json::from_slice(&body_params)
        .map_err(|error| format!("body parameters: {error}"))?;
    let pinned_wb: Vec<_> = body_model
        .wb
        .iter()
        .map(|entry| entry.mode.clone())
        .collect();

    let mut ranked = Vec::new();
    for sim in &manifest.sims {
        let Some(hash) = sim.realization_params else {
            blockers.push(format!(
                "{} has no saved camera settings.",
                data::sim_name(&sim.native_sim)
            ));
            continue;
        };
        let realization_bytes = bundle_file(
            camera,
            epoch,
            &manifest,
            &artifacts,
            &hash.to_hex(),
            Some(hash),
        )
        .await?;
        let realization: looklab_schema::MatchRealizationParams =
            serde_json::from_slice(&realization_bytes)
                .map_err(|error| format!("realization parameters: {error}"))?;
        realization.validate().map_err(|error| error.to_owned())?;
        let pinned_fw: Vec<_> = realization
            .firmware
            .iter()
            .map(|entry| entry.firmware.clone())
            .collect();
        let (scope, summary) =
            data::bundle_qualification(&index, camera, epoch, &sim.native_sim, &manifest.id, mode)
                .await?;
        if scope == SelectionScope::Unassessed {
            blockers.push(format!(
                "{} has no current settings model for this camera.",
                data::sim_name(&sim.native_sim)
            ));
            continue;
        }
        if let Some(summary) = &summary
            && !summary.scope.points.iter().any(|point| {
                physical_covers(
                    point,
                    firmware.as_ref(),
                    wb_mode.as_ref(),
                    &pinned_fw,
                    &pinned_wb,
                )
            })
        {
            blockers.push(format!(
                "{} has no assessment for the requested firmware or white balance.",
                sim.native_sim
            ));
            if effective_mode == SelectionScope::Qualified {
                continue;
            }
        }
        if effective_mode == SelectionScope::Qualified && scope != SelectionScope::Qualified {
            blockers.push(format!(
                "{} has not been checked for these settings.",
                data::sim_name(&sim.native_sim)
            ));
            continue;
        }
        if let Some(summary) = &summary {
            reports.push(ReportPin {
                subject: ReportSubject::Realization(sim.realization.clone()),
                reference: QualificationRef::Report(summary.report),
            });
        }
        ranked.push((sim, summary, realization.realization, realization_bytes));
    }
    if ranked.is_empty() {
        return Err(no_usable_realization(&blockers));
    }

    let bundle_hex = Sha256Hex::parse(&manifest.id.to_hex())
        .map_err(|_| "The selected bundle revision is not a content digest.".to_owned())?;
    let body_yaml = bundle_file(camera, epoch, &manifest, &artifacts, "body.yaml", None).await?;
    let transform_yaml =
        bundle_file(camera, epoch, &manifest, &artifacts, "transform.yaml", None).await?;
    let transform_yaml = String::from_utf8(transform_yaml)
        .map_err(|_| "Camera color conversion is unreadable.".to_owned())?;
    let input_transform_version =
        looklab_wasm::match_api::transform_version(&transform_yaml).unwrap_or(1);
    let mut sims = Vec::with_capacity(ranked.len());
    let mut controls = Vec::new();
    for (sim, summary, pinned, realization_bytes) in ranked {
        let base = bundle_file(
            camera,
            epoch,
            &manifest,
            &artifacts,
            &sim.base_artifact.to_hex(),
            Some(sim.base_artifact),
        )
        .await?;
        let crm = bundle_file(
            camera,
            epoch,
            &manifest,
            &artifacts,
            &sim.control_response.to_hex(),
            Some(sim.control_response),
        )
        .await?;
        let covariance = match sim.covariance {
            Some(hash) => Some(
                bundle_file(
                    camera,
                    epoch,
                    &manifest,
                    &artifacts,
                    &hash.to_hex(),
                    Some(hash),
                )
                .await?,
            ),
            None => None,
        };
        let controls_report = match &summary {
            Some(summary) => Some(
                ControlsReport::from_pinned_summary(summary, &pinned)
                    .map_err(|error| error.to_string())?,
            ),
            None => None,
        };
        sims.push(SimInputs {
            sim: sim.native_sim.clone(),
            base_look: base,
            crm,
            covariance,
            sources: SimSources {
                base: Some(sim.base_artifact.to_hex()),
                crm: Some(sim.control_response.to_hex()),
                covariance: sim.covariance.map(Sha256Hex::to_hex),
            },
            realization_version: Some(sim.realization_params_version),
            params: Some(body_params.clone()),
            realization_params: Some(realization_bytes),
            wb_mode: wb_mode.clone(),
            firmware: firmware.clone(),
        });
        if let Some(controls_report) = controls_report {
            controls.push(controls_report);
        }
    }
    if effective_mode == SelectionScope::Qualified {
        if controls.is_empty() {
            return Err(
                "A qualified match needs a report that declares its camera output encoding."
                    .to_owned(),
            );
        }
        if controls
            .iter()
            .any(|report| report.scope.output_encoding != output_encoding)
        {
            return Err(
                "The requested camera output encoding is not covered by the pinned qualification report."
                    .to_owned(),
            );
        }
    } else if !controls
        .iter()
        .any(|report| report.scope.output_encoding == output_encoding)
    {
        blockers.push(
            "No qualification report covers the requested camera output encoding; matching is experimental."
                .to_owned(),
        );
    }
    let scope = if effective_mode == SelectionScope::Qualified {
        if !blockers.is_empty() {
            return Err(no_usable_realization(&blockers));
        }
        SelectionScope::Qualified
    } else {
        SelectionScope::Experimental
    };
    let excluded = excluded_sims(&manifest);
    let destination = destination_scope(scope, &excluded);
    Ok(ScopedInputs {
        inputs: MatchInputs {
            target_look: artifact(version, "look.looklab")?.to_vec(),
            sims,
            excluded,
            body_yaml: String::from_utf8(body_yaml)
                .map_err(|_| "Camera settings are unreadable.".to_owned())?,
            transform_yaml: Some(transform_yaml),
            emphasis: emphasis.as_str().to_owned(),
            epoch,
            input_transform_version,
            realization_version: 1,
            seed,
            sources: SourceNames {
                target_look: Some("look.looklab".to_owned()),
                body: Some("body.yaml".to_owned()),
                transform: Some("transform.yaml".to_owned()),
            },
            calibration: Calibration::default(),
            selection: Some(MatchSelection {
                bundle: bundle_hex,
                mode: match scope {
                    SelectionScope::Qualified => looklab_match::SelectionMode::Qualified,
                    _ => looklab_match::SelectionMode::Experimental,
                },
                reports,
                controls,
                firmware: firmware.clone(),
                wb_mode: wb_mode.clone(),
            }),
            metric: ScoringMetric::RenderedPostClipV1,
            output_encoding: Some(output_encoding),
            approximated_base: approximated_base(version),
            destination_scope: destination,
        },
        scope,
        blockers,
        target_version: version.manifest.version.clone(),
        camera_model_provenance: looklab_publish::CameraModelProvenance::Unknown,
    })
}

async fn shared_body_params(
    body: &BodyId,
    epoch: u32,
    manifest: &MatchBundleRevision,
    artifacts: &[ArtifactEntry],
) -> Result<Vec<u8>, String> {
    let pinned = manifest.sims.first().and_then(|sim| sim.body_params);
    if pinned.is_none() || manifest.sims.iter().any(|sim| sim.body_params != pinned) {
        return Err("This bundle revision has no single pinned body-params artifact.".to_owned());
    }
    let hash = pinned.expect("checked above");
    bundle_file(body, epoch, manifest, artifacts, BODY_PARAMS, Some(hash)).await
}

async fn bundle_file(
    body: &BodyId,
    epoch: u32,
    manifest: &MatchBundleRevision,
    artifacts: &[ArtifactEntry],
    name: &str,
    sha: Option<Sha256Hex>,
) -> Result<Vec<u8>, String> {
    let entry = artifacts
        .iter()
        .find(|entry| entry.name.as_str() == name)
        .ok_or_else(|| format!("{name} is not listed in the bundle artifacts"))?;
    if let Some(sha) = sha
        && entry.sha256.as_str() != sha.to_hex()
    {
        return Err(format!("{name} does not match its pinned digest"));
    }
    let dir = bundle_dir(body, epoch, &manifest.id).map_err(|error| error.to_string())?;
    let path = dir.join(name);
    let bytes = data::published_raw(&path).await?;
    if ArtifactHash::of(&bytes) != entry.sha256 || bytes.len() as u64 != entry.bytes {
        return Err(format!(
            "{} does not match the bundle artifacts",
            path.as_str()
        ));
    }
    Ok(bytes)
}

fn approximated_base(version: &LoadedVersion) -> Option<String> {
    match &version.content {
        crate::data::VersionContent::Layered(layered) => layered
            .document
            .file
            .definition
            .as_ref()
            .and_then(|definition| definition.declared_base.as_ref())
            .map(looklab_match::base_label),
        crate::data::VersionContent::SourcePreview(_) => None,
    }
}

fn destination_scope(scope: SelectionScope, excluded: &[ExcludedSim]) -> Option<String> {
    let mode = match scope {
        SelectionScope::Qualified => Some(looklab_match::SelectionMode::Qualified),
        SelectionScope::Experimental => Some(looklab_match::SelectionMode::Experimental),
        SelectionScope::Unassessed => None,
    };
    let sims: Vec<String> = excluded
        .iter()
        .map(|entry| entry.sim.as_str().to_owned())
        .collect();
    looklab_match::destination_scope(mode, &sims)
}

fn excluded_sims(manifest: &MatchBundleRevision) -> Vec<ExcludedSim> {
    manifest
        .excluded
        .iter()
        .map(|entry| ExcludedSim {
            sim: entry.native_sim.clone(),
            reason: match entry.reason {
                looklab_schema::BundleExclusionReason::NoPublishedRealization => {
                    SimExclusion::NoPublishedLook
                }
                looklab_schema::BundleExclusionReason::NoControlResponse => {
                    SimExclusion::NoResponse
                }
                looklab_schema::BundleExclusionReason::IncompatibleRevision => {
                    SimExclusion::UnacceptedRealization
                }
                looklab_schema::BundleExclusionReason::MissingFirmware => {
                    SimExclusion::MissingFirmware
                }
            },
        })
        .collect()
}

fn physical_covers(
    point: &looklab_schema::AssessedPoint,
    requested_firmware: Option<&looklab_schema::FirmwareId>,
    requested_wb: Option<&looklab_schema::WbModeKey>,
    pinned_firmware: &[looklab_schema::FirmwareId],
    pinned_wb: &[looklab_schema::WbModeKey],
) -> bool {
    let firmware = match (&point.firmware, requested_firmware) {
        (Some(mode), Some(requested)) => mode == requested && pinned_firmware.contains(mode),
        (Some(mode), None) => pinned_firmware.len() == 1 && pinned_firmware.contains(mode),
        (None, Some(requested)) => pinned_firmware.len() == 1 && &pinned_firmware[0] == requested,
        (None, None) => pinned_firmware.len() == 1,
    };
    let wb = match (&point.wb_mode, requested_wb) {
        (Some(mode), Some(requested)) => mode == requested && pinned_wb.contains(mode),
        (Some(mode), None) => pinned_wb.len() == 1 && pinned_wb.contains(mode),
        (None, Some(requested)) => pinned_wb.len() == 1 && &pinned_wb[0] == requested,
        (None, None) => pinned_wb.len() == 1,
    };
    firmware && wb
}

fn eligibility_status_label(status: looklab_schema::TargetEligibilityStatus) -> &'static str {
    match status {
        looklab_schema::TargetEligibilityStatus::Eligible => "eligible",
        looklab_schema::TargetEligibilityStatus::Ineligible => "not eligible",
        looklab_schema::TargetEligibilityStatus::Invalidated => "no longer valid",
    }
}

fn no_usable_realization(blockers: &[String]) -> String {
    if blockers.is_empty() {
        NO_MODEL.to_owned()
    } else {
        format!("{NO_MODEL} {}", blockers.join(" "))
    }
}

pub async fn inputs_from_permalink(permalink: &Permalink) -> Result<ScopedInputs, String> {
    if let Some(selection) = &permalink.selection {
        return current_inputs_from_permalink(permalink, selection).await;
    }
    let (inputs, camera_model_provenance, target_version) =
        legacy_inputs_from_permalink(permalink).await?;
    Ok(ScopedInputs {
        inputs,
        scope: SelectionScope::Unassessed,
        blockers: vec![
            "This saved link records no check, so it replays as an older, unchecked recipe."
                .to_owned(),
        ],
        target_version,
        camera_model_provenance,
    })
}

/// Replays the exact target and camera bundle a link recorded.
pub async fn replay_inputs_from_permalink(permalink: &Permalink) -> Result<ScopedInputs, String> {
    if let Some(selection) = &permalink.selection {
        return scoped_inputs_from_permalink(permalink, selection).await;
    }
    inputs_from_permalink(permalink).await
}

async fn current_inputs_from_permalink(
    permalink: &Permalink,
    selection: &looklab_match::SelectionContext,
) -> Result<ScopedInputs, String> {
    let page = data::look(permalink.target_slug.as_str()).await?;
    let version = crate::selection::canonical_version(&page).ok_or_else(|| {
        format!(
            "{} has no current accepted target. Choose another look.",
            permalink.target_slug
        )
    })?;
    let loaded =
        data::match_version(permalink.target_slug.as_str(), version.version().as_str()).await?;
    let body = BodyId::new(permalink.body.as_str()).map_err(|error| error.to_string())?;
    let body_page = data::body(body.make(), body.model()).await?;
    let scope = match selection.mode {
        looklab_match::SelectionMode::Qualified => SelectionScope::Qualified,
        looklab_match::SelectionMode::Experimental => SelectionScope::Experimental,
    };
    let output = permalink
        .output
        .map(|context| context.camera.encoding)
        .unwrap_or(OutputEncoding::Srgb);
    let mut scoped = inputs_for_camera(MatchRequest {
        version: &loaded,
        page: &page,
        body_page: &body_page,
        camera: &body,
        emphasis: &permalink.emphasis,
        epoch: permalink.epoch,
        seed: permalink.seed,
        mode: scope,
        wb_mode: selection.wb_mode.clone(),
        firmware: selection.firmware.clone(),
        output_encoding: output,
    })
    .await?;
    scoped.inputs.calibration = permalink.calibration.clone();
    Ok(scoped)
}

async fn scoped_inputs_from_permalink(
    permalink: &Permalink,
    selection: &looklab_match::SelectionContext,
) -> Result<ScopedInputs, String> {
    let index = data::index().await?;
    let body = BodyId::new(permalink.body.as_str()).map_err(|error| error.to_string())?;
    let bundle_hex = selection.bundle.to_hex();
    let entry = index
        .bundles
        .iter()
        .find(|entry| {
            entry.body == body
                && entry.epoch == permalink.epoch
                && entry.bundle.to_hex() == bundle_hex
        })
        .ok_or_else(|| {
            format!(
                "The pinned bundle revision {bundle_hex} is not published, so this link cannot be replayed."
            )
        })?;
    let manifest = data::bundle_revision(&index, entry).await?;
    let artifacts = data::bundle_artifacts(&index, entry).await?;
    if manifest.body != body || manifest.epoch.get() != permalink.epoch {
        return Err(
            "This link's bundle revision is for a different camera or firmware version.".to_owned(),
        );
    }
    let target = data::match_version(
        permalink.target_slug.as_str(),
        permalink.target_version.as_str(),
    )
    .await?;
    let winner_sim =
        looklab_schema::SimKey::new(permalink.sim.as_str()).map_err(|error| error.to_string())?;
    let winner = manifest
        .sims
        .iter()
        .find(|sim| sim.native_sim == winner_sim)
        .ok_or_else(|| {
            format!(
                "The pinned sim {winner_sim} is not in bundle revision {bundle_hex}, so this link cannot reproduce its settings."
            )
        })?;
    let allowed_dirs = source_dirs(&target, Some(&manifest.id), &body, permalink.epoch)?;
    let target_dirs = source_dirs(&target, None, &body, permalink.epoch)?;
    let candidates = source_candidates(&target, &artifacts);
    let target_candidates = source_candidates(&target, &[]);
    pinned_named_source(
        "target look",
        permalink.sources.target_look.as_deref(),
        Some("look.looklab"),
        &target_candidates,
        &target_dirs,
    )?;
    let base_hash = winner.base_artifact.to_hex();
    pinned_hashed_source(
        "base look",
        permalink.sources.base_look.as_deref(),
        Some(base_hash.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    let crm_hash = winner.control_response.to_hex();
    pinned_hashed_source(
        "camera settings model",
        permalink.sources.crm.as_deref(),
        Some(crm_hash.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    let covariance_hash = winner.covariance.map(Sha256Hex::to_hex);
    pinned_hashed_source(
        "covariance",
        permalink.sources.covariance.as_deref(),
        covariance_hash.as_deref(),
        &candidates,
        &allowed_dirs,
    )?;
    let body_entry = artifacts
        .iter()
        .find(|entry| entry.name.as_str() == "body.yaml")
        .ok_or_else(|| "body.yaml is not listed in the bundle artifacts".to_owned())?;
    pinned_hashed_source(
        "body schema",
        permalink.sources.body.as_deref(),
        Some(body_entry.sha256.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    let transform_entry = artifacts
        .iter()
        .find(|entry| entry.name.as_str() == "transform.yaml")
        .ok_or_else(|| "transform.yaml is not listed in the bundle artifacts".to_owned())?;
    pinned_hashed_source(
        "transform",
        permalink.sources.transform.as_deref(),
        Some(transform_entry.sha256.as_str()),
        &candidates,
        &allowed_dirs,
    )?;

    let mut notices = Vec::new();
    for pin in &selection.reports {
        match &pin.reference {
            QualificationRef::Report(id) => {
                let entry = data::report_entry(&index, id)
                    .ok_or_else(|| format!("The pinned report {id} is not published."))?;
                if !entry.applicability.is_current() {
                    notices.push(format!(
                        "Pinned report {id} is {}.",
                        data::applicability_label(&entry.applicability)
                    ));
                }
            }
            QualificationRef::Eligibility(id) => {
                let entry = data::eligibility_entry(&index, id).ok_or_else(|| {
                    "This saved link pins an eligibility record that is no longer published."
                        .to_owned()
                })?;
                if entry.status != looklab_schema::TargetEligibilityStatus::Eligible {
                    notices.push(format!(
                        "This saved link pins an eligibility record that is {}.",
                        eligibility_status_label(entry.status)
                    ));
                }
            }
        }
    }
    let body_params = shared_body_params(&body, permalink.epoch, &manifest, &artifacts).await?;
    let mut sims = Vec::with_capacity(manifest.sims.len());
    let mut controls = Vec::new();
    for sim in &manifest.sims {
        let base = bundle_file(
            &body,
            permalink.epoch,
            &manifest,
            &artifacts,
            &sim.base_artifact.to_hex(),
            Some(sim.base_artifact),
        )
        .await?;
        let crm = bundle_file(
            &body,
            permalink.epoch,
            &manifest,
            &artifacts,
            &sim.control_response.to_hex(),
            Some(sim.control_response),
        )
        .await?;
        let covariance = match sim.covariance {
            Some(hash) => Some(
                bundle_file(
                    &body,
                    permalink.epoch,
                    &manifest,
                    &artifacts,
                    &hash.to_hex(),
                    Some(hash),
                )
                .await?,
            ),
            None => None,
        };
        let realization_params = match sim.realization_params {
            Some(hash) => Some(
                bundle_file(
                    &body,
                    permalink.epoch,
                    &manifest,
                    &artifacts,
                    &hash.to_hex(),
                    Some(hash),
                )
                .await?,
            ),
            None => {
                return Err(format!(
                    "{} has no pinned realization parameters in this bundle revision.",
                    sim.native_sim
                ));
            }
        };
        let controls_report = if let Some(pin) = selection
            .reports
            .iter()
            .find(|pin| pin.subject == ReportSubject::Realization(sim.realization.clone()))
            && let QualificationRef::Report(id) = &pin.reference
        {
            let report = data::report(&index, id).await?;
            let bytes = realization_params.as_deref().ok_or_else(|| {
                format!(
                    "{} has no pinned realization parameters in this bundle revision.",
                    sim.native_sim
                )
            })?;
            let pinned: looklab_schema::MatchRealizationParams = serde_json::from_slice(bytes)
                .map_err(|error| format!("realization parameters: {error}"))?;
            Some(
                ControlsReport::from_pinned_summary(&report.summary, &pinned.realization)
                    .map_err(|error| error.to_string())?,
            )
        } else {
            None
        };
        let sources = if sim.native_sim == winner_sim {
            SimSources {
                base: permalink.sources.base_look.clone(),
                crm: permalink.sources.crm.clone(),
                covariance: permalink.sources.covariance.clone(),
            }
        } else {
            SimSources {
                base: Some(sim.base_artifact.to_hex()),
                crm: Some(sim.control_response.to_hex()),
                covariance: sim.covariance.map(Sha256Hex::to_hex),
            }
        };
        sims.push(SimInputs {
            sim: sim.native_sim.clone(),
            base_look: base,
            crm,
            covariance,
            sources,
            realization_version: Some(sim.realization_params_version),
            params: Some(body_params.clone()),
            realization_params,
            wb_mode: selection.wb_mode.clone(),
            firmware: selection.firmware.clone(),
        });
        if let Some(controls_report) = controls_report {
            controls.push(controls_report);
        }
    }
    if sims.is_empty() {
        return Err(NO_MODEL.to_owned());
    }
    let body_yaml = bundle_file(
        &body,
        permalink.epoch,
        &manifest,
        &artifacts,
        "body.yaml",
        None,
    )
    .await?;
    let transform_yaml = bundle_file(
        &body,
        permalink.epoch,
        &manifest,
        &artifacts,
        "transform.yaml",
        None,
    )
    .await?;
    let transform_yaml = String::from_utf8(transform_yaml)
        .map_err(|_| "Camera color conversion is unreadable.".to_owned())?;
    let scope = match selection.mode {
        looklab_match::SelectionMode::Qualified => SelectionScope::Qualified,
        looklab_match::SelectionMode::Experimental => SelectionScope::Experimental,
    };
    let excluded = excluded_sims(&manifest);
    let destination = destination_scope(scope, &excluded);
    Ok(ScopedInputs {
        inputs: MatchInputs {
            target_look: artifact(&target, "look.looklab")?.to_vec(),
            sims,
            excluded,
            body_yaml: String::from_utf8(body_yaml)
                .map_err(|_| "Camera settings are unreadable.".to_owned())?,
            input_transform_version: looklab_wasm::match_api::transform_version(&transform_yaml)
                .unwrap_or(permalink.input_transform_version),
            transform_yaml: Some(transform_yaml),
            emphasis: permalink.emphasis.as_str().to_owned(),
            epoch: permalink.epoch,
            realization_version: permalink.realization_params_version,
            seed: permalink.seed,
            sources: SourceNames {
                target_look: permalink.sources.target_look.clone(),
                body: permalink.sources.body.clone(),
                transform: permalink.sources.transform.clone(),
            },
            calibration: permalink.calibration.clone(),
            selection: Some(MatchSelection {
                bundle: selection.bundle,
                mode: selection.mode,
                reports: selection.reports.clone(),
                controls,
                firmware: selection.firmware.clone(),
                wb_mode: selection.wb_mode.clone(),
            }),
            metric: permalink.metric,
            output_encoding: permalink.output.map(|context| context.camera.encoding),
            approximated_base: approximated_base(&target),
            destination_scope: destination,
        },
        scope,
        blockers: notices,
        target_version: target.manifest.version.clone(),
        camera_model_provenance: looklab_publish::CameraModelProvenance::Unknown,
    })
}

async fn legacy_inputs(
    version: &LoadedVersion,
    camera: &BodyId,
    emphasis: &looklab_match::Emphasis,
    epoch: u32,
    seed: u64,
    mut blockers: Vec<String>,
) -> Result<ScopedInputs, String> {
    let inputs = legacy_inputs_for_camera(version, camera, emphasis, epoch, seed).await?;
    blockers.push(
        "This camera has no current settings model. The older one has not been checked.".to_owned(),
    );
    Ok(ScopedInputs {
        inputs,
        scope: SelectionScope::Unassessed,
        blockers,
        target_version: version.manifest.version.clone(),
        camera_model_provenance: version.manifest.camera_model_provenance(),
    })
}

async fn legacy_inputs_for_camera(
    version: &LoadedVersion,
    camera: &BodyId,
    emphasis: &looklab_match::Emphasis,
    epoch: u32,
    seed: u64,
) -> Result<MatchInputs, String> {
    let descriptor_name = bundle_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let Some(descriptor) = data::artifact(version, descriptor_name.as_str()) else {
        return Err(NO_MODEL.to_owned());
    };
    let bundle = MatchBundle::decode(descriptor).map_err(|error| error.to_string())?;
    if &bundle.body != camera || bundle.epoch != epoch {
        return Err(
            "This look's settings model is for a different camera or firmware version.".to_owned(),
        );
    }
    if bundle.sims.is_empty() {
        return Err(NO_MODEL.to_owned());
    }
    let body_name = body_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let body_yaml = String::from_utf8(artifact(version, body_name.as_str())?.to_vec())
        .map_err(|_| "Camera settings are unreadable.")?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    if &schema.body != camera {
        return Err(NO_MODEL.to_owned());
    }
    let transform_name = transform_artifact(camera, epoch).map_err(|error| error.to_string())?;
    let transform_yaml = String::from_utf8(artifact(version, transform_name.as_str())?.to_vec())
        .map_err(|_| "Camera color conversion is unreadable.")?;
    let mut sims = Vec::with_capacity(bundle.sims.len());
    for entry in &bundle.sims {
        let covariance = match &entry.covariance {
            Some(name) => Some(artifact(version, name.as_str())?.to_vec()),
            None => None,
        };
        sims.push(SimInputs {
            sim: entry.sim.clone(),
            base_look: artifact(version, entry.base.as_str())?.to_vec(),
            crm: artifact(version, entry.crm.as_str())?.to_vec(),
            covariance,
            sources: SimSources {
                base: Some(entry.base.to_string()),
                crm: Some(entry.crm.to_string()),
                covariance: entry.covariance.as_ref().map(ToString::to_string),
            },
            realization_version: None,
            params: None,
            realization_params: None,
            wb_mode: None,
            firmware: None,
        });
    }
    let destination = destination_scope(SelectionScope::Unassessed, &bundle.excluded);
    Ok(MatchInputs {
        target_look: artifact(version, "look.looklab")?.to_vec(),
        sims,
        excluded: bundle.excluded,
        body_yaml,
        input_transform_version: looklab_wasm::match_api::transform_version(&transform_yaml)
            .unwrap_or(1),
        transform_yaml: Some(transform_yaml),
        emphasis: emphasis.as_str().to_owned(),
        epoch,
        realization_version: 1,
        seed,
        sources: SourceNames {
            target_look: Some("look.looklab".into()),
            body: Some(body_name.to_string()),
            transform: Some(transform_name.to_string()),
        },
        calibration: Calibration::default(),
        selection: None,
        metric: ScoringMetric::LegacyPreClipV1,
        output_encoding: None,
        approximated_base: approximated_base(version),
        destination_scope: destination,
    })
}

async fn legacy_inputs_from_permalink(
    permalink: &Permalink,
) -> Result<(MatchInputs, CameraModelProvenance, LookVersion), String> {
    let target = data::match_version(
        permalink.target_slug.as_str(),
        permalink.target_version.as_str(),
    )
    .await?;
    let body =
        looklab_schema::BodyId::new(permalink.body.as_str()).map_err(|error| error.to_string())?;
    let descriptor_name =
        bundle_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    let Some(descriptor) = data::artifact(&target, descriptor_name.as_str()) else {
        return Err(format!(
            "{} {} has no settings bundle for {} on epoch {}, so this link's original search cannot be replayed. Run the match again to make a new link.",
            target.manifest.look, target.manifest.version, permalink.body, permalink.epoch
        ));
    };
    let bundle = MatchBundle::decode(descriptor).map_err(|error| error.to_string())?;
    if bundle.body != body || bundle.epoch != permalink.epoch {
        return Err(
            "This link's settings bundle is for a different camera or firmware version.".to_owned(),
        );
    }
    if bundle.sims.is_empty() {
        return Err(NO_MODEL.to_owned());
    }
    let winner_sim =
        looklab_schema::SimKey::new(permalink.sim.as_str()).map_err(|error| error.to_string())?;
    let winner = bundle
        .sims
        .iter()
        .find(|entry| entry.sim == winner_sim)
        .ok_or_else(|| {
            format!(
                "The pinned sim {winner_sim} is not in the settings bundle of {} {}, so this link cannot reproduce its settings.",
                target.manifest.look, target.manifest.version
            )
        })?;
    let allowed_dirs = source_dirs(&target, None, &body, permalink.epoch)?;
    let candidates = source_candidates(&target, &[]);
    pinned_named_source(
        "base look",
        permalink.sources.base_look.as_deref(),
        Some(winner.base.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    pinned_named_source(
        "camera settings model",
        permalink.sources.crm.as_deref(),
        Some(winner.crm.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    pinned_named_source(
        "covariance",
        permalink.sources.covariance.as_deref(),
        winner.covariance.as_ref().map(|name| name.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    pinned_named_source(
        "target look",
        permalink.sources.target_look.as_deref(),
        Some("look.looklab"),
        &candidates,
        &allowed_dirs,
    )?;
    let target_look = artifact(&target, "look.looklab")?;
    let body_name = body_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    pinned_named_source(
        "body schema",
        permalink.sources.body.as_deref(),
        Some(body_name.as_str()),
        &candidates,
        &allowed_dirs,
    )?;
    let body_yaml = String::from_utf8(artifact(&target, body_name.as_str())?.to_vec())
        .map_err(|_| "Camera settings are unreadable.")?;
    let schema = looklab_schema::validate_control_schema(&body_yaml)
        .map_err(|errors| format!("{errors:?}"))?;
    if schema.body != body {
        return Err(NO_MODEL.to_owned());
    }
    let transform_name =
        transform_artifact(&body, permalink.epoch).map_err(|error| error.to_string())?;
    let transform_yaml = if data::artifact(&target, transform_name.as_str()).is_some() {
        pinned_named_source(
            "transform",
            permalink.sources.transform.as_deref(),
            Some(transform_name.as_str()),
            &candidates,
            &allowed_dirs,
        )?;
        let bytes = artifact(&target, transform_name.as_str())?;
        Some(
            String::from_utf8(bytes.to_vec())
                .map_err(|_| "Camera color conversion is unreadable.")?,
        )
    } else {
        pinned_named_source(
            "transform",
            permalink.sources.transform.as_deref(),
            None,
            &candidates,
            &allowed_dirs,
        )?;
        None
    };
    let mut sims = Vec::with_capacity(bundle.sims.len());
    for entry in &bundle.sims {
        let sources = if entry.sim == winner_sim {
            SimSources {
                base: permalink.sources.base_look.clone(),
                crm: permalink.sources.crm.clone(),
                covariance: permalink.sources.covariance.clone(),
            }
        } else {
            SimSources {
                base: Some(entry.base.to_string()),
                crm: Some(entry.crm.to_string()),
                covariance: entry.covariance.as_ref().map(ToString::to_string),
            }
        };
        sims.push(SimInputs {
            sim: entry.sim.clone(),
            base_look: artifact(&target, entry.base.as_str())?.to_vec(),
            crm: artifact(&target, entry.crm.as_str())?.to_vec(),
            covariance: entry
                .covariance
                .as_ref()
                .map(|name| artifact(&target, name.as_str()).map(<[u8]>::to_vec))
                .transpose()?,
            sources,
            realization_version: None,
            params: None,
            realization_params: None,
            wb_mode: None,
            firmware: None,
        });
    }
    let destination = destination_scope(SelectionScope::Unassessed, &bundle.excluded);
    Ok((
        MatchInputs {
            target_look: target_look.to_vec(),
            sims,
            excluded: bundle.excluded.clone(),
            body_yaml,
            transform_yaml,
            emphasis: permalink.emphasis.as_str().to_owned(),
            epoch: permalink.epoch,
            input_transform_version: permalink.input_transform_version,
            realization_version: permalink.realization_params_version,
            seed: permalink.seed,
            sources: SourceNames {
                target_look: permalink.sources.target_look.clone(),
                body: permalink.sources.body.clone(),
                transform: permalink.sources.transform.clone(),
            },
            calibration: permalink.calibration.clone(),
            selection: None,
            metric: permalink.metric,
            output_encoding: permalink.output.map(|context| context.camera.encoding),
            approximated_base: approximated_base(&target),
            destination_scope: destination,
        },
        target.manifest.camera_model_provenance(),
        target.manifest.version.clone(),
    ))
}

fn source_basename(value: &str) -> Result<String, String> {
    if value.is_empty() || value.contains("://") {
        return Err(format!(
            "The pinned source {value:?} is not a published artifact path."
        ));
    }
    let normalized = value.replace('\\', "/");
    let path = std::path::Path::new(&normalized);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| format!("The pinned source {value:?} has no artifact file name."))?;
    Ok(name.to_owned())
}

fn source_dir_variants(path: &str) -> Vec<String> {
    let path = path.trim_start_matches('/');
    let path = path.strip_prefix("site/").unwrap_or(path);
    let mut variants = vec![path.to_string()];
    if let Some(stripped) = path.strip_prefix("data/") {
        variants.push(stripped.to_string());
    }
    variants
}

fn source_dirs(
    version: &LoadedVersion,
    bundle: Option<&MatchBundleRevisionId>,
    body: &BodyId,
    epoch: u32,
) -> Result<Vec<String>, String> {
    let mut dirs = Vec::new();
    let version_dir =
        looklab_publish::version_dir(&version.manifest.look, &version.manifest.version)
            .map_err(|error| error.to_string())?;
    dirs.extend(source_dir_variants(version_dir.as_str()));
    if let Some(bundle) = bundle {
        let dir = bundle_dir(body, epoch, bundle).map_err(|error| error.to_string())?;
        dirs.extend(source_dir_variants(dir.as_str()));
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn source_candidates<'a>(
    version: &'a LoadedVersion,
    artifacts: &'a [ArtifactEntry],
) -> Vec<(&'a str, &'a str)> {
    let mut candidates = Vec::new();
    for entry in &version.manifest.artifacts {
        candidates.push((entry.name.as_str(), entry.sha256.as_str()));
    }
    for entry in artifacts {
        candidates.push((entry.name.as_str(), entry.sha256.as_str()));
    }
    candidates.sort();
    candidates.dedup();
    candidates
}

fn validate_source_path(
    what: &str,
    pinned: &str,
    basename: &str,
    allowed_dirs: &[String],
) -> Result<(), String> {
    let normalized = pinned.replace('\\', "/");
    let path = std::path::Path::new(&normalized);
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(());
    };
    let parent = parent.to_string_lossy();
    let parent = parent.trim_end_matches('/');
    let matches = allowed_dirs.iter().any(|dir| {
        let dir = dir.trim_matches('/');
        parent == dir || parent.ends_with(&format!("/{dir}"))
    });
    if matches {
        Ok(())
    } else {
        Err(format!(
            "The pinned {what} source {pinned:?} does not resolve to {basename:?} in the pinned version or bundle."
        ))
    }
}

fn pinned_named_source(
    what: &str,
    pinned: Option<&str>,
    expected: Option<&str>,
    candidates: &[(&str, &str)],
    allowed_dirs: &[String],
) -> Result<(), String> {
    let Some(expected) = expected else {
        return if pinned.is_none() {
            Ok(())
        } else {
            Err(format!(
                "The link pins a {what} source that the pinned artifacts do not publish."
            ))
        };
    };
    if !candidates.iter().any(|(name, _)| *name == expected) {
        return Err(format!(
            "The pinned artifacts do not list {expected} for {what}."
        ));
    }
    let Some(pinned) = pinned else {
        return Err(format!("The link has no pinned {what} source."));
    };
    let basename = source_basename(pinned)?;
    if basename.as_str() != expected {
        return Err(format!(
            "The pinned {what} source {pinned:?} names {basename:?}, not {expected:?}."
        ));
    }
    validate_source_path(what, pinned, &basename, allowed_dirs)
}

fn pinned_hashed_source(
    what: &str,
    pinned: Option<&str>,
    expected: Option<&str>,
    candidates: &[(&str, &str)],
    allowed_dirs: &[String],
) -> Result<(), String> {
    let Some(expected) = expected else {
        return if pinned.is_none() {
            Ok(())
        } else {
            Err(format!(
                "The link pins a {what} source that the pinned artifacts do not publish."
            ))
        };
    };
    let hashed: Vec<(&str, &str)> = candidates
        .iter()
        .copied()
        .filter(|(_, sha)| *sha == expected)
        .collect();
    if hashed.is_empty() {
        return Err(format!(
            "The pinned artifacts do not publish {what} {expected}."
        ));
    }
    let Some(pinned) = pinned else {
        return Err(format!("The link has no pinned {what} source."));
    };
    let basename = source_basename(pinned)?;
    if !hashed
        .iter()
        .any(|(name, sha)| *name == basename.as_str() || *sha == basename.as_str())
    {
        return Err(format!(
            "The pinned {what} source {pinned:?} does not name the pinned artifact."
        ));
    }
    validate_source_path(what, pinned, &basename, allowed_dirs)
}

fn artifact<'a>(version: &'a LoadedVersion, name: &str) -> Result<&'a [u8], String> {
    data::artifact(version, name).ok_or_else(|| {
        format!(
            "{name} is not in the manifest of {} {}",
            version.manifest.look, version.manifest.version
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{pinned_hashed_source, pinned_named_source};

    const NAME: &str = "match-fujifilm-x-t5-1-provia-base.looklab";
    const HASH: &str = "deadbeef";

    fn target_dirs() -> Vec<String> {
        vec![
            "data/looks/synthetic-match/1.0.0".to_owned(),
            "looks/synthetic-match/1.0.0".to_owned(),
        ]
    }

    fn bundle_dirs() -> Vec<String> {
        vec![
            "data/bundles/fujifilm/x-t5/1/right".to_owned(),
            "bundles/fujifilm/x-t5/1/right".to_owned(),
            "data/looks/synthetic-match/1.0.0".to_owned(),
        ]
    }

    #[test]
    fn a_cli_style_target_owned_source_binds_by_artifact_name() {
        let candidates = [(NAME, HASH)];
        assert!(
            pinned_named_source(
                "base look",
                Some(format!("site/data/looks/synthetic-match/1.0.0/{NAME}").as_str()),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_ok()
        );
        assert!(
            pinned_named_source(
                "base look",
                Some(NAME),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_target_owned_source_rejects_another_version_path_or_tamper() {
        let candidates = [(NAME, HASH)];
        assert!(
            pinned_named_source(
                "base look",
                Some(format!("site/data/looks/synthetic-match/2.0.0/{NAME}").as_str()),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_err()
        );
        assert!(
            pinned_named_source(
                "base look",
                Some(format!("site/data/looks/synthetic-match/1.0.0/../other/{NAME}").as_str()),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_err()
        );
        assert!(
            pinned_named_source(
                "base look",
                Some("https://example.com/match.looklab"),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_err()
        );
        assert!(
            pinned_named_source(
                "base look",
                Some("site/data/looks/synthetic-match/1.0.0/other.looklab"),
                Some(NAME),
                &candidates,
                &target_dirs(),
            )
            .is_err()
        );
    }

    #[test]
    fn a_bundle_source_binds_target_owned_or_hash_names_to_the_pinned_hash() {
        let candidates = [(NAME, HASH), (HASH, HASH)];
        assert!(
            pinned_hashed_source(
                "base look",
                Some(format!("site/data/looks/synthetic-match/1.0.0/{NAME}").as_str()),
                Some(HASH),
                &candidates,
                &bundle_dirs(),
            )
            .is_ok()
        );
        assert!(
            pinned_hashed_source(
                "base look",
                Some(format!("../../../../bundles/fujifilm/x-t5/1/right/{HASH}").as_str(),),
                Some(HASH),
                &candidates,
                &bundle_dirs(),
            )
            .is_ok()
        );
    }

    #[test]
    fn a_bundle_source_rejects_another_bundle_or_hash() {
        let candidates = [(NAME, HASH), (HASH, HASH)];
        assert!(
            pinned_hashed_source(
                "base look",
                Some(format!("site/data/bundles/fujifilm/x-t5/1/wrong/{NAME}").as_str()),
                Some(HASH),
                &candidates,
                &bundle_dirs(),
            )
            .is_err()
        );
        assert!(
            pinned_hashed_source(
                "base look",
                Some(format!("site/data/looks/synthetic-match/1.0.0/{NAME}").as_str()),
                Some("feedface"),
                &candidates,
                &bundle_dirs(),
            )
            .is_err()
        );
    }
}
