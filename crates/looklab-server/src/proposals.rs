// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposals, evidence, discussion and community data snapshots ([docs/08 §4 §6]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use looklab_color::Mat3;
use looklab_export::{BakeIlluminant, Baker, CctK, InputTransform, ValidationReport, validate};
use looklab_model::Look;
use looklab_model::probes;
use looklab_schema::{
    AcceptedArtifact, Evidence, FIXTURE_MANIFEST, FixtureDonation, FixtureFileName, LabeledTag,
    Placement, ProposalKind, ProposalStatus, ProposalView, SNAPSHOT_MANIFEST, Sha256Hex,
    SnapshotManifest, ValidatedPayload, apply_sim_mapping, body_export_path, content_hash,
    fixture_manifest_toml, interface_export_path, mapping_status_supported, schema_export_path,
    validate_confirmation, validate_proposal,
};
use looklab_upload::CommunitySnapshotVersion;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use tokio::sync::Mutex;

use crate::admin::{self, AdminError, ModDecision};
use crate::blob::BlobHash;
use crate::body_schema::Bodies;
use crate::db::now_unix;
use crate::ids::{CredentialId, ProposalId, ProposalMessageId};
use crate::state::AppState;

const MAX_MESSAGE_CHARS: usize = 2000;
const SNAPSHOT_MANIFEST_MAX_BYTES: u64 = 4096;
const MAX_EXPORT_NAME_BYTES: usize = 200;
const ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ROUND_TRIP_LUT_NODES: u64 = 2_000_000;

static SNAPSHOT_LOCK: Mutex<()> = Mutex::const_new(());

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
    #[error("the round-trip simulation timed out")]
    RoundTripTimeout,
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
    let nodes: u64 = profile
        .accepts
        .iter()
        .filter_map(|accepted| match accepted {
            AcceptedArtifact::Lut3d(lut) => Some(lut),
            AcceptedArtifact::Matrix3x3(_) | AcceptedArtifact::Curve1d(_) => None,
        })
        .flat_map(|lut| lut.sizes.iter())
        .map(|side| u64::from(*side).pow(3))
        .sum();
    if nodes > MAX_ROUND_TRIP_LUT_NODES {
        return Err(ProposalError::RoundTrip(
            "the profile's LUT grids exceed the simulation's node budget".to_owned(),
        ));
    }
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

async fn verify_fixture_upload(
    state: &AppState,
    credential: &CredentialId,
    donation: &FixtureDonation,
) -> Result<(), ProposalError> {
    let Some(source) = &donation.source else {
        return Ok(());
    };
    if source != &looklab_schema::fixture_source(donation.sha256) {
        return Err(ProposalError::InvalidFixture(
            "the source does not name this file's digest".to_owned(),
        ));
    }
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT file, size FROM fixture_uploads WHERE sha256 = ? AND credential_id = ?",
    )
    .bind(donation.sha256.as_bytes().as_slice())
    .bind(credential.as_str())
    .fetch_optional(&state.db)
    .await?;
    let Some((file, size)) = row else {
        return Err(ProposalError::InvalidFixture(
            "the fixture was not uploaded by this credential".to_owned(),
        ));
    };
    if file != donation.file || u64::try_from(size).ok() != Some(donation.size) {
        return Err(ProposalError::InvalidFixture(
            "the uploaded fixture does not match the donation".to_owned(),
        ));
    }
    let present = state
        .blobs
        .get(BlobHash::from_bytes(*donation.sha256.as_bytes()))
        .map_err(|_| {
            ProposalError::InvalidFixture("the uploaded fixture is unreadable".to_owned())
        })?
        .is_some_and(|bytes| bytes.len() as u64 == donation.size);
    if !present {
        return Err(ProposalError::InvalidFixture(
            "the uploaded fixture is missing or the wrong size".to_owned(),
        ));
    }
    Ok(())
}

async fn round_trip(payload: String) -> Result<RoundTripReport, ProposalError> {
    let task = tokio::task::spawn_blocking(move || round_trip_report(&payload));
    match tokio::time::timeout(ROUND_TRIP_TIMEOUT, task).await {
        Ok(joined) => joined.map_err(|_| ProposalError::Corrupt)?,
        Err(_) => Err(ProposalError::RoundTripTimeout),
    }
}

pub async fn submit(
    state: &AppState,
    credential: &CredentialId,
    submission: Submission,
) -> Result<Submitted, ProposalError> {
    let validated = validate_proposal(submission.kind, &submission.payload)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    for donation in &submission.fixtures {
        donation
            .validate()
            .map_err(|errors| ProposalError::InvalidFixture(join_errors(&errors)))?;
        verify_fixture_upload(state, credential, donation).await?;
    }
    if let ValidatedPayload::Body(body) = &validated
        && !mapping_status_supported(body, &submission.fixtures)
    {
        return Err(ProposalError::InvalidPayload(
            "mapping_status: a fixture-backed body needs a fixture donation for it".to_owned(),
        ));
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
        ValidatedPayload::InterfaceProfile(_) => {
            Some(round_trip(submission.payload.clone()).await?)
        }
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
    .bind(round_trip_json)
    .execute(&state.db)
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
    let donations = approved_donations(db).await?;
    let published: Vec<FixtureDonation> = donations.iter().map(published_donation).collect();
    Ok(fixture_manifest_toml(&published))
}

fn published_donation(donation: &FixtureDonation) -> FixtureDonation {
    let mut published = donation.clone();
    if published.source.is_some() {
        published.placement = Placement::Lfs;
        published.source = None;
    }
    published
}

async fn approved_donations(db: &SqlitePool) -> Result<Vec<FixtureDonation>, ProposalError> {
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
        donations.extend(evidence_donations(&evidence));
    }
    Ok(donations)
}

fn evidence_donations(evidence: &[Evidence]) -> Vec<FixtureDonation> {
    evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::Fixture { donation } => Some(donation.clone()),
            Evidence::Confirmation { .. } | Evidence::Worker { .. } => None,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotRecord {
    pub version: u64,
    pub content_hash: String,
    pub files: usize,
}

struct PreparedSnapshot {
    version: u64,
    content_hash: String,
    files: usize,
    staging: PathBuf,
    target: PathBuf,
}

impl PreparedSnapshot {
    fn record(&self) -> SnapshotRecord {
        SnapshotRecord {
            version: self.version,
            content_hash: self.content_hash.clone(),
            files: self.files,
        }
    }
}

pub async fn record_decision(
    state: &AppState,
    moderator: &CredentialId,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
) -> Result<Option<SnapshotRecord>, ProposalError> {
    let row: Option<(String, String, String)> =
        sqlx::query_as("SELECT type, payload, evidence FROM proposals WHERE id = ?")
            .bind(proposal.as_str())
            .fetch_optional(&state.db)
            .await?;
    let Some((kind, payload, evidence)) = row else {
        return Err(ProposalError::UnknownProposal);
    };
    let kind = kind
        .parse::<ProposalKind>()
        .map_err(|_| ProposalError::Corrupt)?;
    if decision != ModDecision::Approve || !kind.contributes_document() {
        admin::decide(&state.db, moderator, proposal, decision, reason).await?;
        return Ok(None);
    }
    let evidence: Vec<Evidence> =
        serde_json::from_str(&evidence).map_err(|_| ProposalError::Corrupt)?;
    let _guard = SNAPSHOT_LOCK.lock().await;
    publish_pending_locked(state).await?;
    let prepared = prepare_snapshot(state, kind, &payload, &evidence_donations(&evidence)).await?;
    admin::decide_recording(
        &state.db,
        moderator,
        proposal,
        decision,
        reason,
        Some(admin::Publication {
            version: prepared.version,
            content_hash: &prepared.content_hash,
        }),
    )
    .await?;
    publish_prepared(state, proposal, &prepared).await?;
    Ok(Some(prepared.record()))
}

/// # Errors
/// A pending publication that cannot be reproduced, or a filesystem failure.
pub(crate) async fn publish_pending(state: &AppState) -> Result<(), ProposalError> {
    let _guard = SNAPSHOT_LOCK.lock().await;
    publish_pending_locked(state).await
}

async fn publish_pending_locked(state: &AppState) -> Result<(), ProposalError> {
    loop {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, type FROM proposals WHERE publication = 'pending'
             ORDER BY published_version, created_at LIMIT 1",
        )
        .fetch_optional(&state.db)
        .await?;
        let Some((id, kind)) = row else {
            return Ok(());
        };
        let proposal = ProposalId::try_from(id).map_err(|_| ProposalError::Corrupt)?;
        let kind = kind
            .parse::<ProposalKind>()
            .map_err(|_| ProposalError::Corrupt)?;
        let (payload, evidence, version, hash): (String, String, i64, String) = sqlx::query_as(
            "SELECT payload, evidence, published_version, published_hash
             FROM proposals WHERE id = ?",
        )
        .bind(proposal.as_str())
        .fetch_one(&state.db)
        .await?;
        let version = CommunitySnapshotVersion::new(
            u64::try_from(version).map_err(|_| ProposalError::Corrupt)?,
        );
        let hash = Sha256Hex::parse(&hash).map_err(|_| ProposalError::Corrupt)?;
        let evidence: Vec<Evidence> =
            serde_json::from_str(&evidence).map_err(|_| ProposalError::Corrupt)?;
        let prepared = match recover_prepared(state, version, hash)? {
            Some(prepared) => prepared,
            None => {
                let prepared =
                    prepare_snapshot(state, kind, &payload, &evidence_donations(&evidence)).await?;
                if prepared.version != version.get()
                    || Sha256Hex::parse(&prepared.content_hash)
                        .map_err(|_| ProposalError::Corrupt)?
                        != hash
                {
                    return Err(ProposalError::Snapshot(
                        "a pending publication cannot be reproduced".to_owned(),
                    ));
                }
                prepared
            }
        };
        publish_prepared(state, &proposal, &prepared).await?;
    }
}

fn recover_prepared(
    state: &AppState,
    version: CommunitySnapshotVersion,
    hash: Sha256Hex,
) -> Result<Option<PreparedSnapshot>, ProposalError> {
    let target = state.config.snapshot_dir.join(version.to_string());
    if !target.is_dir() {
        return Ok(None);
    }
    let (found, documents) = previous_snapshot(&state.config.snapshot_dir)?;
    if found != version.get() {
        return Ok(None);
    }
    let files: Vec<(String, Vec<u8>)> = documents.into_iter().collect();
    let computed_hash = content_hash(&files);
    if Sha256Hex::parse(&computed_hash).map_err(|_| ProposalError::Corrupt)? != hash {
        return Ok(None);
    }
    Ok(Some(PreparedSnapshot {
        version: found,
        content_hash: computed_hash,
        files: files.len(),
        staging: target.clone(),
        target,
    }))
}

async fn publish_prepared(
    state: &AppState,
    proposal: &ProposalId,
    prepared: &PreparedSnapshot,
) -> Result<(), ProposalError> {
    if prepared.staging != prepared.target {
        std::fs::rename(&prepared.staging, &prepared.target)
            .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    }
    let reloaded = Bodies::load(&state.config.bodies_dir, &state.config.snapshot_dir);
    let bodies = match reloaded {
        Ok(bodies) => bodies,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&prepared.target);
            return Err(ProposalError::Snapshot(error.to_string()));
        }
    };
    *state.bodies.write().await = bodies;
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE proposals SET publication = 'published' WHERE id = ?")
        .bind(proposal.as_str())
        .execute(&mut *tx)
        .await?;
    crate::store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn prepare_snapshot(
    state: &AppState,
    kind: ProposalKind,
    payload: &str,
    extra: &[FixtureDonation],
) -> Result<PreparedSnapshot, ProposalError> {
    let validated = validate_proposal(kind, payload)
        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
    if let ValidatedPayload::Body(body) = &validated
        && !mapping_status_supported(body, extra)
    {
        return Err(ProposalError::InvalidPayload(
            "mapping_status: a fixture-backed body needs a fixture donation for it".to_owned(),
        ));
    }
    let dir = &state.config.snapshot_dir;
    let (previous_version, mut documents) = previous_snapshot(dir)?;
    if previous_version == 0 && documents.is_empty() {
        documents = baseline_documents(state)?;
    }
    for (path, bytes) in documents_for(state, &validated, payload, &documents)? {
        documents.insert(path, bytes);
    }
    let mut donations = approved_donations(&state.db).await?;
    donations.extend(extra.iter().cloned());
    dedupe_donations(&mut donations);
    let mut attachments: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut published = Vec::with_capacity(donations.len());
    for donation in &donations {
        let entry = published_donation(donation);
        published.push(entry.clone());
        let Some(source) = &donation.source else {
            continue;
        };
        donation.validate().map_err(|errors| {
            ProposalError::Snapshot(format!(
                "a donated fixture is not publishable: {}",
                join_errors(&errors)
            ))
        })?;
        if source != &looklab_schema::fixture_source(donation.sha256) {
            return Err(ProposalError::Snapshot(
                "a donated fixture source does not name its digest".to_owned(),
            ));
        }
        let bytes = state
            .blobs
            .get(BlobHash::from_bytes(*donation.sha256.as_bytes()))
            .map_err(|_| ProposalError::Snapshot("a donated fixture is unreadable".to_owned()))?
            .ok_or_else(|| {
                ProposalError::Snapshot(format!("the fixture {} is missing", donation.file))
            })?;
        if bytes.len() as u64 != donation.size {
            return Err(ProposalError::Snapshot(format!(
                "the fixture {} does not match its recorded size",
                donation.file
            )));
        }
        let file = FixtureFileName::parse(&donation.file).map_err(|_| {
            ProposalError::Snapshot(format!(
                "the fixture {} is not a plain file name",
                donation.file
            ))
        })?;
        let Some(path) = entry.placement.snapshot_path(file) else {
            return Err(ProposalError::Snapshot(format!(
                "the fixture {} has no published placement",
                donation.file
            )));
        };
        match attachments.get(&path) {
            Some(existing) if existing != &bytes => {
                return Err(ProposalError::Snapshot(format!(
                    "two donations share the file name {}",
                    donation.file
                )));
            }
            Some(_) => {}
            None => {
                attachments.insert(path, bytes);
            }
        }
    }
    documents.insert(
        FIXTURE_MANIFEST.to_owned(),
        fixture_manifest_toml(&published).into_bytes(),
    );
    let version = previous_version + 1;
    let staging = dir.join(format!("v{version}.tmp"));
    let target = dir.join(format!("v{version}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    }
    let files: Vec<(String, Vec<u8>)> = documents
        .iter()
        .map(|(path, bytes)| (path.clone(), bytes.clone()))
        .collect();
    let manifest = SnapshotManifest {
        version,
        content_hash: content_hash(&files),
    };
    let manifest_json = serde_json::to_string(&manifest).map_err(|_| ProposalError::Corrupt)?;
    std::fs::create_dir_all(&staging)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    for (relative, bytes) in documents.iter().chain(attachments.iter()) {
        let file = staging.join(relative);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
        }
        std::fs::write(&file, bytes).map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    }
    std::fs::write(staging.join(SNAPSHOT_MANIFEST), manifest_json)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    let failures = looklab_schema::validate_snapshot_dir(&staging)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    if !failures.is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(ProposalError::Snapshot(format!(
            "the approved documents do not validate: {failures:?}"
        )));
    }
    Ok(PreparedSnapshot {
        version,
        content_hash: manifest.content_hash,
        files: files.len() + attachments.len(),
        staging,
        target,
    })
}

fn baseline_documents(state: &AppState) -> Result<BTreeMap<String, Vec<u8>>, ProposalError> {
    let root = state
        .config
        .bodies_dir
        .parent()
        .ok_or_else(|| ProposalError::Snapshot("bodies directory has no parent".to_owned()))?;
    let mut shipped = BTreeMap::new();
    for sub in ["bodies", "interfaces"] {
        collect_documents(&root.join(sub), sub, &mut shipped)?;
    }
    let mut documents = BTreeMap::new();
    for (path, bytes) in shipped {
        let text = std::str::from_utf8(&bytes).map_err(|_| ProposalError::Corrupt)?;
        let export = if path.starts_with("bodies/") && path.ends_with(".body.yaml") {
            let body = looklab_schema::validate_body(text).map_err(|errors| {
                ProposalError::Snapshot(format!(
                    "the shipped body {path} is invalid: {}",
                    join_errors(&errors)
                ))
            })?;
            body_export_path(&body.id)
        } else if path.starts_with("bodies/") {
            let schema = looklab_schema::validate_control_schema(text).map_err(|errors| {
                ProposalError::Snapshot(format!(
                    "the shipped schema {path} is invalid: {}",
                    join_errors(&errors)
                ))
            })?;
            schema_export_path(&schema.body)
        } else {
            let profile = looklab_schema::validate_interface_profile(text).map_err(|errors| {
                ProposalError::Snapshot(format!(
                    "the shipped interface {path} is invalid: {}",
                    join_errors(&errors)
                ))
            })?;
            interface_export_path(&profile.id)
        };
        documents.insert(export, bytes);
    }
    Ok(documents)
}

fn dedupe_donations(donations: &mut Vec<FixtureDonation>) {
    let mut seen = std::collections::HashSet::new();
    donations.retain(|donation| seen.insert((donation.file.clone(), donation.sha256.to_string())));
}

fn documents_for(
    state: &AppState,
    validated: &ValidatedPayload,
    payload: &str,
    base: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<(String, Vec<u8>)>, ProposalError> {
    let document = match validated {
        ValidatedPayload::Body(body) => vec![(
            export_name(body_export_path(&body.id))?,
            payload.as_bytes().to_vec(),
        )],
        ValidatedPayload::SchemaChange(schema) => vec![(
            export_name(schema_export_path(&schema.body))?,
            payload.as_bytes().to_vec(),
        )],
        ValidatedPayload::InterfaceProfile(profile) => vec![(
            export_name(interface_export_path(&profile.id))?,
            payload.as_bytes().to_vec(),
        )],
        ValidatedPayload::Mapping(mapping) => {
            let path = export_name(schema_export_path(&mapping.body))?;
            let schema = match base.get(&path) {
                Some(bytes) => {
                    String::from_utf8(bytes.clone()).map_err(|_| ProposalError::Corrupt)?
                }
                None => {
                    let file = state
                        .config
                        .bodies_dir
                        .join(format!("{}.yaml", mapping.body.as_str().replace('/', "-")));
                    let text = std::fs::read_to_string(&file).map_err(|_| {
                        ProposalError::Snapshot(format!("no control schema for {}", mapping.body))
                    })?;
                    let shipped = looklab_schema::validate_control_schema(&text)
                        .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
                    if shipped.body != mapping.body {
                        return Err(ProposalError::Snapshot(format!(
                            "{} belongs to another body",
                            file.display()
                        )));
                    }
                    text
                }
            };
            let updated = apply_sim_mapping(
                &schema,
                mapping.epoch,
                &mapping.firmware_from,
                mapping.firmware_to.as_ref(),
                &mapping.native_sim(),
            )
            .map_err(|errors| ProposalError::InvalidPayload(join_errors(&errors)))?;
            vec![(path, updated.into_bytes())]
        }
        ValidatedPayload::LutLook(_)
        | ValidatedPayload::EpochSplit(_)
        | ValidatedPayload::LookSplit(_)
        | ValidatedPayload::GateOverride(_) => {
            return Err(ProposalError::Snapshot(
                "this proposal kind has no document".to_owned(),
            ));
        }
    };
    Ok(document)
}

fn export_name(path: String) -> Result<String, ProposalError> {
    let name = path.rsplit('/').next().unwrap_or(&path);
    if name.len() > MAX_EXPORT_NAME_BYTES {
        return Err(ProposalError::Snapshot(
            "the export file name is too long".to_owned(),
        ));
    }
    Ok(path)
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
    if let Ok(bytes) = std::fs::read(path.join(FIXTURE_MANIFEST)) {
        documents.insert(FIXTURE_MANIFEST.to_owned(), bytes);
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
    let failures = looklab_schema::validate_snapshot_dir(&path)
        .map_err(|error| ProposalError::Snapshot(error.to_string()))?;
    if !failures.is_empty() {
        return Err(ProposalError::Snapshot(format!(
            "the previous snapshot does not validate: {failures:?}"
        )));
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

pub(crate) fn rfc3339(seconds: i64) -> String {
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
        let payload = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");
        let report = round_trip_report(payload).expect("report");
        assert_eq!(report.ccts[0].cct, 6500.0);
        assert!(!report.ccts.is_empty());
        assert!(report.probes > 0);
    }
}
