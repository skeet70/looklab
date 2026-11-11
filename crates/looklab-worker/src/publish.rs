// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use looklab_model::file::{
    CameraRendering, LicenseRecord, LookKind, LookPin, LookVersion, LutEmulation, Slug, WbPolicy,
};
use looklab_model::{LookDocument, emulation_matches_camera_rendering};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, BundleSelection, ConformanceEntry, Decision, FamilyHeadPage, InputTransformVersion,
    InterfaceProfileManifest, InterfaceProvenance, LicensePage, LookListing, LookPage, LookRef,
    LookStatus, LookVersionPage, ManualPage, PreparationBlocker, PreparationPage, ProposalRef,
    PublishedBundleRevisionPage, PublishedCurrentProjection, PublishedInputTransform,
    PublishedInterfaceProfile, PublishedReportSummary, RELEASE_ARTIFACT, RealizationCard, Role,
    SCENE_SOURCE_INDEX, SOURCE_LUT_ARTIFACT, Scene, SelectedTargetSummary, SelectionScope,
    SiteData, SitePath, SiteReport, SiteSink, SourceLutPreview, SplitKind, SubjectCurrentPage,
    TargetEligibilitySnapshot, Timestamp, VersionLookShape, VersionNotes, VersionRef,
    compare_versions, look_shape, manual_path, read_scene_sources, reference_round_trip,
    test_chart_scenes, write_conformance_pack, write_site,
};
use looklab_schema::{
    Applicability, ArtifactRole, BodyId, BodyRealizationId, BodyRealizationVersionId,
    CapabilityKey, ConfirmationBasis, ControlEpochId, FamilyHead, InvalidationReason, ManualUrl,
    ModelReportId, ModelSubject, ModelVersionId, ModeratorEvidenceKind, ModeratorFirmwareScope,
    ProjectionUse, QualificationCapability, RealizationRevisionId, RealizationStatus, SubjectKey,
    TargetEligibilityStatus,
};
use looklab_server::admin::{self, ListingState, LookListingRecord, ManifestArtifact};
use looklab_server::model_store::{
    self, StoredBundleRevision, StoredRealizationRevision, StoredReport,
};
use looklab_server::{BlobError, BlobHash, BlobStore as _, FsBlobStore, write_guarded};
use looklab_upload::LookSlug;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::body::{DataDir, TransformDir};
use crate::clock::{now_unix, rfc3339};
use crate::config::WorkerConfig;
use crate::db::{self, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::refit::{LOOK_ARTIFACT, ReleaseRecord, gate_summary};

#[derive(Debug, Clone)]
pub struct PublishConfig {
    pub db: PathBuf,
    pub blobs: PathBuf,
    pub data: PathBuf,
    /// Documentation catalog, outside the community snapshot.
    pub manuals: PathBuf,
    /// Approved community snapshots; the DB activation row selects the active one.
    pub snapshot_dir: PathBuf,
    /// Body input transforms, one `<make>-<model>.yaml` per body.
    pub transforms: PathBuf,
    pub out: PathBuf,
}

/// Hidden inside the output directory so every publisher of the same tree shares it.
const OUT_LOCK_FILE: &str = ".looklab-publish.lock";

/// An exclusive hold on one output directory, released when dropped.
#[derive(Debug)]
pub struct OutputLock {
    _file: File,
}

impl OutputLock {
    fn acquire(out: &Path) -> Result<Self> {
        std::fs::create_dir_all(out)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(out.join(OUT_LOCK_FILE))?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

impl PublishConfig {
    pub(crate) fn worker_config(&self) -> WorkerConfig {
        let mut config = WorkerConfig::new(&self.db, &self.blobs);
        config.data = self.data.clone();
        config.manuals = self.manuals.clone();
        config.snapshot_dir = self.snapshot_dir.clone();
        config.site = self.out.clone();
        config.transforms = self.transforms.clone();
        config
    }
    pub fn from_worker(config: &WorkerConfig) -> Self {
        Self {
            db: config.db.clone(),
            blobs: config.blobs.clone(),
            data: config.data.clone(),
            manuals: config.manuals.clone(),
            snapshot_dir: config.snapshot_dir.clone(),
            transforms: config.transforms.clone(),
            out: config.site.clone(),
        }
    }

    /// Blocks while another publisher of `out` holds it.
    pub fn lock_output(&self) -> Result<OutputLock> {
        OutputLock::acquire(&self.out)
    }

    /// Waits on the blocking pool so a queued publisher leaves the executor free.
    pub(crate) async fn lock_output_async(&self) -> Result<OutputLock> {
        let out = self.out.clone();
        tokio::task::spawn_blocking(move || OutputLock::acquire(&out)).await?
    }
}

#[derive(Debug, Clone)]
pub struct PublishSummary {
    pub root: String,
    pub files: u64,
}

pub async fn publish_site(config: &PublishConfig) -> Result<PublishSummary> {
    let pool = looklab_server::open_database(&config.db).await?;
    publish_with_pool(&pool, config).await
}

pub async fn publish_with_pool(
    pool: &SqlitePool,
    config: &PublishConfig,
) -> Result<PublishSummary> {
    let lock = config.lock_output_async().await?;
    publish_with_pool_locked(pool, config, &lock).await
}

pub(crate) async fn publish_with_pool_locked(
    pool: &SqlitePool,
    config: &PublishConfig,
    _lock: &OutputLock,
) -> Result<PublishSummary> {
    let site = site_data(pool, config).await?;
    let conformance = conformance_entries(&config.data.join("card/conformance-v1"))?;
    let mut sink = DirSink::new(&config.out);
    let root = write_site(&mut sink, &site)?;
    prune_unlisted(&config.out, &site)?;
    write_conformance_pack(&mut sink, &conformance)?;
    write_site_root(&config.out, root.as_str())?;
    Ok(PublishSummary {
        root: root.as_str().to_owned(),
        files: sink.files,
    })
}

fn conformance_entries(root: &Path) -> Result<Vec<ConformanceEntry>> {
    let mut entries = Vec::new();
    collect_conformance(root, root, &mut entries)?;
    Ok(entries)
}

fn collect_conformance(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ConformanceEntry>,
) -> Result<()> {
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.sort();
    for path in paths {
        let kind = std::fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            collect_conformance(root, &path, entries)?;
        } else if kind.is_file() {
            let relative = path.strip_prefix(root).map_err(|_| {
                WorkerError::Data(format!(
                    "{} is outside the conformance tree",
                    path.display()
                ))
            })?;
            let name = relative
                .to_str()
                .ok_or_else(|| WorkerError::Data(format!("{} is not UTF-8", relative.display())))?;
            entries.push(ConformanceEntry {
                path: SitePath::new(name)?,
                bytes: std::fs::read(&path)?,
            });
        } else {
            return Err(WorkerError::Data(format!(
                "{} is not a regular conformance file",
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn write_site_root(out: &Path, root: &str) -> Result<PathBuf> {
    let path = out.join("site-root.txt");
    std::fs::write(&path, format!("{root}\n"))?;
    Ok(path)
}

struct DirSink {
    root: PathBuf,
    files: u64,
}

impl DirSink {
    fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            files: 0,
        }
    }
}

impl SiteSink for DirSink {
    fn put(&mut self, path: &SitePath, bytes: &[u8]) -> io::Result<()> {
        let full = self.root.join(path.as_str());
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, bytes)?;
        self.files += 1;
        Ok(())
    }
}

fn prune_unlisted(root: &Path, site: &SiteData) -> Result<()> {
    for look in &site.looks {
        if look.listing.is_listed() {
            continue;
        }
        let slug = look.slug.as_str();
        let dir = root.join("data/looks").join(slug);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        let page = root.join("data/looks").join(format!("{slug}.json"));
        if page.exists() {
            std::fs::remove_file(&page)?;
        }
    }
    prune_manuals(root, &site.manuals)
}

fn prune_manuals(root: &Path, manuals: &[ManualPage]) -> Result<()> {
    let expected = manuals
        .iter()
        .map(|manual| manual_path(&manual.id).map(|path| path.as_str().to_owned()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let dir = root.join("data/manuals");
    if !dir.is_dir() {
        return Ok(());
    }
    for make in std::fs::read_dir(&dir)? {
        let make = make?;
        if !make.file_type()?.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(make.path())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let relative = format!(
                "data/manuals/{}/{}",
                make.file_name().to_string_lossy(),
                entry.file_name().to_string_lossy()
            );
            if !expected.contains(&relative) {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

struct LookMetadata {
    version_pin: LookVersion,
    kind: LookKind,
    name: Option<String>,
    emulation: Option<LutEmulation>,
    rendering: Option<CameraRendering>,
    license: Option<LicensePage>,
}

async fn supersede_looks(
    pool: &SqlitePool,
    metadata: &BTreeMap<String, LookMetadata>,
) -> Result<()> {
    let natives: Vec<(&str, &LookVersion, &CameraRendering)> = metadata
        .iter()
        .filter_map(|(slug, meta)| {
            meta.rendering
                .as_ref()
                .map(|rendering| (slug.as_str(), &meta.version_pin, rendering))
        })
        .collect();
    for (slug, meta) in metadata {
        let Some(emulation) = &meta.emulation else {
            continue;
        };
        let Some((native_slug, native_version, _)) = natives
            .iter()
            .filter(|(_, _, rendering)| {
                emulation_matches_camera_rendering(emulation, &rendering.maker, &rendering.name)
            })
            .max_by(|left, right| {
                compare_versions(left.1, right.1).then_with(|| left.0.cmp(right.0))
            })
        else {
            continue;
        };
        let look = LookSlug::try_from(slug.as_str())
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        let pin = LookPin::new(native_slug, native_version.as_str())
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        admin::set_superseded_by(pool, &look, &pin).await?;
    }
    Ok(())
}

async fn interface_profiles(
    pool: &SqlitePool,
    config: &PublishConfig,
    worker_config: &WorkerConfig,
    data: &Path,
) -> Result<Vec<PublishedInterfaceProfile>> {
    let rows = sqlx::query(
        "SELECT id, payload, round_trip_report, published_version, published_hash
         FROM proposals
         WHERE type = ?
           AND status = ?
           AND publication = 'published'
         ORDER BY published_version, created_at, id",
    )
    .bind(looklab_schema::ProposalKind::InterfaceProfile.as_str())
    .bind(looklab_schema::ProposalStatus::Approved.as_str())
    .fetch_all(pool)
    .await?;

    let mut roots: BTreeMap<(u64, String), PathBuf> = BTreeMap::new();
    let mut approved = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: String = row.get("id");
        let payload: String = row.get("payload");
        let report: Option<String> = row.get("round_trip_report");
        let version: Option<i64> = row.get("published_version");
        let hash: Option<String> = row.get("published_hash");
        let (Some(version), Some(hash)) = (version, hash) else {
            return Err(WorkerError::Data(format!(
                "interface proposal {id} has no published snapshot identity"
            )));
        };
        let version = u64::try_from(version).map_err(|_| {
            WorkerError::Data(format!(
                "interface proposal {id} has a negative snapshot version"
            ))
        })?;
        let snapshot_hash = ArtifactHash::parse(&hash)?;
        let snapshot_root = match roots.get(&(version, hash.clone())) {
            Some(root) => root.clone(),
            None => {
                let root = crate::body::verified_snapshot_root(worker_config, version, &hash)?;
                roots.insert((version, hash.clone()), root.clone());
                root
            }
        };
        let profile = looklab_schema::validate_interface_profile(&payload).map_err(|errors| {
            WorkerError::Data(format!(
                "interface proposal {id} has an invalid definition: {errors:?}"
            ))
        })?;
        let export = snapshot_root.join(looklab_schema::interface_export_path(&profile.id));
        let definition = std::fs::read(&export).map_err(|error| {
            WorkerError::Data(format!(
                "interface proposal {id} definition {} is unreadable: {error}",
                export.display()
            ))
        })?;
        if definition.as_slice() != payload.as_bytes() {
            return Err(WorkerError::Data(format!(
                "interface proposal {id} does not match snapshot {version}"
            )));
        }
        let report = report.ok_or_else(|| {
            WorkerError::Data(format!(
                "interface proposal {id} has no reference round-trip report"
            ))
        })?;
        let proposal =
            ProposalRef::new(id.as_str()).map_err(|error| WorkerError::Data(error.to_owned()))?;
        approved.push(
            PublishedInterfaceProfile::from_report_bytes(
                InterfaceProvenance::Approved {
                    proposal,
                    snapshot_version: version,
                    snapshot_sha256: snapshot_hash,
                },
                definition,
                report.into_bytes(),
                false,
            )
            .map_err(|error| {
                WorkerError::Data(format!(
                    "interface proposal {id} has an unreadable reference round-trip report: {error}"
                ))
            })?,
        );
    }

    let mut current: BTreeMap<looklab_schema::InterfaceId, usize> = BTreeMap::new();
    for (index, page) in approved.iter().enumerate() {
        current.insert(page.manifest.id.clone(), index);
    }
    for index in current.into_values() {
        approved[index].current = true;
    }

    let approved_ids: BTreeSet<looklab_schema::InterfaceId> = approved
        .iter()
        .map(|page| page.manifest.id.clone())
        .collect();
    let mut pages = approved;
    let shipped_dir = data.join("interfaces");
    let mut shipped_ids = BTreeSet::new();
    collect_bootstrap_interfaces(&shipped_dir, &mut pages, &approved_ids, &mut shipped_ids)?;

    for page in &pages {
        record_interface_revision(pool, config, page).await?;
    }
    let mut revisions: BTreeMap<ArtifactHash, PublishedInterfaceProfile> = BTreeMap::new();
    for page in load_output_interface_history(pool, config).await? {
        revisions.insert(page.revision()?, page);
    }
    for page in load_interface_history(pool, config).await? {
        revisions.insert(page.revision()?, page);
    }
    for page in pages {
        revisions.insert(page.revision()?, page);
    }
    Ok(revisions.into_values().collect())
}

fn collect_bootstrap_interfaces(
    dir: &Path,
    pages: &mut Vec<PublishedInterfaceProfile>,
    approved_ids: &BTreeSet<looklab_schema::InterfaceId>,
    shipped_ids: &mut BTreeSet<looklab_schema::InterfaceId>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut files = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    for file in files {
        if file.extension().and_then(|value| value.to_str()) != Some("yaml") {
            continue;
        }
        let definition = std::fs::read(&file)?;
        let text = std::str::from_utf8(&definition)
            .map_err(|_| WorkerError::Data(format!("{} is not UTF-8", file.display())))?;
        let profile = looklab_schema::validate_interface_profile(text)
            .map_err(|errors| WorkerError::Data(format!("{}: {errors:?}", file.display())))?;
        let first = shipped_ids.insert(profile.id.clone());
        if pages
            .iter()
            .any(|page| page.manifest.id == profile.id && page.definition == definition)
        {
            continue;
        }
        let current = !approved_ids.contains(&profile.id) && first;
        let report = reference_round_trip(&profile)?;
        pages.push(PublishedInterfaceProfile::new(
            InterfaceProvenance::Bootstrap,
            definition,
            report,
            current,
        )?);
    }
    Ok(())
}

fn put_publication_blob(
    root: &Path,
    hash: &ArtifactHash,
    bytes: &[u8],
) -> std::result::Result<(), BlobError> {
    let store = FsBlobStore::new(root.to_path_buf())?;
    let hash = BlobHash::from_hex(hash.as_str()).ok_or_else(|| {
        BlobError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid artifact hash",
        ))
    })?;
    store.put(hash, bytes)
}

async fn write_publication_blobs(
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
    root: PathBuf,
    blobs: Vec<(ArtifactHash, Vec<u8>)>,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    let (tx, ()) = write_guarded(tx, move || {
        let result = blobs
            .iter()
            .try_for_each(|(hash, bytes)| put_publication_blob(&root, hash, bytes));
        ((), result)
    })
    .await
    .map_err(|error| WorkerError::Data(error.to_string()))?;
    Ok(tx)
}

async fn record_interface_revision(
    pool: &SqlitePool,
    config: &PublishConfig,
    page: &PublishedInterfaceProfile,
) -> Result<()> {
    let tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let manifest = page.revision()?;
    let mut tx = write_publication_blobs(
        tx,
        config.blobs.clone(),
        vec![
            (manifest.clone(), page.manifest_bytes.clone()),
            (
                page.manifest.definition.sha256.clone(),
                page.definition.clone(),
            ),
            (
                page.manifest.report.sha256.clone(),
                page.report_bytes.clone(),
            ),
        ],
    )
    .await?;
    sqlx::query(
        "INSERT INTO published_interface_revisions
         (manifest_sha256, definition_sha256, report_sha256, created_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (manifest_sha256) DO NOTHING",
    )
    .bind(manifest.as_str())
    .bind(page.manifest.definition.sha256.as_str())
    .bind(page.manifest.report.sha256.as_str())
    .bind(now_unix())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn load_interface_history(
    pool: &SqlitePool,
    config: &PublishConfig,
) -> Result<Vec<PublishedInterfaceProfile>> {
    let rows = sqlx::query(
        "SELECT manifest_sha256, definition_sha256, report_sha256
         FROM published_interface_revisions
         ORDER BY created_at, manifest_sha256",
    )
    .fetch_all(pool)
    .await?;
    let mut pages = Vec::with_capacity(rows.len());
    for row in &rows {
        let manifest: String = row.get("manifest_sha256");
        let definition: String = row.get("definition_sha256");
        let report: String = row.get("report_sha256");
        pages.push(PublishedInterfaceProfile::from_published(
            read_blob_hash(config, &manifest)?,
            read_blob_hash(config, &definition)?,
            read_blob_hash(config, &report)?,
            false,
        )?);
    }
    Ok(pages)
}

async fn load_output_interface_history(
    pool: &SqlitePool,
    config: &PublishConfig,
) -> Result<Vec<PublishedInterfaceProfile>> {
    let root = config.out.join("data/interfaces");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut dirs = std::fs::read_dir(&root)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    dirs.sort();
    let mut pages = Vec::new();
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let expected = ArtifactHash::parse(name)?;
        let manifest_bytes = std::fs::read(dir.join(looklab_publish::MANIFEST_NAME))?;
        if ArtifactHash::of(&manifest_bytes) != expected {
            return Err(WorkerError::Data(format!(
                "{}: manifest bytes do not match the directory name",
                dir.display()
            )));
        }
        let manifest: InterfaceProfileManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| WorkerError::Data(format!("{}: {error}", dir.display())))?;
        let definition = std::fs::read(dir.join(manifest.definition.name.as_str()))?;
        let report = std::fs::read(dir.join(manifest.report.name.as_str()))?;
        let page =
            PublishedInterfaceProfile::from_published(manifest_bytes, definition, report, false)?;
        record_interface_revision(pool, config, &page).await?;
        pages.push(page);
    }
    Ok(pages)
}

#[derive(serde::Deserialize)]
struct PublishedTransformIdentity {
    version: u32,
    #[serde(default)]
    body: Option<String>,
}

async fn input_transforms(
    pool: &SqlitePool,
    config: &PublishConfig,
    transforms: &TransformDir,
    resources: &[crate::body::BodyResources],
) -> Result<Vec<PublishedInputTransform>> {
    let mut current = Vec::new();
    for resource in resources {
        match transforms.load_with_bytes(&resource.body.id) {
            Ok((parsed, bytes)) => {
                let version = InputTransformVersion::new(parsed.version())?;
                current.push(PublishedInputTransform::new(
                    resource.body.id.clone(),
                    version,
                    bytes,
                    true,
                )?);
            }
            Err(WorkerError::TransformUnavailable { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    for page in &current {
        record_input_transform(pool, config, page).await?;
    }

    let mut pages: BTreeMap<(looklab_schema::BodyId, ArtifactHash), PublishedInputTransform> =
        BTreeMap::new();
    for page in load_input_transform_history(pool, config, transforms).await? {
        pages.insert((page.entry.body.clone(), page.entry.sha256.clone()), page);
    }
    for page in load_body_state_input_transforms(pool, config, transforms).await? {
        pages.insert((page.entry.body.clone(), page.entry.sha256.clone()), page);
    }
    for page in load_output_input_transforms(pool, config, transforms).await? {
        pages.insert((page.entry.body.clone(), page.entry.sha256.clone()), page);
    }
    for page in current {
        pages.insert((page.entry.body.clone(), page.entry.sha256.clone()), page);
    }
    Ok(pages.into_values().collect())
}

async fn record_input_transform(
    pool: &SqlitePool,
    config: &PublishConfig,
    page: &PublishedInputTransform,
) -> Result<()> {
    let tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut tx = write_publication_blobs(
        tx,
        config.blobs.clone(),
        vec![(page.entry.sha256.clone(), page.bytes.clone())],
    )
    .await?;
    sqlx::query(
        "INSERT INTO published_input_transforms (body, version, sha256, created_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (body, sha256) DO NOTHING",
    )
    .bind(page.entry.body.as_str())
    .bind(i64::from(page.entry.version.get()))
    .bind(page.entry.sha256.as_str())
    .bind(now_unix())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn load_input_transform_history(
    pool: &SqlitePool,
    config: &PublishConfig,
    transforms: &TransformDir,
) -> Result<Vec<PublishedInputTransform>> {
    let rows = sqlx::query(
        "SELECT body, version, sha256
         FROM published_input_transforms
         ORDER BY created_at, body, sha256",
    )
    .fetch_all(pool)
    .await?;
    let mut pages = Vec::with_capacity(rows.len());
    for row in &rows {
        let body: String = row.get("body");
        let body = looklab_schema::BodyId::new(&body)
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        let version: i64 = row.get("version");
        let version =
            InputTransformVersion::new(u32::try_from(version).map_err(|_| {
                WorkerError::Data("input transform version is not a u32".to_owned())
            })?)?;
        let sha256: String = row.get("sha256");
        let bytes = read_blob_hash(config, &sha256)?;
        transforms.validate(&body, version.get(), &bytes)?;
        pages.push(PublishedInputTransform::new(body, version, bytes, false)?);
    }
    Ok(pages)
}

async fn load_body_state_input_transforms(
    pool: &SqlitePool,
    config: &PublishConfig,
    transforms: &TransformDir,
) -> Result<Vec<PublishedInputTransform>> {
    let rows = sqlx::query(
        "SELECT DISTINCT body, profile_sha256
         FROM model_body_states
         WHERE profile_sha256 IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    let mut pages = Vec::new();
    for row in &rows {
        let body: String = row.get("body");
        let body = looklab_schema::BodyId::new(&body)
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        let sha256: String = row.get("profile_sha256");
        let bytes = read_blob_hash(config, &sha256)?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| WorkerError::Data(format!("input transform {sha256} is not UTF-8")))?;
        let identity: PublishedTransformIdentity = serde_norway::from_str(text)?;
        if let Some(named) = &identity.body
            && named != body.as_str()
        {
            return Err(WorkerError::Data(format!(
                "input transform {sha256} names {named}, the body state names {body}"
            )));
        }
        let version = InputTransformVersion::new(identity.version)?;
        transforms.validate(&body, version.get(), &bytes)?;
        let page = PublishedInputTransform::new(body, version, bytes, false)?;
        record_input_transform(pool, config, &page).await?;
        pages.push(page);
    }
    Ok(pages)
}

async fn load_output_input_transforms(
    pool: &SqlitePool,
    config: &PublishConfig,
    transforms: &TransformDir,
) -> Result<Vec<PublishedInputTransform>> {
    let dir = config.out.join("data/input-transforms");
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = std::fs::read_dir(&dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    let mut pages = Vec::new();
    for file in files {
        if file.extension().and_then(|value| value.to_str()) != Some("yaml") {
            continue;
        }
        let bytes = std::fs::read(&file)?;
        let sha256 = ArtifactHash::of(&bytes);
        let Some(name) = file.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if name != sha256.as_str() {
            return Err(WorkerError::Data(format!(
                "{} is not named by its content hash {sha256}",
                file.display()
            )));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| WorkerError::Data(format!("{} is not UTF-8", file.display())))?;
        let identity: PublishedTransformIdentity = serde_norway::from_str(text)?;
        let Some(body) = identity.body else {
            tracing::warn!(
                path = %file.display(),
                "published input transform has no body; keeping the existing file"
            );
            continue;
        };
        let body = looklab_schema::BodyId::new(&body)
            .map_err(|error| WorkerError::Data(error.to_string()))?;
        let version = InputTransformVersion::new(identity.version)?;
        transforms.validate(&body, version.get(), &bytes)?;
        let page = PublishedInputTransform::new(body, version, bytes, false)?;
        record_input_transform(pool, config, &page).await?;
        pages.push(page);
    }
    Ok(pages)
}

async fn site_data(pool: &SqlitePool, config: &PublishConfig) -> Result<SiteData> {
    let worker_config = config.worker_config();
    let (snapshot_version, root) = crate::body::published_snapshot(pool, &worker_config).await?;
    let resources = DataDir::new(root.clone()).bodies()?;
    let transforms = TransformDir::new(&config.transforms);
    let mut bodies = Vec::new();
    for entry in &resources {
        let preparation = if transforms.load_optional(&entry.body.id)?.is_some() {
            PreparationPage::ready()
        } else {
            PreparationPage::unavailable(PreparationBlocker::InputTransformMissing)
        };
        bodies.push(BodyPage::from_schema(
            &entry.body,
            &entry.schema,
            &BodyExtras {
                preparation,
                ..BodyExtras::default()
            },
        )?);
    }

    let mut conn = pool.acquire().await?;
    let stored_reports = model_store::reports(&mut conn).await?;
    let heads = model_store::family_heads(&mut conn).await?;
    let projections = model_store::current_projections_all(&mut conn).await?;
    let revisions = model_store::realization_revisions(&mut conn, None).await?;
    let eligibilities = model_store::target_eligibilities(&mut conn).await?;
    let accepted = model_store::accepted_family_versions(&mut conn, None).await?;
    let legacy_scopes = model_store::legacy_scopes(&mut conn).await?;
    let stored_bundles = model_store::bundle_revisions(&mut conn).await?;
    let current_bundles = model_store::current_bundles(&mut conn).await?;
    let mut report_applicable = BTreeMap::new();
    for stored in &stored_reports {
        report_applicable.insert(
            stored.id,
            model_store::report_applicable(&mut conn, &stored.id).await?,
        );
    }
    drop(conn);

    record_evidence_requests(pool, &stored_reports).await?;

    let heads_by_family: BTreeMap<String, FamilyHead> = heads
        .into_iter()
        .map(|head| (head.target.family.as_str().to_owned(), head))
        .collect();
    let eligibility_by_target: BTreeMap<String, TargetEligibilitySnapshot> = eligibilities
        .iter()
        .map(|eligibility| (eligibility.target.to_key(), eligibility.into()))
        .collect();
    let mut revisions_by_subject: BTreeMap<SubjectKey, Vec<StoredRealizationRevision>> =
        BTreeMap::new();
    for revision in revisions {
        revisions_by_subject
            .entry(
                ModelSubject::BodyRealization {
                    realization: revision.subject.clone(),
                }
                .key(),
            )
            .or_default()
            .push(revision);
    }
    let mut projections_by_subject: BTreeMap<SubjectKey, Vec<looklab_schema::CurrentProjection>> =
        BTreeMap::new();
    for projection in projections {
        projections_by_subject
            .entry(projection.subject.clone())
            .or_default()
            .push(projection);
    }
    let reports_by_id: BTreeMap<ModelReportId, &StoredReport> = stored_reports
        .iter()
        .map(|stored| (stored.id, stored))
        .collect();
    let accepted_by: BTreeMap<(String, i64), model_store::AcceptedFamilyVersion> = accepted
        .into_iter()
        .map(|accepted| {
            (
                (
                    accepted.target.family.as_str().to_owned(),
                    i64::from(accepted.target.version.get()),
                ),
                accepted,
            )
        })
        .collect();
    let legacy_versions: std::collections::BTreeSet<String> = legacy_scopes
        .into_iter()
        .filter(|scope| scope.kind == looklab_schema::LegacyScopeKind::LookVersion)
        .map(|scope| scope.target.as_str().to_owned())
        .collect();

    let rows = sqlx::query(
        "SELECT v.look, v.version, v.status, v.changelog, m.artifacts
         FROM look_versions v JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.status IN ('published', 'rooted')
         ORDER BY v.look, v.version",
    )
    .fetch_all(pool)
    .await?;

    let mut looks: BTreeMap<String, Vec<LookVersionPage>> = BTreeMap::new();
    let mut identities: BTreeMap<(String, i64), LookIdentity> = BTreeMap::new();
    for row in &rows {
        let look: String = row.get("look");
        let version: i64 = row.get("version");
        let status = db::LookVersionStatus::parse(row.get::<String, _>("status").as_str())?;
        let changelog: Option<String> = row.get("changelog");
        let artifacts: Vec<ManifestArtifact> =
            serde_json::from_str(&row.get::<String, _>("artifacts"))?;
        let blobs = load_blobs(config, &artifacts)?;
        let release = load_release(&blobs)?;
        let notes = VersionNotes {
            changelog: changelog
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default(),
            weak_spots: Vec::new(),
            notice: None,
        };
        let gate = release
            .as_ref()
            .map(|release| {
                gate_summary(
                    release.gate.as_ref(),
                    release.converged,
                    release.approximation_p95,
                )
            })
            .unwrap_or_default();
        let provenance = release
            .map(|release| release.provenance)
            .unwrap_or_default();
        let page_version = LookVersion::new(format!("{version}.0.0"))?;
        let mut page = LookVersionPage::new(
            Slug::new(&look)?,
            page_version,
            publish_status(status),
            gate,
            provenance,
            blobs,
            notes,
        )?;
        let identity = match look_shape(&page.manifest)? {
            VersionLookShape::Layered => {
                let blob = page
                    .blobs
                    .iter()
                    .find(|blob| blob.name.as_str() == LOOK_ARTIFACT)
                    .ok_or(looklab_publish::PublishError::MissingLookArtifact)?;
                let document = LookDocument::decode(&blob.bytes)?;
                page.matchable = document.is_matchable();
                page.approximated = document
                    .file
                    .definition
                    .as_ref()
                    .is_some_and(|definition| definition.declared_base.is_some());
                if page.approximated {
                    page.card_eligible = false;
                }
                LookIdentity {
                    kind: document.file.look.kind,
                    name: document
                        .file
                        .definition
                        .as_ref()
                        .map(|definition| definition.name.clone()),
                    emulation: document
                        .file
                        .definition
                        .as_ref()
                        .and_then(|definition| definition.emulates.clone()),
                    rendering: document.file.rendering.clone(),
                    license: None,
                }
            }
            VersionLookShape::SourcePreview => {
                let preview = source_preview(&page)?;
                page.matchable = false;
                page.card_eligible = false;
                LookIdentity {
                    kind: preview.look.kind,
                    name: Some(preview.name.clone()),
                    emulation: preview.emulates.clone(),
                    rendering: None,
                    license: Some(license_page(&preview.license)),
                }
            }
        };
        let kind = identity.kind;
        identities.insert((look.clone(), version), identity);
        let target_key = format!("{look}@{version}");
        let keep = match kind {
            LookKind::NativeFamily => {
                accepted_by.contains_key(&(look.clone(), version))
                    || legacy_versions.contains(&target_key)
            }
            LookKind::LutDefined => {
                eligibility_by_target
                    .get(&target_key)
                    .is_some_and(|snapshot| {
                        matches!(
                            snapshot.status,
                            looklab_schema::TargetEligibilityStatus::Eligible
                                | looklab_schema::TargetEligibilityStatus::Invalidated
                        )
                    })
                    || legacy_versions.contains(&target_key)
                    || !page.matchable
            }
        };
        if !keep {
            continue;
        }
        if let Some(accepted) = accepted_by.get(&(look.clone(), version)) {
            page.manifest.status = match accepted.status {
                model_store::AcceptedFamilyVersionStatus::Provisional => LookStatus::Provisional,
                model_store::AcceptedFamilyVersionStatus::Published => LookStatus::Published,
                model_store::AcceptedFamilyVersionStatus::Rooted => LookStatus::Rooted,
                model_store::AcceptedFamilyVersionStatus::Deprecated => LookStatus::Deprecated,
                model_store::AcceptedFamilyVersionStatus::Invalidated(_) => LookStatus::Published,
            };
            if let model_store::AcceptedFamilyVersionStatus::Invalidated(reason) = &accepted.status
            {
                page.applicability = Applicability::Invalidated { reason: *reason };
            }
        }
        looks.entry(look).or_default().push(page);
    }

    let mut metadata: BTreeMap<String, LookMetadata> = BTreeMap::new();
    for (slug, versions) in &looks {
        let head_version = heads_by_family
            .get(slug)
            .map(|head| i64::from(head.target.version.get()));
        let chosen = head_version
            .filter(|version| identities.contains_key(&(slug.clone(), *version)))
            .or_else(|| {
                versions
                    .iter()
                    .map(|page| version_number(page.version()))
                    .min()
            });
        if let Some(version) = chosen
            && let Some(identity) = identities.get(&(slug.clone(), version))
        {
            metadata.insert(
                slug.clone(),
                LookMetadata {
                    version_pin: LookVersion::new(format!("{version}.0.0"))?,
                    kind: identity.kind,
                    name: identity.name.clone(),
                    emulation: identity.emulation.clone(),
                    rendering: identity.rendering.clone(),
                    license: identity.license.clone(),
                },
            );
        }
    }

    supersede_looks(pool, &metadata).await?;
    let listings: BTreeMap<String, LookListingRecord> = admin::look_listings(pool)
        .await?
        .into_iter()
        .map(|record| (record.look.as_str().to_owned(), record))
        .collect();

    let mut look_pages = Vec::with_capacity(looks.len());
    for (slug, mut versions) in looks {
        let head = heads_by_family.get(&slug);
        for page in &mut versions {
            let number = version_number(page.version());
            if let Some(snapshot) = eligibility_by_target.get(&format!("{slug}@{number}")) {
                page.eligibility = Some(snapshot.id);
                page.eligibility_status = Some(snapshot.status);
                page.applicability = match snapshot.status {
                    TargetEligibilityStatus::Invalidated => Applicability::Invalidated {
                        reason: InvalidationReason::EvidenceLoss,
                    },
                    TargetEligibilityStatus::Eligible | TargetEligibilityStatus::Ineligible => {
                        Applicability::Current
                    }
                };
            }
            if let Some(head) = head
                && head.target.version.get() as i64 == number
            {
                page.canonical = true;
                page.applicability = head.applicability.clone();
            }
        }
        let head_page = match head {
            Some(head) if versions.iter().any(|page| page.canonical) => {
                Some(FamilyHeadPage::new(head.target.clone(), head)?)
            }
            _ => None,
        };
        let record = listings.get(&slug);
        let kind = metadata
            .get(&slug)
            .map(|meta| meta.kind)
            .unwrap_or(LookKind::NativeFamily);
        let selected = match kind {
            LookKind::NativeFamily => head_page.as_ref().map(|head| SelectedTargetSummary {
                version: head.version.clone(),
                eligibility: None,
                applicability: head.applicability.clone(),
            }),
            LookKind::LutDefined => versions
                .iter()
                .filter(|page| {
                    page.matchable
                        && page.eligibility_status == Some(TargetEligibilityStatus::Eligible)
                })
                .max_by_key(|page| version_number(page.version()))
                .map(|page| SelectedTargetSummary {
                    version: page.version().clone(),
                    eligibility: page.eligibility,
                    applicability: Applicability::Current,
                }),
        };
        let name = metadata
            .get(&slug)
            .and_then(|meta| meta.name.clone())
            .unwrap_or_else(|| slug.clone());
        look_pages.push(LookPage {
            slug: Slug::new(&slug)?,
            name,
            kind,
            wb_policy: WbPolicy::Adaptive,
            description: String::new(),
            aliases: Vec::new(),
            license: metadata.get(&slug).and_then(|meta| meta.license.clone()),
            listing: match record.map(|record| record.listing) {
                Some(ListingState::Hidden) => LookListing::Hidden,
                Some(ListingState::Removed) => LookListing::Removed,
                Some(ListingState::Listed) | None => LookListing::Listed,
            },
            superseded_by: record.and_then(|record| record.superseded_by.clone()),
            head: head_page,
            selected,
            versions,
        });
    }

    let mut published_status: BTreeMap<(String, i64), LookStatus> = BTreeMap::new();
    for look in &look_pages {
        for version in &look.versions {
            published_status.insert(
                (
                    look.slug.as_str().to_owned(),
                    version_number(version.version()),
                ),
                version.manifest.status,
            );
        }
    }
    for body in &mut bodies {
        for card in &mut body.native_looks {
            let subject = ModelSubject::BodyRealization {
                realization: BodyRealizationId {
                    body: body.id.clone(),
                    epoch: ControlEpochId::new(card.epoch),
                    native_sim: card.sim_key.clone(),
                },
            }
            .key();
            let revisions = revisions_by_subject
                .get(&subject)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let subject_projections = projections_by_subject
                .get(&subject)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let Some((revision, scope)) =
                choose_card(subject_projections, revisions, &eligibility_by_target)
            else {
                card.scope = SelectionScope::Unassessed;
                continue;
            };
            let number = i64::from(revision.target.version.get());
            let published = published_status
                .get(&(card.look.as_str().to_owned(), number))
                .copied();
            card.version = match published {
                Some(_) => Some(LookVersion::new(format!("{number}.0.0"))?),
                None => None,
            };
            card.status = published;
            card.scope = if published.is_some() {
                scope
            } else {
                SelectionScope::Unassessed
            };
            card.blockers = blockers(&reports_by_id, revision);
            if published.is_none() {
                card.blockers
                    .push("target version is not published".to_owned());
            }
            card.realization = Some(RealizationCard {
                subject: revision.subject.clone(),
                revision: revision.id.clone(),
                version: revision.version,
                target: revision.target.clone(),
                status: revision.status,
                safety: revision.safety,
                scope: card.scope,
                blockers: card.blockers.clone(),
            });
        }
        body.bundles = current_bundles
            .iter()
            .filter(|bundle| bundle.body == body.id)
            .map(|bundle| BundleSelection {
                body: bundle.body.clone(),
                epoch: bundle.epoch,
                use_: bundle.use_,
                bundle: bundle.bundle,
                scope: match bundle.use_ {
                    ProjectionUse::QualifiedMatch => SelectionScope::Qualified,
                    ProjectionUse::ExperimentalMatch | ProjectionUse::Preview => {
                        SelectionScope::Experimental
                    }
                },
                applicability: bundle.applicability.clone(),
            })
            .collect();
    }

    let mut current_pages = Vec::with_capacity(projections_by_subject.len());
    for (subject_key, subject_projections) in projections_by_subject {
        let subject = subject_key
            .parse()
            .map_err(|error: looklab_schema::SubjectKeyError| {
                WorkerError::Data(error.to_string())
            })?;
        let page_projections = subject_projections
            .iter()
            .map(|projection| PublishedCurrentProjection {
                use_: projection.use_,
                capability: projection.capability,
                scope_key: projection.scope_key.clone(),
                selected_report: projection.selected_report,
                selected_candidate: projection.selected_candidate.clone(),
                selected_eligibility: projection.selected_candidate.as_ref().and_then(
                    |candidate| match candidate {
                        ModelVersionId::SharedFamily { target } => eligibility_by_target
                            .get(&target.to_key())
                            .map(|eligibility| eligibility.id),
                        ModelVersionId::BodyRealization { .. } => None,
                    },
                ),
                pending_generation: projection.pending_generation,
                applicability: projection.applicability.clone(),
            })
            .collect();
        let bundles = current_bundles
            .iter()
            .filter(|bundle| match &subject {
                ModelSubject::BodyRealization { realization } => {
                    bundle.body == realization.body && bundle.epoch == realization.epoch.get()
                }
                ModelSubject::SharedFamily { .. } => false,
            })
            .map(|bundle| BundleSelection {
                body: bundle.body.clone(),
                epoch: bundle.epoch,
                use_: bundle.use_,
                bundle: bundle.bundle,
                scope: match bundle.use_ {
                    ProjectionUse::QualifiedMatch => SelectionScope::Qualified,
                    ProjectionUse::ExperimentalMatch | ProjectionUse::Preview => {
                        SelectionScope::Experimental
                    }
                },
                applicability: bundle.applicability.clone(),
            })
            .collect();
        current_pages.push(SubjectCurrentPage::new(subject, page_projections, bundles));
    }

    let mut bundle_pages = Vec::with_capacity(stored_bundles.len());
    for stored in &stored_bundles {
        if let Some(page) = bundle_page(config, stored)? {
            bundle_pages.push(page);
        }
    }

    let reports: Vec<SiteReport> = stored_reports
        .iter()
        .map(|stored| SiteReport {
            summary: PublishedReportSummary::new(
                &stored.report,
                stored.id,
                stored.artifacts.clone(),
            ),
            applicability: if report_applicable.get(&stored.id).copied().unwrap_or(false) {
                Applicability::Current
            } else {
                Applicability::Invalidated {
                    reason: InvalidationReason::EvidenceLoss,
                }
            },
        })
        .collect();
    let eligibility: Vec<TargetEligibilitySnapshot> =
        eligibilities.iter().map(Into::into).collect();

    let manuals = load_manuals(&config.manuals, &bodies, pool).await?;
    let interfaces = interface_profiles(pool, config, &worker_config, &config.data).await?;
    let input_transforms = input_transforms(pool, config, &transforms, &resources).await?;

    Ok(SiteData {
        community_snapshot_version: snapshot_version,
        bodies,
        manuals,
        interfaces,
        input_transforms,
        looks: look_pages,
        audit_log: audit_log(pool).await?,
        scenes: canonical_scenes(config)?,
        reports,
        eligibility,
        current: current_pages,
        bundles: bundle_pages,
        reference_preview: None,
    })
}

struct LookIdentity {
    kind: LookKind,
    name: Option<String>,
    emulation: Option<LutEmulation>,
    rendering: Option<CameraRendering>,
    license: Option<LicensePage>,
}

fn source_preview(page: &LookVersionPage) -> Result<SourceLutPreview> {
    let descriptor = page
        .blobs
        .iter()
        .find(|blob| blob.name.as_str() == SOURCE_LUT_ARTIFACT)
        .ok_or(looklab_publish::PublishError::MissingLookArtifact)?;
    let preview = SourceLutPreview::decode(&descriptor.bytes)?;
    preview.verify_artifacts(&page.manifest)?;
    Ok(preview)
}

fn license_page(license: &LicenseRecord) -> LicensePage {
    LicensePage {
        declaration: license
            .source_license
            .clone()
            .unwrap_or_else(|| looklab_schema::DATA_LICENSE.to_owned()),
        attribution: license.attribution.clone(),
    }
}

fn version_number(version: &LookVersion) -> i64 {
    version
        .as_str()
        .split('.')
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(0)
}

fn choose_card<'a>(
    projections: &[looklab_schema::CurrentProjection],
    revisions: &'a [StoredRealizationRevision],
    eligibility: &BTreeMap<String, TargetEligibilitySnapshot>,
) -> Option<(&'a StoredRealizationRevision, SelectionScope)> {
    let qualified = projections.iter().find_map(|projection| {
        if projection.use_ != ProjectionUse::QualifiedMatch
            || projection.capability != QualificationCapability::ControlsMatching
            || !projection.applicability.is_current()
        {
            return None;
        }
        let ModelVersionId::BodyRealization { realization } =
            projection.selected_candidate.as_ref()?
        else {
            return None;
        };
        revisions.iter().find(|revision| {
            revision.status == RealizationStatus::Candidate
                && revision.safety.accepted()
                && RealizationRevisionId::for_version(realization) == revision.id
        })
    });
    if let Some(revision) = qualified {
        return Some((revision, SelectionScope::Qualified));
    }
    let experimental = revisions
        .iter()
        .filter(|revision| {
            revision.status == RealizationStatus::Candidate
                && revision.safety.accepted()
                && eligibility
                    .get(&revision.target.to_key())
                    .is_some_and(|snapshot| snapshot.status == TargetEligibilityStatus::Eligible)
        })
        .max_by_key(|revision| revision.version);
    if let Some(revision) = experimental {
        return Some((revision, SelectionScope::Experimental));
    }
    None
}

fn blockers(
    reports: &BTreeMap<ModelReportId, &StoredReport>,
    revision: &StoredRealizationRevision,
) -> Vec<String> {
    let candidate = ModelVersionId::BodyRealization {
        realization: BodyRealizationVersionId {
            realization: revision.subject.clone(),
            version: revision.version,
            target: revision.target.clone(),
        },
    };
    let mut blockers = Vec::new();
    if let Some(report) = reports.values().rev().find(|stored| {
        stored.report.candidate == candidate
            && stored.report.capability == QualificationCapability::NativePrediction
    }) {
        for request in &report.report.evidence_requests {
            blockers.push(request.detail.clone());
        }
    }
    blockers.sort();
    blockers.dedup();
    blockers
}

fn bundle_page(
    config: &PublishConfig,
    stored: &StoredBundleRevision,
) -> Result<Option<PublishedBundleRevisionPage>> {
    let manifest = stored.revision.clone();
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for binding in &stored.artifacts {
        let hash = binding.sha256.to_hex();
        let name = match binding.role {
            ArtifactRole::Manifest => "body.yaml".to_owned(),
            ArtifactRole::InputTransform => "transform.yaml".to_owned(),
            ArtifactRole::CalibrationProfile => "profile.yaml".to_owned(),
            ArtifactRole::BodyRealizationParams => "body_params.json".to_owned(),
            ArtifactRole::Gauge => "gauge.json".to_owned(),
            _ => hash.clone(),
        };
        blobs.insert(name, read_blob_hash(config, &hash)?);
    }
    let blobs = blobs
        .into_iter()
        .map(|(name, bytes)| Ok(ArtifactBlob::new(ArtifactName::new(name)?, bytes)))
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(PublishedBundleRevisionPage::new(manifest, blobs)?))
}

fn read_blob_hash(config: &PublishConfig, hash: &str) -> Result<Vec<u8>> {
    let path = config.blobs.join(hash);
    let bytes = std::fs::read(&path)?;
    let digest = hex(&Sha256::digest(&bytes));
    if digest != hash {
        return Err(WorkerError::Data(format!(
            "{}: bytes hash to {digest}, the bundle records {hash}",
            path.display()
        )));
    }
    Ok(bytes)
}

async fn record_evidence_requests(pool: &SqlitePool, reports: &[StoredReport]) -> Result<()> {
    let now = now_unix();
    let mut conn = pool.acquire().await?;
    for stored in reports {
        let subject = stored.report.subject.key();
        if stored.report.evidence_requests.is_empty() {
            model_store::prune_evidence_requests(&mut conn, &stored.id).await?;
            continue;
        }
        for request in &stored.report.evidence_requests {
            model_store::record_evidence_request(
                &mut conn,
                &stored.id,
                &subject,
                stored.report.capability,
                request,
                now,
            )
            .await?;
        }
    }
    Ok(())
}
async fn load_manuals(
    path: &Path,
    bodies: &[BodyPage],
    pool: &sqlx::SqlitePool,
) -> Result<Vec<ManualPage>> {
    let (catalog, digest) = if path.is_dir() {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file = entry.path();
            if file.extension().and_then(|value| value.to_str()) != Some("yaml") {
                continue;
            }
            files.push((
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read_to_string(&file)?,
            ));
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        let mut catalogs = Vec::with_capacity(files.len());
        for (name, source) in &files {
            let catalog = looklab_schema::validate_manual_catalog(source)
                .map_err(|errors| WorkerError::Data(format!("{name}: {errors:?}")))?;
            catalogs.push(catalog);
        }
        let catalog = looklab_schema::merge_manual_catalogs(catalogs)
            .map_err(|errors| WorkerError::Data(format!("{path:?}: {errors:?}")))?;
        (catalog, looklab_schema::manual_catalog_files_digest(&files))
    } else if path.is_file() {
        let source = std::fs::read_to_string(path)?;
        let catalog = looklab_schema::validate_manual_catalog(&source)
            .map_err(|errors| WorkerError::Data(format!("{path:?}: {errors:?}")))?;
        (catalog, looklab_schema::manual_catalog_digest(&source))
    } else {
        return Ok(Vec::new());
    };
    let preparations = bodies
        .iter()
        .map(|body| (body.id.clone(), body.preparation.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut pages = Vec::with_capacity(catalog.cameras.len());
    for camera in &catalog.cameras {
        let summary = looklab_server::manual_summary(pool, camera).await?;
        let preparation = preparations.get(&camera.id).cloned().unwrap_or_default();
        pages.push(ManualPage::from_camera(
            camera,
            catalog.catalog_version,
            digest,
            preparation,
            summary,
        ));
    }
    Ok(pages)
}

const SCENE_SIZE: u32 = 128;

/// Photo scenes prepared under `<data>/scenes`, then the test charts.
fn canonical_scenes(config: &PublishConfig) -> Result<Vec<Scene>> {
    let sources = config.data.join("scenes");
    let mut scenes = Vec::new();
    if sources.join(SCENE_SOURCE_INDEX).is_file() {
        scenes.extend(read_scene_sources(&sources)?);
    } else {
        tracing::warn!(
            path = %sources.display(),
            "no prepared photo scenes; publishing test charts only"
        );
    }
    scenes.extend(test_chart_scenes(SCENE_SIZE)?);
    Ok(scenes)
}

fn load_blobs(config: &PublishConfig, artifacts: &[ManifestArtifact]) -> Result<Vec<ArtifactBlob>> {
    artifacts
        .iter()
        .map(|artifact| {
            let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
            let digest = hex(&Sha256::digest(&bytes));
            if digest != artifact.sha256 {
                return Err(WorkerError::Data(format!(
                    "{}: bytes hash to {digest}, manifest records {}",
                    artifact.name, artifact.sha256
                )));
            }
            Ok(ArtifactBlob::new(ArtifactName::new(&artifact.name)?, bytes))
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    db::hex(bytes)
}

fn load_release(blobs: &[ArtifactBlob]) -> Result<Option<ReleaseRecord>> {
    let Some(blob) = blobs
        .iter()
        .find(|blob| blob.name.as_str() == RELEASE_ARTIFACT)
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&blob.bytes)?))
}

fn publish_status(status: LookVersionStatus) -> LookStatus {
    match status {
        LookVersionStatus::Provisional => LookStatus::Provisional,
        LookVersionStatus::Rooted => LookStatus::Rooted,
        LookVersionStatus::Published => LookStatus::Published,
        LookVersionStatus::Deprecated => LookStatus::Deprecated,
    }
}

async fn audit_log(pool: &SqlitePool) -> Result<AuditLog> {
    let rows = sqlx::query("SELECT seq, type, payload, ts FROM audit_log ORDER BY seq")
        .fetch_all(pool)
        .await?;
    let mut entries = Vec::new();
    for row in &rows {
        let kind: String = row.get("type");
        let payload: serde_json::Value = serde_json::from_str(&row.get::<String, _>("payload"))?;
        let ts: i64 = row.get("ts");
        let Some(event) = audit_event(pool, &kind, &payload, ts).await? else {
            continue;
        };
        let seq: i64 = row.get("seq");
        entries.push(AuditEntry {
            seq: u64::try_from(seq).unwrap_or(0),
            ts: Timestamp::new(rfc3339(ts)).map_err(|error| WorkerError::Data(error.to_owned()))?,
            event,
        });
    }
    Ok(AuditLog::new(entries))
}

fn text<'a>(payload: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(|value| value.as_str())
}

fn number(payload: &serde_json::Value, key: &str) -> Option<u64> {
    payload.get(key).and_then(|value| value.as_u64())
}

fn reference(value: &str) -> Result<LookRef> {
    LookRef::new(value).map_err(|error| WorkerError::Data(error.to_owned()))
}

fn version(value: u64) -> Result<VersionRef> {
    VersionRef::new(value.to_string()).map_err(|error| WorkerError::Data(error.to_owned()))
}

fn role(value: &str) -> Option<Role> {
    match value {
        "seeder" => Some(Role::Seeder),
        "moderator" => Some(Role::Moderator),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

fn decision(value: &str) -> Option<Decision> {
    match value {
        "approved" => Some(Decision::Approve),
        "rejected" => Some(Decision::Reject),
        "request_info" => Some(Decision::RequestInfo),
        _ => None,
    }
}

fn credential_ref(value: &str) -> Result<looklab_publish::CredentialRef> {
    looklab_publish::CredentialRef::new(value).map_err(|error| WorkerError::Data(error.to_owned()))
}

fn contribution_ref(value: &str) -> Result<looklab_publish::ContributionRef> {
    looklab_publish::ContributionRef::new(value)
        .map_err(|error| WorkerError::Data(error.to_owned()))
}

fn body_id(value: &str) -> Result<BodyId> {
    BodyId::new(value).map_err(|error| WorkerError::Data(error.to_string()))
}

fn capability_key(value: &str) -> Result<CapabilityKey> {
    value
        .parse::<CapabilityKey>()
        .map_err(|()| WorkerError::Data(format!("unknown documented control `{value}`")))
}

fn confirmation_basis(value: &serde_json::Value) -> Option<ConfirmationBasis> {
    serde_json::from_value(value.clone()).ok()
}

/// The public projection of a manual audit row: display fields only, never the
/// moderator's internal credential id or a control definition digest.
fn manual_audit_event(kind: &str, payload: &serde_json::Value) -> Result<Option<AuditEvent>> {
    let event = match kind {
        "manual_confirmation" => {
            let (Some(camera), Some(firmware), Some(controls)) = (
                text(payload, "camera"),
                text(payload, "firmware"),
                number(payload, "controls"),
            ) else {
                return Ok(None);
            };
            AuditEvent::ManualConfirmation {
                camera: body_id(camera)?,
                firmware: firmware.to_owned(),
                controls: usize::try_from(controls).map_err(|_| {
                    WorkerError::Data("manual confirmation control count does not fit".to_owned())
                })?,
            }
        }
        "manual_confirmation_promotion" => {
            let (Some(camera), Some(control), Some(firmware)) = (
                text(payload, "camera"),
                text(payload, "control"),
                text(payload, "firmware"),
            ) else {
                return Ok(None);
            };
            AuditEvent::ManualConfirmationPromotion {
                camera: body_id(camera)?,
                control: capability_key(control)?,
                firmware: firmware.to_owned(),
                groups: u32::try_from(number(payload, "groups").unwrap_or(0)).unwrap_or(u32::MAX),
                basis: payload.get("basis").and_then(confirmation_basis),
            }
        }
        "manual_confirmation_withdrawal" => {
            let (Some(camera), Some(withdrawn)) =
                (text(payload, "camera"), number(payload, "withdrawn"))
            else {
                return Ok(None);
            };
            AuditEvent::ManualConfirmationWithdrawal {
                camera: body_id(camera)?,
                withdrawn,
            }
        }
        "manual_moderator_confirmation" => {
            let (Some(camera), Some(control), Some(decision)) = (
                text(payload, "camera"),
                text(payload, "control"),
                text(payload, "decision"),
            ) else {
                return Ok(None);
            };
            let confirmed = match decision {
                "confirm" => true,
                "reject" => false,
                _ => return Ok(None),
            };
            let Some(scope) = payload.get("firmware_scope") else {
                return Ok(None);
            };
            let firmware_scope = serde_json::from_value::<ModeratorFirmwareScope>(scope.clone())
                .map_err(|error| {
                    WorkerError::Data(format!("manual moderator firmware scope: {error}"))
                })?;
            let evidence = text(payload, "evidence_kind")
                .and_then(|value| value.parse::<ModeratorEvidenceKind>().ok())
                .ok_or_else(|| {
                    WorkerError::Data("manual moderator evidence kind is missing".to_owned())
                })?;
            let evidence_url = text(payload, "evidence_url")
                .map(ManualUrl::new)
                .transpose()
                .map_err(|error| WorkerError::Data(error.to_string()))?;
            AuditEvent::ManualModeratorConfirmation {
                camera: body_id(camera)?,
                control: capability_key(control)?,
                confirmed,
                firmware_scope,
                evidence,
                evidence_url,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

async fn audit_event(
    pool: &SqlitePool,
    kind: &str,
    payload: &serde_json::Value,
    ts: i64,
) -> Result<Option<AuditEvent>> {
    if let Some(event) = manual_audit_event(kind, payload)? {
        return Ok(Some(event));
    }
    let event = match kind {
        "grant_created" | "grant_revoked" => {
            let (Some(actor), Some(subject), Some(role)) = (
                text(payload, "actor"),
                text(payload, "subject"),
                text(payload, "grant").and_then(role),
            ) else {
                return Ok(None);
            };
            let reason = text(payload, "reason").unwrap_or_default().to_owned();
            if kind == "grant_created" {
                AuditEvent::Grant {
                    subject: credential_ref(subject)?,
                    role,
                    granted_by: credential_ref(actor)?,
                    reason,
                }
            } else {
                AuditEvent::Revocation {
                    subject: credential_ref(subject)?,
                    role,
                    revoked_by: credential_ref(actor)?,
                    reason,
                }
            }
        }
        "endorsement_issued" => {
            let (Some(actor), Some(subject)) = (text(payload, "actor"), text(payload, "subject"))
            else {
                return Ok(None);
            };
            let reason = payload
                .get("endorsement")
                .and_then(|endorsement| endorsement.get("reason"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned();
            AuditEvent::Endorsement {
                issuer: credential_ref(actor)?,
                subject: credential_ref(subject)?,
                reason,
            }
        }
        "endorsement_revoked" => {
            let (Some(actor), Some(subject)) = (text(payload, "actor"), text(payload, "subject"))
            else {
                return Ok(None);
            };
            AuditEvent::Revocation {
                subject: credential_ref(subject)?,
                role: Role::Seeder,
                revoked_by: credential_ref(actor)?,
                reason: String::new(),
            }
        }
        "root_admitted" => {
            let (Some(actor), Some(subject)) = (text(payload, "actor"), text(payload, "subject"))
            else {
                return Ok(None);
            };
            let Some(contribution) = root_contribution(pool, actor, subject, ts).await? else {
                return Ok(None);
            };
            AuditEvent::RootAdmission {
                admitted_by: credential_ref(actor)?,
                contribution: looklab_publish::ContributionRef::new(contribution)
                    .map_err(|error| WorkerError::Data(error.to_owned()))?,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "moderation_decision" => {
            let (Some(actor), Some(proposal), Some(outcome)) = (
                text(payload, "actor"),
                text(payload, "proposal"),
                text(payload, "outcome").and_then(decision),
            ) else {
                return Ok(None);
            };
            AuditEvent::ModerationDecision {
                proposal: ProposalRef::new(proposal)
                    .map_err(|error| WorkerError::Data(error.to_owned()))?,
                moderator: credential_ref(actor)?,
                decision: outcome,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "proposal_expired" => {
            let (Some(proposal), Some(reason)) =
                (text(payload, "proposal"), text(payload, "reason"))
            else {
                return Ok(None);
            };
            AuditEvent::ProposalExpired {
                proposal: ProposalRef::new(proposal)
                    .map_err(|error| WorkerError::Data(error.to_owned()))?,
                reason: reason.to_owned(),
            }
        }
        "proposal_legacy_rejected" => {
            let (Some(proposal), Some(reason)) =
                (text(payload, "proposal"), text(payload, "reason"))
            else {
                return Ok(None);
            };
            AuditEvent::ProposalLegacyRejected {
                proposal: ProposalRef::new(proposal)
                    .map_err(|error| WorkerError::Data(error.to_owned()))?,
                reason: reason.to_owned(),
            }
        }
        "re_anchor" => {
            let (Some(look), Some(released)) = (text(payload, "look"), number(payload, "version"))
            else {
                return Ok(None);
            };
            AuditEvent::ReAnchor {
                look: reference(look)?,
                version: version(released)?,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "revert" => {
            let (Some(look), Some(plan)) = (text(payload, "look"), payload.get("plan")) else {
                return Ok(None);
            };
            let Some(target) = number(plan, "target_version") else {
                return Ok(None);
            };
            let rejected = plan
                .get("rejected_contributions")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str())
                        .map(contribution_ref)
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            let legacy_excluded = plan
                .get("refit_excluding")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str())
                        .map(credential_ref)
                        .collect::<Result<Vec<_>>>()
                })
                .transpose()?
                .unwrap_or_default();
            let held = plan
                .get("restoration")
                .is_some_and(|value| value.as_str() != Some("current"));
            AuditEvent::Revert {
                look: reference(look)?,
                to_version: version(target)?,
                rejected,
                legacy_excluded,
                held,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "split" => {
            let Some(look) = text(payload, "look") else {
                return Ok(None);
            };
            let from = number(payload, "from")
                .map(|value| value.to_string())
                .unwrap_or_default();
            let into: Vec<String> = payload
                .get("into")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            AuditEvent::Split {
                kind: SplitKind::Look,
                detail: format!(
                    "{look} from {from} into {}: {}",
                    into.join(", "),
                    text(payload, "reason").unwrap_or_default()
                ),
            }
        }
        "look_hidden" | "look_removed" | "look_relisted" => {
            let (Some(actor), Some(look)) = (text(payload, "actor"), text(payload, "look")) else {
                return Ok(None);
            };
            let state = match kind {
                "look_hidden" => LookListing::Hidden,
                "look_removed" => LookListing::Removed,
                _ => LookListing::Listed,
            };
            AuditEvent::LookListing {
                look: reference(look)?,
                state,
                moderator: credential_ref(actor)?,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "credential_intake_blocked" => {
            let (Some(actor), Some(subject)) = (text(payload, "actor"), text(payload, "subject"))
            else {
                return Ok(None);
            };
            AuditEvent::CredentialIntakeBlocked {
                moderator: credential_ref(actor)?,
                subject: credential_ref(subject)?,
                reason: text(payload, "reason").unwrap_or_default().to_owned(),
            }
        }
        "release_manifest" => {
            let (Some(look), Some(released), Some(sha256)) = (
                text(payload, "look"),
                number(payload, "version"),
                text(payload, "manifest_sha256"),
            ) else {
                return Ok(None);
            };
            AuditEvent::Release {
                look: reference(look)?,
                version: version(released)?,
                manifest_sha256: ArtifactHash::parse(sha256)?,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(event))
}

/// The admission row a `root_admitted` payload refers to; the payload carries the
/// seeder, the contributing credential and the time, not the contribution id.
async fn root_contribution(
    pool: &SqlitePool,
    admitted_by: &str,
    subject: &str,
    ts: i64,
) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT a.contribution_id FROM root_admissions a
         JOIN contributions c ON c.id = a.contribution_id
         WHERE a.admitted_by = ? AND c.credential_id = ? AND a.admitted_at <= ?
           AND c.render_class = 'colour'
         ORDER BY a.admitted_at DESC, a.id DESC LIMIT 1",
    )
    .bind(admitted_by)
    .bind(subject)
    .bind(ts)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODERATOR: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const DIGEST: &str = "aa2c1f4e6d8b0a3c5e7f90112233445566778899aabbccddeeff001122334455";

    #[test]
    fn manual_audit_rows_publish_display_fields_only() {
        let payload = serde_json::json!({
            "camera": "fujifilm/x-t5",
            "control": "native_simulations",
            "definition_digest": DIGEST,
            "decision": "confirm",
            "firmware_scope": {"kind": "exact", "firmware": "1.00"},
            "moderator": MODERATOR,
            "evidence_kind": "video",
            "evidence_url": "https://example.com/manual",
        });
        let event = manual_audit_event("manual_moderator_confirmation", &payload)
            .expect("valid row")
            .expect("mapped event");
        assert_eq!(event.kind(), "manual_moderator_confirmation");
        let published = serde_json::to_string(&event).expect("serialize");
        assert!(!published.contains(MODERATOR), "{published}");
        assert!(!published.contains(DIGEST), "{published}");
        assert!(published.contains("fujifilm/x-t5"), "{published}");
        assert!(published.contains("native_simulations"), "{published}");
        assert!(published.contains("1.00"), "{published}");
    }

    #[test]
    fn every_manual_audit_kind_has_a_public_event() {
        let rows = [
            (
                "manual_confirmation",
                serde_json::json!({
                    "confirmation": 3,
                    "camera": "fujifilm/x-t5",
                    "firmware": "1.00",
                    "controls": 4,
                }),
            ),
            (
                "manual_confirmation_promotion",
                serde_json::json!({
                    "camera": "fujifilm/x-t5",
                    "control": "native_simulations",
                    "groups": 3,
                    "firmware": "1.00",
                    "basis": "corrected",
                    "confirmed_actual": {"definition_digest": DIGEST},
                }),
            ),
            (
                "manual_confirmation_withdrawal",
                serde_json::json!({
                    "camera": "fujifilm/x-t5",
                    "withdrawn": 2,
                }),
            ),
        ];
        for (kind, payload) in rows {
            let event = manual_audit_event(kind, &payload)
                .expect("valid row")
                .expect("mapped event");
            assert_eq!(event.kind(), kind);
            let published = serde_json::to_string(&event).expect("serialize");
            assert!(!published.contains(DIGEST), "{published}");
        }
        assert!(
            manual_audit_event("grant_created", &serde_json::json!({}))
                .expect("not a manual row")
                .is_none()
        );
    }
}
