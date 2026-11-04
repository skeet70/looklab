// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposals, evidence, discussion and community data snapshots ([docs/08 §4 §6]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use looklab_color::Mat3;
use looklab_export::{BakeIlluminant, Baker, CctK, InputTransform, ValidationReport, validate};
use looklab_model::Look;
use looklab_model::probes;
use looklab_schema::{
    Evidence, FixtureDonation, LabeledTag, ProposalKind, ProposalStatus, ProposalView,
    SNAPSHOT_MANIFEST, SnapshotManifest, ValidatedPayload, content_hash, fixture_manifest_toml,
    validate_confirmation, validate_proposal,
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::admin::{self, AdminError, ModDecision};
use crate::db::now_unix;
use crate::ids::{CredentialId, ProposalId, ProposalMessageId};
use crate::state::AppState;

const MAX_MESSAGE_CHARS: usize = 2000;
const SNAPSHOT_MANIFEST_MAX_BYTES: u64 = 4096;

static SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum ProposalError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("moderation: {0}")]
    Admin(#[from] AdminError),
    #[error("the payload is invalid: {0}")]
    InvalidPayload(String),
    #[error("the fixture consent is invalid: {0}")]
    InvalidFixture(String),
    #[error("the proposal is unknown")]
    UnknownProposal,
    #[error("the proposal is closed")]
    Closed,
    #[error("the interface profile cannot be baked: {0}")]
    RoundTrip(String),
    #[error("snapshot: {0}")]
    Snapshot(String),
    #[error("the stored record is not readable")]
    Corrupt,
}

#[derive(Debug, Clone)]
pub struct Submission {
    pub kind: ProposalKind,
    pub payload: String,
    pub fixtures: Vec<FixtureDonation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Submitted {
    pub view: ProposalView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_trip: Option<RoundTripReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundTripReport {
    pub probes: usize,
    pub target_clipped: usize,
    pub ccts: Vec<CctSummary>,
    pub flagged: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CctSummary {
    pub cct: f64,
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
    pub unsupported: usize,
    pub clipped: usize,
}

impl RoundTripReport {
    fn from_report(report: &ValidationReport) -> Self {
        let ccts = std::iter::once(&report.primary)
            .chain(&report.others)
            .map(|entry| CctSummary {
                cct: entry.cct.kelvin(),
                count: entry.stats.count,
                median: entry.stats.median,
                p95: entry.stats.p95,
                max: entry.stats.max,
                unsupported: entry.unsupported,
                clipped: entry.clipped,
            })
            .collect();
        Self {
            probes: report.probes,
            target_clipped: report.target_clipped,
            ccts,
            flagged: report.flagged(),
        }
    }
}

/// A fixed camera model for the round-trip simulation.
#[derive(Debug, Clone, Copy)]
struct ReferenceTransform(Mat3);

impl InputTransform for ReferenceTransform {
    fn camera_to_xyz(&self, _cct: CctK) -> Option<Mat3> {
        Some(self.0)
    }
}

/// # Errors
/// A profile the exporter cannot bake, or an invalid schema document.
pub fn round_trip_report(payload: &str) -> Result<RoundTripReport, ProposalError> {
    let ValidatedPayload::InterfaceProfile(profile) =
        validate_proposal(ProposalKind::InterfaceProfile, payload)
            .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?
    else {
        return Err(ProposalError::InvalidPayload(
            "not an interface profile".to_owned(),
        ));
    };
    let look = Look::colorimetric();
    let transform = ReferenceTransform(Mat3::IDENTITY);
    let illuminant = match profile.insertion.illuminant_handling {
        looklab_schema::IlluminantHandling::Single => BakeIlluminant::Single { cct: CctK::D65 },
        looklab_schema::IlluminantHandling::DualInterpolated => BakeIlluminant::DualInterpolated,
    };
    let output = Baker::new(&profile, &look, &transform)
        .bake(illuminant)
        .map_err(|error| ProposalError::RoundTrip(error.to_string()))?;
    let report = validate(&output, &look, &transform, &probes::v1(), None)
        .map_err(|error| ProposalError::RoundTrip(error.to_string()))?;
    Ok(RoundTripReport::from_report(&report))
}

fn join_errors(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

pub async fn submit(
    db: &SqlitePool,
    credential: &CredentialId,
    submission: Submission,
) -> Result<Submitted, ProposalError> {
    let validated = validate_proposal(submission.kind, &submission.payload)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    for donation in &submission.fixtures {
        donation
            .validate()
            .map_err(|errors| ProposalError::InvalidFixture(join_errors(&errors)))?;
    }
    let summary = validated.summary();
    let evidence: Vec<Evidence> = submission
        .fixtures
        .iter()
        .map(|donation| Evidence::Fixture {
            donation: donation.clone(),
        })
        .collect();
    let round_trip = match &validated {
        ValidatedPayload::InterfaceProfile(_) => Some(round_trip_report(&submission.payload)?),
        _ => None,
    };
    let evidence_json = serde_json::to_string(&evidence).map_err(|_| ProposalError::Corrupt)?;
    let round_trip_json = match &round_trip {
        Some(report) => Some(serde_json::to_string(report).map_err(|_| ProposalError::Corrupt)?),
        None => None,
    };
    let id = ProposalId::new();
    let created_at = now_unix();
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at,
          round_trip_report)
         VALUES (?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(submission.kind.as_str())
    .bind(&submission.payload)
    .bind(summary.text())
    .bind(&evidence_json)
    .bind(created_at)
    .bind(&round_trip_json)
    .execute(db)
    .await?;
    Ok(Submitted {
        view: ProposalView {
            id: id.as_str().try_into().map_err(|_| ProposalError::Corrupt)?,
            kind: submission.kind,
            summary,
            status: ProposalStatus::Pending,
            evidence,
            created_at,
        },
        round_trip,
    })
}

pub async fn view(db: &SqlitePool, proposal: &ProposalId) -> Result<ProposalView, ProposalError> {
    let row: Option<(String, String, String, String, i64)> = sqlx::query_as(
        "SELECT type, payload, evidence, status, created_at FROM proposals WHERE id = ?",
    )
    .bind(proposal.as_str())
    .fetch_optional(db)
    .await?;
    let Some((kind, payload, evidence, status, created_at)) = row else {
        return Err(ProposalError::UnknownProposal);
    };
    let kind = kind
        .parse::<ProposalKind>()
        .map_err(|_| ProposalError::Corrupt)?;
    let summary = validate_proposal(kind, &payload)
        .map_err(|_| ProposalError::Corrupt)?
        .summary();
    Ok(ProposalView {
        id: proposal
            .as_str()
            .try_into()
            .map_err(|_| ProposalError::Corrupt)?,
        kind,
        summary,
        status: ProposalStatus::parse(&status).ok_or(ProposalError::Corrupt)?,
        evidence: serde_json::from_str(&evidence).map_err(|_| ProposalError::Corrupt)?,
        created_at,
    })
}

pub async fn confirm(
    db: &SqlitePool,
    credential: &CredentialId,
    proposal: &ProposalId,
    tags: Vec<LabeledTag>,
) -> Result<ProposalView, ProposalError> {
    validate_confirmation(&tags)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    let (status, evidence): (String, String) =
        sqlx::query_as("SELECT status, evidence FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(db)
            .await?
            .ok_or(ProposalError::UnknownProposal)?;
    let status = ProposalStatus::parse(&status).ok_or(ProposalError::Corrupt)?;
    if !status.is_open() {
        return Err(ProposalError::Closed);
    }
    let handle: Option<String> =
        sqlx::query_scalar("SELECT display_handle FROM credentials WHERE id = ?")
            .bind(credential.as_str())
            .fetch_optional(db)
            .await?
            .flatten();
    let mut evidence: Vec<Evidence> =
        serde_json::from_str(&evidence).map_err(|_| ProposalError::Corrupt)?;
    evidence.push(Evidence::Confirmation {
        author: handle,
        tags,
        recorded_at: rfc3339(now_unix()),
    });
    let evidence_json = serde_json::to_string(&evidence).map_err(|_| ProposalError::Corrupt)?;
    sqlx::query("UPDATE proposals SET evidence = ? WHERE id = ?")
        .bind(&evidence_json)
        .bind(proposal.as_str())
        .execute(db)
        .await?;
    view(db, proposal).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAuthor {
    Proposer,
    Moderator,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModProposal {
    pub proposal: ProposalView,
    pub payload: String,
    pub thread: Vec<ProposalMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub round_trip: Option<RoundTripReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProposalMessage {
    pub author: MessageAuthor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    pub body: String,
    pub created_at: i64,
}

pub async fn post_message(
    db: &SqlitePool,
    credential: &CredentialId,
    proposal: &ProposalId,
    author: MessageAuthor,
    body: &str,
) -> Result<(), ProposalError> {
    let body = body.trim();
    if body.is_empty() || body.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ProposalError::InvalidPayload(
            "a message between 1 and 2000 characters".to_owned(),
        ));
    }
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT credential_id, status FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(db)
            .await?;
    let Some((proposer, status)) = row else {
        return Err(ProposalError::UnknownProposal);
    };
    if !ProposalStatus::parse(&status)
        .ok_or(ProposalError::Corrupt)?
        .is_open()
    {
        return Err(ProposalError::Closed);
    }
    if author == MessageAuthor::Proposer && proposer != credential.as_str() {
        return Err(ProposalError::UnknownProposal);
    }
    sqlx::query(
        "INSERT INTO proposal_messages
         (id, proposal_id, author, author_credential, body, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(ProposalMessageId::new().as_str())
    .bind(proposal.as_str())
    .bind(author_name(author))
    .bind(credential.as_str())
    .bind(body)
    .bind(now_unix())
    .execute(db)
    .await?;
    Ok(())
}

fn author_name(author: MessageAuthor) -> &'static str {
    match author {
        MessageAuthor::Proposer => "proposer",
        MessageAuthor::Moderator => "moderator",
    }
}

fn parse_author(value: &str) -> Result<MessageAuthor, ProposalError> {
    match value {
        "proposer" => Ok(MessageAuthor::Proposer),
        "moderator" => Ok(MessageAuthor::Moderator),
        _ => Err(ProposalError::Corrupt),
    }
}

pub async fn proposer_thread(
    db: &SqlitePool,
    credential: &CredentialId,
    proposal: &ProposalId,
) -> Result<Vec<ProposalMessage>, ProposalError> {
    let owner: Option<String> =
        sqlx::query_scalar("SELECT credential_id FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(db)
            .await?;
    if owner.as_deref() != Some(credential.as_str()) {
        return Err(ProposalError::UnknownProposal);
    }
    thread(db, proposal).await
}

async fn thread(
    db: &SqlitePool,
    proposal: &ProposalId,
) -> Result<Vec<ProposalMessage>, ProposalError> {
    let rows = sqlx::query(
        "SELECT m.author, m.body, m.created_at, c.display_handle
         FROM proposal_messages m
         JOIN credentials c ON c.id = m.author_credential
         WHERE m.proposal_id = ? ORDER BY m.created_at, m.rowid",
    )
    .bind(proposal.as_str())
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(ProposalMessage {
                author: parse_author(row.get::<String, _>("author").as_str())?,
                handle: row.get("display_handle"),
                body: row.get("body"),
                created_at: row.get("created_at"),
            })
        })
        .collect()
}

pub async fn moderator_view(
    db: &SqlitePool,
    proposal: &ProposalId,
) -> Result<ModProposal, ProposalError> {
    let view = view(db, proposal).await?;
    let payload: String = sqlx::query_scalar("SELECT payload FROM proposals WHERE id = ?")
        .bind(proposal.as_str())
        .fetch_optional(db)
        .await?
        .ok_or(ProposalError::UnknownProposal)?;
    let thread = thread(db, proposal).await?;
    let round_trip: Option<String> =
        sqlx::query_scalar("SELECT round_trip_report FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(db)
            .await?
            .flatten();
    let round_trip = match round_trip {
        Some(json) => Some(serde_json::from_str(&json).map_err(|_| ProposalError::Corrupt)?),
        None => None,
    };
    Ok(ModProposal {
        proposal: view,
        payload,
        thread,
        round_trip,
    })
}

pub async fn fixture_manifest(db: &SqlitePool) -> Result<String, ProposalError> {
    let rows = sqlx::query(
        "SELECT evidence FROM proposals WHERE status = 'approved' ORDER BY created_at, id",
    )
    .fetch_all(db)
    .await?;
    let mut donations = Vec::new();
    for row in rows {
        let evidence: Vec<Evidence> =
            serde_json::from_str(row.get::<String, _>("evidence").as_str())
                .map_err(|_| ProposalError::Corrupt)?;
        for item in evidence {
            if let Evidence::Fixture { donation } = item {
                donations.push(donation);
            }
        }
    }
    Ok(fixture_manifest_toml(&donations))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotRecord {
    pub version: u64,
    pub content_hash: String,
    pub files: usize,
}

pub async fn record_decision(
    state: &AppState,
    moderator: &CredentialId,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
) -> Result<Option<SnapshotRecord>, ProposalError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT type, payload FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(&state.db)
            .await?;
    let Some((kind, payload)) = row else {
        return Err(ProposalError::UnknownProposal);
    };
    let kind = kind
        .parse::<ProposalKind>()
        .map_err(|_| ProposalError::Corrupt)?;
    if decision != ModDecision::Approve {
        admin::decide(&state.db, moderator, proposal, decision, reason).await?;
        return Ok(None);
    }
    if !kind.contributes_document() {
        admin::decide(&state.db, moderator, proposal, decision, reason).await?;
        return Ok(None);
    }
    validate_proposal(kind, &payload)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    admin::decide(&state.db, moderator, proposal, decision, reason).await?;
    write_snapshot(&state.config.snapshot_dir, kind, &payload).map(Some)
}

fn write_snapshot(
    dir: &Path,
    kind: ProposalKind,
    payload: &str,
) -> Result<SnapshotRecord, ProposalError> {
    let validated = validate_proposal(kind, payload)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    let (path, bytes) = document(&validated, payload)?;
    let _guard = SNAPSHOT_LOCK
        .lock()
        .map_err(|_| ProposalError::Snapshot("the snapshot lock is poisoned".to_owned()))?;
    let (previous_version, mut documents) = previous_snapshot(dir)?;
    documents.insert(path, bytes);
    let version = previous_version + 1;
    let staging = dir.join(format!("v{version}.tmp"));
    let target = dir.join(format!("v{version}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    }
    for (relative, bytes) in &documents {
        let file = staging.join(relative);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
        }
        std::fs::write(&file, bytes).map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    }
    let failures = looklab_schema::validate_dir(&staging)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    if !failures.is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(ProposalError::Snapshot(format!(
            "the approved documents do not validate: {failures:?}"
        )));
    }
    let files: Vec<(String, Vec<u8>)> = documents.into_iter().collect();
    let manifest = SnapshotManifest {
        version,
        content_hash: content_hash(&files),
    };
    let manifest_json = serde_json::to_string(&manifest).map_err(|_| ProposalError::Corrupt)?;
    std::fs::write(staging.join(SNAPSHOT_MANIFEST), manifest_json)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    std::fs::rename(&staging, &target)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    Ok(SnapshotRecord {
        version,
        content_hash: manifest.content_hash,
        files: files.len(),
    })
}

fn document(
    validated: &ValidatedPayload,
    payload: &str,
) -> Result<(String, Vec<u8>), ProposalError> {
    let relative = match validated {
        ValidatedPayload::Body(body) => {
            format!("bodies/{}.body.yaml", body.id.as_str().replace('/', "-"))
        }
        ValidatedPayload::SchemaChange(schema) => {
            format!("bodies/{}.yaml", schema.body.as_str().replace('/', "-"))
        }
        ValidatedPayload::InterfaceProfile(profile) => {
            format!("interfaces/{}.yaml", profile.id.as_str().replace('/', "-"))
        }
        _ => {
            return Err(ProposalError::Snapshot(
                "this proposal kind has no document".to_owned(),
            ));
        }
    };
    Ok((relative, payload.as_bytes().to_vec()))
}

fn previous_snapshot(dir: &Path) -> Result<(u64, BTreeMap<String, Vec<u8>>), ProposalError> {
    let mut best: Option<(u64, PathBuf)> = None;
    if dir.is_dir() {
        let entries =
            std::fs::read_dir(dir).map_err(|error| ProposalError::Snapshot(error.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|error| ProposalError::Snapshot(error.to_string()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(version) = name
                .strip_prefix('v')
                .and_then(|digits| digits.parse().ok())
            else {
                continue;
            };
            if best.as_ref().is_none_or(|(current, _)| version > *current) {
                best = Some((version, path));
            }
        }
    }
    let Some((version, path)) = best else {
        return Ok((0, BTreeMap::new()));
    };
    let manifest_bytes = std::fs::metadata(path.join(SNAPSHOT_MANIFEST))
        .map(|metadata| metadata.len())
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    if manifest_bytes > SNAPSHOT_MANIFEST_MAX_BYTES {
        return Err(ProposalError::Snapshot(
            "snapshot manifest too large".to_owned(),
        ));
    }
    let manifest: SnapshotManifest = serde_json::from_str(
        &std::fs::read_to_string(path.join(SNAPSHOT_MANIFEST))
            .map_err(|error| ProposalError::Snapshot(error.to_string()))?,
    )
    .map_err(|_| ProposalError::Snapshot("unreadable snapshot manifest".to_owned()))?;
    let mut documents = BTreeMap::new();
    for sub in ["bodies", "interfaces"] {
        collect_documents(&path.join(sub), sub, &mut documents)?;
    }
    let files: Vec<(String, Vec<u8>)> = documents
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();
    if content_hash(&files) != manifest.content_hash {
        return Err(ProposalError::Snapshot(
            "the previous snapshot does not match its content hash".to_owned(),
        ));
    }
    Ok((version, documents))
}

fn collect_documents(
    dir: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), ProposalError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(dir).map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|error| ProposalError::Snapshot(error.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| ProposalError::Snapshot("non-UTF-8 path".to_owned()))?;
            collect_documents(&path, &format!("{prefix}/{name}"), out)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "yaml")
        {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| ProposalError::Snapshot("non-UTF-8 path".to_owned()))?;
            let bytes =
                std::fs::read(&path).map_err(|error| ProposalError::Snapshot(error.to_string()))?;
            out.insert(format!("{prefix}/{name}"), bytes);
        }
    }
    Ok(())
}

fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Howard Hinnant's `civil_from_days`, for the audit-free timestamp format.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_a_known_instant() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn a_round_trip_report_has_one_entry_per_bake_cct() {
        let payload = include_str!("../../../data/interfaces/example-firmware-sim-v1.yaml");
        let report = round_trip_report(payload).expect("report");
        assert_eq!(report.ccts[0].cct, 6500.0);
        assert!(!report.ccts.is_empty());
        assert!(report.probes > 0);
    }
}
