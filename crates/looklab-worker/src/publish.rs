// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, LookPage, LookRef, LookStatus, LookVersionPage, SiteData, SitePath, SiteSink,
    Timestamp, VersionNotes, VersionRef, write_site,
};
use looklab_server::admin::ManifestArtifact;
use sqlx::{Row, SqlitePool};

use crate::body::DataDir;
use crate::clock::rfc3339;
use crate::config::WorkerConfig;
use crate::db::{self, LookVersionStatus};
use crate::error::{Result, WorkerError};
use crate::refit::{RELEASE_ARTIFACT, ReleaseRecord, gate_summary};

#[derive(Debug, Clone)]
pub struct PublishConfig {
    pub db: PathBuf,
    pub blobs: PathBuf,
    pub data: PathBuf,
    pub out: PathBuf,
}

impl PublishConfig {
    pub fn from_worker(config: &WorkerConfig) -> Self {
        Self {
            db: config.db.clone(),
            blobs: config.blobs.clone(),
            data: config.data.clone(),
            out: config.site.clone(),
        }
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
    let site = site_data(pool, config).await?;
    let mut sink = DirSink::new(&config.out);
    let root = write_site(&mut sink, &site)?;
    write_site_root(&config.out, root.as_str())?;
    Ok(PublishSummary {
        root: root.as_str().to_owned(),
        files: sink.files,
    })
}

pub fn write_site_root(out: &Path, root: &str) -> Result<PathBuf> {
    let parent = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => out,
    };
    let path = parent.join("site-root.txt");
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

async fn site_data(pool: &SqlitePool, config: &PublishConfig) -> Result<SiteData> {
    let mut bodies = Vec::new();
    for resources in DataDir::new(&config.data).bodies()? {
        bodies.push(BodyPage::from_schema(
            &resources.body,
            &resources.schema,
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
            .map(|release| gate_summary(&release.gate))
            .unwrap_or_default();
        let provenance = release
            .map(|release| release.provenance)
            .unwrap_or_default();
        let page = LookVersionPage::new(
            Slug::new(&look)?,
            LookVersion::new(format!("{version}.0.0"))?,
            publish_status(status),
            gate,
            provenance,
            blobs,
            notes,
        )?;
        looks.entry(look).or_default().push(page);
    }

    let looks = looks
        .into_iter()
        .map(|(slug, versions)| {
            Ok(LookPage {
                slug: Slug::new(&slug)?,
                name: slug,
                kind: LookKind::NativeFamily,
                wb_policy: WbPolicy::Adaptive,
                description: String::new(),
                aliases: Vec::new(),
                license: None,
                versions,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SiteData {
        bodies,
        looks,
        audit_log: audit_log(pool).await?,
        scenes: Vec::new(),
    })
}

fn load_blobs(config: &PublishConfig, artifacts: &[ManifestArtifact]) -> Result<Vec<ArtifactBlob>> {
    artifacts
        .iter()
        .map(|artifact| {
            let bytes = std::fs::read(config.blobs.join(&artifact.sha256))?;
            Ok(ArtifactBlob::new(ArtifactName::new(&artifact.name)?, bytes))
        })
        .collect()
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
    let rows = sqlx::query(
        "SELECT seq, payload, ts FROM audit_log
         WHERE type = 'release_manifest' ORDER BY seq",
    )
    .fetch_all(pool)
    .await?;
    let mut entries = Vec::new();
    for row in &rows {
        let payload: serde_json::Value = serde_json::from_str(&row.get::<String, _>("payload"))?;
        let (Some(look), Some(version), Some(sha256)) = (
            payload.get("look").and_then(|value| value.as_str()),
            payload.get("version").and_then(|value| value.as_u64()),
            payload
                .get("manifest_sha256")
                .and_then(|value| value.as_str()),
        ) else {
            continue;
        };
        let seq: i64 = row.get("seq");
        let ts: i64 = row.get("ts");
        entries.push(AuditEntry {
            seq: u64::try_from(seq).unwrap_or(0),
            ts: Timestamp::new(rfc3339(ts)).map_err(|error| WorkerError::Data(error.to_owned()))?,
            event: AuditEvent::Release {
                look: LookRef::new(look).map_err(|error| WorkerError::Data(error.to_owned()))?,
                version: VersionRef::new(version.to_string())
                    .map_err(|error| WorkerError::Data(error.to_owned()))?,
                manifest_sha256: ArtifactHash::parse(sha256)?,
            },
        });
    }
    Ok(AuditLog::new(entries))
}
