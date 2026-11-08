// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use looklab_model::file::{
    CameraRendering, LookKind, LookPin, LookVersion, LutEmulation, Slug, WbPolicy,
};
use looklab_model::{LookDocument, emulation_matches_camera_rendering};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, ConformanceEntry, Decision, LookListing, LookPage, LookRef, LookStatus,
    LookVersionPage, ProposalRef, Role, SCENE_SOURCE_INDEX, Scene, SiteData, SitePath, SiteSink,
    SplitKind, Timestamp, VersionNotes, VersionRef, compare_versions, read_scene_sources,
    test_chart_scenes, write_conformance_pack, write_site,
};
use looklab_server::admin::{self, ListingState, LookListingRecord, ManifestArtifact};
use looklab_upload::LookSlug;
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};

use crate::body::DataDir;
use crate::clock::rfc3339;
use crate::config::WorkerConfig;
use crate::db::{self, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::refit::{LOOK_ARTIFACT, RELEASE_ARTIFACT, ReleaseRecord, gate_summary};

#[derive(Debug, Clone)]
pub struct PublishConfig {
    pub db: PathBuf,
    pub blobs: PathBuf,
    pub data: PathBuf,
    /// Approved community snapshots; the newest `v<version>` directory wins.
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
    pub fn from_worker(config: &WorkerConfig) -> Self {
        Self {
            db: config.db.clone(),
            blobs: config.blobs.clone(),
            data: config.data.clone(),
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
    Ok(())
}

struct LookMetadata {
    version: i64,
    version_pin: LookVersion,
    kind: LookKind,
    name: Option<String>,
    emulation: Option<LutEmulation>,
    rendering: Option<CameraRendering>,
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

async fn site_data(pool: &SqlitePool, config: &PublishConfig) -> Result<SiteData> {
    let (snapshot_version, root) =
        crate::body::current_snapshot(&config.snapshot_dir, &config.data)?;
    let resources = DataDir::new(root).bodies()?;
    let mut bodies = Vec::new();
    for entry in &resources {
        bodies.push(BodyPage::from_schema(
            &entry.body,
            &entry.schema,
            &BodyExtras::default(),
        )?);
    }

    let rows = sqlx::query(
        "SELECT v.look, v.version, v.status, v.changelog, m.artifacts
         FROM look_versions v JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.status IN ('published', 'rooted')
         ORDER BY v.look, v.version",
    )
    .fetch_all(pool)
    .await?;

    let mut looks: BTreeMap<String, Vec<LookVersionPage>> = BTreeMap::new();
    let mut metadata: BTreeMap<String, LookMetadata> = BTreeMap::new();
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
            .map(|release| gate_summary(&release.gate, release.converged))
            .unwrap_or_default();
        let provenance = release
            .map(|release| release.provenance)
            .unwrap_or_default();
        let page_version = LookVersion::new(format!("{version}.0.0"))?;
        let mut page = LookVersionPage::new(
            Slug::new(&look)?,
            page_version.clone(),
            publish_status(status),
            gate,
            provenance,
            blobs,
            notes,
        )?;
        if let Some(blob) = page
            .blobs
            .iter()
            .find(|blob| blob.name.as_str() == LOOK_ARTIFACT)
        {
            let document = LookDocument::decode(&blob.bytes)?;
            page.matchable = document.is_matchable();
            if metadata
                .get(&look)
                .is_none_or(|known| known.version < version)
            {
                metadata.insert(
                    look.clone(),
                    LookMetadata {
                        version,
                        version_pin: page_version,
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
                    },
                );
            }
        }
        looks.entry(look).or_default().push(page);
    }

    supersede_looks(pool, &metadata).await?;
    let listings: BTreeMap<String, LookListingRecord> = admin::look_listings(pool)
        .await?
        .into_iter()
        .map(|record| (record.look.as_str().to_owned(), record))
        .collect();

    let looks = looks
        .into_iter()
        .map(|(slug, versions)| {
            let record = listings.get(&slug);
            let kind = metadata
                .get(&slug)
                .map(|meta| meta.kind)
                .unwrap_or(LookKind::NativeFamily);
            let name = metadata
                .get(&slug)
                .and_then(|meta| meta.name.clone())
                .unwrap_or_else(|| slug.clone());
            Ok(LookPage {
                slug: Slug::new(&slug)?,
                name,
                kind,
                wb_policy: WbPolicy::Adaptive,
                description: String::new(),
                aliases: Vec::new(),
                license: None,
                listing: match record.map(|record| record.listing) {
                    Some(ListingState::Hidden) => LookListing::Hidden,
                    Some(ListingState::Removed) => LookListing::Removed,
                    Some(ListingState::Listed) | None => LookListing::Listed,
                },
                superseded_by: record.and_then(|record| record.superseded_by.clone()),
                versions,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SiteData {
        community_snapshot_version: snapshot_version,
        bodies,
        looks,
        audit_log: audit_log(pool).await?,
        scenes: canonical_scenes(config)?,
        reference_preview: None,
    })
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

async fn audit_event(
    pool: &SqlitePool,
    kind: &str,
    payload: &serde_json::Value,
    ts: i64,
) -> Result<Option<AuditEvent>> {
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
            let excluded = plan
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
            AuditEvent::Revert {
                look: reference(look)?,
                to_version: version(target)?,
                excluded,
                reason: String::new(),
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
