// SPDX-License-Identifier: AGPL-3.0-or-later
//! Row access for uploads, credentials, blobs, versions and moderation items.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use looklab_server::admin::{self, ManifestArtifact};
use looklab_server::{ContributionId, CredentialId};
use looklab_trust::{CredentialId as TrustCredentialId, Reputation, Scope, Tier};
use looklab_upload::{
    BodyId, CommunitySnapshotVersion, ContributionKind, ControlSettings, LookSlug, RenderClass,
};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{Result, WorkerError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageId {
    value: uuid::Uuid,
}

impl ImageId {
    pub fn parse(text: &str) -> Option<Self> {
        uuid::Uuid::parse_str(text).ok().map(|value| Self { value })
    }

    pub fn as_str(&self) -> String {
        self.value.hyphenated().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionStatus {
    Pending,
    Accepted,
    Quarantined,
    Withdrawn,
}

impl ContributionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Quarantined => "quarantined",
            Self::Withdrawn => "withdrawn",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "accepted" => Self::Accepted,
            "quarantined" => Self::Quarantined,
            "withdrawn" => Self::Withdrawn,
            other => {
                return Err(WorkerError::Data(format!(
                    "unknown contribution status `{other}`"
                )));
            }
        })
    }
}

/// What a consistency pass may write to `contributions.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionOutcome {
    Accepted,
    Quarantined,
}

impl ContributionOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Quarantined => "quarantined",
        }
    }
}

/// What a consistency pass may write to `contribution_images.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageOutcome {
    Accepted,
    DownWeighted,
    Quarantined,
}

impl ImageOutcome {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "down_weighted" => Some(Self::DownWeighted),
            "quarantined" => Some(Self::Quarantined),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::DownWeighted => "down_weighted",
            Self::Quarantined => "quarantined",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LookVersionStatus {
    Provisional,
    Rooted,
    Published,
    Deprecated,
}

impl LookVersionStatus {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "provisional" => Self::Provisional,
            "rooted" => Self::Rooted,
            "published" => Self::Published,
            "deprecated" => Self::Deprecated,
            other => {
                return Err(WorkerError::Data(format!(
                    "unknown version status `{other}`"
                )));
            }
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Rooted => "rooted",
            Self::Published => "published",
            Self::Deprecated => "deprecated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionIntent {
    Native,
    Recipe,
    /// A firmware card render, grouped by firmware rather than fitted.
    Card,
}

impl ContributionIntent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Recipe => "recipe",
            Self::Card => "card",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContributionRow {
    pub id: ContributionId,
    pub credential: CredentialId,
    pub kind: ContributionKind,
    pub intent: ContributionIntent,
    pub target_look: Option<LookSlug>,
    pub body: BodyId,
    pub firmware: String,
    pub sim_key: String,
    pub settings: ControlSettings,
    pub render_class: RenderClass,
    pub deferred_until: Option<i64>,
    pub community_snapshot_version: CommunitySnapshotVersion,
    pub serial_hmac: Option<[u8; 32]>,
    pub status: ContributionStatus,
    pub created_at: i64,
}

impl ContributionRow {
    /// Canonical `settings` column text for recipe rows.
    pub fn settings_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.settings)?)
    }
}

#[derive(Debug, Clone)]
pub struct ImageRow {
    pub id: ImageId,
    pub contribution: ContributionId,
    pub samples_hash: [u8; 32],
    pub n_samples: i64,
    pub meta_json: String,
    pub duplicate_of: Option<ImageId>,
    /// `None` until a consistency pass has scored the image.
    pub status: Option<ImageOutcome>,
}

#[derive(Debug, Clone)]
pub struct CredentialInfo {
    pub id: CredentialId,
    pub trust: TrustCredentialId,
    pub tier: Tier,
    pub reputation: Reputation,
    pub scope: Option<Scope>,
}

pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub fn blob_path(root: &Path, hash: &[u8; 32]) -> PathBuf {
    root.join(hex(hash))
}

pub async fn credential(pool: &SqlitePool, id: &CredentialId) -> Result<CredentialInfo> {
    let row: (String, f64) = sqlx::query_as(
        "SELECT tier, reputation FROM credentials WHERE id = ? AND status = 'active'",
    )
    .bind(id.as_str())
    .fetch_one(pool)
    .await?;
    let tier = parse_tier(&row.0)?;
    let scope = credential_scope(pool, id).await?;
    Ok(CredentialInfo {
        id: id.clone(),
        trust: admin::trust_id(id)?,
        tier,
        reputation: Reputation::saturating(row.1),
        scope,
    })
}

/// An unrestricted grant or endorsement is `None`; no active scope is an empty scope.
async fn credential_scope(pool: &SqlitePool, id: &CredentialId) -> Result<Option<Scope>> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT scope FROM grants
         WHERE subject_credential = ? AND role_or_tier = 'seeder' AND revoked_at IS NULL
         UNION ALL
         SELECT scope FROM endorsements
         WHERE subject_credential = ? AND revoked_at IS NULL",
    )
    .bind(id.as_str())
    .bind(id.as_str())
    .fetch_all(pool)
    .await?;
    let mut rules = Vec::new();
    for (scope,) in rows {
        let Some(text) = scope else {
            return Ok(None);
        };
        let scope: Scope = serde_json::from_str(&text)?;
        rules.extend(scope.rules);
    }
    Ok(Some(Scope::new(rules)))
}

/// Credentials a moderator excluded from later fits of one Look.
pub async fn look_credential_exclusions(
    pool: &SqlitePool,
    look: &str,
) -> Result<BTreeSet<CredentialId>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT credential_id FROM look_credential_exclusions WHERE look = ?")
            .bind(look)
            .fetch_all(pool)
            .await?;
    rows.into_iter()
        .map(|(id,)| {
            CredentialId::try_from(id)
                .map_err(|_| WorkerError::Data("excluded credential id is not a UUID".to_owned()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefitRequest {
    pub look: LookSlug,
    pub generation: i64,
    pub reverted_version: u32,
}

pub async fn pending_refit_requests(pool: &SqlitePool) -> Result<Vec<RefitRequest>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT look, generation, reverted_version FROM look_refit_requests
         ORDER BY requested_at, look",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|(look, generation, reverted_version)| {
            Ok(RefitRequest {
                look: LookSlug::try_from(look)
                    .map_err(|_| WorkerError::Data("refit look is not a slug".to_owned()))?,
                generation,
                reverted_version: u32::try_from(reverted_version)
                    .map_err(|_| WorkerError::Data("reverted version is not a u32".to_owned()))?,
            })
        })
        .collect()
}

pub async fn clear_refit_request(
    pool: &SqlitePool,
    look: &LookSlug,
    generation: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM look_refit_requests WHERE look = ? AND generation = ?")
        .bind(look.as_str())
        .bind(generation)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn body_refitted_after(
    pool: &SqlitePool,
    look: &LookSlug,
    body: &str,
    version: u32,
) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM release_body_params p
         JOIN look_versions v ON v.look = p.look AND v.version = p.version
         WHERE p.look = ? AND p.body = ? AND p.version > ?
           AND v.status IN ('published', 'rooted')",
    )
    .bind(look.as_str())
    .bind(body)
    .bind(i64::from(version))
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

fn parse_tier(value: &str) -> Result<Tier> {
    Ok(match value {
        "new" => Tier::New,
        "endorsed" => Tier::Endorsed,
        "seeder" => Tier::Seeder,
        other => return Err(WorkerError::Data(format!("unknown tier `{other}`"))),
    })
}

fn parse_kind(value: &str) -> Result<ContributionKind> {
    Ok(match value {
        "library" => ContributionKind::Library,
        "rerender" => ContributionKind::Rerender,
        "pattern" => ContributionKind::Pattern,
        "gradient" => ContributionKind::Gradient,
        "chart" => ContributionKind::Chart,
        other => {
            return Err(WorkerError::Data(format!(
                "unknown contribution kind `{other}`"
            )));
        }
    })
}

fn parse_render_class(value: &str) -> Result<RenderClass> {
    match value {
        "colour" => Ok(RenderClass::Colour),
        "monochrome" => Ok(RenderClass::Monochrome),
        other => Err(WorkerError::Data(format!("unknown render class `{other}`"))),
    }
}

fn parse_intent(value: &str) -> Result<ContributionIntent> {
    Ok(match value {
        "native" => ContributionIntent::Native,
        "recipe" => ContributionIntent::Recipe,
        "card" => ContributionIntent::Card,
        other => return Err(WorkerError::Data(format!("unknown intent `{other}`"))),
    })
}

fn contribution_row(row: &sqlx::sqlite::SqliteRow) -> Result<ContributionRow> {
    let id: String = row.get("id");
    let id = ContributionId::try_from(id)
        .map_err(|_| WorkerError::Data("contribution id is not a UUID".to_owned()))?;
    let credential: String = row.get("credential_id");
    let credential = CredentialId::try_from(credential)
        .map_err(|_| WorkerError::Data("credential id is not a UUID".to_owned()))?;
    let body: String = row.get("body_id");
    let body =
        BodyId::new(&body).map_err(|_| WorkerError::Data(format!("invalid body id `{body}`")))?;
    let target_look: Option<String> = row.get("target_look_id");
    let target_look = target_look
        .map(|look| {
            LookSlug::new(&look)
                .map_err(|_| WorkerError::Data(format!("invalid look slug `{look}`")))
        })
        .transpose()?;
    let serial: Option<Vec<u8>> = row.get("serial_hmac");
    let serial_hmac = serial
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| WorkerError::Data("serial HMAC is not 32 bytes".to_owned()))
        })
        .transpose()?;
    Ok(ContributionRow {
        id,
        credential,
        kind: parse_kind(row.get::<String, _>("kind").as_str())?,
        intent: parse_intent(row.get::<String, _>("intent").as_str())?,
        target_look,
        body,
        firmware: row.get("firmware_version"),
        sim_key: row.get("sim_key"),
        settings: serde_json::from_str::<ControlSettings>(&row.get::<String, _>("settings"))?,
        render_class: parse_render_class(&row.get::<String, _>("render_class"))?,
        deferred_until: row.get("deferred_until"),
        community_snapshot_version: CommunitySnapshotVersion::new(
            u64::try_from(row.get::<i64, _>("community_snapshot_version"))
                .map_err(|_| WorkerError::Data("snapshot version is not a u64".to_owned()))?,
        ),
        serial_hmac,
        status: ContributionStatus::parse(row.get::<String, _>("status").as_str())?,
        created_at: row.get("created_at"),
    })
}

fn image_row(row: &sqlx::sqlite::SqliteRow) -> Result<ImageRow> {
    let id: String = row.get("id");
    let Some(id) = ImageId::parse(&id) else {
        return Err(WorkerError::Data("image id is not a UUID".to_owned()));
    };
    let contribution: String = row.get("contribution_id");
    let contribution = ContributionId::try_from(contribution)
        .map_err(|_| WorkerError::Data("contribution id is not a UUID".to_owned()))?;
    let hash: Vec<u8> = row.get("samples_hash");
    let samples_hash = hash
        .try_into()
        .map_err(|_| WorkerError::Data("samples hash is not 32 bytes".to_owned()))?;
    let duplicate_of: Option<String> = row.get("duplicate_of");
    let duplicate_of = duplicate_of
        .map(|text| {
            ImageId::parse(&text)
                .ok_or_else(|| WorkerError::Data("duplicate id is not a UUID".to_owned()))
        })
        .transpose()?;
    let status: String = row.get("status");
    let status = match status.as_str() {
        "pending" => None,
        other => Some(
            ImageOutcome::parse(other)
                .ok_or_else(|| WorkerError::Data(format!("unknown image status `{other}`")))?,
        ),
    };
    Ok(ImageRow {
        id,
        contribution,
        samples_hash,
        n_samples: row.get("n_samples"),
        meta_json: row.get("meta"),
        duplicate_of,
        status,
    })
}

const CONTRIBUTION_COLUMNS: &str = "c.id, c.credential_id, c.kind, c.intent, c.target_look_id,
    c.body_id, c.firmware_version, c.sim_key, c.settings, c.render_class, c.deferred_until,
    c.community_snapshot_version, c.serial_hmac, c.status, c.created_at";

pub async fn contribution(pool: &SqlitePool, id: &ContributionId) -> Result<ContributionRow> {
    let sql = format!("SELECT {CONTRIBUTION_COLUMNS} FROM contributions c WHERE c.id = ?");
    let row = sqlx::query(&sql).bind(id.as_str()).fetch_one(pool).await?;
    contribution_row(&row)
}

pub async fn images(pool: &SqlitePool, id: &ContributionId) -> Result<Vec<ImageRow>> {
    let rows = sqlx::query(
        "SELECT id, contribution_id, samples_hash, n_samples, meta, duplicate_of, status
         FROM contribution_images WHERE contribution_id = ? ORDER BY id",
    )
    .bind(id.as_str())
    .fetch_all(pool)
    .await?;
    rows.iter().map(image_row).collect()
}

/// Non-withdrawn contributions on one body, with their non-quarantined images.
pub async fn body_pool(
    pool: &SqlitePool,
    body: &BodyId,
) -> Result<Vec<(ContributionRow, Vec<ImageRow>)>> {
    let sql = format!(
        "SELECT {CONTRIBUTION_COLUMNS} FROM contributions c
         WHERE c.body_id = ? AND c.intent != 'card' AND c.render_class = 'colour'
           AND c.status NOT IN ('withdrawn', 'quarantined') ORDER BY c.id"
    );
    let contributions = sqlx::query(&sql)
        .bind(body.as_str())
        .fetch_all(pool)
        .await?;
    let mut out = Vec::with_capacity(contributions.len());
    for row in &contributions {
        let contribution = contribution_row(row)?;
        let images = sqlx::query(
            "SELECT id, contribution_id, samples_hash, n_samples, meta, duplicate_of, status
             FROM contribution_images
             WHERE contribution_id = ? AND status != 'quarantined' ORDER BY id",
        )
        .bind(contribution.id.as_str())
        .fetch_all(pool)
        .await?;
        let images = images.iter().map(image_row).collect::<Result<Vec<_>>>()?;
        out.push((contribution, images));
    }
    Ok(out)
}

pub async fn all_contributions(pool: &SqlitePool) -> Result<Vec<ContributionRow>> {
    let sql = format!(
        "SELECT {CONTRIBUTION_COLUMNS} FROM contributions c
         WHERE c.status != 'withdrawn' AND c.intent != 'card' AND c.render_class = 'colour'
         ORDER BY c.id"
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;
    rows.iter().map(contribution_row).collect()
}

pub async fn images_for_contributions(
    pool: &SqlitePool,
    ids: &[ContributionId],
) -> Result<Vec<ImageRow>> {
    let mut out = Vec::new();
    for id in ids {
        out.extend(images(pool, id).await?);
    }
    Ok(out)
}

pub async fn serial_links(pool: &SqlitePool) -> Result<Vec<(CredentialId, [u8; 32])>> {
    let rows = sqlx::query(
        "SELECT DISTINCT credential_id, serial_hmac FROM contributions
         WHERE serial_hmac IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let credential: String = row.get("credential_id");
            let credential = CredentialId::try_from(credential)
                .map_err(|_| WorkerError::Data("credential id is not a UUID".to_owned()))?;
            let hmac: Vec<u8> = row.get("serial_hmac");
            let hmac = hmac
                .try_into()
                .map_err(|_| WorkerError::Data("serial HMAC is not 32 bytes".to_owned()))?;
            Ok((credential, hmac))
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct ImageStatsWrite {
    pub rms_de: Option<f64>,
    pub z: Option<f64>,
    pub reason: Option<String>,
    pub weight_components: Option<String>,
    pub status: ImageOutcome,
    pub status_reason: Option<String>,
}

/// Writes the score and outcome, leaving withdrawn contributions untouched. A
/// withdrawal committed during the fit wins the race; the write reports it.
pub async fn write_image_stats(
    pool: &SqlitePool,
    id: &ImageId,
    stats: &ImageStatsWrite,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE contribution_images
         SET rms_de = ?, z = ?, reason = ?, weight_components = ?, status = ?, status_reason = ?
         WHERE id = ?
           AND contribution_id IN (SELECT id FROM contributions
                                   WHERE status != 'withdrawn' AND render_class = 'colour')",
    )
    .bind(stats.rms_de)
    .bind(stats.z)
    .bind(&stats.reason)
    .bind(&stats.weight_components)
    .bind(stats.status.as_str())
    .bind(&stats.status_reason)
    .bind(id.as_str())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Withdrawal is terminal for automated status writes.
pub async fn set_contribution_status(
    pool: &SqlitePool,
    id: &ContributionId,
    outcome: ContributionOutcome,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE contributions SET status = ?
             WHERE id = ? AND status != 'withdrawn' AND render_class = 'colour'",
    )
    .bind(outcome.as_str())
    .bind(id.as_str())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn clear_duplicates(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE contribution_images SET duplicate_of = NULL WHERE duplicate_of IS NOT NULL",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_duplicate(pool: &SqlitePool, image: &ImageId, of: &ImageId) -> Result<()> {
    sqlx::query("UPDATE contribution_images SET duplicate_of = ? WHERE id = ?")
        .bind(of.as_str())
        .bind(image.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct LookVersionRow {
    pub version: u32,
    pub status: LookVersionStatus,
    pub changelog: Option<String>,
    pub created_at: i64,
}

pub async fn look_versions(pool: &SqlitePool, look: &str) -> Result<Vec<LookVersionRow>> {
    let rows = sqlx::query(
        "SELECT version, status, changelog, created_at FROM look_versions
         WHERE look = ? ORDER BY version",
    )
    .bind(look)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let version: i64 = row.get("version");
            Ok(LookVersionRow {
                version: u32::try_from(version)
                    .map_err(|_| WorkerError::Data("version is not a u32".to_owned()))?,
                status: LookVersionStatus::parse(row.get::<String, _>("status").as_str())?,
                changelog: row.get("changelog"),
                created_at: row.get("created_at"),
            })
        })
        .collect()
}

pub async fn manifest_artifacts(
    pool: &SqlitePool,
    look: &str,
    version: u32,
) -> Result<Option<Vec<ManifestArtifact>>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = ?",
    )
    .bind(look)
    .bind(i64::from(version))
    .fetch_optional(pool)
    .await?;
    row.map(|(artifacts,)| serde_json::from_str(&artifacts).map_err(WorkerError::from))
        .transpose()
}

pub async fn gate_failure_pending(pool: &SqlitePool, look: &str, version: u32) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM gate_failures WHERE look = ? AND version = ? AND status = 'pending'",
    )
    .bind(look)
    .bind(i64::from(version))
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

pub async fn insert_gate_failure(
    pool: &SqlitePool,
    look: &str,
    version: u32,
    report: &str,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO gate_failures (id, look, version, report, status, created_at)
         VALUES (?, ?, ?, ?, 'pending', ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(look)
    .bind(i64::from(version))
    .bind(report)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WithdrawalId(String);

impl WithdrawalId {
    pub fn parse(text: &str) -> Option<Self> {
        uuid::Uuid::parse_str(text)
            .ok()
            .map(|_| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct WithdrawalRow {
    pub id: WithdrawalId,
    pub contribution: ContributionId,
    pub delete_after: i64,
}

pub async fn withdrawals_due(
    pool: &SqlitePool,
    contribution: Option<&ContributionId>,
    now: i64,
) -> Result<Vec<WithdrawalRow>> {
    let rows = sqlx::query(
        "SELECT id, contribution_id, delete_after FROM withdrawals
         WHERE status = 'pending' AND delete_after <= ? AND (? IS NULL OR contribution_id = ?)
         ORDER BY delete_after",
    )
    .bind(now)
    .bind(contribution.map(|id| id.as_str()))
    .bind(contribution.map(|id| id.as_str()))
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let id: String = row.get("contribution_id");
            let contribution = ContributionId::try_from(id)
                .map_err(|_| WorkerError::Data("contribution id is not a UUID".to_owned()))?;
            let id: String = row.get("id");
            let id = WithdrawalId::parse(&id)
                .ok_or_else(|| WorkerError::Data("withdrawal id is not a UUID".to_owned()))?;
            Ok(WithdrawalRow {
                id,
                contribution,
                delete_after: row.get("delete_after"),
            })
        })
        .collect()
}

pub async fn mark_withdrawal_deleted(
    pool: &SqlitePool,
    contribution: &ContributionId,
) -> Result<()> {
    sqlx::query("UPDATE withdrawals SET status = 'deleted' WHERE contribution_id = ?")
        .bind(contribution.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredExpiryId(String);

impl DeferredExpiryId {
    pub fn parse(text: &str) -> Option<Self> {
        uuid::Uuid::parse_str(text)
            .ok()
            .map(|_| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct DeferredExpiryRow {
    pub id: DeferredExpiryId,
    pub contribution: ContributionId,
    pub due_at: i64,
}

pub async fn deferred_expiries_due(
    pool: &SqlitePool,
    contribution: Option<&ContributionId>,
    now: i64,
) -> Result<Vec<DeferredExpiryRow>> {
    let rows = sqlx::query(
        "SELECT d.id, d.contribution_id, d.due_at FROM deferred_expiries d
         WHERE d.status = 'pending' AND d.due_at <= ? AND (? IS NULL OR d.contribution_id = ?)
           AND NOT EXISTS (
               SELECT 1 FROM withdrawals w
               WHERE w.contribution_id = d.contribution_id
                 AND w.status = 'pending' AND w.delete_after <= ?
           )
         ORDER BY d.due_at",
    )
    .bind(now)
    .bind(contribution.map(|id| id.as_str()))
    .bind(contribution.map(|id| id.as_str()))
    .bind(now)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|row| {
            let id: String = row.get("id");
            let id = DeferredExpiryId::parse(&id)
                .ok_or_else(|| WorkerError::Data("expiry id is not a UUID".to_owned()))?;
            let contribution: String = row.get("contribution_id");
            let contribution = ContributionId::try_from(contribution)
                .map_err(|_| WorkerError::Data("contribution id is not a UUID".to_owned()))?;
            Ok(DeferredExpiryRow {
                id,
                contribution,
                due_at: row.get("due_at"),
            })
        })
        .collect()
}

pub async fn expire_deferred_contribution(
    pool: &SqlitePool,
    contribution: &ContributionId,
) -> Result<()> {
    sqlx::query(
        "UPDATE contributions SET status = 'withdrawn'
         WHERE id = ? AND render_class = 'monochrome' AND status != 'withdrawn'",
    )
    .bind(contribution.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_deferred_expiry_deleted(
    pool: &SqlitePool,
    contribution: &ContributionId,
) -> Result<()> {
    sqlx::query("UPDATE deferred_expiries SET status = 'deleted' WHERE contribution_id = ?")
        .bind(contribution.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn release_image_blob_once(
    pool: &SqlitePool,
    blobs: &Path,
    image: &ImageId,
    hash: &[u8; 32],
    now: i64,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let claimed = sqlx::query(
        "UPDATE contribution_images SET blob_released_at = ?
         WHERE id = ? AND blob_released_at IS NULL",
    )
    .bind(now)
    .bind(image.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 1 {
        sqlx::query("UPDATE blobs SET ref_count = ref_count - 1 WHERE hash = ?")
            .bind(hash.as_slice())
            .execute(&mut *tx)
            .await?;
        let remaining: Option<i64> =
            sqlx::query_scalar("SELECT ref_count FROM blobs WHERE hash = ?")
                .bind(hash.as_slice())
                .fetch_optional(&mut *tx)
                .await?;
        if remaining.unwrap_or(0) <= 0 {
            sqlx::query("DELETE FROM blobs WHERE hash = ?")
                .bind(hash.as_slice())
                .execute(&mut *tx)
                .await?;
            if !blob_referenced(&mut *tx, hash).await? {
                remove_blob_file(blobs, hash)?;
            }
        }
    }
    tx.commit().await?;
    Ok(())
}

pub async fn blob_referenced(conn: &mut SqliteConnection, hash: &[u8; 32]) -> Result<bool> {
    let referenced: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM blob_references WHERE hash = ?)")
            .bind(hex(hash))
            .fetch_one(conn)
            .await?;
    Ok(referenced != 0)
}

pub async fn expired_staging_hashes(
    conn: &mut SqliteConnection,
    now: i64,
) -> Result<Vec<[u8; 32]>> {
    let rows: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT DISTINCT hash FROM blob_staging WHERE expires_at <= ?")
            .bind(now)
            .fetch_all(conn)
            .await?;
    rows.into_iter()
        .map(|row| {
            row.try_into()
                .map_err(|_| WorkerError::Data("blob hash is not 32 bytes".to_owned()))
        })
        .collect()
}

pub async fn delete_expired_staging(
    conn: &mut SqliteConnection,
    hash: &[u8; 32],
    now: i64,
) -> Result<()> {
    sqlx::query("DELETE FROM blob_staging WHERE hash = ? AND expires_at <= ?")
        .bind(hash.as_slice())
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

pub fn remove_blob_file(root: &Path, hash: &[u8; 32]) -> Result<()> {
    match std::fs::remove_file(blob_path(root, hash)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Versions a moderator explicitly re-anchored (docs/05 §8).
pub async fn re_anchor_versions(pool: &SqlitePool, look: &str) -> Result<Vec<u32>> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT payload FROM audit_log WHERE type = 're_anchor' ORDER BY seq")
            .fetch_all(pool)
            .await?;
    let mut versions = Vec::new();
    for (payload,) in rows {
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        if payload.get("look").and_then(|value| value.as_str()) != Some(look) {
            continue;
        }
        if let Some(version) = payload
            .get("version")
            .and_then(|value| value.as_u64())
            .and_then(|version| u32::try_from(version).ok())
        {
            versions.push(version);
        }
    }
    Ok(versions)
}

pub async fn pool_fingerprint(pool: &SqlitePool, look: &str, body: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT fingerprint FROM look_pool_state WHERE look = ? AND body = ?")
            .bind(look)
            .bind(body)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn record_pool_fingerprint(
    pool: &SqlitePool,
    look: &str,
    body: &str,
    fingerprint: &str,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO look_pool_state (look, body, fingerprint, updated_at)
         VALUES (?, ?, ?, ?)
         ON CONFLICT (look, body) DO UPDATE
         SET fingerprint = excluded.fingerprint, updated_at = excluded.updated_at",
    )
    .bind(look)
    .bind(body)
    .bind(fingerprint)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// A serialized resumable fit state, opaque to the worker.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct FitState(String);

impl FitState {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Non-converged native fit state carried across batches for one Look.
#[derive(Debug, Clone, Default)]
pub struct FitAttempt {
    pub states: BTreeMap<String, FitState>,
    pub consecutive: u32,
    /// First non-converged batch in the current streak.
    pub started_at: i64,
}

pub async fn fit_attempt(pool: &SqlitePool, look: &str) -> Result<Option<FitAttempt>> {
    let row: Option<(String, i64, i64)> =
        sqlx::query_as("SELECT states, consecutive, started_at FROM fit_attempts WHERE look = ?")
            .bind(look)
            .fetch_optional(pool)
            .await?;
    row.map(|(states, consecutive, started_at)| {
        let states: BTreeMap<String, FitState> = serde_json::from_str(&states)?;
        Ok(FitAttempt {
            states,
            consecutive: u32::try_from(consecutive).unwrap_or(u32::MAX),
            started_at,
        })
    })
    .transpose()
}

/// Stores one body's state and returns the Look's consecutive non-converged count.
pub async fn record_fit_attempt(
    pool: &SqlitePool,
    look: &str,
    body: &str,
    state: Option<&FitState>,
    now: i64,
) -> Result<u32> {
    let mut attempt = fit_attempt(pool, look).await?.unwrap_or_default();
    if let Some(state) = state {
        attempt.states.insert(body.to_owned(), state.clone());
    }
    let consecutive = attempt.consecutive.saturating_add(1);
    let started_at = if attempt.consecutive == 0 {
        now
    } else {
        attempt.started_at
    };
    let states = serde_json::to_string(&attempt.states)?;
    sqlx::query(
        "INSERT INTO fit_attempts (look, states, consecutive, started_at, updated_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (look) DO UPDATE
         SET states = excluded.states,
             consecutive = excluded.consecutive,
             started_at = excluded.started_at,
             updated_at = excluded.updated_at",
    )
    .bind(look)
    .bind(states)
    .bind(i64::from(consecutive))
    .bind(started_at)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(consecutive)
}

pub async fn clear_fit_attempt(pool: &SqlitePool, look: &str) -> Result<()> {
    sqlx::query("DELETE FROM fit_attempts WHERE look = ?")
        .bind(look)
        .execute(pool)
        .await?;
    Ok(())
}

/// The `fit_holds.status` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FitHoldStatus {
    Pending,
    Resolved,
}

impl FitHoldStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Resolved => "resolved",
        }
    }
}

/// Raises the moderation item for a Look whose batches keep not converging.
pub async fn open_fit_hold(
    pool: &SqlitePool,
    look: &str,
    consecutive: u32,
    first_attempt: i64,
    last_attempt: i64,
    diagnostics: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO fit_holds
         (look, consecutive, first_attempt, last_attempt, diagnostics, status)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (look) DO UPDATE
         SET consecutive = excluded.consecutive,
             last_attempt = excluded.last_attempt,
             diagnostics = excluded.diagnostics,
             status = excluded.status",
    )
    .bind(look)
    .bind(i64::from(consecutive))
    .bind(first_attempt)
    .bind(last_attempt)
    .bind(diagnostics)
    .bind(FitHoldStatus::Pending.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn resolve_fit_hold(pool: &SqlitePool, look: &str) -> Result<()> {
    sqlx::query("UPDATE fit_holds SET status = ? WHERE look = ? AND status = ?")
        .bind(FitHoldStatus::Resolved.as_str())
        .bind(look)
        .bind(FitHoldStatus::Pending.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// The two stages of the hierarchical fit (docs/02 §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyStage {
    Native,
    Recipe,
}

impl BodyStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Recipe => "recipe",
        }
    }
}

pub async fn store_release_body_params(
    pool: &SqlitePool,
    look: &str,
    version: u32,
    body: &str,
    stage: BodyStage,
    params: &str,
    now: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO release_body_params (look, version, body, stage, params, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (look, version, body, stage) DO UPDATE SET params = excluded.params",
    )
    .bind(look)
    .bind(i64::from(version))
    .bind(body)
    .bind(stage.as_str())
    .bind(params)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn release_body_params(
    pool: &SqlitePool,
    look: &str,
    version: u32,
    body: &str,
    stage: BodyStage,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT params FROM release_body_params
         WHERE look = ? AND version = ? AND body = ? AND stage = ?",
    )
    .bind(look)
    .bind(i64::from(version))
    .bind(body)
    .bind(stage.as_str())
    .fetch_optional(pool)
    .await?)
}

/// Whether a version manifest was already committed for these inputs.
pub async fn manifest_exists(pool: &SqlitePool, look: &str, version: u32) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = ?",
    )
    .bind(look)
    .bind(i64::from(version))
    .fetch_one(pool)
    .await?;
    Ok(count > 0)
}

/// Applies one credential's reputation change once per release, inside the caller's
/// transaction. Returns false when an earlier attempt already applied it.
pub async fn apply_reputation_once(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    look: &str,
    version: u32,
    credential: &CredentialId,
    value: f64,
    now: i64,
) -> Result<bool> {
    let inserted = sqlx::query(
        "INSERT INTO release_reputation_applied (look, version, credential_id, applied_at)
         VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
    )
    .bind(look)
    .bind(i64::from(version))
    .bind(credential.as_str())
    .bind(now)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(false);
    }
    sqlx::query("UPDATE credentials SET reputation = ? WHERE id = ?")
        .bind(value)
        .bind(credential.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(true)
}
