// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
use looklab_publish::{
    ArtifactBlob, ArtifactHash, ArtifactName, AuditEntry, AuditEvent, AuditLog, BodyExtras,
    BodyPage, Decision, LookPage, LookRef, LookStatus, LookVersionPage, ProposalRef, Role, Scene,
    SceneName, SiteData, SitePath, SiteSink, SplitKind, Timestamp, VersionNotes, VersionRef,
    write_site,
};
use looklab_server::admin::ManifestArtifact;
use sha2::{Digest, Sha256};
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
            .map(|release| gate_summary(&release.gate, release.converged))
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
        community_snapshot_version: 0,
        bodies,
        looks,
        audit_log: audit_log(pool).await?,
        scenes: canonical_scenes(),
    })
}

const SCENE_SIZE: u32 = 128;

/// Neutral/hue gradient, hue wheel and memory-colour patches in scene-linear
/// D65 XYZ (docs/03 §5).
fn canonical_scenes() -> Vec<Scene> {
    vec![gradient_scene(), hue_wheel_scene(), patches_scene()]
}

fn gradient_scene() -> Scene {
    let colours = [
        [0.82, 0.18, 0.16],
        [0.88, 0.55, 0.18],
        [0.86, 0.82, 0.24],
        [0.28, 0.62, 0.30],
        [0.22, 0.44, 0.78],
        [0.48, 0.28, 0.66],
    ];
    let mut xyz = Vec::with_capacity((SCENE_SIZE * SCENE_SIZE) as usize);
    for y in 0..SCENE_SIZE {
        let v = f64::from(y) / f64::from(SCENE_SIZE - 1);
        for x in 0..SCENE_SIZE {
            let u = f64::from(x) / f64::from(SCENE_SIZE - 1);
            let encoded = if u < 0.5 {
                let level = 0.002 + (u / 0.5);
                [level, level, level]
            } else {
                let band =
                    (((u - 0.5) * colours.len() as f64).floor() as usize).min(colours.len() - 1);
                let shade = 0.10 + 0.90 * (1.0 - v);
                [
                    colours[band][0] * shade,
                    colours[band][1] * shade,
                    colours[band][2] * shade,
                ]
            };
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(
        SceneName::new("gradient").expect("scene name"),
        SCENE_SIZE,
        SCENE_SIZE,
        xyz,
    )
    .expect("gradient scene")
}

fn hue_wheel_scene() -> Scene {
    let mut xyz = Vec::with_capacity((SCENE_SIZE * SCENE_SIZE) as usize);
    let centre = f64::from(SCENE_SIZE - 1) / 2.0;
    for y in 0..SCENE_SIZE {
        for x in 0..SCENE_SIZE {
            let dx = f64::from(x) - centre;
            let dy = f64::from(y) - centre;
            let distance = (dx * dx + dy * dy).sqrt() / centre;
            let angle = dy.atan2(dx);
            let hue = (angle / (2.0 * core::f64::consts::PI)).rem_euclid(1.0);
            let saturation = distance.clamp(0.0, 1.0);
            let value = 1.0 - 0.55 * saturation;
            xyz.push(encoded_xyz(hsv(hue, saturation, value)));
        }
    }
    Scene::new(
        SceneName::new("hue-wheel").expect("scene name"),
        SCENE_SIZE,
        SCENE_SIZE,
        xyz,
    )
    .expect("hue wheel scene")
}

fn patches_scene() -> Scene {
    let patches = [
        [0.91, 0.76, 0.66],
        [0.55, 0.36, 0.26],
        [0.22, 0.42, 0.16],
        [0.44, 0.66, 0.92],
        [0.18, 0.18, 0.18],
        [0.92, 0.92, 0.92],
        [0.04, 0.04, 0.04],
        [0.78, 0.12, 0.10],
        [0.16, 0.60, 0.22],
        [0.12, 0.32, 0.78],
        [0.96, 0.72, 0.28],
        [0.10, 0.09, 0.12],
    ];
    let cols = 4_u32;
    let rows = 3_u32;
    let mut xyz = Vec::with_capacity((SCENE_SIZE * SCENE_SIZE) as usize);
    for y in 0..SCENE_SIZE {
        let row = (y * rows / SCENE_SIZE).min(rows - 1) as usize;
        let v = f64::from(y % (SCENE_SIZE / rows)) / f64::from(SCENE_SIZE / rows);
        for x in 0..SCENE_SIZE {
            let col = (x * cols / SCENE_SIZE).min(cols - 1) as usize;
            let patch = patches[(row * cols as usize + col).min(patches.len() - 1)];
            let shade = 0.72 + 0.28 * v;
            xyz.push(encoded_xyz([
                patch[0] * shade,
                patch[1] * shade,
                patch[2] * shade,
            ]));
        }
    }
    Scene::new(
        SceneName::new("patches").expect("scene name"),
        SCENE_SIZE,
        SCENE_SIZE,
        xyz,
    )
    .expect("patches scene")
}

fn encoded_xyz(encoded: [f64; 3]) -> [f32; 3] {
    let xyz = looklab_color::RgbSpace::Srgb
        .rgb_encoded_to_xyz(looklab_color::Rgb::new(encoded[0], encoded[1], encoded[2]));
    [xyz.x as f32, xyz.y as f32, xyz.z as f32]
}

fn hsv(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i64).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
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
         ORDER BY a.admitted_at DESC, a.id DESC LIMIT 1",
    )
    .bind(admitted_by)
    .bind(subject)
    .bind(ts)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}
