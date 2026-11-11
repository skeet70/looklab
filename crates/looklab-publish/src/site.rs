// SPDX-License-Identifier: AGPL-3.0-or-later
//! Site data layout and the sink-based publisher (docs/03 §5).

use std::io;

use looklab_model::file::{LookKind, LookPin, LookVersion, Slug, WbPolicy};
use looklab_schema::{Applicability, BodyId, EligibilitySnapshotId, ModelReportId};
use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::audit::AuditLog;
use crate::body::{BodyKind, BodyPage, BodyStatus, MappingStatus, PreparationPage};
use crate::hash::{ArtifactHash, ArtifactName, SitePath};
use crate::input_transform::{InputTransformEntry, PublishedInputTransform, input_transform_path};
use crate::interface::{
    INTERFACE_DEFINITION_ARTIFACT, INTERFACE_REPORT_ARTIFACT, InterfaceProfileEntry,
    PublishedInterfaceProfile, interface_dir, interface_manifest_path,
};
use crate::manifest::{
    ArtifactBlob, ArtifactEntry, GateSummary, LookStatus, ProvenanceSummary, VersionManifest,
    verify_artifact,
};
use crate::manual::{ManualArtifact, ManualPage, ManualSummary};
use crate::qualification::{
    BundleEntry, CurrentEntry, EligibilityEntry, FamilyHeadPage, PublishedBundleRevisionPage,
    ReportSummaryEntry, SelectedTargetSummary, SiteReport, SubjectCurrentPage,
    TargetEligibilitySnapshot, bundle_artifacts_path, bundle_dir, bundle_manifest_path,
    current_path, eligibility_path, report_path,
};
use crate::scene::{SCENE_FORMAT, Scene, SceneIndex, SceneName, SceneSummary};
use crate::source_lut::{SOURCE_LUT_ARTIFACT, SourceLutPreview, VersionLookShape, look_shape};

pub const SITE_FORMAT: &str = "looklab-site/1";
pub const INDEX_PATH: &str = "data/index.json";
pub const AUDIT_LOG_PATH: &str = "data/audit-log.json";
pub const SCENE_INDEX_PATH: &str = "data/scenes/index.json";
pub const MANIFEST_NAME: &str = "manifest.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicensePage {
    pub declaration: String,
    pub attribution: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WeakSpot {
    pub region: String,
    pub note: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounty: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookVersionPage {
    pub manifest: VersionManifest,
    #[serde(skip)]
    pub blobs: Vec<ArtifactBlob>,
    pub notes: VersionNotes,
    #[serde(default = "matchable_default")]
    pub matchable: bool,
    #[serde(default)]
    pub card_eligible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub approximated: bool,
    #[serde(default)]
    pub canonical: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligibility: Option<EligibilitySnapshotId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligibility_status: Option<looklab_schema::TargetEligibilityStatus>,
    #[serde(default = "unassessed_applicability")]
    pub applicability: Applicability,
}

fn unassessed_applicability() -> Applicability {
    Applicability::Unassessed
}

fn matchable_default() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionNotes {
    pub changelog: Vec<String>,
    pub weak_spots: Vec<WeakSpot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

impl LookVersionPage {
    /// # Errors
    /// Rejects an empty artifact list, duplicate artifact names, or a contradictory artifact set.
    pub fn new(
        look: Slug,
        version: LookVersion,
        status: LookStatus,
        gate_summary: GateSummary,
        provenance: ProvenanceSummary,
        blobs: Vec<ArtifactBlob>,
        notes: VersionNotes,
    ) -> Result<Self, PublishError> {
        let manifest =
            VersionManifest::from_files(look, version, status, gate_summary, provenance, &blobs)?;
        let layered = match look_shape(&manifest)? {
            VersionLookShape::Layered => true,
            VersionLookShape::SourcePreview => {
                source_preview(&manifest, &blobs)?;
                false
            }
        };
        let card_eligible = layered
            && matches!(manifest.status, LookStatus::Published | LookStatus::Rooted)
            && manifest
                .artifact(&ArtifactName::new(crate::manifest::CARD_ARTIFACT)?)
                .is_some();
        Ok(Self {
            manifest,
            blobs,
            notes,
            matchable: layered,
            card_eligible,
            approximated: false,
            canonical: false,
            eligibility: None,
            eligibility_status: None,
            applicability: Applicability::Unassessed,
        })
    }

    pub fn version(&self) -> &LookVersion {
        &self.manifest.version
    }

    pub fn artifacts(&self) -> &[ArtifactEntry] {
        &self.manifest.artifacts
    }
}

fn source_preview(
    manifest: &VersionManifest,
    blobs: &[ArtifactBlob],
) -> Result<SourceLutPreview, PublishError> {
    let descriptor = blobs
        .iter()
        .find(|blob| blob.name.as_str() == SOURCE_LUT_ARTIFACT)
        .ok_or(PublishError::MissingLookArtifact)?;
    let preview = SourceLutPreview::decode(&descriptor.bytes)?;
    preview.verify_artifacts(manifest)?;
    Ok(preview)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookListing {
    #[default]
    Listed,
    Hidden,
    Removed,
}

impl LookListing {
    pub fn is_listed(self) -> bool {
        self == Self::Listed
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookPage {
    pub slug: Slug,
    pub name: String,
    pub kind: LookKind,
    pub wb_policy: WbPolicy,
    pub description: String,
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<LicensePage>,
    #[serde(default)]
    pub listing: LookListing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<LookPin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<FamilyHeadPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<SelectedTargetSummary>,
    pub versions: Vec<LookVersionPage>,
}

impl LookPage {
    pub fn latest(&self) -> Option<&LookVersionPage> {
        self.versions
            .iter()
            .max_by(|left, right| compare_versions(left.version(), right.version()))
    }
}

/// Orders versions by their numeric dotted segments, then by their text.
pub fn compare_versions(left: &LookVersion, right: &LookVersion) -> core::cmp::Ordering {
    version_parts(left)
        .cmp(&version_parts(right))
        .then_with(|| left.as_str().cmp(right.as_str()))
}

fn version_parts(version: &LookVersion) -> Vec<u64> {
    version
        .as_str()
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionSummary {
    pub version: LookVersion,
    pub status: LookStatus,
    #[serde(default = "matchable_default")]
    pub matchable: bool,
    #[serde(default)]
    pub card_eligible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub approximated: bool,
    #[serde(default = "unassessed_applicability")]
    pub applicability: Applicability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eligibility_status: Option<looklab_schema::TargetEligibilityStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<ArtifactEntry>,
    pub manifest: SitePath,
    pub manifest_sha256: ArtifactHash,
    pub manifest_bytes: u64,
    pub artifact: ArtifactName,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

impl VersionSummary {
    pub fn permits_new_outputs(&self) -> bool {
        !matches!(self.applicability, Applicability::Invalidated { .. })
            && self
                .eligibility_status
                .is_none_or(|status| status == looklab_schema::TargetEligibilityStatus::Eligible)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LookVisibility {
    #[default]
    Public,
    TestFixture,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookSummary {
    pub slug: Slug,
    pub name: String,
    pub kind: LookKind,
    pub wb_policy: WbPolicy,
    #[serde(default)]
    pub visibility: LookVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<FamilyHeadPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<SelectedTargetSummary>,
    pub versions: Vec<VersionSummary>,
}

impl LookSummary {
    pub fn latest(&self) -> Option<&VersionSummary> {
        self.versions
            .iter()
            .max_by(|left, right| compare_versions(&left.version, &right.version))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodySummary {
    pub id: BodyId,
    pub make: String,
    pub model: String,
    pub kind: BodyKind,
    pub status: BodyStatus,
    pub mapping_status: MappingStatus,
    #[serde(default)]
    pub preparation: PreparationPage,
    pub epochs: usize,
    pub looks: Vec<Slug>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencePreview {
    pub path: SitePath,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteIndex {
    pub format: String,
    /// The community data snapshot the site was built from.
    #[serde(default)]
    pub community_snapshot_version: u64,
    pub bodies: Vec<BodySummary>,
    #[serde(default)]
    pub manuals: Vec<ManualSummary>,
    #[serde(default)]
    pub interfaces: Vec<InterfaceProfileEntry>,
    #[serde(default)]
    pub input_transforms: Vec<InputTransformEntry>,
    pub looks: Vec<LookSummary>,
    pub scenes: Vec<SceneSummary>,
    #[serde(default)]
    pub reports: Vec<ReportSummaryEntry>,
    #[serde(default)]
    pub eligibility: Vec<EligibilityEntry>,
    #[serde(default)]
    pub current: Vec<CurrentEntry>,
    #[serde(default)]
    pub bundles: Vec<BundleEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_preview: Option<ReferencePreview>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteData {
    /// The community data snapshot the site is built from.
    pub community_snapshot_version: u64,
    pub bodies: Vec<BodyPage>,
    pub manuals: Vec<ManualPage>,
    pub interfaces: Vec<PublishedInterfaceProfile>,
    pub input_transforms: Vec<PublishedInputTransform>,
    pub looks: Vec<LookPage>,
    pub audit_log: AuditLog,
    pub scenes: Vec<Scene>,
    pub reports: Vec<SiteReport>,
    pub eligibility: Vec<TargetEligibilitySnapshot>,
    pub current: Vec<SubjectCurrentPage>,
    pub bundles: Vec<PublishedBundleRevisionPage>,
    pub reference_preview: Option<ArtifactBlob>,
}

/// A consumer of published files, such as a directory writer or an in-memory test sink.
pub trait SiteSink {
    fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()>;
}

impl<F> SiteSink for F
where
    F: FnMut(&SitePath, &[u8]) -> io::Result<()>,
{
    fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()> {
        self(path, bytes)
    }
}

pub fn index_path() -> SitePath {
    SitePath::new(INDEX_PATH).expect("static site path")
}

pub fn audit_log_path() -> SitePath {
    SitePath::new(AUDIT_LOG_PATH).expect("static site path")
}

pub fn scene_index_path() -> SitePath {
    SitePath::new(SCENE_INDEX_PATH).expect("static site path")
}

/// # Errors
/// Rejects a body id whose segments are not safe path components.
pub fn body_path(id: &BodyId) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/bodies/{}/{}.json", id.make(), id.model()))
}

/// # Errors
/// Rejects a body id whose segments are not safe path components.
pub fn manual_path(id: &BodyId) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/manuals/{}/{}.json", id.make(), id.model()))
}

/// # Errors
/// Rejects a look slug that is not a safe path component.
pub fn look_path(slug: &Slug) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/looks/{}.json", slug.as_str()))
}

/// # Errors
/// Rejects a look slug or version that is not a safe path component.
pub fn version_dir(slug: &Slug, version: &LookVersion) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/looks/{}/{}", slug.as_str(), version.as_str()))
}

/// # Errors
/// Rejects a scene name that is not a safe path component.
pub fn scene_path(name: &SceneName) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/scenes/{}.png", name.as_str()))
}

/// # Errors
/// Rejects a scene name that is not a safe path component.
pub fn scene_thumbnail_path(name: &SceneName) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/scenes/{}-thumb.png", name.as_str()))
}

/// # Errors
/// Rejects a look slug or version that is not a safe path component.
pub fn manifest_path(slug: &Slug, version: &LookVersion) -> Result<SitePath, PublishError> {
    SitePath::new(format!(
        "data/looks/{}/{}/{}",
        slug.as_str(),
        version.as_str(),
        MANIFEST_NAME
    ))
}

/// Writes every published file through `sink`, including artifacts and manifests.
///
/// # Errors
/// Rejects duplicate ids and unsafe paths, and propagates sink failures.
pub fn write_site<S: SiteSink + ?Sized>(
    sink: &mut S,
    site: &SiteData,
) -> Result<ArtifactHash, PublishError> {
    validate(site)?;

    let mut manuals = Vec::with_capacity(site.manuals.len());
    for manual in &site.manuals {
        let encoded = EncodedManual::new(manual)?;
        sink.put(&encoded.summary.artifact.path, &encoded.bytes)?;
        manuals.push(encoded.summary);
    }

    let mut interfaces = Vec::new();
    for page in &site.interfaces {
        let revision = page.revision()?;
        let dir = interface_dir(&revision)?;
        sink.put(&dir.join(INTERFACE_DEFINITION_ARTIFACT), &page.definition)?;
        sink.put(&dir.join(INTERFACE_REPORT_ARTIFACT), &page.report_bytes)?;
        sink.put(&dir.join(MANIFEST_NAME), &page.manifest_bytes)?;
        if page.current {
            interfaces.push(InterfaceProfileEntry {
                id: page.manifest.id.clone(),
                name: page.manifest.name.clone(),
                manifest: interface_manifest_path(&revision)?,
                manifest_sha256: revision,
                manifest_bytes: page.manifest_bytes.len() as u64,
            });
        }
    }

    let mut input_transforms = Vec::new();
    for page in &site.input_transforms {
        page.entry.verify(&page.bytes)?;
        sink.put(&page.entry.path, &page.bytes)?;
        if page.current {
            input_transforms.push(page.entry.clone());
        }
    }

    for body in &site.bodies {
        let mut page = body.clone();
        page.native_looks.retain(|card| {
            site.looks
                .iter()
                .all(|look| look.slug != card.look || look.listing.is_listed())
        });
        sink.put(&body_path(&page.id)?, &json(&page)?)?;
    }

    for look in &site.looks {
        if !look.listing.is_listed() {
            continue;
        }
        let page_path = look_path(&look.slug)?;
        sink.put(&page_path, &json(look)?)?;
        for version in &look.versions {
            let dir = version_dir(&look.slug, &version.manifest.version)?;
            for blob in &version.blobs {
                verify_artifact(&version.manifest, &blob.name, &blob.bytes)?;
                sink.put(&dir.join(blob.name.as_str()), &blob.bytes)?;
            }
            sink.put(
                &manifest_path(&look.slug, &version.manifest.version)?,
                &version.manifest.to_json()?,
            )?;
        }
    }

    let mut reports = Vec::with_capacity(site.reports.len());
    for report in &site.reports {
        let path = report_path(&report.summary.report)?;
        let bytes = json(&report.summary)?;
        sink.put(&path, &bytes)?;
        reports.push(ReportSummaryEntry {
            report: report.summary.report,
            subject: report.summary.subject.clone(),
            candidate: report.summary.candidate.clone(),
            capability: report.summary.capability,
            outcome: report.summary.outcome,
            applicability: report.applicability.clone(),
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
    }

    let mut eligibility = Vec::with_capacity(site.eligibility.len());
    for snapshot in &site.eligibility {
        let path = eligibility_path(&snapshot.id)?;
        let bytes = json(snapshot)?;
        sink.put(&path, &bytes)?;
        eligibility.push(EligibilityEntry {
            id: snapshot.id,
            target: snapshot.target.clone(),
            basis: snapshot.basis,
            status: snapshot.status,
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
    }

    let mut current = Vec::with_capacity(site.current.len());
    for page in &site.current {
        let path = current_path(&page.subject.key())?;
        let bytes = json(page)?;
        sink.put(&path, &bytes)?;
        current.push(CurrentEntry {
            subject: page.subject.clone(),
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
    }

    let mut bundles = Vec::with_capacity(site.bundles.len());
    for page in &site.bundles {
        let manifest = &page.manifest;
        let epoch = manifest.epoch.get();
        let dir = bundle_dir(&manifest.body, epoch, &manifest.id)?;
        for blob in &page.blobs {
            sink.put(&dir.join(blob.name.as_str()), &blob.bytes)?;
        }
        let path = bundle_manifest_path(&manifest.body, epoch, &manifest.id)?;
        let bytes = page.to_json()?;
        sink.put(&path, &bytes)?;
        let artifacts_path = bundle_artifacts_path(&manifest.body, epoch, &manifest.id)?;
        let artifacts = page.artifacts_json()?;
        sink.put(&artifacts_path, &artifacts)?;
        bundles.push(BundleEntry {
            bundle: manifest.id,
            body: manifest.body.clone(),
            epoch,
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
            artifacts_path,
            artifacts_sha256: ArtifactHash::of(&artifacts),
            artifacts_bytes: artifacts.len() as u64,
        });
    }

    if let Some(preview) = &site.reference_preview {
        sink.put(&reference_preview_path(&preview.name)?, &preview.bytes)?;
    }

    let mut scenes = Vec::with_capacity(site.scenes.len());
    for scene in &site.scenes {
        let encoded = EncodedScene::new(scene)?;
        sink.put(&encoded.summary.path, &encoded.png)?;
        sink.put(&encoded.summary.thumbnail.path, &encoded.thumbnail_png)?;
        scenes.push(encoded.summary);
    }
    let scene_index = SceneIndex {
        format: SCENE_FORMAT.to_owned(),
        scenes,
    };
    sink.put(&scene_index_path(), &json(&scene_index)?)?;
    sink.put(&audit_log_path(), &json(&site.audit_log)?)?;
    let index = json(&site_index(
        site,
        SiteEntries {
            scenes: scene_index.scenes,
            reports,
            eligibility,
            current,
            bundles,
            interfaces,
            input_transforms,
            manuals,
        },
    )?)?;
    sink.put(&index_path(), &index)?;
    Ok(ArtifactHash::of(&index))
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>, PublishError> {
    Ok(serde_json::to_vec_pretty(value)?)
}

struct EncodedScene {
    summary: SceneSummary,
    png: Vec<u8>,
    thumbnail_png: Vec<u8>,
}

impl EncodedScene {
    fn new(scene: &Scene) -> Result<Self, PublishError> {
        let png = scene.to_png()?;
        let thumbnail = scene.downscale(220);
        let thumbnail_png = thumbnail.to_png()?;
        let summary = SceneSummary {
            name: scene.name.clone(),
            kind: scene.kind,
            title: scene.title.clone(),
            credit: scene.credit.clone(),
            thumbnail: crate::SceneArtifact {
                width: thumbnail.width,
                height: thumbnail.height,
                path: scene_thumbnail_path(&scene.name)?,
                sha256: ArtifactHash::of(&thumbnail_png),
                bytes: thumbnail_png.len() as u64,
            },
            width: scene.width,
            height: scene.height,
            path: scene_path(&scene.name)?,
            sha256: ArtifactHash::of(&png),
            bytes: png.len() as u64,
        };
        Ok(Self {
            summary,
            png,
            thumbnail_png,
        })
    }
}

struct EncodedManual {
    summary: ManualSummary,
    bytes: Vec<u8>,
}

impl EncodedManual {
    fn new(manual: &ManualPage) -> Result<Self, PublishError> {
        let bytes = json(manual)?;
        let path = manual_path(&manual.id)?;
        let summary = manual.summary(ManualArtifact {
            path,
            sha256: ArtifactHash::of(&bytes),
            bytes: bytes.len() as u64,
        });
        Ok(Self { summary, bytes })
    }
}

fn reference_preview_path(name: &ArtifactName) -> Result<SitePath, PublishError> {
    SitePath::new(format!("data/reference/{}", name.as_str()))
}

/// The per-entry vectors `site_index` folds into a `SiteIndex`.
struct SiteEntries {
    scenes: Vec<SceneSummary>,
    reports: Vec<ReportSummaryEntry>,
    eligibility: Vec<EligibilityEntry>,
    current: Vec<CurrentEntry>,
    bundles: Vec<BundleEntry>,
    interfaces: Vec<InterfaceProfileEntry>,
    input_transforms: Vec<InputTransformEntry>,
    manuals: Vec<ManualSummary>,
}

fn site_index(site: &SiteData, entries: SiteEntries) -> Result<SiteIndex, PublishError> {
    let SiteEntries {
        scenes,
        reports,
        eligibility,
        current,
        bundles,
        interfaces,
        input_transforms,
        manuals,
    } = entries;
    let bodies = site
        .bodies
        .iter()
        .map(|body| {
            let mut looks: Vec<Slug> = body
                .native_looks
                .iter()
                .map(|card| card.look.clone())
                .filter(|slug| {
                    site.looks
                        .iter()
                        .all(|look| look.slug != *slug || look.listing.is_listed())
                })
                .collect();
            looks.sort();
            looks.dedup();
            BodySummary {
                id: body.id.clone(),
                make: body.make.clone(),
                model: body.model.clone(),
                kind: body.kind,
                status: body.status,
                mapping_status: body.mapping_status,
                preparation: body.preparation.clone(),
                epochs: body.epochs.len(),
                looks,
            }
        })
        .collect();
    let looks = site
        .looks
        .iter()
        .filter(|look| look.listing.is_listed())
        .map(|look| {
            let versions = look
                .versions
                .iter()
                .map(|version| {
                    let artifact = match look_shape(&version.manifest)? {
                        VersionLookShape::Layered => version
                            .artifacts()
                            .first()
                            .ok_or_else(|| PublishError::NoArtifacts {
                                look: look.slug.as_str().to_owned(),
                                version: version.version().as_str().to_owned(),
                            })?
                            .clone(),
                        VersionLookShape::SourcePreview => version
                            .manifest
                            .artifact(&ArtifactName::new(SOURCE_LUT_ARTIFACT)?)
                            .ok_or(PublishError::MissingLookArtifact)?
                            .clone(),
                    };
                    let manifest = version.manifest.to_json()?;
                    Ok(VersionSummary {
                        version: version.version().clone(),
                        status: version.manifest.status,
                        matchable: version.matchable,
                        card_eligible: version.card_eligible,
                        approximated: version.approximated,
                        applicability: version.applicability.clone(),
                        eligibility_status: version.eligibility_status,
                        card: version
                            .artifacts()
                            .iter()
                            .find(|artifact| {
                                artifact.name.as_str() == crate::manifest::CARD_ARTIFACT
                            })
                            .cloned(),
                        manifest: manifest_path(&look.slug, version.version())?,
                        manifest_sha256: ArtifactHash::of(&manifest),
                        manifest_bytes: manifest.len() as u64,
                        artifact: artifact.name,
                        sha256: artifact.sha256,
                        bytes: artifact.bytes,
                    })
                })
                .collect::<Result<Vec<_>, PublishError>>()?;
            Ok(LookSummary {
                slug: look.slug.clone(),
                name: look.name.clone(),
                kind: look.kind,
                wb_policy: look.wb_policy,
                visibility: LookVisibility::Public,
                head: look.head.clone(),
                selected: look.selected.clone(),
                versions,
            })
        })
        .collect::<Result<Vec<_>, PublishError>>()?;
    Ok(SiteIndex {
        format: SITE_FORMAT.to_owned(),
        community_snapshot_version: site.community_snapshot_version,
        bodies,
        manuals,
        interfaces,
        input_transforms,
        looks,
        scenes,
        reports,
        eligibility,
        current,
        bundles,
        reference_preview: site
            .reference_preview
            .as_ref()
            .map(|preview| -> Result<ReferencePreview, PublishError> {
                Ok(ReferencePreview {
                    path: reference_preview_path(&preview.name)?,
                    sha256: ArtifactHash::of(&preview.bytes),
                    bytes: preview.bytes.len() as u64,
                })
            })
            .transpose()?,
    })
}

fn validate(site: &SiteData) -> Result<(), PublishError> {
    let mut body_ids = Vec::with_capacity(site.bodies.len());
    for body in &site.bodies {
        if body_ids.contains(&body.id) {
            return Err(PublishError::DuplicateBody(body.id.to_string()));
        }
        body_ids.push(body.id.clone());
        body_path(&body.id)?;
    }

    let mut manual_ids = Vec::with_capacity(site.manuals.len());
    for manual in &site.manuals {
        if manual_ids.contains(&manual.id) {
            return Err(PublishError::DuplicateBody(manual.id.to_string()));
        }
        manual_ids.push(manual.id.clone());
        manual_path(&manual.id)?;
    }

    let mut look_slugs = Vec::with_capacity(site.looks.len());
    for look in &site.looks {
        if look_slugs.contains(&look.slug) {
            return Err(PublishError::DuplicateLook(look.slug.to_string()));
        }
        look_slugs.push(look.slug.clone());
        look_path(&look.slug)?;
        let mut versions = Vec::with_capacity(look.versions.len());
        for version in &look.versions {
            if versions.contains(&version.manifest.version) {
                return Err(PublishError::DuplicateVersion {
                    look: look.slug.to_string(),
                    version: version.version().to_string(),
                });
            }
            if version.manifest.look != look.slug {
                return Err(PublishError::ManifestLook {
                    expected: look.slug.to_string(),
                    found: version.manifest.look.to_string(),
                });
            }
            versions.push(version.manifest.version.clone());
            version_dir(&look.slug, version.version())?;
            for blob in &version.blobs {
                verify_artifact(&version.manifest, &blob.name, &blob.bytes)?;
            }
            if version.blobs.is_empty() {
                return Err(PublishError::NoArtifacts {
                    look: look.slug.to_string(),
                    version: version.version().to_string(),
                });
            }
            if look_shape(&version.manifest)? == VersionLookShape::SourcePreview {
                source_preview(&version.manifest, &version.blobs)?;
                if version.matchable || version.card_eligible {
                    return Err(PublishError::SourceLut(format!(
                        "{} {} is a source-only preview published as matchable or card-eligible",
                        look.slug,
                        version.version()
                    )));
                }
            }
        }
        if let Some(selected) = &look.selected
            && let Some(version) = look
                .versions
                .iter()
                .find(|version| version.version() == &selected.version)
            && look_shape(&version.manifest)? == VersionLookShape::SourcePreview
        {
            return Err(PublishError::SourceLut(format!(
                "{} selects the source-only preview {}",
                look.slug, selected.version
            )));
        }
    }

    let mut report_ids: Vec<ModelReportId> = Vec::with_capacity(site.reports.len());
    for report in &site.reports {
        if report_ids.contains(&report.summary.report) {
            return Err(PublishError::DuplicateReport(
                report.summary.report.to_hex(),
            ));
        }
        report_ids.push(report.summary.report);
        report_path(&report.summary.report)?;
    }

    let mut eligibility_ids: Vec<EligibilitySnapshotId> =
        Vec::with_capacity(site.eligibility.len());
    for snapshot in &site.eligibility {
        if eligibility_ids.contains(&snapshot.id) {
            return Err(PublishError::DuplicateEligibility(snapshot.id.to_string()));
        }
        eligibility_ids.push(snapshot.id);
        eligibility_path(&snapshot.id)?;
    }

    let mut current_subjects: Vec<looklab_schema::SubjectKey> =
        Vec::with_capacity(site.current.len());
    for page in &site.current {
        let key = page.subject.key();
        if current_subjects.contains(&key) {
            return Err(PublishError::DuplicateCurrent(key.to_string()));
        }
        current_subjects.push(key);
        current_path(&page.subject.key())?;
        for projection in &page.projections {
            if let Some(report) = &projection.selected_report
                && !report_ids.contains(report)
            {
                return Err(PublishError::UnknownReference(format!(
                    "current projection selects unpublished report {}",
                    report.to_hex()
                )));
            }
            if let Some(eligibility) = &projection.selected_eligibility
                && !eligibility_ids.contains(eligibility)
            {
                return Err(PublishError::UnknownReference(format!(
                    "current projection selects unpublished eligibility {eligibility}"
                )));
            }
        }
    }

    let mut bundle_ids: Vec<(BodyId, u32, looklab_schema::MatchBundleRevisionId)> =
        Vec::with_capacity(site.bundles.len());
    for page in &site.bundles {
        let manifest = &page.manifest;
        let key = (manifest.body.clone(), manifest.epoch.get(), manifest.id);
        if bundle_ids.contains(&key) {
            return Err(PublishError::DuplicateBundle(manifest.id.to_hex()));
        }
        bundle_ids.push(key);
        PublishedBundleRevisionPage::new(manifest.clone(), page.blobs.clone())?;
    }

    for body in &site.bodies {
        for card in &body.native_looks {
            let Some(version) = &card.version else {
                continue;
            };
            let Some(look) = site.looks.iter().find(|look| look.slug == card.look) else {
                continue;
            };
            if look
                .versions
                .iter()
                .all(|candidate| &candidate.manifest.version != version)
            {
                return Err(PublishError::UnknownReference(format!(
                    "body {} pins unpublished {}@{}",
                    body.id,
                    card.look,
                    version.as_str()
                )));
            }
        }
    }

    let mut current_interfaces: Vec<looklab_schema::InterfaceId> = Vec::new();
    for page in &site.interfaces {
        page.verify_manifest()?;
        page.manifest.verify_definition(&page.definition)?;
        page.manifest.verify_report(&page.report_bytes)?;
        if page.current {
            if current_interfaces.contains(&page.manifest.id) {
                return Err(PublishError::DuplicateInterface(
                    page.manifest.id.to_string(),
                ));
            }
            current_interfaces.push(page.manifest.id.clone());
        }
        interface_dir(&page.revision()?)?;
    }

    let mut current_transforms: Vec<looklab_schema::BodyId> = Vec::new();
    for page in &site.input_transforms {
        page.entry.verify(&page.bytes)?;
        if page.entry.path != input_transform_path(&page.entry.sha256)? {
            return Err(PublishError::InputTransform(format!(
                "input transform for {} does not name its content",
                page.entry.body
            )));
        }
        if page.current {
            if current_transforms.contains(&page.entry.body) {
                return Err(PublishError::InputTransform(format!(
                    "body {} has two current input transforms",
                    page.entry.body
                )));
            }
            current_transforms.push(page.entry.body.clone());
        }
    }

    let mut scene_names = Vec::with_capacity(site.scenes.len());
    for scene in &site.scenes {
        if scene_names.contains(&scene.name) {
            return Err(PublishError::DuplicateScene(scene.name.to_string()));
        }
        scene_names.push(scene.name.clone());
        scene_path(&scene.name)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
    use looklab_schema::{validate_body, validate_control_schema};

    use super::*;
    use crate::audit::AuditLog;
    use crate::body::{BodyExtras, BodyPage};
    use crate::hash::ArtifactName;
    use crate::input_transform::InputTransformVersion;
    use crate::manifest::LookStatus;
    use crate::manifest::ProvenanceSummary;

    struct MemorySink(BTreeMap<String, Vec<u8>>);

    impl SiteSink for MemorySink {
        fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()> {
            self.0.insert(path.as_str().to_owned(), bytes.to_vec());
            Ok(())
        }
    }

    fn body() -> BodyPage {
        let body = validate_body(
            "id: fujifilm/x-t5\nmake: Fujifilm\nmodel: X-T5\nexif_aliases: [X-T5]\nkind: camera\ncapabilities:\n  in_camera_raw_conversion: true\n  jpeg_color_spaces: [srgb]\n  heif: false\nstatus: active\nmapping_status: provisional\n",
        )
        .expect("body");
        let schema =
            validate_control_schema(include_str!("../../../data/bodies/fujifilm-x-t5.yaml"))
                .expect("schema");
        BodyPage::from_schema(&body, &schema, &BodyExtras::default()).expect("page")
    }

    fn look() -> LookPage {
        let blob = ArtifactBlob::new(
            ArtifactName::new("look.looklab").expect("name"),
            vec![1, 2, 3, 4],
        );
        let version = LookVersionPage::new(
            Slug::new("provia").expect("slug"),
            LookVersion::new("1.0.0").expect("version"),
            LookStatus::Published,
            GateSummary::default(),
            ProvenanceSummary::default(),
            vec![blob],
            VersionNotes {
                changelog: vec!["initial".to_owned()],
                weak_spots: Vec::new(),
                notice: None,
            },
        )
        .expect("version");
        LookPage {
            slug: Slug::new("provia").expect("slug"),
            name: "Provia".to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            description: "Standard".to_owned(),
            aliases: Vec::new(),
            license: None,
            listing: LookListing::Listed,
            superseded_by: None,
            head: None,
            selected: None,
            versions: vec![version],
        }
    }

    fn scene() -> Scene {
        Scene::new(
            SceneName::new("gradient").expect("name"),
            1,
            1,
            vec![[0.18, 0.18, 0.18]],
        )
        .expect("scene")
    }

    #[test]
    fn writer_emits_every_indexed_file() {
        let site = SiteData {
            community_snapshot_version: 0,
            bodies: vec![body()],
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![look()],
            audit_log: AuditLog::default(),
            scenes: vec![scene()],
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        for path in [
            "data/index.json",
            "data/audit-log.json",
            "data/scenes/index.json",
            "data/bodies/fujifilm/x-t5.json",
            "data/looks/provia.json",
            "data/looks/provia/1.0.0/look.looklab",
            "data/looks/provia/1.0.0/manifest.json",
            "data/scenes/gradient.png",
            "data/scenes/gradient-thumb.png",
        ] {
            assert!(sink.0.contains_key(path), "{path}");
        }
    }

    #[test]
    fn index_links_the_manifest_and_cards_keep_their_scoped_pin() {
        let site = SiteData {
            community_snapshot_version: 3,
            bodies: vec![body()],
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![look()],
            audit_log: AuditLog::default(),
            scenes: vec![scene()],
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex =
            serde_json::from_slice(&sink.0["data/index.json"]).expect("index json");
        assert_eq!(index.community_snapshot_version, 3);
        assert_eq!(
            index.looks[0].versions[0].manifest.as_str(),
            "data/looks/provia/1.0.0/manifest.json"
        );
        let page: BodyPage =
            serde_json::from_slice(&sink.0["data/bodies/fujifilm/x-t5.json"]).expect("body json");
        assert!(
            page.native_looks
                .iter()
                .filter(|card| card.look.as_str() == "provia")
                .all(|card| card.version.is_none())
        );
    }

    #[test]
    fn hiding_a_native_look_removes_its_body_card_and_summary_entry() {
        let mut hidden = look();
        hidden.listing = LookListing::Hidden;
        let site = SiteData {
            bodies: vec![body()],
            looks: vec![hidden],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let page: BodyPage =
            serde_json::from_slice(&sink.0["data/bodies/fujifilm/x-t5.json"]).expect("body json");
        assert!(
            page.native_looks
                .iter()
                .all(|card| card.look.as_str() != "provia")
        );
        assert!(
            page.native_looks
                .iter()
                .any(|card| card.look.as_str() == "classic-chrome")
        );
        let index: SiteIndex =
            serde_json::from_slice(&sink.0["data/index.json"]).expect("index json");
        assert!(
            index.bodies[0]
                .looks
                .iter()
                .all(|slug| slug.as_str() != "provia")
        );
        assert!(
            index.bodies[0]
                .looks
                .iter()
                .any(|slug| slug.as_str() == "classic-chrome")
        );
    }

    #[test]
    fn index_publishes_version_matchability() {
        let mut look = look();
        look.versions[0].matchable = false;
        let site = SiteData {
            looks: vec![look],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex =
            serde_json::from_slice(&sink.0["data/index.json"]).expect("index json");
        assert!(!index.looks[0].versions[0].matchable);
    }

    #[test]
    fn writer_rejects_duplicate_looks() {
        let site = SiteData {
            community_snapshot_version: 0,
            bodies: Vec::new(),
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![look(), look()],
            audit_log: AuditLog::default(),
            scenes: Vec::new(),
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        };
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(&mut sink, &site),
            Err(PublishError::DuplicateLook(_))
        ));
    }

    #[test]
    fn scene_indexes_authenticate_published_images_and_preserve_source_metadata() {
        let mut photo = Scene::new(
            SceneName::new("sample-photo").expect("name"),
            440,
            2,
            vec![[0.18; 3]; 880],
        )
        .expect("scene");
        photo.kind = crate::scene::SceneKind::Photo;
        photo.title = "Sample photograph".to_owned();
        photo.credit = Some(crate::scene::SceneCredit {
            text: "Photographer".to_owned(),
            source_url: "https://example.com/sample.raw".to_owned(),
            license: "CC0-1.0".to_owned(),
            source_sha256: ArtifactHash::of(b"source RAW"),
        });
        let site = SiteData {
            scenes: vec![photo.clone()],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        let root = write_site(&mut sink, &site).expect("write");
        assert_eq!(root, ArtifactHash::of(&sink.0[INDEX_PATH]));
        let index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("site index");
        let scenes: SceneIndex =
            serde_json::from_slice(&sink.0[SCENE_INDEX_PATH]).expect("scene index");
        assert_eq!(index.scenes, scenes.scenes);
        assert_eq!(index.scenes.len(), 1);
        let summary = &index.scenes[0];
        assert_eq!(summary.name, photo.name);
        assert_eq!(summary.kind, photo.kind);
        assert_eq!(summary.title, photo.title);
        assert_eq!(summary.credit, photo.credit);
        assert_eq!((summary.width, summary.height), (440, 2));
        assert_eq!(summary.path, scene_path(&photo.name).expect("scene path"));
        let full = &sink.0[summary.path.as_str()];
        assert_eq!(summary.sha256, ArtifactHash::of(full));
        assert_eq!(summary.bytes, full.len() as u64);
        let decoded = Scene::from_png(photo.name.clone(), full).expect("full PNG");
        assert_eq!((decoded.width, decoded.height), (440, 2));
        let thumbnail = &summary.thumbnail;
        assert_eq!(
            thumbnail.path,
            scene_thumbnail_path(&photo.name).expect("thumbnail path")
        );
        let bytes = &sink.0[thumbnail.path.as_str()];
        assert_eq!(thumbnail.sha256, ArtifactHash::of(bytes));
        assert_eq!(thumbnail.bytes, bytes.len() as u64);
        assert_eq!((thumbnail.width, thumbnail.height), (220, 1));
        let decoded = Scene::from_png(photo.name.clone(), bytes).expect("thumbnail PNG");
        assert_eq!((decoded.width, decoded.height), (220, 1));
    }

    #[test]
    fn writer_rejects_oversized_scenes() {
        let scene = Scene::new(
            SceneName::new("oversized").expect("name"),
            1601,
            1,
            vec![[0.18; 3]; 1601],
        )
        .expect("RAW intermediate");
        let path = scene_path(&scene.name).expect("path");
        let site = SiteData {
            scenes: vec![scene],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(&mut sink, &site),
            Err(PublishError::SceneDimensions)
        ));
        assert!(!sink.0.contains_key(path.as_str()));
        assert!(!sink.0.contains_key(INDEX_PATH));
    }

    #[test]
    fn reference_preview_index_authenticates_the_published_identity_lut() {
        let bytes = crate::bake_lut(&looklab_model::Look::colorimetric(), |xyz| xyz);
        let site = SiteData {
            reference_preview: Some(ArtifactBlob::new(
                ArtifactName::new("neutral.lut33").expect("artifact name"),
                bytes.clone(),
            )),
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        let root = write_site(&mut sink, &site).expect("write");
        let index_bytes = &sink.0[INDEX_PATH];
        assert_eq!(root, ArtifactHash::of(index_bytes));
        let index: SiteIndex = serde_json::from_slice(index_bytes).expect("site index");
        let reference = index.reference_preview.expect("reference metadata");
        let published = &sink.0[reference.path.as_str()];
        assert_eq!(published, &bytes);
        assert_eq!(reference.bytes, published.len() as u64);
        assert_eq!(reference.sha256, ArtifactHash::of(published));
        assert_eq!(published.len(), crate::LUT_BYTES);
    }

    #[test]
    fn older_site_index_has_no_reference_preview() {
        let index: SiteIndex = serde_json::from_str(
            r#"{"format":"looklab-site/1","bodies":[],"looks":[],"scenes":[]}"#,
        )
        .expect("older site index");
        assert!(index.reference_preview.is_none());
        assert!(index.interfaces.is_empty());
    }

    fn interface_report() -> crate::interface::RoundTripReport {
        crate::interface::RoundTripReport {
            probes: 1057,
            target_clipped: 3,
            ccts: vec![crate::interface::CctSummary {
                cct: 6500.0,
                count: 1057,
                median: 0.2,
                p95: 0.6,
                max: 1.2,
                unsupported: 0,
                clipped: 3,
            }],
            flagged: false,
        }
    }

    fn interface(definition: &str, current: bool) -> PublishedInterfaceProfile {
        PublishedInterfaceProfile::new(
            crate::interface::InterfaceProvenance::Approved {
                proposal: crate::audit::ProposalRef::new("proposal-1").expect("proposal"),
                snapshot_version: 4,
                snapshot_sha256: ArtifactHash::of(b"snapshot"),
            },
            definition.as_bytes().to_vec(),
            interface_report(),
            current,
        )
        .expect("interface")
    }

    #[test]
    fn interface_index_pins_the_manifest_and_the_manifest_pins_its_artifacts() {
        let definition = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
        let site = SiteData {
            interfaces: vec![interface(definition, true)],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        assert_eq!(index.interfaces.len(), 1);
        let entry = &index.interfaces[0];
        let manifest_bytes = &sink.0[entry.manifest.as_str()];
        assert_eq!(entry.manifest_sha256, ArtifactHash::of(manifest_bytes));
        assert_eq!(entry.manifest_bytes, manifest_bytes.len() as u64);
        let manifest: crate::interface::InterfaceProfileManifest =
            serde_json::from_slice(manifest_bytes).expect("manifest");
        assert_eq!(manifest.id, entry.id);
        assert_eq!(manifest.name, entry.name);
        assert_eq!(
            manifest.provenance,
            crate::interface::InterfaceProvenance::Approved {
                proposal: crate::audit::ProposalRef::new("proposal-1").expect("proposal"),
                snapshot_version: 4,
                snapshot_sha256: ArtifactHash::of(b"snapshot"),
            }
        );
        let dir = interface_dir(&entry.manifest_sha256).expect("dir");
        manifest
            .definition
            .verify(&sink.0[dir.join(INTERFACE_DEFINITION_ARTIFACT).as_str()])
            .expect("definition");
        manifest
            .report
            .verify(&sink.0[dir.join(INTERFACE_REPORT_ARTIFACT).as_str()])
            .expect("report");
    }

    #[test]
    fn a_replacement_revision_preserves_the_previous_manifest_and_bytes() {
        let first = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
        let second = first.replace(
            "Example editor 3D LUT module",
            "Example editor 3D LUT module v2",
        );
        let mut sink = MemorySink(BTreeMap::new());
        write_site(
            &mut sink,
            &SiteData {
                interfaces: vec![interface(first, true)],
                ..SiteData::default()
            },
        )
        .expect("first");
        let first_index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        let old_path = first_index.interfaces[0].manifest.clone();
        let old_bytes = sink.0[old_path.as_str()].clone();
        write_site(
            &mut sink,
            &SiteData {
                interfaces: vec![interface(&second, true)],
                ..SiteData::default()
            },
        )
        .expect("second");
        assert_eq!(sink.0[old_path.as_str()], old_bytes);
        let second_index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        assert_eq!(second_index.interfaces.len(), 1);
        assert_ne!(second_index.interfaces[0].manifest, old_path);
        assert_eq!(
            second_index.interfaces[0].name,
            "Example editor 3D LUT module v2"
        );
    }

    #[test]
    fn interface_writer_rejects_tampered_definition_report_and_manifest() {
        let definition = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");

        let mut page = interface(definition, true);
        page.definition.push(b'\n');
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(
                &mut sink,
                &SiteData {
                    interfaces: vec![page],
                    ..SiteData::default()
                }
            ),
            Err(PublishError::Manifest(_))
        ));

        let mut page = interface(definition, true);
        page.report_bytes.push(b'\n');
        assert!(matches!(
            write_site(
                &mut sink,
                &SiteData {
                    interfaces: vec![page],
                    ..SiteData::default()
                }
            ),
            Err(PublishError::Manifest(_))
        ));

        let mut page = interface(definition, true);
        page.manifest.name = "Different name".to_owned();
        assert!(matches!(
            write_site(
                &mut sink,
                &SiteData {
                    interfaces: vec![page],
                    ..SiteData::default()
                }
            ),
            Err(PublishError::Interface(_))
        ));
    }

    #[test]
    fn interface_report_verification_rejects_negative_measurements() {
        let definition = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
        let mut report = interface_report();
        report.ccts[0].median = -1.0;
        assert!(matches!(
            PublishedInterfaceProfile::new(
                crate::interface::InterfaceProvenance::Bootstrap,
                definition.as_bytes().to_vec(),
                report,
                true,
            ),
            Err(PublishError::Interface(_))
        ));
    }

    fn transform_bytes(version: u32) -> Vec<u8> {
        format!("format: looklab-transform/1\nversion: {version}\nbody: fujifilm/x-t5\n")
            .into_bytes()
    }

    fn input_transform(bytes: Vec<u8>, current: bool) -> PublishedInputTransform {
        PublishedInputTransform::new(
            looklab_schema::BodyId::new("fujifilm/x-t5").expect("body"),
            InputTransformVersion::new(2).expect("version"),
            bytes,
            current,
        )
        .expect("transform")
    }

    #[test]
    fn input_transform_index_pins_the_content_addressed_bytes() {
        let bytes = transform_bytes(2);
        let site = SiteData {
            input_transforms: vec![input_transform(bytes.clone(), true)],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        assert_eq!(index.input_transforms.len(), 1);
        let entry = &index.input_transforms[0];
        assert_eq!(
            entry.path,
            input_transform_path(&ArtifactHash::of(&bytes)).expect("path")
        );
        assert_eq!(entry.sha256, ArtifactHash::of(&bytes));
        assert_eq!(entry.bytes, bytes.len() as u64);
        assert_eq!(sink.0[entry.path.as_str()], bytes);
    }

    #[test]
    fn a_replacement_input_transform_preserves_the_previous_bytes() {
        let first = transform_bytes(2);
        let second = transform_bytes(3);
        let mut sink = MemorySink(BTreeMap::new());
        write_site(
            &mut sink,
            &SiteData {
                input_transforms: vec![input_transform(first.clone(), true)],
                ..SiteData::default()
            },
        )
        .expect("first");
        let old_path = input_transform_path(&ArtifactHash::of(&first)).expect("path");
        write_site(
            &mut sink,
            &SiteData {
                input_transforms: vec![input_transform(second.clone(), true)],
                ..SiteData::default()
            },
        )
        .expect("second");
        assert_eq!(sink.0[old_path.as_str()], first);
        let index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        assert_eq!(index.input_transforms.len(), 1);
        assert_eq!(index.input_transforms[0].sha256, ArtifactHash::of(&second));
    }

    #[test]
    fn input_transform_writer_rejects_tampered_bytes_and_paths() {
        let bytes = transform_bytes(2);
        let mut page = input_transform(bytes.clone(), true);
        page.bytes.push(b'\n');
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(
                &mut sink,
                &SiteData {
                    input_transforms: vec![page],
                    ..SiteData::default()
                }
            ),
            Err(PublishError::InputTransform(_))
        ));

        let mut page = input_transform(bytes, true);
        page.entry.path = SitePath::new("data/input-transforms/other.yaml").expect("path");
        assert!(matches!(
            write_site(
                &mut sink,
                &SiteData {
                    input_transforms: vec![page],
                    ..SiteData::default()
                }
            ),
            Err(PublishError::InputTransform(_))
        ));
    }

    fn source_preview_blobs(extra: Vec<ArtifactBlob>) -> Vec<ArtifactBlob> {
        use crate::source_lut::{
            PreviewBaseline, SOURCE_CUBE_ARTIFACT, SourceLutFormat, SourceLutPreviewContext,
        };

        let source =
            b"LUT_3D_SIZE 2\n0 0 0\n1 0 0\n0 1 0\n1 1 0\n0 0 1\n1 0 1\n0 1 1\n1 1 1\n".to_vec();
        let lut = vec![0_u8; crate::LUT_BYTES];
        let descriptor = SourceLutPreview {
            format: SourceLutFormat::V1,
            look: looklab_model::file::LookIdentity::new(
                "film-lut",
                "1.0.0",
                LookKind::LutDefined,
                WbPolicy::Adaptive,
            )
            .expect("identity"),
            name: "Film LUT".to_owned(),
            license: looklab_model::file::LicenseRecord {
                own_work: true,
                source_license: None,
                source_url: None,
                attribution: "A contributor".to_owned(),
            },
            encoding: looklab_model::file::DeclaredEncoding::DisplayReferred {
                base: looklab_model::file::BaseReference::Look {
                    slug: Slug::new("provia").expect("slug"),
                    version: LookVersion::new("1.0.0").expect("version"),
                },
                status: looklab_model::file::BaseStatus::BaseIdentifiedUnmeasured,
            },
            output: looklab_model::file::OutputEncoding::Srgb,
            emulates: None,
            source: ArtifactEntry {
                name: ArtifactName::new(SOURCE_CUBE_ARTIFACT).expect("name"),
                sha256: ArtifactHash::of(&source),
                bytes: source.len() as u64,
            },
            preview: SourceLutPreviewContext {
                baseline: PreviewBaseline::NeutralFallback,
                lut: ArtifactEntry {
                    name: ArtifactName::new(crate::manifest::PREVIEW_LUT_ARTIFACT).expect("name"),
                    sha256: ArtifactHash::of(&lut),
                    bytes: lut.len() as u64,
                },
            },
        };
        let mut blobs = vec![
            ArtifactBlob::new(
                ArtifactName::new(SOURCE_LUT_ARTIFACT).expect("name"),
                descriptor.to_json().expect("descriptor json"),
            ),
            ArtifactBlob::new(
                ArtifactName::new(crate::manifest::PREVIEW_LUT_ARTIFACT).expect("name"),
                lut,
            ),
            ArtifactBlob::new(
                ArtifactName::new(SOURCE_CUBE_ARTIFACT).expect("name"),
                source,
            ),
        ];
        blobs.extend(extra);
        blobs
    }

    fn source_preview_version(blobs: Vec<ArtifactBlob>) -> LookVersionPage {
        LookVersionPage::new(
            Slug::new("film-lut").expect("slug"),
            LookVersion::new("1.0.0").expect("version"),
            LookStatus::Published,
            GateSummary::default(),
            ProvenanceSummary::default(),
            blobs,
            VersionNotes::default(),
        )
        .expect("version")
    }

    fn source_preview_look(version: LookVersionPage) -> LookPage {
        LookPage {
            slug: Slug::new("film-lut").expect("slug"),
            name: "Film LUT".to_owned(),
            kind: LookKind::LutDefined,
            wb_policy: WbPolicy::Adaptive,
            description: String::new(),
            aliases: Vec::new(),
            license: None,
            listing: LookListing::Listed,
            superseded_by: None,
            head: None,
            selected: None,
            versions: vec![version],
        }
    }

    #[test]
    fn a_source_only_preview_indexes_its_descriptor() {
        let version = source_preview_version(source_preview_blobs(Vec::new()));
        assert!(!version.matchable);
        assert!(!version.card_eligible);
        let site = SiteData {
            looks: vec![source_preview_look(version)],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        write_site(&mut sink, &site).expect("write");
        let index: SiteIndex = serde_json::from_slice(&sink.0[INDEX_PATH]).expect("index");
        let look = &index.looks[0];
        assert!(look.selected.is_none());
        let summary = &look.versions[0];
        assert_eq!(summary.artifact.as_str(), SOURCE_LUT_ARTIFACT);
        assert!(!summary.matchable);
        assert!(!summary.card_eligible);
        assert!(summary.card.is_none());
        for name in [
            SOURCE_LUT_ARTIFACT,
            crate::source_lut::SOURCE_CUBE_ARTIFACT,
            crate::manifest::PREVIEW_LUT_ARTIFACT,
        ] {
            assert!(
                sink.0
                    .contains_key(&format!("data/looks/film-lut/1.0.0/{name}")),
                "{name}"
            );
        }
        assert!(!sink.0.keys().any(|path| path.ends_with("look.looklab")));
    }

    #[test]
    fn a_layered_and_source_only_version_is_rejected() {
        let document = ArtifactBlob::new(
            ArtifactName::new(crate::manifest::DOCUMENT_ARTIFACT).expect("name"),
            vec![1, 2, 3],
        );
        assert!(matches!(
            LookVersionPage::new(
                Slug::new("film-lut").expect("slug"),
                LookVersion::new("1.0.0").expect("version"),
                LookStatus::Published,
                GateSummary::default(),
                ProvenanceSummary::default(),
                source_preview_blobs(vec![document]),
                VersionNotes::default(),
            ),
            Err(PublishError::ContradictoryLookArtifacts)
        ));
    }

    #[test]
    fn a_source_only_preview_cannot_be_the_selected_target() {
        let version = source_preview_version(source_preview_blobs(Vec::new()));
        let mut look = source_preview_look(version);
        look.selected = Some(SelectedTargetSummary {
            version: LookVersion::new("1.0.0").expect("version"),
            eligibility: None,
            applicability: Applicability::Current,
        });
        let site = SiteData {
            looks: vec![look],
            ..SiteData::default()
        };
        let mut sink = MemorySink(BTreeMap::new());
        assert!(matches!(
            write_site(&mut sink, &site),
            Err(PublishError::SourceLut(_))
        ));
    }
}
