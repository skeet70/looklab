// SPDX-License-Identifier: AGPL-3.0-or-later
//! Grants, endorsements, root admissions, moderation state and version manifests,
//! shared by the HTTP API and `looklab-cli admin` ([docs/05 §3 §7 §11], [docs/08 §6]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use looklab_export::card::{CardMeta, CardOptions, card_status, write_card};
use looklab_model::LookDocument;
use looklab_model::file::{
    BaseReference, BaseStatus, CameraRendering, LookKind, LookPin, LookVersion, LutEmulation,
};
use looklab_publish::RELEASE_ARTIFACT;
use looklab_schema::{
    ArtifactBinding, ArtifactRole, DependencyGeneration, DependencyKind, DependencyRef,
    DependencyTarget, EligibilitySnapshotId, FamilyTargetId, LookId, ManualCheckReview,
    PublishedTargetId, Sha256Hex, SubjectKey, TargetBasis, TargetEligibility,
    TargetEligibilityStatus, TargetVersion,
};
use looklab_schema::{ModelSubject, TargetEligibilityReason};
use looklab_trust::{
    Actor, ArtifactHash, CredentialId as TrustCredentialId, CredentialState, Endorsement,
    EvidenceRef, Grant, Reputation, Role, RootAdmission as TrustRootAdmission, RootDataKind,
    RootSession, RootTarget, Scope, ScopeRule, Tier as TrustTier, TrustError, TrustParams,
    TrustState, VersionId, admission_eligible,
};
use looklab_upload::{ContributionKind as UploadKind, Intent, LookSlug};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::pool::PoolConnection;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, SqlitePool};

use crate::audit::{self, AuditKind};
use crate::blob::{BlobHash, BlobStore, FsBlobStore};
use crate::db::now_unix;
use crate::ids::{
    ContributionId, ContributionImageId, CredentialId, EndorsementId, GrantId, LookVersionId,
    ModerationDecisionId, ProposalId, RootAdmissionId,
};
use crate::model_store::{
    AcceptedFamilyVersionStatus, JobLease, NumericalSources, SubjectRevisionRole,
};
use crate::store::ContributionStatus;
use crate::trust::Tier;

const MAX_REASON_CHARS: usize = 2000;
const MAX_SCOPE_RULES: usize = 64;
const MAX_EVIDENCE_REFS: usize = 20;
const MAX_EVIDENCE_CHARS: usize = 200;
const MAX_ARTIFACTS: usize = 256;
const MAX_ARTIFACT_NAME: usize = 128;
const MAX_AUDIT_PAGE: i64 = 500;
const MAX_LIST_PAGE: i64 = 100;
const MAX_REVERT_REJECTIONS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    #[error("trust state: {0}")]
    Trust(#[from] TrustError),
    #[error("the credential is unknown")]
    UnknownCredential,
    #[error("the credential may not perform this action")]
    NotAuthorized,
    #[error("the reason is empty or too long")]
    InvalidReason,
    #[error("the evidence reference is empty or too long")]
    InvalidEvidence,
    #[error("the scope must list at least one rule")]
    EmptyScope,
    #[error("the scope has too many rules")]
    ScopeTooLarge,
    #[error("the contribution is unknown")]
    UnknownContribution,
    #[error("the contribution is withdrawn")]
    ContributionWithdrawn,
    #[error("the contribution is not part of the Look's evidence")]
    ContributionNotInLookEvidence,
    #[error("the contribution kind cannot root data")]
    IneligibleContribution,
    #[error("no active {0} record")]
    NoActiveRecord(&'static str),
    #[error("the root admission is unknown")]
    UnknownRootAdmission,
    #[error("the root admission {0} is revoked")]
    RootRevoked(RootAdmissionId),
    #[error("the proposal payload is invalid: {0}")]
    InvalidProposal(String),
    #[error("the version is unknown")]
    UnknownVersion,
    #[error("the version is not a published release")]
    VersionNotPublished,
    #[error("too many contribution rejections")]
    TooManyRejections,
    #[error("the manifest has no artifacts")]
    EmptyManifest,
    #[error("an artifact is invalid: {0}")]
    InvalidArtifact(String),
    #[error("the version manifest already exists")]
    ManifestExists,
    #[error("the proposal is unknown")]
    UnknownProposal,
    #[error("the proposal is already decided")]
    ProposalDecided,
    #[error("the Look is unknown")]
    UnknownLook,
    #[error("the base would make the LUT depend on itself")]
    CyclicBaseDependency,
    #[error("the Look is {0}")]
    LookState(&'static str),
    #[error("the stored record is not readable")]
    Corrupt,
    #[error("the model subject changed during the action")]
    Conflict,
    #[error("model store: {0}")]
    Model(#[from] crate::model_store::StoreError),
}

pub fn trust_id(id: &CredentialId) -> Result<TrustCredentialId, AdminError> {
    let uuid = uuid::Uuid::parse_str(id.as_str()).map_err(|_| AdminError::Corrupt)?;
    Ok(TrustCredentialId::from_u128(uuid.as_u128()))
}

pub fn local_credential(id: TrustCredentialId) -> Result<CredentialId, AdminError> {
    let uuid = uuid::Uuid::from_u128(id.to_u128());
    CredentialId::try_from(uuid.hyphenated().to_string()).map_err(|_| AdminError::Corrupt)
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(crate) fn normalize_reason(reason: &str) -> Result<String, AdminError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > MAX_REASON_CHARS {
        return Err(AdminError::InvalidReason);
    }
    Ok(reason.to_owned())
}

fn normalize_scope(scope: &Scope) -> Result<Scope, AdminError> {
    if scope.rules.is_empty() {
        return Err(AdminError::EmptyScope);
    }
    if scope.rules.len() > MAX_SCOPE_RULES {
        return Err(AdminError::ScopeTooLarge);
    }
    Ok(Scope::new(scope.rules.iter().cloned()))
}

pub fn scope_json(scope: &Scope) -> Result<String, AdminError> {
    serde_json::to_string(scope).map_err(|_| AdminError::Corrupt)
}

pub fn parse_scope(text: Option<&str>) -> Result<Option<Scope>, AdminError> {
    let Some(text) = text else {
        return Ok(None);
    };
    let scope: Scope = serde_json::from_str(text).map_err(|_| AdminError::Corrupt)?;
    Ok(Some(Scope::new(scope.rules)))
}

fn normalize_evidence(evidence: Vec<String>) -> Result<Vec<EvidenceRef>, AdminError> {
    if evidence.len() > MAX_EVIDENCE_REFS {
        return Err(AdminError::InvalidEvidence);
    }
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    for reference in evidence {
        let reference = reference.trim();
        if reference.is_empty() || reference.chars().count() > MAX_EVIDENCE_CHARS {
            return Err(AdminError::InvalidEvidence);
        }
        if seen.insert(reference.to_owned()) {
            refs.push(EvidenceRef::new(reference));
        }
    }
    Ok(refs)
}

enum EvidenceTarget {
    Contribution(ContributionId),
    Image(ContributionImageId),
    Bare(String),
}

fn parse_evidence_target(reference: &str) -> Result<Option<EvidenceTarget>, AdminError> {
    if let Some(id) = reference.strip_prefix("contribution:") {
        return ContributionId::try_from(id)
            .map(|id| Some(EvidenceTarget::Contribution(id)))
            .map_err(|_| AdminError::InvalidEvidence);
    }
    if let Some(id) = reference.strip_prefix("image:") {
        return ContributionImageId::try_from(id)
            .map(|id| Some(EvidenceTarget::Image(id)))
            .map_err(|_| AdminError::InvalidEvidence);
    }
    Ok(uuid::Uuid::parse_str(reference)
        .ok()
        .map(|id| EvidenceTarget::Bare(id.hyphenated().to_string())))
}

async fn reject_deferred_evidence(
    conn: &mut SqliteConnection,
    evidence: &[EvidenceRef],
) -> Result<(), AdminError> {
    for reference in evidence {
        let Some(target) = parse_evidence_target(reference.as_str())? else {
            continue;
        };
        let class: Option<String> = match target {
            EvidenceTarget::Contribution(id) => {
                sqlx::query_scalar("SELECT render_class FROM contributions WHERE id = ?")
                    .bind(id.as_str())
                    .fetch_optional(&mut *conn)
                    .await?
            }
            EvidenceTarget::Image(id) => {
                sqlx::query_scalar(
                    "SELECT c.render_class FROM contribution_images i
                     JOIN contributions c ON c.id = i.contribution_id
                     WHERE i.id = ?",
                )
                .bind(id.as_str())
                .fetch_optional(&mut *conn)
                .await?
            }
            EvidenceTarget::Bare(id) => {
                let contribution: Option<String> =
                    sqlx::query_scalar("SELECT render_class FROM contributions WHERE id = ?")
                        .bind(&id)
                        .fetch_optional(&mut *conn)
                        .await?;
                match contribution {
                    Some(class) => Some(class),
                    None => {
                        sqlx::query_scalar(
                            "SELECT c.render_class FROM contribution_images i
                             JOIN contributions c ON c.id = i.contribution_id
                             WHERE i.id = ?",
                        )
                        .bind(&id)
                        .fetch_optional(&mut *conn)
                        .await?
                    }
                }
            }
        };
        let Some(class) = class else {
            return Err(AdminError::InvalidEvidence);
        };
        if crate::store::render_class_from_db(&class)
            == Some(looklab_upload::RenderClass::Monochrome)
        {
            return Err(AdminError::IneligibleContribution);
        }
    }
    Ok(())
}

fn parse_tier(value: &str) -> Result<TrustTier, AdminError> {
    match value {
        "new" => Ok(TrustTier::New),
        "endorsed" => Ok(TrustTier::Endorsed),
        "seeder" => Ok(TrustTier::Seeder),
        _ => Err(AdminError::Corrupt),
    }
}

fn tier_str(tier: TrustTier) -> &'static str {
    match tier {
        TrustTier::New => "new",
        TrustTier::Endorsed => "endorsed",
        TrustTier::Seeder => "seeder",
    }
}

fn parse_grant(value: &str) -> Result<Grant, AdminError> {
    match value {
        "seeder" => Ok(Grant::Seeder),
        "moderator" => Ok(Grant::Moderator),
        "admin" => Ok(Grant::Admin),
        _ => Err(AdminError::Corrupt),
    }
}

pub(crate) fn grant_str(grant: Grant) -> &'static str {
    match grant {
        Grant::Seeder => "seeder",
        Grant::Moderator => "moderator",
        Grant::Admin => "admin",
    }
}

/// An internal credential id or the literal `cli`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct ActorId(String);

impl ActorId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn actor_text(actor: Actor) -> Result<ActorId, AdminError> {
    match actor {
        Actor::Cli => Ok(ActorId("cli".to_owned())),
        Actor::Credential(id) => Ok(ActorId(local_credential(id)?.as_str().to_owned())),
    }
}

async fn begin_write(db: &SqlitePool) -> Result<PoolConnection<Sqlite>, AdminError> {
    let mut conn = db.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    Ok(conn)
}

async fn finish<T>(
    conn: &mut SqliteConnection,
    result: Result<T, AdminError>,
) -> Result<T, AdminError> {
    match result {
        Ok(value) => {
            if let Err(error) = sqlx::query("COMMIT").execute(&mut *conn).await {
                let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                return Err(error.into());
            }
            Ok(value)
        }
        Err(error) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(error)
        }
    }
}

#[derive(Debug, Default)]
enum GrantAuthority {
    #[default]
    None,
    Unrestricted,
    Scoped(Vec<ScopeRule>),
}

fn merge_grant(authority: &mut GrantAuthority, scope: Option<Scope>) {
    match (scope, authority) {
        (None, slot) => *slot = GrantAuthority::Unrestricted,
        (Some(_), GrantAuthority::Unrestricted) => {}
        (Some(scope), GrantAuthority::Scoped(rules)) => rules.extend(scope.rules),
        (Some(scope), slot @ GrantAuthority::None) => *slot = GrantAuthority::Scoped(scope.rules),
    }
}

/// Seeder scope comes only from active seeder grants; endorsed scope only from the
/// latest active endorsement.
async fn trust_state(conn: &mut SqliteConnection) -> Result<TrustState, AdminError> {
    let mut grant_authority: HashMap<CredentialId, GrantAuthority> = HashMap::new();
    let grants = sqlx::query(
        "SELECT subject_credential, scope FROM grants
         WHERE revoked_at IS NULL AND role_or_tier = 'seeder'
         ORDER BY granted_at, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in grants {
        let subject = CredentialId::try_from(row.get::<String, _>("subject_credential"))
            .map_err(|_| AdminError::Corrupt)?;
        let scope = parse_scope(row.get::<Option<String>, _>("scope").as_deref())?;
        merge_grant(grant_authority.entry(subject).or_default(), scope);
    }

    let mut endorsement_scope: HashMap<CredentialId, Scope> = HashMap::new();
    let endorsements = sqlx::query(
        "SELECT subject_credential, scope FROM endorsements
         WHERE revoked_at IS NULL ORDER BY issued_at, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for row in endorsements {
        let subject = CredentialId::try_from(row.get::<String, _>("subject_credential"))
            .map_err(|_| AdminError::Corrupt)?;
        let scope = parse_scope(Some(row.get::<String, _>("scope").as_str()))?
            .ok_or(AdminError::Corrupt)?;
        endorsement_scope.insert(subject, scope);
    }

    let rows =
        sqlx::query("SELECT id, tier, roles, reputation FROM credentials WHERE status = 'active'")
            .fetch_all(&mut *conn)
            .await?;
    let mut state = TrustState::new();
    for row in rows {
        let id =
            CredentialId::try_from(row.get::<String, _>("id")).map_err(|_| AdminError::Corrupt)?;
        let tier = parse_tier(row.get::<String, _>("tier").as_str())?;
        let roles: BTreeSet<Role> = serde_json::from_str(row.get::<String, _>("roles").as_str())
            .map_err(|_| AdminError::Corrupt)?;
        let scope = if tier == TrustTier::Seeder {
            match grant_authority.get(&id) {
                Some(GrantAuthority::Scoped(rules)) => Some(Scope::new(rules.iter().cloned())),
                Some(GrantAuthority::Unrestricted) | Some(GrantAuthority::None) | None => None,
            }
        } else {
            endorsement_scope.get(&id).cloned()
        };
        state.insert(
            trust_id(&id)?,
            CredentialState {
                tier,
                roles,
                reputation: Reputation::saturating(row.get::<f64, _>("reputation")),
                scope,
            },
        );
    }
    Ok(state)
}

async fn update_tier(
    conn: &mut SqliteConnection,
    id: &CredentialId,
    state: &TrustState,
) -> Result<(), AdminError> {
    let record = state
        .credential(trust_id(id)?)
        .ok_or(AdminError::UnknownCredential)?;
    sqlx::query("UPDATE credentials SET tier = ? WHERE id = ?")
        .bind(tier_str(record.tier))
        .bind(id.as_str())
        .execute(&mut *conn)
        .await?;
    crate::model_events::standing_changed(conn, id, now_unix()).await?;
    Ok(())
}

async fn update_roles(
    conn: &mut SqliteConnection,
    id: &CredentialId,
    state: &TrustState,
) -> Result<(), AdminError> {
    let record = state
        .credential(trust_id(id)?)
        .ok_or(AdminError::UnknownCredential)?;
    let roles = serde_json::to_string(&record.roles).map_err(|_| AdminError::Corrupt)?;
    sqlx::query("UPDATE credentials SET roles = ? WHERE id = ?")
        .bind(roles)
        .bind(id.as_str())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn update_tier_and_reputation(
    conn: &mut SqliteConnection,
    id: &CredentialId,
    state: &TrustState,
) -> Result<(), AdminError> {
    let record = state
        .credential(trust_id(id)?)
        .ok_or(AdminError::UnknownCredential)?;
    sqlx::query("UPDATE credentials SET tier = ?, reputation = ? WHERE id = ?")
        .bind(tier_str(record.tier))
        .bind(record.reputation.value())
        .bind(id.as_str())
        .execute(&mut *conn)
        .await?;
    crate::model_events::standing_changed(conn, id, now_unix()).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct GrantRecord {
    pub id: GrantId,
    pub subject: CredentialId,
    pub subject_handle: Option<String>,
    pub grant: Grant,
    pub scope: Option<Scope>,
    pub scope_text: Option<String>,
    pub granted_by: ActorId,
    pub granted_by_handle: Option<String>,
    pub reason: String,
    pub granted_at: i64,
    pub revoked_at: Option<i64>,
}

impl GrantRecord {
    pub fn grant_name(&self) -> &'static str {
        grant_str(self.grant)
    }
}

const GRANT_COLUMNS: &str = "g.id, g.subject_credential, s.display_handle AS subject_handle, \
     g.role_or_tier, g.scope, g.granted_by, a.display_handle AS granted_by_handle, \
     g.reason, g.granted_at, g.revoked_at";

fn grant_from_row(row: &SqliteRow) -> Result<GrantRecord, AdminError> {
    let scope = parse_scope(row.get::<Option<String>, _>("scope").as_deref())?;
    Ok(GrantRecord {
        id: GrantId::try_from(row.get::<String, _>("id")).map_err(|_| AdminError::Corrupt)?,
        subject: CredentialId::try_from(row.get::<String, _>("subject_credential"))
            .map_err(|_| AdminError::Corrupt)?,
        subject_handle: row.get("subject_handle"),
        grant: parse_grant(row.get::<String, _>("role_or_tier").as_str())?,
        scope_text: scope.as_ref().map(Scope::render),
        scope,
        granted_by: ActorId(row.get("granted_by")),
        granted_by_handle: row.get("granted_by_handle"),
        reason: row.get("reason"),
        granted_at: row.get("granted_at"),
        revoked_at: row.get("revoked_at"),
    })
}

async fn grant_by_id(db: &SqlitePool, id: &GrantId) -> Result<GrantRecord, AdminError> {
    let sql = format!(
        "SELECT {GRANT_COLUMNS} FROM grants g
         LEFT JOIN credentials s ON s.id = g.subject_credential
         LEFT JOIN credentials a ON a.id = g.granted_by
         WHERE g.id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(id.as_str())
        .fetch_optional(db)
        .await?
        .ok_or(AdminError::NoActiveRecord("grant"))?;
    grant_from_row(&row)
}

#[derive(Debug, Clone, Serialize)]
pub struct GrantPage {
    pub grants: Vec<GrantRecord>,
    pub next_after: Option<GrantId>,
}

pub async fn list_grants(
    db: &SqlitePool,
    after: Option<&GrantId>,
    limit: i64,
) -> Result<GrantPage, AdminError> {
    let limit = limit.clamp(1, MAX_LIST_PAGE);
    let sql = format!(
        "SELECT {GRANT_COLUMNS} FROM grants g
         LEFT JOIN credentials s ON s.id = g.subject_credential
         LEFT JOIN credentials a ON a.id = g.granted_by
         WHERE (? IS NULL OR g.id > ?)
         ORDER BY g.id LIMIT ?"
    );
    let after = after.map(GrantId::as_str);
    let rows = sqlx::query(&sql)
        .bind(after)
        .bind(after)
        .bind(limit + 1)
        .fetch_all(db)
        .await?;
    let mut grants = rows
        .iter()
        .map(grant_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = i64::try_from(grants.len()).unwrap_or(i64::MAX) > limit;
    if has_more {
        grants.pop();
    }
    let next_after = if has_more {
        grants.last().map(|grant| grant.id.clone())
    } else {
        None
    };
    Ok(GrantPage { grants, next_after })
}

pub async fn grant_credential(
    db: &SqlitePool,
    actor: Actor,
    subject: &CredentialId,
    grant: Grant,
    scope: Option<Scope>,
    reason: &str,
) -> Result<GrantRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let scope = match scope {
        Some(scope) => Some(normalize_scope(&scope)?),
        None => None,
    };
    let subject_trust = trust_id(subject)?;
    let id = GrantId::new();
    let granted_by = actor_text(actor)?;
    let scope_text_db = scope.as_ref().map(scope_json).transpose()?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let next = trust_state(&mut conn).await?;
        let (applied, event) =
            next.apply_grant(actor, subject_trust, grant, &TrustParams::default())?;
        sqlx::query(
            "INSERT INTO grants
             (id, subject_credential, role_or_tier, scope, granted_by, reason, granted_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.as_str())
        .bind(subject.as_str())
        .bind(grant_str(grant))
        .bind(scope_text_db.as_deref())
        .bind(granted_by.as_str())
        .bind(&reason)
        .bind(now_unix())
        .execute(&mut *conn)
        .await?;
        match grant {
            Grant::Seeder => update_tier_and_reputation(&mut conn, subject, &applied).await?,
            Grant::Moderator | Grant::Admin => update_roles(&mut conn, subject, &applied).await?,
        }
        let (kind, payload) = audit::trust_event(&event)?;
        audit::insert(&mut conn, kind, &payload).await?;
        Ok(())
    }
    .await;
    finish(&mut conn, result).await?;
    grant_by_id(db, &id).await
}

pub async fn revoke_grant(
    db: &SqlitePool,
    actor: Actor,
    subject: &CredentialId,
    grant: Grant,
    reason: &str,
) -> Result<GrantRecord, AdminError> {
    normalize_reason(reason)?;
    let subject_trust = trust_id(subject)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let state = trust_state(&mut conn).await?;
        let (applied, event) = state.revoke_grant(actor, subject_trust, grant)?;
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT id FROM grants
             WHERE subject_credential = ? AND role_or_tier = ? AND revoked_at IS NULL
             ORDER BY granted_at DESC, id DESC LIMIT 1",
        )
        .bind(subject.as_str())
        .bind(grant_str(grant))
        .fetch_optional(&mut *conn)
        .await?;
        let Some((grant_id,)) = row else {
            return Err(AdminError::NoActiveRecord("grant"));
        };
        let id = GrantId::try_from(grant_id).map_err(|_| AdminError::Corrupt)?;
        sqlx::query("UPDATE grants SET revoked_at = ? WHERE id = ?")
            .bind(now_unix())
            .bind(id.as_str())
            .execute(&mut *conn)
            .await?;
        match grant {
            Grant::Seeder => update_tier(&mut conn, subject, &applied).await?,
            Grant::Moderator | Grant::Admin => update_roles(&mut conn, subject, &applied).await?,
        }
        let (kind, payload) = audit::trust_event(&event)?;
        audit::insert(&mut conn, kind, &payload).await?;
        Ok(id)
    }
    .await;
    let id = finish(&mut conn, result).await?;
    grant_by_id(db, &id).await
}

#[derive(Debug, Clone, Serialize)]
pub struct EndorsementRecord {
    pub id: EndorsementId,
    pub issuer: CredentialId,
    pub issuer_handle: Option<String>,
    pub subject: CredentialId,
    pub subject_handle: Option<String>,
    pub scope: Scope,
    pub scope_text: String,
    pub reason: String,
    pub evidence: Vec<String>,
    pub issued_at: i64,
    pub revoked_at: Option<i64>,
}

const ENDORSEMENT_COLUMNS: &str = "e.id, e.issuer_credential, i.display_handle AS issuer_handle, \
     e.subject_credential, s.display_handle AS subject_handle, e.scope, e.reason, \
     e.evidence_refs, e.issued_at, e.revoked_at";

fn endorsement_from_row(row: &SqliteRow) -> Result<EndorsementRecord, AdminError> {
    let scope =
        parse_scope(Some(row.get::<String, _>("scope").as_str()))?.ok_or(AdminError::Corrupt)?;
    let evidence: Vec<String> =
        serde_json::from_str(row.get::<String, _>("evidence_refs").as_str())
            .map_err(|_| AdminError::Corrupt)?;
    Ok(EndorsementRecord {
        id: EndorsementId::try_from(row.get::<String, _>("id")).map_err(|_| AdminError::Corrupt)?,
        issuer: CredentialId::try_from(row.get::<String, _>("issuer_credential"))
            .map_err(|_| AdminError::Corrupt)?,
        issuer_handle: row.get("issuer_handle"),
        subject: CredentialId::try_from(row.get::<String, _>("subject_credential"))
            .map_err(|_| AdminError::Corrupt)?,
        subject_handle: row.get("subject_handle"),
        scope_text: scope.render(),
        scope,
        reason: row.get("reason"),
        evidence,
        issued_at: row.get("issued_at"),
        revoked_at: row.get("revoked_at"),
    })
}

async fn endorsement_by_id(
    db: &SqlitePool,
    id: &EndorsementId,
) -> Result<EndorsementRecord, AdminError> {
    let sql = format!(
        "SELECT {ENDORSEMENT_COLUMNS} FROM endorsements e
         LEFT JOIN credentials i ON i.id = e.issuer_credential
         LEFT JOIN credentials s ON s.id = e.subject_credential
         WHERE e.id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(id.as_str())
        .fetch_optional(db)
        .await?
        .ok_or(AdminError::NoActiveRecord("endorsement"))?;
    endorsement_from_row(&row)
}

#[derive(Debug, Clone, Serialize)]
pub struct EndorsementPage {
    pub endorsements: Vec<EndorsementRecord>,
    pub next_after: Option<EndorsementId>,
}

pub async fn list_endorsements(
    db: &SqlitePool,
    after: Option<&EndorsementId>,
    limit: i64,
) -> Result<EndorsementPage, AdminError> {
    let limit = limit.clamp(1, MAX_LIST_PAGE);
    let sql = format!(
        "SELECT {ENDORSEMENT_COLUMNS} FROM endorsements e
         LEFT JOIN credentials i ON i.id = e.issuer_credential
         LEFT JOIN credentials s ON s.id = e.subject_credential
         WHERE (? IS NULL OR e.id > ?)
         ORDER BY e.id LIMIT ?"
    );
    let after = after.map(EndorsementId::as_str);
    let rows = sqlx::query(&sql)
        .bind(after)
        .bind(after)
        .bind(limit + 1)
        .fetch_all(db)
        .await?;
    let mut endorsements = rows
        .iter()
        .map(endorsement_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    let has_more = i64::try_from(endorsements.len()).unwrap_or(i64::MAX) > limit;
    if has_more {
        endorsements.pop();
    }
    let next_after = if has_more {
        endorsements.last().map(|record| record.id.clone())
    } else {
        None
    };
    Ok(EndorsementPage {
        endorsements,
        next_after,
    })
}

pub async fn endorse(
    db: &SqlitePool,
    actor: Actor,
    subject: &CredentialId,
    scope: Scope,
    reason: &str,
    evidence: Vec<String>,
) -> Result<EndorsementRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let scope = normalize_scope(&scope)?;
    let evidence = normalize_evidence(evidence)?;
    let Actor::Credential(issuer_trust) = actor else {
        return Err(AdminError::NotAuthorized);
    };
    let issuer = local_credential(issuer_trust)?;
    let subject_trust = trust_id(subject)?;
    let id = EndorsementId::new();
    let evidence_json =
        serde_json::to_string(&evidence.iter().map(EvidenceRef::as_str).collect::<Vec<_>>())
            .map_err(|_| AdminError::Corrupt)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        reject_deferred_evidence(&mut conn, &evidence).await?;
        let state = trust_state(&mut conn).await?;
        let (applied, event) = state.apply_endorsement(
            actor,
            subject_trust,
            Endorsement {
                scope: scope.clone(),
                reason: reason.clone(),
                evidence: evidence.clone(),
            },
        )?;
        let now = now_unix();
        let previous: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM endorsements WHERE subject_credential = ? AND revoked_at IS NULL",
        )
        .bind(subject.as_str())
        .fetch_all(&mut *conn)
        .await?;
        if !previous.is_empty() {
            sqlx::query(
                "UPDATE endorsements SET revoked_at = ?
                 WHERE subject_credential = ? AND revoked_at IS NULL",
            )
            .bind(now)
            .bind(subject.as_str())
            .execute(&mut *conn)
            .await?;
            let previous = previous
                .into_iter()
                .map(|(id,)| EndorsementId::try_from(id).map_err(|_| AdminError::Corrupt))
                .collect::<Result<Vec<_>, _>>()?;
            let payload = json!({
                "event": "endorsement_superseded",
                "actor": actor_text(actor)?,
                "subject": subject,
                "endorsements": previous,
            });
            audit::insert(&mut conn, AuditKind::EndorsementSuperseded, &payload).await?;
        }
        sqlx::query(
            "INSERT INTO endorsements
             (id, issuer_credential, subject_credential, scope, reason, evidence_refs, issued_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.as_str())
        .bind(issuer.as_str())
        .bind(subject.as_str())
        .bind(scope_json(&scope)?)
        .bind(&reason)
        .bind(&evidence_json)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        update_tier(&mut conn, subject, &applied).await?;
        let (kind, payload) = audit::trust_event(&event)?;
        audit::insert(&mut conn, kind, &payload).await?;
        Ok(())
    }
    .await;
    finish(&mut conn, result).await?;
    endorsement_by_id(db, &id).await
}

#[derive(Debug, Clone, Serialize)]
pub struct RevokedEndorsement {
    pub subject: CredentialId,
    pub revoked: u64,
    pub ids: Vec<EndorsementId>,
}

fn scope_within(seeder_scope: Option<&Scope>, endorsement: &Scope) -> bool {
    seeder_scope.is_none_or(|scope| scope.contains(endorsement))
}

pub async fn revoke_endorsement(
    db: &SqlitePool,
    actor: Actor,
    subject: &CredentialId,
    reason: &str,
) -> Result<RevokedEndorsement, AdminError> {
    normalize_reason(reason)?;
    let subject_trust = trust_id(subject)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let state = trust_state(&mut conn).await?;
        let (is_admin, seeder_scope) = match actor {
            Actor::Cli => (true, None),
            Actor::Credential(actor_trust) => {
                let record = state
                    .credential(actor_trust)
                    .ok_or(AdminError::UnknownCredential)?;
                let is_admin = record.roles.contains(&Role::Admin);
                if !is_admin && record.tier != TrustTier::Seeder {
                    return Err(AdminError::NotAuthorized);
                }
                (is_admin, record.scope.clone())
            }
        };
        let rows = sqlx::query(
            "SELECT id, issuer_credential, scope FROM endorsements
             WHERE subject_credential = ? AND revoked_at IS NULL ORDER BY issued_at, id",
        )
        .bind(subject.as_str())
        .fetch_all(&mut *conn)
        .await?;
        if rows.is_empty() {
            return Err(AdminError::NoActiveRecord("endorsement"));
        }
        let mut ids = Vec::new();
        for row in rows {
            let id = EndorsementId::try_from(row.get::<String, _>("id"))
                .map_err(|_| AdminError::Corrupt)?;
            let issuer = CredentialId::try_from(row.get::<String, _>("issuer_credential"))
                .map_err(|_| AdminError::Corrupt)?;
            let scope = parse_scope(Some(row.get::<String, _>("scope").as_str()))?
                .ok_or(AdminError::Corrupt)?;
            let issued_by_actor = matches!(
                actor,
                Actor::Credential(actor_id) if local_credential(actor_id)? == issuer
            );
            if is_admin || issued_by_actor || scope_within(seeder_scope.as_ref(), &scope) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            return Err(AdminError::NotAuthorized);
        }
        let now = now_unix();
        for id in &ids {
            sqlx::query("UPDATE endorsements SET revoked_at = ? WHERE id = ?")
                .bind(now)
                .bind(id.as_str())
                .execute(&mut *conn)
                .await?;
        }
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM endorsements
             WHERE subject_credential = ? AND revoked_at IS NULL",
        )
        .bind(subject.as_str())
        .fetch_one(&mut *conn)
        .await?;
        let record = state
            .credential(subject_trust)
            .ok_or(AdminError::UnknownCredential)?;
        if remaining == 0 && record.tier == TrustTier::Endorsed {
            sqlx::query("UPDATE credentials SET tier = 'new' WHERE id = ?")
                .bind(subject.as_str())
                .execute(&mut *conn)
                .await?;
        }
        let payload = json!({
            "event": "endorsement_revoked",
            "actor": actor_text(actor)?,
            "subject": subject,
            "endorsements": ids,
        });
        audit::insert(&mut conn, AuditKind::EndorsementRevoked, &payload).await?;
        let revoked = u64::try_from(ids.len()).unwrap_or(u64::MAX);
        Ok((revoked, ids))
    }
    .await;
    let (revoked, ids) = finish(&mut conn, result).await?;
    Ok(RevokedEndorsement {
        subject: subject.clone(),
        revoked,
        ids,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct RootAdmissionRecord {
    pub id: RootAdmissionId,
    pub admitted_by: CredentialId,
    pub contribution_id: ContributionId,
    pub subject: CredentialId,
    pub scope: Scope,
    pub scope_text: String,
    pub reason: String,
    pub admitted_at: i64,
    pub revoked_at: Option<i64>,
}

fn parse_contribution_kind(value: &str) -> Result<UploadKind, AdminError> {
    match value {
        "library" => Ok(UploadKind::Library),
        "rerender" => Ok(UploadKind::Rerender),
        "pattern" => Ok(UploadKind::Pattern),
        "gradient" => Ok(UploadKind::Gradient),
        "chart" => Ok(UploadKind::Chart),
        _ => Err(AdminError::Corrupt),
    }
}

#[derive(sqlx::FromRow)]
struct ContributionAdmissionRow {
    credential_id: String,
    kind: String,
    status: String,
    body_id: String,
    target_look_id: Option<String>,
    intent: String,
    render_class: String,
}

pub async fn admit_root(
    db: &SqlitePool,
    actor: Actor,
    contribution_id: &ContributionId,
    scope: Scope,
    reason: &str,
) -> Result<RootAdmissionRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let scope = normalize_scope(&scope)?;
    let Actor::Credential(actor_trust) = actor else {
        return Err(AdminError::NotAuthorized);
    };
    let admitted_by = local_credential(actor_trust)?;
    let id = RootAdmissionId::new();
    let admitted_at = now_unix();
    let scope_text = scope_json(&scope)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let state = trust_state(&mut conn).await?;
        let actor_record = state
            .credential(actor_trust)
            .ok_or(AdminError::UnknownCredential)?;
        if actor_record.tier != TrustTier::Seeder {
            return Err(AdminError::NotAuthorized);
        }
        let row: Option<ContributionAdmissionRow> = sqlx::query_as(
            "SELECT credential_id, kind, status, body_id, target_look_id, intent, render_class
                 FROM contributions WHERE id = ?",
        )
        .bind(contribution_id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else {
            return Err(AdminError::UnknownContribution);
        };
        if crate::store::render_class_from_db(&row.render_class)
            != Some(looklab_upload::RenderClass::Colour)
        {
            return Err(AdminError::IneligibleContribution);
        }
        let kind = parse_contribution_kind(&row.kind)?;
        match ContributionStatus::from_db(&row.status).ok_or(AdminError::Corrupt)? {
            ContributionStatus::Pending | ContributionStatus::Accepted => {}
            ContributionStatus::Quarantined | ContributionStatus::Withdrawn => {
                return Err(AdminError::IneligibleContribution);
            }
        }
        let contribution_body =
            looklab_upload::BodyId::new(&row.body_id).map_err(|_| AdminError::Corrupt)?;
        let target_look = row
            .target_look_id
            .as_deref()
            .map(|look| LookSlug::try_from(look).map_err(|_| AdminError::Corrupt))
            .transpose()?;
        let intent = match row.intent.as_str() {
            "native" => Intent::Native,
            "recipe" => Intent::Recipe {
                target_look: target_look.clone().ok_or(AdminError::Corrupt)?,
            },
            "card" => return Err(AdminError::IneligibleContribution),
            _ => return Err(AdminError::Corrupt),
        };
        let subject = CredentialId::try_from(row.credential_id).map_err(|_| AdminError::Corrupt)?;
        let subject_trust = trust_id(&subject)?;
        let subject_record = state
            .credential(subject_trust)
            .ok_or(AdminError::UnknownCredential)?;
        let seeder = TrustRootAdmission {
            seeder: actor_trust,
            seeder_tier: TrustTier::Seeder,
            seeder_scope: actor_record.scope.clone(),
        };
        for rule in &scope.rules {
            let body = rule
                .body
                .clone()
                .unwrap_or_else(|| contribution_body.clone());
            if body != contribution_body {
                return Err(AdminError::IneligibleContribution);
            }
            let target = match &rule.look {
                Some(look) => {
                    if let Some(actual) = &target_look
                        && actual != look
                    {
                        return Err(AdminError::IneligibleContribution);
                    }
                    RootTarget::Look {
                        look: look.clone(),
                        body,
                    }
                }
                None => RootTarget::Body(body),
            };
            let session = RootSession {
                credential: subject_trust,
                tier: subject_record.tier,
                scope: subject_record.scope.clone(),
                anchored: true,
                kind: RootDataKind::Session(kind),
                target,
                covered_regions: BTreeSet::new(),
            };
            if !admission_eligible(&session, &seeder, &intent) {
                return Err(AdminError::IneligibleContribution);
            }
        }
        sqlx::query(
            "INSERT INTO root_admissions
             (id, admitted_by, contribution_id, scope, reason, admitted_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.as_str())
        .bind(admitted_by.as_str())
        .bind(contribution_id.as_str())
        .bind(&scope_text)
        .bind(&reason)
        .bind(admitted_at)
        .execute(&mut *conn)
        .await?;
        let payload = json!({
            "event": "root_admitted",
            "actor": admitted_by,
            "subject": subject,
            "contribution_id": contribution_id,
            "scope": scope,
            "reason": reason,
        });
        audit::insert(&mut conn, AuditKind::RootAdmitted, &payload).await?;
        crate::model_events::root_admitted(&mut conn, &scope, admitted_at).await?;
        enqueue_nightly_refit(&mut conn, admitted_at).await?;
        Ok(RootAdmissionRecord {
            id: id.clone(),
            admitted_by: admitted_by.clone(),
            contribution_id: contribution_id.clone(),
            subject,
            scope_text: scope.render(),
            scope: scope.clone(),
            reason: reason.clone(),
            admitted_at,
            revoked_at: None,
        })
    }
    .await;
    finish(&mut conn, result).await
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionRef {
    pub look: LookSlug,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct RootRevocation {
    pub root_admission: RootAdmissionId,
    pub new_versions: Vec<VersionRef>,
    pub regating: Vec<LookVersionId>,
    pub lut_dependents: Vec<VersionRef>,
}

pub async fn revoke_root(
    db: &SqlitePool,
    actor: Actor,
    root_admission: &RootAdmissionId,
    reason: &str,
) -> Result<RootRevocation, AdminError> {
    let reason = normalize_reason(reason)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let state = trust_state(&mut conn).await?;
        let (is_admin, actor_scope, actor_id) = match actor {
            Actor::Cli => (true, None, None),
            Actor::Credential(actor_trust) => {
                let record = state
                    .credential(actor_trust)
                    .ok_or(AdminError::UnknownCredential)?;
                let is_admin = record.roles.contains(&Role::Admin);
                if !is_admin && record.tier != TrustTier::Seeder {
                    return Err(AdminError::NotAuthorized);
                }
                (
                    is_admin,
                    record.scope.clone(),
                    Some(local_credential(actor_trust)?),
                )
            }
        };
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT scope, admitted_by, contribution_id FROM root_admissions
             WHERE id = ? AND revoked_at IS NULL",
        )
        .bind(root_admission.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        let Some((scope_text, admitted_by, contribution_id)) = row else {
            return Err(AdminError::UnknownRootAdmission);
        };
        let scope = parse_scope(Some(&scope_text))?.ok_or(AdminError::Corrupt)?;
        if !is_admin {
            let own = actor_id
                .as_ref()
                .is_some_and(|id| id.as_str() == admitted_by);
            if !own && !scope_within(actor_scope.as_ref(), &scope) {
                return Err(AdminError::NotAuthorized);
            }
        }
        let now = now_unix();
        sqlx::query("UPDATE root_admissions SET revoked_at = ? WHERE id = ?")
            .bind(now)
            .bind(root_admission.as_str())
            .execute(&mut *conn)
            .await?;
        crate::model_events::root_revoked(&mut conn, root_admission, now).await?;

        let mut looks: BTreeSet<LookSlug> = scope
            .rules
            .iter()
            .filter_map(|rule| rule.look.clone())
            .collect();
        let dependent: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT v.look FROM version_depends_on_root d
             JOIN look_versions v ON v.id = d.look_version_id
             WHERE d.root_admission_id = ?",
        )
        .bind(root_admission.as_str())
        .fetch_all(&mut *conn)
        .await?;
        for (look,) in dependent {
            looks.insert(LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?);
        }
        let target: Option<(Option<String>,)> =
            sqlx::query_as("SELECT target_look_id FROM contributions WHERE id = ?")
                .bind(&contribution_id)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some((Some(look),)) = target {
            looks.insert(LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?);
        }

        let changelog = format!("root admission {root_admission} revoked");
        let mut new_versions = Vec::new();
        for look in looks {
            let version = next_version(&mut conn, &look).await?;
            let id = LookVersionId::new();
            sqlx::query(
                "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
                 VALUES (?, ?, ?, 'provisional', ?, ?)",
            )
            .bind(id.as_str())
            .bind(look.as_str())
            .bind(version.get())
            .bind(&changelog)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            new_versions.push(VersionRef {
                look: look.clone(),
                version: version.get(),
            });
        }

        sqlx::query(
            "INSERT INTO version_regating (look_version_id, reason, marked_at)
             SELECT look_version_id, ?, ? FROM version_depends_on_root
             WHERE root_admission_id = ?
             ON CONFLICT (look_version_id) DO UPDATE
             SET reason = excluded.reason, marked_at = excluded.marked_at",
        )
        .bind(&changelog)
        .bind(now)
        .bind(root_admission.as_str())
        .execute(&mut *conn)
        .await?;
        let regating: Vec<(String,)> = sqlx::query_as(
            "SELECT d.look_version_id FROM version_depends_on_root d
             WHERE d.root_admission_id = ? ORDER BY d.look_version_id",
        )
        .bind(root_admission.as_str())
        .fetch_all(&mut *conn)
        .await?;
        let regating = regating
            .into_iter()
            .map(|(id,)| LookVersionId::try_from(id).map_err(|_| AdminError::Corrupt))
            .collect::<Result<Vec<_>, _>>()?;
        let lut_dependents = affected_lut_dependents_conn(&mut conn, root_admission).await?;
        mark_lut_dependents_conn(&mut conn, now, None).await?;
        enqueue_lut_rebase(&mut conn, now).await?;
        let payload = json!({
            "event": "root_revoked",
            "actor": actor_text(actor)?,
            "root_admission": root_admission,
            "scope": scope,
            "reason": reason,
            "lut_dependents": &lut_dependents,
        });
        audit::insert(&mut conn, AuditKind::RootRevoked, &payload).await?;
        enqueue_nightly_refit(&mut conn, now).await?;
        Ok(RootRevocation {
            root_admission: root_admission.clone(),
            new_versions,
            regating,
            lut_dependents,
        })
    }
    .await;
    finish(&mut conn, result).await
}

/// Publishes `target_version` as a new version, rejects named contributions and requests an immediate refit (docs/05 §8).
#[derive(Debug, Clone, Serialize)]
pub struct RevertRecord {
    pub look: LookSlug,
    pub target_version: u32,
    pub new_version: u32,
    pub rejected: Vec<ContributionId>,
    pub restoration: Restoration,
    pub lut_dependents: Vec<VersionRef>,
    pub refit_queued: bool,
    pub reverted_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorationHold {
    DependencyInvalidated,
    DependencyHistoryUnavailable,
    CurrentTargetUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Restoration {
    Current,
    HistoricalNeedsRebuild { reason: RestorationHold },
}

#[derive(Debug, Clone, Serialize)]
pub struct IntakeBlockRecord {
    pub credential: CredentialId,
    pub blocked_at: i64,
}

async fn authorize_moderator(conn: &mut SqliteConnection, actor: Actor) -> Result<(), AdminError> {
    let Actor::Credential(actor_trust) = actor else {
        return Ok(());
    };
    let state = trust_state(conn).await?;
    let record = state
        .credential(actor_trust)
        .ok_or(AdminError::UnknownCredential)?;
    if !record.roles.contains(&Role::Moderator) && !record.roles.contains(&Role::Admin) {
        return Err(AdminError::NotAuthorized);
    }
    Ok(())
}

fn manifest_hash(
    look: &LookSlug,
    version: u32,
    artifacts: &[ManifestArtifact],
) -> Result<String, AdminError> {
    let canonical = json!({
        "look": look,
        "version": version,
        "artifacts": artifacts,
    });
    let canonical = serde_json::to_vec(&canonical).map_err(|_| AdminError::Corrupt)?;
    Ok(hex(&Sha256::digest(&canonical)))
}

const BASE_LOOK_SUFFIX: &str = "-base.looklab";

fn read_artifact_blob(
    store: &FsBlobStore,
    artifact: &ManifestArtifact,
) -> Result<Vec<u8>, AdminError> {
    let hash = BlobHash::from_hex(&artifact.sha256).ok_or(AdminError::Corrupt)?;
    let bytes = store
        .get(hash)
        .map_err(|_| AdminError::Corrupt)?
        .ok_or(AdminError::Corrupt)?;
    if BlobHash::of(&bytes) != hash {
        return Err(AdminError::Corrupt);
    }
    Ok(bytes)
}

fn put_artifact(
    store: &FsBlobStore,
    artifacts: &mut [ManifestArtifact],
    name: &str,
    bytes: &[u8],
) -> Result<bool, AdminError> {
    let Some(artifact) = artifacts.iter_mut().find(|artifact| artifact.name == name) else {
        return Ok(false);
    };
    let hash = BlobHash::of(bytes);
    store.put(hash, bytes).map_err(|_| AdminError::Corrupt)?;
    artifact.sha256 = hash.to_hex();
    Ok(true)
}

fn card_meta(look: &LookSlug) -> CardMeta {
    let short: String = look.as_str().chars().take(27).collect();
    CardMeta::new(look.as_str(), short)
}

/// Re-encodes the artifacts that carry a Look identity for the new version.
fn republish_artifacts(
    blob_dir: &Path,
    look: &LookSlug,
    new_version: u32,
    anchor: u32,
    latest_release: Option<&Value>,
    artifacts: &[ManifestArtifact],
    now: i64,
) -> Result<Vec<ManifestArtifact>, AdminError> {
    let store = FsBlobStore::new(blob_dir).map_err(|_| AdminError::Corrupt)?;
    let mut artifacts = artifacts.to_vec();
    let identity = format!("{new_version}.0.0");
    let anchor_text = format!("{anchor}.0.0");
    let look_artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        .cloned()
        .ok_or(AdminError::Corrupt)?;
    let bytes = read_artifact_blob(&store, &look_artifact)?;
    let mut document = LookDocument::decode(&bytes).map_err(|_| AdminError::Corrupt)?;
    document.file.look.version =
        LookVersion::new(identity.clone()).map_err(|_| AdminError::Corrupt)?;
    document.file.provenance.root.anchor_version = anchor_text.clone();
    let document_bytes = document.encode().map_err(|_| AdminError::Corrupt)?;
    put_artifact(
        &store,
        &mut artifacts,
        looklab_publish::DOCUMENT_ARTIFACT,
        &document_bytes,
    )?;
    if artifacts
        .iter()
        .any(|artifact| artifact.name == looklab_publish::CARD_ARTIFACT)
    {
        let status = card_status(&document).ok_or(AdminError::Corrupt)?;
        let card = write_card(
            &document,
            &card_meta(look).with_status(status),
            &CardOptions::default(),
        )
        .map_err(|_| AdminError::Corrupt)?;
        put_artifact(
            &store,
            &mut artifacts,
            looklab_publish::CARD_ARTIFACT,
            &card,
        )?;
    }
    if let Some(release_artifact) = artifacts
        .iter()
        .find(|artifact| artifact.name == RELEASE_ARTIFACT)
        .cloned()
    {
        let bytes = read_artifact_blob(&store, &release_artifact)?;
        let mut release: Value = serde_json::from_slice(&bytes).map_err(|_| AdminError::Corrupt)?;
        let Value::Object(fields) = &mut release else {
            return Err(AdminError::Corrupt);
        };
        fields.insert("version".to_owned(), json!(new_version));
        fields.insert("anchor".to_owned(), json!(anchor));
        fields.insert("created_at".to_owned(), json!(now));
        if let Some(gated) = latest_release.and_then(|value| value.get("gated_since_anchor")) {
            fields.insert("gated_since_anchor".to_owned(), gated.clone());
        }
        if let Some(provenance) = fields.get_mut("provenance").and_then(Value::as_object_mut) {
            provenance.insert("anchor_version".to_owned(), json!(anchor_text.as_str()));
        }
        let bytes = serde_json::to_vec(&release).map_err(|_| AdminError::Corrupt)?;
        put_artifact(&store, &mut artifacts, RELEASE_ARTIFACT, &bytes)?;
    }
    let base_names: Vec<String> = artifacts
        .iter()
        .filter(|artifact| artifact.name.ends_with(BASE_LOOK_SUFFIX))
        .map(|artifact| artifact.name.clone())
        .collect();
    for name in base_names {
        let artifact = artifacts
            .iter()
            .find(|artifact| artifact.name == name)
            .cloned()
            .ok_or(AdminError::Corrupt)?;
        let bytes = read_artifact_blob(&store, &artifact)?;
        let mut base = LookDocument::decode(&bytes).map_err(|_| AdminError::Corrupt)?;
        if base.file.look.slug.as_str() != look.as_str() {
            continue;
        }
        base.file.look.version =
            LookVersion::new(identity.clone()).map_err(|_| AdminError::Corrupt)?;
        base.file.provenance.root.anchor_version = anchor_text.clone();
        let bytes = base.encode().map_err(|_| AdminError::Corrupt)?;
        put_artifact(&store, &mut artifacts, &name, &bytes)?;
    }
    Ok(artifacts)
}

async fn selectable_versions(
    conn: &mut SqliteConnection,
    look: &LookSlug,
) -> Result<BTreeSet<u32>, AdminError> {
    let look_id = LookId::new(look.as_str()).map_err(|_| AdminError::Corrupt)?;
    let accepted = crate::model_store::accepted_family_versions(conn, Some(&look_id))
        .await
        .map_err(|_| AdminError::Corrupt)?;
    let invalidated: BTreeSet<u32> = accepted
        .iter()
        .filter(|accepted| matches!(accepted.status, AcceptedFamilyVersionStatus::Invalidated(_)))
        .map(|accepted| accepted.target.version.get())
        .collect();
    let mut selectable: BTreeSet<u32> = accepted
        .into_iter()
        .filter(|accepted| !matches!(accepted.status, AcceptedFamilyVersionStatus::Invalidated(_)))
        .map(|accepted| accepted.target.version.get())
        .collect();
    let legacy: Vec<String> = sqlx::query_scalar(
        "SELECT target FROM model_legacy_scopes
         WHERE kind = 'look_version' AND family = ?",
    )
    .bind(look.as_str())
    .fetch_all(&mut *conn)
    .await?;
    for target in legacy {
        if let Some((_, version)) = target.rsplit_once('@')
            && let Ok(version) = version.parse()
        {
            selectable.insert(version);
        }
    }
    let lut: Vec<String> = sqlx::query_scalar(
        "SELECT c.target FROM model_target_eligibility_current c
         JOIN model_target_eligibility_snapshots s ON s.id = c.snapshot_id
         WHERE s.basis = 'lut_composition' AND s.status = 'eligible'",
    )
    .fetch_all(&mut *conn)
    .await?;
    for target in lut {
        if let Some((slug, version)) = target.rsplit_once('@')
            && slug == look.as_str()
            && let Ok(version) = version.parse()
        {
            selectable.insert(version);
        }
    }
    selectable.retain(|version| !invalidated.contains(version));
    Ok(selectable)
}

async fn current_anchor(
    conn: &mut SqliteConnection,
    look: &LookSlug,
) -> Result<Option<u32>, AdminError> {
    let selectable = selectable_versions(&mut *conn, look).await?;
    let mut candidates = Vec::new();
    let first_rooted: Option<i64> = sqlx::query_scalar(
        "SELECT MIN(v.version) FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.status = 'rooted'",
    )
    .bind(look.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if let Some(version) = first_rooted.and_then(|version| u32::try_from(version).ok())
        && selectable.contains(&version)
    {
        candidates.push(version);
    }
    let released: BTreeSet<i64> = sqlx::query_scalar(
        "SELECT v.version FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.status IN ('published', 'rooted')",
    )
    .bind(look.as_str())
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let rows: Vec<(String,)> = sqlx::query_as("SELECT payload FROM audit_log WHERE type = ?")
        .bind(AuditKind::ReAnchor.as_str())
        .fetch_all(&mut *conn)
        .await?;
    for (payload,) in rows {
        let value: Value = serde_json::from_str(&payload).map_err(|_| AdminError::Corrupt)?;
        if value.get("look").and_then(Value::as_str) != Some(look.as_str()) {
            continue;
        }
        if let Some(version) = value
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|version| u32::try_from(version).ok())
            && released.contains(&i64::from(version))
            && selectable.contains(&version)
        {
            candidates.push(version);
        }
    }
    Ok(candidates.into_iter().max())
}

async fn latest_release_value(
    conn: &mut SqliteConnection,
    blob_dir: &Path,
    look: &LookSlug,
) -> Result<Option<Value>, AdminError> {
    let look_id = LookId::new(look.as_str()).map_err(|_| AdminError::Corrupt)?;
    let version: Option<i64> = if let Some(target) =
        crate::model_store::family_target(conn, &look_id)
            .await
            .map_err(|_| AdminError::Corrupt)?
    {
        Some(i64::from(target.version.get()))
    } else {
        let lut: Option<i64> = sqlx::query_scalar(
            "SELECT MAX(CAST(substr(c.target, instr(c.target, '@') + 1) AS INTEGER))
                 FROM model_target_eligibility_current c
                 JOIN model_target_eligibility_snapshots s ON s.id = c.snapshot_id
                 WHERE s.basis = 'lut_composition' AND s.status = 'eligible'
                   AND c.target LIKE ?",
        )
        .bind(format!("{}@%", look.as_str()))
        .fetch_one(&mut *conn)
        .await?;
        match lut {
            Some(version) => Some(version),
            None => {
                let selectable = selectable_versions(&mut *conn, look).await?;
                let released: Vec<i64> = sqlx::query_scalar(
                    "SELECT v.version FROM look_versions v
                         JOIN version_manifests m ON m.look_version_id = v.id
                         WHERE v.look = ? AND v.status IN ('published', 'rooted')",
                )
                .bind(look.as_str())
                .fetch_all(&mut *conn)
                .await?;
                released
                    .into_iter()
                    .filter(|version| {
                        u32::try_from(*version)
                            .ok()
                            .is_some_and(|version| selectable.contains(&version))
                    })
                    .max()
            }
        }
    };
    let Some(version) = version else {
        return Ok(None);
    };
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT m.artifacts FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.version = ?",
    )
    .bind(look.as_str())
    .bind(version)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((artifacts,)) = row else {
        return Ok(None);
    };
    let artifacts: Vec<ManifestArtifact> =
        serde_json::from_str(&artifacts).map_err(|_| AdminError::Corrupt)?;
    let Some(release) = artifacts
        .iter()
        .find(|artifact| artifact.name == RELEASE_ARTIFACT)
    else {
        return Ok(None);
    };
    let store = FsBlobStore::new(blob_dir).map_err(|_| AdminError::Corrupt)?;
    let bytes = read_artifact_blob(&store, release)?;
    Ok(Some(
        serde_json::from_slice(&bytes).map_err(|_| AdminError::Corrupt)?,
    ))
}

async fn enqueue_nightly_refit(conn: &mut SqliteConnection, now: i64) -> Result<bool, AdminError> {
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = ? AND status = 'queued'")
            .bind(crate::store::JobKind::NightlyRefit.as_str())
            .fetch_one(&mut *conn)
            .await?;
    if queued > 0 {
        return Ok(false);
    }
    crate::store::enqueue_job(conn, crate::store::JobKind::NightlyRefit, "{}", now).await?;
    Ok(true)
}

async fn enqueue_revert_refit(conn: &mut SqliteConnection, now: i64) -> Result<bool, AdminError> {
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = ? AND status = 'queued'")
            .bind(crate::store::JobKind::RevertRefit.as_str())
            .fetch_one(&mut *conn)
            .await?;
    if queued > 0 {
        return Ok(false);
    }
    crate::store::enqueue_job(conn, crate::store::JobKind::RevertRefit, "{}", now).await?;
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DependencyOwner {
    Report,
    TargetEligibility,
    RealizationRevision,
    BundleRevision,
    BodyState,
    FamilyHead,
}

impl DependencyOwner {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::TargetEligibility => "target_eligibility",
            Self::RealizationRevision => "realization_revision",
            Self::BundleRevision => "bundle_revision",
            Self::BodyState => "body_state",
            Self::FamilyHead => "family_head",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cleanliness {
    Clean,
    Invalidated,
    HistoryUnavailable,
}

fn eligibility_artifacts(
    artifacts: &[ManifestArtifact],
) -> Result<Vec<ArtifactBinding>, AdminError> {
    artifacts
        .iter()
        .filter_map(|artifact| {
            let role = match artifact.name.as_str() {
                looklab_publish::DOCUMENT_ARTIFACT => ArtifactRole::Look,
                looklab_publish::CARD_ARTIFACT => ArtifactRole::Card,
                looklab_publish::PREVIEW_LUT_ARTIFACT => ArtifactRole::Preview,
                RELEASE_ARTIFACT => ArtifactRole::Release,
                _ => return None,
            };
            Some(Sha256Hex::parse(&artifact.sha256).map(|sha256| ArtifactBinding { sha256, role }))
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AdminError::Corrupt)
}

fn target_look_kind(
    blob_dir: &Path,
    artifacts: &[ManifestArtifact],
) -> Result<LookKind, AdminError> {
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        .ok_or(AdminError::Corrupt)?;
    let store = FsBlobStore::new(blob_dir).map_err(|_| AdminError::Corrupt)?;
    let bytes = read_artifact_blob(&store, artifact)?;
    let document = LookDocument::decode(&bytes).map_err(|_| AdminError::Corrupt)?;
    Ok(document.file.look.kind)
}

fn restored_eligibility(
    source: &TargetEligibility,
    target: PublishedTargetId,
    artifacts: Vec<ArtifactBinding>,
    source_target: &PublishedTargetId,
) -> TargetEligibility {
    let mut eligibility = source.clone();
    eligibility.id = EligibilitySnapshotId::from_digest([0; 32]);
    eligibility.target = target;
    eligibility.artifacts = artifacts;
    let dependency = DependencyRef::required(
        DependencyKind::PublishedTarget,
        DependencyTarget::published_target(source_target),
    );
    if !eligibility.dependencies.contains(&dependency) {
        eligibility.dependencies.push(dependency);
    }
    eligibility.sealed()
}

fn held_eligibility(
    source: Option<&TargetEligibility>,
    target: PublishedTargetId,
    kind: LookKind,
    artifacts: Vec<ArtifactBinding>,
    source_target: &PublishedTargetId,
) -> TargetEligibility {
    let mut dependencies = source
        .map(|eligibility| eligibility.dependencies.clone())
        .unwrap_or_default();
    dependencies.push(DependencyRef::required(
        DependencyKind::PublishedTarget,
        DependencyTarget::published_target(source_target),
    ));
    dependencies.sort();
    dependencies.dedup();
    TargetEligibility {
        id: EligibilitySnapshotId::from_digest([0; 32]),
        target,
        basis: match kind {
            LookKind::NativeFamily => TargetBasis::NativeFamily,
            LookKind::LutDefined => TargetBasis::LutComposition,
        },
        artifacts,
        base_pin: source.and_then(|eligibility| eligibility.base_pin.clone()),
        support_uncertainty_source: source
            .and_then(|eligibility| eligibility.support_uncertainty_source.clone()),
        status: TargetEligibilityStatus::Invalidated,
        reasons: vec![TargetEligibilityReason::DependencyInvalidated],
        dependencies,
    }
}

async fn record_restored_subject(
    conn: &mut SqliteConnection,
    family: &LookId,
    version: u32,
    manifest: Sha256Hex,
    now: i64,
) -> Result<(), AdminError> {
    crate::model_store::register_subject(
        conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        now,
    )
    .await?;
    crate::model_store::record_subject_revision(
        conn,
        &SubjectKey::for_family(family),
        version,
        SubjectRevisionRole::Target,
        Some(manifest),
        None,
        now,
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_held_restoration(
    conn: &mut SqliteConnection,
    family: &LookId,
    version: u32,
    kind: LookKind,
    source: Option<&TargetEligibility>,
    source_target: &PublishedTargetId,
    artifacts: Vec<ArtifactBinding>,
    manifest: Sha256Hex,
    now: i64,
) -> Result<(), AdminError> {
    let target = PublishedTargetId {
        look: family.clone(),
        version: TargetVersion::new(version).ok_or(AdminError::Corrupt)?,
    };
    let eligibility = held_eligibility(source, target, kind, artifacts, source_target);
    crate::model_store::record_target_eligibility(conn, &eligibility, now).await?;
    if kind == LookKind::NativeFamily {
        record_restored_subject(conn, family, version, manifest, now).await?;
    }
    Ok(())
}

async fn current_eligibility_snapshot(
    conn: &mut SqliteConnection,
    target: &str,
) -> Result<Option<String>, AdminError> {
    Ok(sqlx::query_scalar(
        "SELECT snapshot_id FROM model_target_eligibility_current WHERE target = ?",
    )
    .bind(target)
    .fetch_optional(&mut *conn)
    .await?)
}

#[derive(Debug, Clone, Default)]
struct ClosureEvidence {
    contribution_ids: HashSet<String>,
    images: usize,
    incomplete: bool,
    revoked: bool,
}

async fn dependency_closure(
    conn: &mut SqliteConnection,
    eligibility: &TargetEligibility,
) -> Result<ClosureEvidence, AdminError> {
    let mut stack = vec![(DependencyOwner::TargetEligibility, eligibility.id.to_hex())];
    let mut visited: HashSet<(DependencyOwner, String)> = HashSet::new();
    let mut closure = ClosureEvidence::default();
    while let Some((owner_kind, owner_id)) = stack.pop() {
        if !visited.insert((owner_kind, owner_id.clone())) {
            continue;
        }
        let dependencies =
            crate::model_store::dependencies(conn, owner_kind.as_str(), &owner_id).await?;
        if dependencies.is_empty() {
            closure.incomplete = true;
            continue;
        }
        for dependency in dependencies {
            if !dependency.required {
                continue;
            }
            if crate::model_store::dependency_generation(conn, &dependency)
                .await?
                .is_none()
            {
                closure.incomplete = true;
            }
            let revoked: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM model_revoked_dependencies WHERE kind = ? AND target = ?",
            )
            .bind(dependency.kind.as_str())
            .bind(dependency.target.as_str())
            .fetch_one(&mut *conn)
            .await?;
            if revoked > 0 {
                closure.revoked = true;
            }
            match dependency.kind {
                DependencyKind::Contribution => {
                    closure
                        .contribution_ids
                        .insert(dependency.target.as_str().to_owned());
                }
                DependencyKind::Image => closure.images += 1,
                DependencyKind::Report => stack.push((
                    DependencyOwner::Report,
                    dependency.target.as_str().to_owned(),
                )),
                DependencyKind::PublishedTarget => {
                    if let Some(snapshot) =
                        current_eligibility_snapshot(conn, dependency.target.as_str()).await?
                    {
                        stack.push((DependencyOwner::TargetEligibility, snapshot));
                    } else {
                        closure.incomplete = true;
                    }
                }
                DependencyKind::RealizationRevision => stack.push((
                    DependencyOwner::RealizationRevision,
                    dependency.target.as_str().to_owned(),
                )),
                DependencyKind::BundleRevision => stack.push((
                    DependencyOwner::BundleRevision,
                    dependency.target.as_str().to_owned(),
                )),
                DependencyKind::BodyState => stack.push((
                    DependencyOwner::BodyState,
                    dependency.target.as_str().to_owned(),
                )),
                DependencyKind::FamilyVersion => stack.push((
                    DependencyOwner::FamilyHead,
                    dependency.target.as_str().to_owned(),
                )),
                _ => {}
            }
        }
    }
    Ok(closure)
}

async fn restoration_cleanliness(
    conn: &mut SqliteConnection,
    eligibility: Option<&TargetEligibility>,
) -> Result<Cleanliness, AdminError> {
    let Some(eligibility) = eligibility else {
        return Ok(Cleanliness::HistoryUnavailable);
    };
    if eligibility.status != TargetEligibilityStatus::Eligible {
        return Ok(Cleanliness::Invalidated);
    }
    let closure = dependency_closure(conn, eligibility).await?;
    if closure.revoked {
        Ok(Cleanliness::Invalidated)
    } else if closure.incomplete || closure.contribution_ids.is_empty() || closure.images == 0 {
        Ok(Cleanliness::HistoryUnavailable)
    } else {
        Ok(Cleanliness::Clean)
    }
}

async fn target_contribution_evidence(
    conn: &mut SqliteConnection,
    eligibility: Option<&TargetEligibility>,
) -> Result<HashSet<String>, AdminError> {
    let Some(eligibility) = eligibility else {
        return Ok(HashSet::new());
    };
    Ok(dependency_closure(conn, eligibility)
        .await?
        .contribution_ids)
}

async fn target_evidence_bodies(
    conn: &mut SqliteConnection,
    contribution_ids: &HashSet<String>,
) -> Result<HashSet<String>, AdminError> {
    let mut bodies = HashSet::new();
    for id in contribution_ids {
        if let Some(body) =
            sqlx::query_scalar::<_, String>("SELECT body_id FROM contributions WHERE id = ?")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?
        {
            bodies.insert(body);
        }
    }
    Ok(bodies)
}

pub async fn block_intake(
    db: &SqlitePool,
    actor: Actor,
    credential: &CredentialId,
    reason: &str,
) -> Result<IntakeBlockRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        authorize_moderator(&mut conn, actor).await?;
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM credentials WHERE id = ?")
            .bind(credential.as_str())
            .fetch_one(&mut *conn)
            .await?;
        if exists == 0 {
            return Err(AdminError::UnknownCredential);
        }
        let now = now_unix();
        let inserted = sqlx::query(
            "INSERT INTO credential_intake_blocks
             (credential_id, moderator_credential, reason, blocked_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(credential_id) DO NOTHING",
        )
        .bind(credential.as_str())
        .bind(actor_text(actor)?.as_str())
        .bind(&reason)
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        let blocked_at: i64 = sqlx::query_scalar(
            "SELECT blocked_at FROM credential_intake_blocks WHERE credential_id = ?",
        )
        .bind(credential.as_str())
        .fetch_one(&mut *conn)
        .await?;
        if inserted == 1 {
            let payload = json!({
                "event": "credential_intake_blocked",
                "actor": actor_text(actor)?,
                "subject": credential,
                "reason": reason,
            });
            audit::insert(&mut conn, AuditKind::CredentialIntakeBlocked, &payload).await?;
        }
        Ok(IntakeBlockRecord {
            credential: credential.clone(),
            blocked_at,
        })
    }
    .await;
    finish(&mut conn, result).await
}

pub async fn revert(
    db: &SqlitePool,
    blob_dir: &Path,
    actor: Actor,
    look: &LookSlug,
    target_version: u32,
    rejected: &[ContributionId],
    reason: &str,
) -> Result<RevertRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let mut rejected = rejected.to_vec();
    rejected.sort();
    rejected.dedup();
    if rejected.len() > MAX_REVERT_REJECTIONS {
        return Err(AdminError::TooManyRejections);
    }
    let mut conn = begin_write(db).await?;
    let result = async {
        authorize_moderator(&mut conn, actor).await?;

        let source: Option<(String, String)> = sqlx::query_as(
            "SELECT v.id, m.artifacts FROM look_versions v
             JOIN version_manifests m ON m.look_version_id = v.id
             WHERE v.look = ? AND v.version = ? AND v.status IN ('published', 'rooted')",
        )
        .bind(look.as_str())
        .bind(i64::from(target_version))
        .fetch_optional(&mut *conn)
        .await?;
        let Some((source_id, artifacts_text)) = source else {
            let exists: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM look_versions WHERE look = ? AND version = ?",
            )
            .bind(look.as_str())
            .bind(i64::from(target_version))
            .fetch_one(&mut *conn)
            .await?;
            if exists == 0 {
                return Err(AdminError::UnknownVersion);
            }
            return Err(AdminError::VersionNotPublished);
        };
        if !selectable_versions(&mut conn, look)
            .await?
            .contains(&target_version)
        {
            return Err(AdminError::VersionNotPublished);
        }
        let source_id = LookVersionId::try_from(source_id).map_err(|_| AdminError::Corrupt)?;
        let artifacts: Vec<ManifestArtifact> =
            serde_json::from_str(&artifacts_text).map_err(|_| AdminError::Corrupt)?;
        let artifacts = normalize_artifacts(&artifacts)?;

        let family = LookId::new(look.as_str()).map_err(|_| AdminError::Corrupt)?;
        let source_target = PublishedTargetId {
            look: family.clone(),
            version: TargetVersion::new(target_version).ok_or(AdminError::Corrupt)?,
        };
        let source_eligibility_before =
            crate::model_store::target_eligibility(&mut conn, &source_target).await?;
        let evidence_contributions =
            target_contribution_evidence(&mut conn, source_eligibility_before.as_ref()).await?;
        let evidence_bodies = target_evidence_bodies(&mut conn, &evidence_contributions).await?;

        for contribution in &rejected {
            let row: Option<(String, Option<String>, String)> = sqlx::query_as(
                "SELECT status, target_look_id, body_id FROM contributions WHERE id = ?",
            )
            .bind(contribution.as_str())
            .fetch_optional(&mut *conn)
            .await?;
            let Some((status, target_look, body_id)) = row else {
                return Err(AdminError::UnknownContribution);
            };
            match ContributionStatus::from_db(&status).ok_or(AdminError::Corrupt)? {
                ContributionStatus::Withdrawn => {
                    return Err(AdminError::ContributionWithdrawn);
                }
                ContributionStatus::Pending
                | ContributionStatus::Accepted
                | ContributionStatus::Quarantined => {}
            }
            if !evidence_contributions.contains(contribution.as_str())
                && target_look.as_deref() != Some(look.as_str())
                && !evidence_bodies.contains(&body_id)
            {
                return Err(AdminError::ContributionNotInLookEvidence);
            }
        }

        let now = now_unix();
        let moderator = actor_text(actor)?;
        for contribution in &rejected {
            let inserted = sqlx::query(
                "INSERT INTO contribution_rejections
                 (contribution_id, moderator_credential, reason, rejected_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(contribution_id) DO NOTHING",
            )
            .bind(contribution.as_str())
            .bind(moderator.as_str())
            .bind(&reason)
            .bind(now)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            sqlx::query(
                "UPDATE contributions SET status = 'quarantined'
                 WHERE id = ? AND status != 'withdrawn'",
            )
            .bind(contribution.as_str())
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                "UPDATE contribution_images
                 SET status = 'quarantined', status_reason = ?
                 WHERE contribution_id = ? AND status != 'quarantined'",
            )
            .bind(&reason)
            .bind(contribution.as_str())
            .execute(&mut *conn)
            .await?;
            if inserted == 1 {
                crate::model_events::contribution_rejected(&mut conn, contribution, now).await?;
            }
        }

        let version = next_version(&mut conn, look).await?;
        let target_artifacts = artifacts;
        let latest_release = latest_release_value(&mut conn, blob_dir, look).await?;
        let anchor = current_anchor(&mut conn, look)
            .await?
            .or_else(|| {
                latest_release
                    .as_ref()
                    .and_then(|value| value.get("anchor"))
                    .and_then(Value::as_u64)
                    .and_then(|anchor| u32::try_from(anchor).ok())
            })
            .unwrap_or_else(|| version.get());
        let artifacts = normalize_artifacts(&republish_artifacts(
            blob_dir,
            look,
            version.get(),
            anchor,
            latest_release.as_ref(),
            &target_artifacts,
            now,
        )?)?;
        let source_eligibility =
            crate::model_store::target_eligibility(&mut conn, &source_target).await?;
        let cleanliness = restoration_cleanliness(&mut conn, source_eligibility.as_ref()).await?;
        let kind = target_look_kind(blob_dir, &target_artifacts)?;
        let version_id = LookVersionId::new();
        let changelog = if rejected.is_empty() {
            format!("Reverted to version {target_version}.")
        } else {
            format!(
                "Reverted to version {target_version} and rejected {} contribution(s).",
                rejected.len()
            )
        };
        let look_status = if matches!(cleanliness, Cleanliness::Clean) {
            VersionStatus::Published
        } else {
            VersionStatus::Deprecated
        };
        sqlx::query(
            "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(version_id.as_str())
        .bind(look.as_str())
        .bind(i64::from(version.get()))
        .bind(look_status.as_str())
        .bind(&changelog)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        let artifact_json = serde_json::to_string(&artifacts).map_err(|_| AdminError::Corrupt)?;
        sqlx::query(
            "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(version_id.as_str())
        .bind(&artifact_json)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        let new_manifest_sha256 = manifest_hash(look, version.get(), &artifacts)?;
        let manifest_payload = json!({
            "event": "release_manifest",
            "look": look,
            "version": version.get(),
            "manifest_sha256": new_manifest_sha256,
        });
        audit::insert(&mut conn, AuditKind::ReleaseManifest, &manifest_payload).await?;
        sqlx::query(
            "INSERT INTO version_depends_on_root (look_version_id, root_admission_id)
             SELECT ?, root_admission_id FROM version_depends_on_root
             WHERE look_version_id = ?",
        )
        .bind(version_id.as_str())
        .bind(source_id.as_str())
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO lut_base_pins
             (look, version, base, base_status, base_look, base_version, recorded_at)
             SELECT look, ?, base, base_status, base_look, base_version, ?
             FROM lut_base_pins WHERE look = ? AND version = ?",
        )
        .bind(i64::from(version.get()))
        .bind(now)
        .bind(look.as_str())
        .bind(i64::from(target_version))
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO lut_native_renderings (look, version, maker, rendering, published_at)
             SELECT look, ?, maker, rendering, ? FROM lut_native_renderings
             WHERE look = ? AND version = ?",
        )
        .bind(i64::from(version.get()))
        .bind(now)
        .bind(look.as_str())
        .bind(i64::from(target_version))
        .execute(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO release_body_params (look, version, body, stage, params, created_at)
             SELECT look, ?, body, stage, params, ? FROM release_body_params
             WHERE look = ? AND version = ?",
        )
        .bind(i64::from(version.get()))
        .bind(now)
        .bind(look.as_str())
        .bind(i64::from(target_version))
        .execute(&mut *conn)
        .await?;

        let new_target = PublishedTargetId {
            look: family.clone(),
            version: TargetVersion::new(version.get()).ok_or(AdminError::Corrupt)?,
        };
        let new_family_target = FamilyTargetId {
            family: family.clone(),
            version: TargetVersion::new(version.get()).ok_or(AdminError::Corrupt)?,
        };
        let eligibility_artifacts = eligibility_artifacts(&artifacts)?;
        let manifest = Sha256Hex::parse(&manifest_hash(look, version.get(), &artifacts)?)
            .map_err(|_| AdminError::Corrupt)?;
        let restoration = match (cleanliness, kind) {
            (Cleanliness::Clean, LookKind::NativeFamily)
                if source_eligibility
                    .as_ref()
                    .is_some_and(|source| source.basis == TargetBasis::NativeFamily) =>
            {
                let source = source_eligibility.as_ref().ok_or(AdminError::Corrupt)?;
                let eligibility = restored_eligibility(
                    source,
                    new_target.clone(),
                    eligibility_artifacts.clone(),
                    &source_target,
                );
                let head_exists: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM model_family_heads WHERE family = ?")
                        .bind(family.as_str())
                        .fetch_one(&mut *conn)
                        .await?;
                if head_exists == 0 {
                    crate::model_store::record_target_eligibility(&mut conn, &eligibility, now)
                        .await?;
                    record_restored_subject(&mut conn, &family, version.get(), manifest, now)
                        .await?;
                    Restoration::Current
                } else {
                    let assessed_generation = crate::model_store::pending_generation(
                        &mut conn,
                        &SubjectKey::for_family(&family),
                    )
                    .await?;
                    let restore = crate::model_store::RestoredHeadMetadata::for_restored_target(
                        &eligibility,
                        assessed_generation,
                    );
                    if crate::model_store::advance_family_head_from_moderator(
                        &mut conn,
                        &family,
                        &new_family_target,
                        &eligibility,
                        manifest,
                        &restore,
                        now,
                    )
                    .await?
                    {
                        Restoration::Current
                    } else {
                        record_held_restoration(
                            &mut conn,
                            &family,
                            version.get(),
                            kind,
                            source_eligibility.as_ref(),
                            &source_target,
                            eligibility_artifacts,
                            manifest,
                            now,
                        )
                        .await?;
                        Restoration::HistoricalNeedsRebuild {
                            reason: RestorationHold::CurrentTargetUnavailable,
                        }
                    }
                }
            }
            (Cleanliness::Clean, LookKind::LutDefined)
                if source_eligibility
                    .as_ref()
                    .is_some_and(|source| source.basis == TargetBasis::LutComposition) =>
            {
                let source = source_eligibility.as_ref().ok_or(AdminError::Corrupt)?;
                let eligibility = restored_eligibility(
                    source,
                    new_target.clone(),
                    eligibility_artifacts,
                    &source_target,
                );
                crate::model_store::record_target_eligibility(&mut conn, &eligibility, now).await?;
                Restoration::Current
            }
            (Cleanliness::Invalidated, _) => {
                record_held_restoration(
                    &mut conn,
                    &family,
                    version.get(),
                    kind,
                    source_eligibility.as_ref(),
                    &source_target,
                    eligibility_artifacts,
                    manifest,
                    now,
                )
                .await?;
                Restoration::HistoricalNeedsRebuild {
                    reason: RestorationHold::DependencyInvalidated,
                }
            }
            (Cleanliness::HistoryUnavailable, _) => {
                record_held_restoration(
                    &mut conn,
                    &family,
                    version.get(),
                    kind,
                    source_eligibility.as_ref(),
                    &source_target,
                    eligibility_artifacts,
                    manifest,
                    now,
                )
                .await?;
                Restoration::HistoricalNeedsRebuild {
                    reason: RestorationHold::DependencyHistoryUnavailable,
                }
            }
            (Cleanliness::Clean, LookKind::NativeFamily | LookKind::LutDefined) => {
                record_held_restoration(
                    &mut conn,
                    &family,
                    version.get(),
                    kind,
                    source_eligibility.as_ref(),
                    &source_target,
                    eligibility_artifacts,
                    manifest,
                    now,
                )
                .await?;
                Restoration::HistoricalNeedsRebuild {
                    reason: RestorationHold::CurrentTargetUnavailable,
                }
            }
        };
        if !matches!(restoration, Restoration::Current) {
            sqlx::query("UPDATE look_versions SET status = ? WHERE id = ?")
                .bind(VersionStatus::Deprecated.as_str())
                .bind(version_id.as_str())
                .execute(&mut *conn)
                .await?;
        }

        let republished = VersionRef {
            look: look.clone(),
            version: version.get(),
        };
        let lut_dependents = mark_lut_dependents_conn(&mut conn, now, Some(&republished)).await?;
        enqueue_lut_rebase(&mut conn, now).await?;

        let payload = json!({
            "event": "revert",
            "actor": moderator,
            "look": look,
            "reason": reason,
            "plan": {
                "target_version": target_version,
                "publish_as_new": manifest_hash(look, target_version, &target_artifacts)?,
                "rejected_contributions": &rejected,
                "restoration": restoration,
            },
        });
        audit::insert(&mut conn, AuditKind::Revert, &payload).await?;

        sqlx::query(
            "INSERT INTO look_refit_requests (look, generation, reverted_version, requested_at)
             VALUES (?, 1, ?, ?)
             ON CONFLICT (look) DO UPDATE SET
               generation = look_refit_requests.generation + 1,
               reverted_version = excluded.reverted_version,
               requested_at = excluded.requested_at",
        )
        .bind(look.as_str())
        .bind(i64::from(version.get()))
        .bind(now)
        .execute(&mut *conn)
        .await?;

        crate::store::request_publication(&mut conn).await?;
        let refit_queued = enqueue_revert_refit(&mut conn, now).await?;

        Ok(RevertRecord {
            look: look.clone(),
            target_version,
            new_version: version.get(),
            rejected,
            restoration,
            lut_dependents,
            refit_queued,
            reverted_at: now,
        })
    }
    .await;
    finish(&mut conn, result).await
}

#[derive(Debug, Clone, Serialize)]
pub struct ReAnchorRecord {
    pub look: LookSlug,
    pub version: u32,
    pub reason: String,
    pub anchored_at: i64,
}

/// Starts the cumulative anchor at an existing released version.
pub async fn reanchor(
    db: &SqlitePool,
    actor: Actor,
    look: &LookSlug,
    version: u32,
    reason: &str,
) -> Result<ReAnchorRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        authorize_moderator(&mut conn, actor).await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ? AND version = ?")
                .bind(look.as_str())
                .bind(i64::from(version))
                .fetch_optional(&mut *conn)
                .await?;
        let Some(status) = status else {
            return Err(AdminError::UnknownVersion);
        };
        if !matches!(
            parse_version_status(&status)?,
            VersionStatus::Published | VersionStatus::Rooted
        ) {
            return Err(AdminError::VersionNotPublished);
        }
        if !selectable_versions(&mut conn, look)
            .await?
            .contains(&version)
        {
            return Err(AdminError::VersionNotPublished);
        }
        let manifest: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM version_manifests m
             JOIN look_versions v ON v.id = m.look_version_id
             WHERE v.look = ? AND v.version = ?",
        )
        .bind(look.as_str())
        .bind(i64::from(version))
        .fetch_one(&mut *conn)
        .await?;
        if manifest == 0 {
            return Err(AdminError::VersionNotPublished);
        }
        let now = now_unix();
        let family = LookId::new(look.as_str()).map_err(|_| AdminError::Corrupt)?;
        let subject = SubjectKey::for_family(&family);
        let model_subject: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM model_subjects WHERE key = ?")
                .bind(subject.as_str())
                .fetch_one(&mut *conn)
                .await?;
        if model_subject > 0 {
            let state = crate::model_store::subject_state(&mut conn, &subject).await?;
            let expected = state.and_then(|state| state.anchor_revision);
            if !crate::model_store::set_subject_anchor(&mut conn, &subject, expected, version, now)
                .await?
            {
                return Err(AdminError::Conflict);
            }
        }
        let payload = json!({
            "event": AuditKind::ReAnchor.as_str(),
            "actor": actor_text(actor)?,
            "look": look,
            "subject": subject,
            "version": version,
            "reason": reason,
        });
        audit::insert(&mut conn, AuditKind::ReAnchor, &payload).await?;
        crate::store::request_publication(&mut conn).await?;
        Ok(ReAnchorRecord {
            look: look.clone(),
            version,
            reason: reason.clone(),
            anchored_at: now,
        })
    }
    .await;
    finish(&mut conn, result).await
}

#[derive(Debug, Clone)]
pub struct WorkerProposal {
    pub kind: looklab_schema::ProposalKind,
    pub payload: String,
    pub evidence: Value,
}

struct PreparedWorkerProposal {
    kind: looklab_schema::ProposalKind,
    payload: String,
    summary: String,
    evidence: String,
}

fn prepare_worker_proposal(proposal: WorkerProposal) -> Result<PreparedWorkerProposal, AdminError> {
    let validated = looklab_schema::validate_proposal(proposal.kind, &proposal.payload)
        .map_err(|errors| AdminError::InvalidProposal(format!("{errors:?}")))?;
    Ok(PreparedWorkerProposal {
        kind: proposal.kind,
        payload: proposal.payload,
        summary: validated.summary().text(),
        evidence: serde_json::to_string(&proposal.evidence).map_err(|_| AdminError::Corrupt)?,
    })
}

async fn insert_worker_proposal(
    conn: &mut SqliteConnection,
    proposal: &PreparedWorkerProposal,
) -> Result<Option<ProposalId>, AdminError> {
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM proposals
         WHERE origin = 'worker' AND type = ? AND payload = ? AND status = 'pending'",
    )
    .bind(proposal.kind.as_str())
    .bind(&proposal.payload)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((id,)) = existing {
        return ProposalId::try_from(id)
            .map_err(|_| AdminError::Corrupt)
            .map(Some);
    }
    let id = ProposalId::new();
    let now = now_unix();
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, origin, type, payload, human_summary, evidence, status, created_at)
         VALUES (?, NULL, 'worker', ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(id.as_str())
    .bind(proposal.kind.as_str())
    .bind(&proposal.payload)
    .bind(&proposal.summary)
    .bind(&proposal.evidence)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let payload = json!({
        "event": "proposal_opened",
        "actor": "worker",
        "proposal": id,
        "kind": proposal.kind.as_str(),
        "summary": &proposal.summary,
    });
    audit::insert(&mut *conn, AuditKind::ProposalOpened, &payload).await?;
    Ok(Some(id))
}

/// Opens a worker proposal only while the caller still owns the job and its
/// accepted numerical sources and evidence snapshot are current.
pub async fn submit_worker_proposal_fenced(
    db: &SqlitePool,
    proposal: WorkerProposal,
    lease: JobLease<'_>,
    sources: &NumericalSources,
    dependencies: &[DependencyRef],
    snapshot: &[DependencyGeneration],
) -> Result<Option<ProposalId>, AdminError> {
    let proposal = prepare_worker_proposal(proposal)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        if !crate::model_store::execution_current(&mut conn, lease, sources, dependencies, snapshot)
            .await?
        {
            return Err(AdminError::Conflict);
        }
        insert_worker_proposal(&mut conn, &proposal).await
    }
    .await;
    finish(&mut conn, result).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRootAdmission {
    pub id: RootAdmissionId,
    pub contribution: ContributionId,
    pub scope: Scope,
    pub admitted_at: i64,
}

/// Oldest first.
pub async fn active_root_admissions(
    db: &SqlitePool,
) -> Result<Vec<ActiveRootAdmission>, AdminError> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT id, contribution_id, scope, admitted_at FROM root_admissions
         WHERE revoked_at IS NULL ORDER BY admitted_at, id",
    )
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(id, contribution, scope, admitted_at)| {
            Ok(ActiveRootAdmission {
                id: RootAdmissionId::try_from(id).map_err(|_| AdminError::Corrupt)?,
                contribution: ContributionId::try_from(contribution)
                    .map_err(|_| AdminError::Corrupt)?,
                scope: parse_scope(Some(&scope))?.ok_or(AdminError::Corrupt)?,
                admitted_at,
            })
        })
        .collect()
}

async fn next_version(
    conn: &mut SqliteConnection,
    look: &LookSlug,
) -> Result<VersionId, AdminError> {
    let current: Option<i64> =
        sqlx::query_scalar("SELECT MAX(version) FROM look_versions WHERE look = ?")
            .bind(look.as_str())
            .fetch_one(&mut *conn)
            .await?;
    let next = current.unwrap_or(0) + 1;
    let next = u32::try_from(next).map_err(|_| AdminError::Corrupt)?;
    Ok(VersionId::new(next))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestArtifact {
    pub name: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionStatus {
    Provisional,
    Rooted,
    Published,
    Deprecated,
}

impl VersionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Rooted => "rooted",
            Self::Published => "published",
            Self::Deprecated => "deprecated",
        }
    }
}

fn parse_version_status(value: &str) -> Result<VersionStatus, AdminError> {
    match value {
        "provisional" => Ok(VersionStatus::Provisional),
        "rooted" => Ok(VersionStatus::Rooted),
        "published" => Ok(VersionStatus::Published),
        "deprecated" => Ok(VersionStatus::Deprecated),
        _ => Err(AdminError::Corrupt),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestRecord {
    pub look: LookSlug,
    pub version: u32,
    pub status: VersionStatus,
    pub artifacts: Vec<ManifestArtifact>,
    pub created_at: i64,
}

fn valid_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ARTIFACT_NAME
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains("..")
        && name.bytes().all(|byte| byte.is_ascii_graphic())
}

fn normalize_artifacts(
    artifacts: &[ManifestArtifact],
) -> Result<Vec<ManifestArtifact>, AdminError> {
    if artifacts.is_empty() {
        return Err(AdminError::EmptyManifest);
    }
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(AdminError::InvalidArtifact("too many artifacts".to_owned()));
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for artifact in artifacts {
        if !valid_artifact_name(&artifact.name) {
            return Err(AdminError::InvalidArtifact(artifact.name.clone()));
        }
        let hash = ArtifactHash::from_str(&artifact.sha256)
            .map_err(|_| AdminError::InvalidArtifact(artifact.name.clone()))?;
        if !seen.insert(artifact.name.clone()) {
            return Err(AdminError::InvalidArtifact(artifact.name.clone()));
        }
        normalized.push(ManifestArtifact {
            name: artifact.name.clone(),
            sha256: hash.to_string(),
        });
    }
    normalized.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(normalized)
}

/// `status` applies only when the version row is created.
pub async fn write_version_manifest(
    db: &SqlitePool,
    look: &LookSlug,
    version: VersionId,
    status: VersionStatus,
    artifacts: &[ManifestArtifact],
    depends_on_root: &[RootAdmissionId],
) -> Result<ManifestRecord, AdminError> {
    let mut conn = begin_write(db).await?;
    let result =
        write_version_manifest_conn(&mut conn, look, version, status, artifacts, depends_on_root)
            .await;
    finish(&mut conn, result).await
}

pub async fn write_version_manifest_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: VersionId,
    status: VersionStatus,
    artifacts: &[ManifestArtifact],
    depends_on_root: &[RootAdmissionId],
) -> Result<ManifestRecord, AdminError> {
    let artifacts = normalize_artifacts(artifacts)?;
    let mut depends_on_root = depends_on_root.to_vec();
    depends_on_root.sort();
    depends_on_root.dedup();

    for root in &depends_on_root {
        let revoked_at: Option<Option<i64>> =
            sqlx::query_scalar("SELECT revoked_at FROM root_admissions WHERE id = ?")
                .bind(root.as_str())
                .fetch_optional(&mut *conn)
                .await?;
        match revoked_at {
            None => return Err(AdminError::UnknownRootAdmission),
            Some(Some(_)) => return Err(AdminError::RootRevoked(root.clone())),
            Some(None) => {}
        }
    }
    let now = now_unix();
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, ?, ?, NULL, ?)
         ON CONFLICT (look, version) DO NOTHING",
    )
    .bind(LookVersionId::new().as_str())
    .bind(look.as_str())
    .bind(version.get())
    .bind(status.as_str())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let row: (String, String, i64) = sqlx::query_as(
        "SELECT id, status, created_at FROM look_versions WHERE look = ? AND version = ?",
    )
    .bind(look.as_str())
    .bind(version.get())
    .fetch_one(&mut *conn)
    .await?;
    let (version_id, stored_status, created_at) = row;
    let status = parse_version_status(&stored_status)?;
    let version_id = LookVersionId::try_from(version_id).map_err(|_| AdminError::Corrupt)?;
    let existing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM version_manifests WHERE look_version_id = ?")
            .bind(version_id.as_str())
            .fetch_one(&mut *conn)
            .await?;
    if existing > 0 {
        return Err(AdminError::ManifestExists);
    }
    let artifact_json = serde_json::to_string(&artifacts).map_err(|_| AdminError::Corrupt)?;
    sqlx::query(
        "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
         VALUES (?, ?, ?)",
    )
    .bind(version_id.as_str())
    .bind(&artifact_json)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    for root in &depends_on_root {
        sqlx::query(
            "INSERT INTO version_depends_on_root (look_version_id, root_admission_id)
             VALUES (?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(version_id.as_str())
        .bind(root.as_str())
        .execute(&mut *conn)
        .await?;
    }
    mark_lut_dependents_conn(&mut *conn, now, None).await?;
    enqueue_lut_rebase(&mut *conn, now).await?;
    let manifest_sha256 = manifest_hash(look, version.get(), &artifacts)?;
    let payload = json!({
        "event": "release_manifest",
        "look": look,
        "version": version.get(),
        "manifest_sha256": manifest_sha256,
    });
    audit::insert(&mut *conn, AuditKind::ReleaseManifest, &payload).await?;
    Ok(ManifestRecord {
        look: look.clone(),
        version: version.get(),
        status,
        artifacts: artifacts.clone(),
        created_at,
    })
}

pub async fn manifest_record(
    db: &SqlitePool,
    look: &LookSlug,
    version: VersionId,
) -> Result<ManifestRecord, AdminError> {
    let row: Option<(String, String, i64)> = sqlx::query_as(
        "SELECT v.status, m.artifacts, m.created_at FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? AND v.version = ?",
    )
    .bind(look.as_str())
    .bind(version.get())
    .fetch_optional(db)
    .await?;
    let Some((status, artifacts, created_at)) = row else {
        return Err(AdminError::UnknownVersion);
    };
    let artifacts: Vec<ManifestArtifact> =
        serde_json::from_str(&artifacts).map_err(|_| AdminError::Corrupt)?;
    Ok(ManifestRecord {
        look: look.clone(),
        version: version.get(),
        status: parse_version_status(&status)?,
        artifacts,
        created_at,
    })
}

/// The display base a published `lut_defined` version was fitted against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LutBasePin {
    pub look: LookSlug,
    pub version: u32,
    pub base: BaseReference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LutRebaseReason {
    BasePublished,
    BaseRevoked,
    FamilyResolved,
}

impl LutRebaseReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BasePublished => "base_published",
            Self::BaseRevoked => "base_revoked",
            Self::FamilyResolved => "family_resolved",
        }
    }

    fn parse(value: &str) -> Result<Self, AdminError> {
        match value {
            "base_published" => Ok(Self::BasePublished),
            "base_revoked" => Ok(Self::BaseRevoked),
            "family_resolved" => Ok(Self::FamilyResolved),
            _ => Err(AdminError::Corrupt),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LutRebaseWork {
    pub look: LookSlug,
    pub version: u32,
    pub reason: LutRebaseReason,
}

/// The major component of a Look version, matching `look_versions.version`.
fn version_number(version: &LookVersion) -> Result<u32, AdminError> {
    version
        .as_str()
        .split('.')
        .next()
        .and_then(|major| major.parse().ok())
        .ok_or(AdminError::Corrupt)
}

pub async fn record_lut_base(
    db: &SqlitePool,
    look: &LookSlug,
    version: u32,
    base: &BaseReference,
    status: &BaseStatus,
) -> Result<(), AdminError> {
    let mut conn = begin_write(db).await?;
    let result = record_lut_base_conn(&mut conn, look, version, base, status).await;
    finish(&mut conn, result).await
}

pub async fn record_lut_base_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
    base: &BaseReference,
    status: &BaseStatus,
) -> Result<(), AdminError> {
    let (base_look, base_version) = match base {
        BaseReference::Look { slug, version } => (
            Some(slug.as_str().to_owned()),
            version_number(version).ok().map(i64::from),
        ),
        _ => (None, None),
    };
    if let BaseReference::Look { slug, version } = base
        && base_reaches_look(
            conn,
            slug.as_str(),
            version_number(version).ok(),
            look.as_str(),
        )
        .await?
    {
        return Err(AdminError::CyclicBaseDependency);
    }
    sqlx::query(
        "INSERT INTO lut_base_pins
         (look, version, base, base_status, base_look, base_version, recorded_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (look, version) DO UPDATE SET
           base = excluded.base,
           base_status = excluded.base_status,
           base_look = excluded.base_look,
           base_version = excluded.base_version",
    )
    .bind(look.as_str())
    .bind(i64::from(version))
    .bind(serde_json::to_string(base).map_err(|_| AdminError::Corrupt)?)
    .bind(serde_json::to_string(status).map_err(|_| AdminError::Corrupt)?)
    .bind(base_look)
    .bind(base_version)
    .bind(now_unix())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub async fn record_lut_preview_base_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
    preview_base: &BaseReference,
) -> Result<(), AdminError> {
    sqlx::query("UPDATE lut_base_pins SET preview_base = ? WHERE look = ? AND version = ?")
        .bind(serde_json::to_string(preview_base).map_err(|_| AdminError::Corrupt)?)
        .bind(look.as_str())
        .bind(i64::from(version))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn base_reaches_look(
    conn: &mut SqliteConnection,
    base_look: &str,
    base_version: Option<u32>,
    target: &str,
) -> Result<bool, AdminError> {
    let mut current = Some((base_look.to_owned(), base_version));
    let mut seen = HashSet::new();
    while let Some((look, version)) = current {
        if look == target {
            return Ok(true);
        }
        let Some(version) = version else {
            return Ok(false);
        };
        if !seen.insert((look.clone(), version)) {
            return Ok(true);
        }
        let row: Option<(String,)> =
            sqlx::query_as("SELECT base FROM lut_base_pins WHERE look = ? AND version = ?")
                .bind(&look)
                .bind(i64::from(version))
                .fetch_optional(&mut *conn)
                .await?;
        let Some((base,)) = row else {
            return Ok(false);
        };
        let base: BaseReference = serde_json::from_str(&base).map_err(|_| AdminError::Corrupt)?;
        current = match base {
            BaseReference::Look { slug, version } => {
                Some((slug.as_str().to_owned(), version_number(&version).ok()))
            }
            _ => None,
        };
    }
    Ok(false)
}

pub async fn record_native_rendering(
    db: &SqlitePool,
    look: &LookSlug,
    version: u32,
    rendering: &CameraRendering,
) -> Result<(), AdminError> {
    sqlx::query(
        "INSERT INTO lut_native_renderings (look, version, maker, rendering, published_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (look, version) DO UPDATE SET
           maker = excluded.maker,
           rendering = excluded.rendering",
    )
    .bind(look.as_str())
    .bind(i64::from(version))
    .bind(&rendering.maker)
    .bind(&rendering.name)
    .bind(now_unix())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn lut_base_pin(
    db: &SqlitePool,
    look: &LookSlug,
    version: u32,
) -> Result<Option<LutBasePin>, AdminError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT base FROM lut_base_pins WHERE look = ? AND version = ?")
            .bind(look.as_str())
            .bind(i64::from(version))
            .fetch_optional(db)
            .await?;
    let Some((base,)) = row else {
        return Ok(None);
    };
    Ok(Some(LutBasePin {
        look: look.clone(),
        version,
        base: serde_json::from_str(&base).map_err(|_| AdminError::Corrupt)?,
    }))
}

pub async fn pending_lut_rebases(db: &SqlitePool) -> Result<Vec<LutRebaseWork>, AdminError> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT look, version, reason FROM lut_rebase_pending ORDER BY look, version",
    )
    .fetch_all(db)
    .await?;
    rows.into_iter()
        .map(|(look, version, reason)| {
            Ok(LutRebaseWork {
                look: LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?,
                version: u32::try_from(version).map_err(|_| AdminError::Corrupt)?,
                reason: LutRebaseReason::parse(&reason)?,
            })
        })
        .collect()
}

pub async fn clear_lut_rebase_pending(
    db: &SqlitePool,
    look: &LookSlug,
    version: u32,
) -> Result<(), AdminError> {
    let mut conn = db.acquire().await?;
    clear_lut_rebase_pending_conn(&mut conn, look, version).await
}

pub async fn clear_lut_rebase_pending_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
) -> Result<(), AdminError> {
    sqlx::query("DELETE FROM lut_rebase_pending WHERE look = ? AND version = ?")
        .bind(look.as_str())
        .bind(i64::from(version))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Published versions whose root was revoked, plus their transitive LUT dependents.
pub async fn invalidated_versions(db: &SqlitePool) -> Result<Vec<VersionRef>, AdminError> {
    let mut conn = db.acquire().await?;
    invalidated_versions_conn(&mut conn).await
}

pub async fn invalidated_versions_conn(
    conn: &mut SqliteConnection,
) -> Result<Vec<VersionRef>, AdminError> {
    invalidated_set_conn(&mut *conn)
        .await?
        .into_iter()
        .map(|(look, version)| {
            Ok(VersionRef {
                look: LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?,
                version,
            })
        })
        .collect()
}

async fn invalidated_set_conn(
    conn: &mut SqliteConnection,
) -> Result<BTreeSet<(String, u32)>, AdminError> {
    let pins = pins_conn(&mut *conn).await?;
    let revoked: Vec<(String, i64)> = sqlx::query_as(
        "SELECT DISTINCT v.look, v.version FROM version_depends_on_root d
         JOIN look_versions v ON v.id = d.look_version_id
         JOIN root_admissions r ON r.id = d.root_admission_id
         WHERE r.revoked_at IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut invalid: BTreeSet<(String, u32)> = BTreeSet::new();
    for (look, version) in revoked {
        invalid.insert((
            look,
            u32::try_from(version).map_err(|_| AdminError::Corrupt)?,
        ));
    }
    loop {
        let mut changed = false;
        for (look, version, base, _, _) in &pins {
            if let BaseReference::Look {
                slug,
                version: pinned,
            } = base
            {
                let Ok(pinned) = version_number(pinned) else {
                    continue;
                };
                if invalid.contains(&(slug.as_str().to_owned(), pinned)) {
                    changed |= invalid.insert((look.as_str().to_owned(), *version));
                }
            }
        }
        if !changed {
            break;
        }
    }
    Ok(invalid)
}

type Pins = Vec<(
    LookSlug,
    u32,
    BaseReference,
    BaseStatus,
    Option<BaseReference>,
)>;

async fn pins_conn(conn: &mut SqliteConnection) -> Result<Pins, AdminError> {
    let rows: Vec<(String, i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT look, version, base, base_status, preview_base FROM lut_base_pins
         ORDER BY look, version",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut pins = Vec::with_capacity(rows.len());
    for (look, version, base, status, preview_base) in rows {
        pins.push((
            LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?,
            u32::try_from(version).map_err(|_| AdminError::Corrupt)?,
            serde_json::from_str::<BaseReference>(&base).map_err(|_| AdminError::Corrupt)?,
            serde_json::from_str::<BaseStatus>(&status).map_err(|_| AdminError::Corrupt)?,
            preview_base
                .map(|base| serde_json::from_str::<BaseReference>(&base))
                .transpose()
                .map_err(|_| AdminError::Corrupt)?,
        ));
    }
    Ok(pins)
}

/// Every pinned LUT version beneath the versions rooted in one admission.
pub async fn affected_lut_dependents_conn(
    conn: &mut SqliteConnection,
    root_admission: &RootAdmissionId,
) -> Result<Vec<VersionRef>, AdminError> {
    let pins = pins_conn(&mut *conn).await?;
    let seeds: Vec<(String, i64)> = sqlx::query_as(
        "SELECT DISTINCT v.look, v.version FROM version_depends_on_root d
         JOIN look_versions v ON v.id = d.look_version_id
         WHERE d.root_admission_id = ?",
    )
    .bind(root_admission.as_str())
    .fetch_all(&mut *conn)
    .await?;
    let mut closure: BTreeSet<(String, u32)> = BTreeSet::new();
    for (look, version) in seeds {
        closure.insert((
            look,
            u32::try_from(version).map_err(|_| AdminError::Corrupt)?,
        ));
    }
    loop {
        let mut changed = false;
        for (look, version, base, _, _) in &pins {
            if let BaseReference::Look {
                slug,
                version: pinned,
            } = base
            {
                let Ok(pinned) = version_number(pinned) else {
                    continue;
                };
                if closure.contains(&(slug.as_str().to_owned(), pinned)) {
                    changed |= closure.insert((look.as_str().to_owned(), *version));
                }
            }
        }
        if !changed {
            break;
        }
    }
    closure
        .into_iter()
        .filter(|(look, version)| {
            pins.iter().any(|(pin_look, pin_version, ..)| {
                pin_look.as_str() == look.as_str() && pin_version == version
            })
        })
        .map(|(look, version)| {
            Ok(VersionRef {
                look: LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?,
                version,
            })
        })
        .collect()
}

pub async fn is_version_invalidated_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
) -> Result<bool, AdminError> {
    Ok(invalidated_set_conn(&mut *conn)
        .await?
        .contains(&(look.as_str().to_owned(), version)))
}

pub async fn insert_gate_failure_conn(
    conn: &mut SqliteConnection,
    look: &LookSlug,
    version: u32,
    report: &str,
    now: i64,
) -> Result<(), AdminError> {
    sqlx::query(
        "INSERT INTO gate_failures (id, look, version, report, status, created_at)
         VALUES (?, ?, ?, ?, 'pending', ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(look.as_str())
    .bind(i64::from(version))
    .bind(report)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

const LUT_REBASE_JOB: &str = "lut_rebase";

async fn enqueue_lut_rebase(conn: &mut SqliteConnection, now: i64) -> Result<(), AdminError> {
    let queued: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = ? AND status = 'queued'")
            .bind(LUT_REBASE_JOB)
            .fetch_one(&mut *conn)
            .await?;
    if queued > 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, ?, '{}', 'queued', 0, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(LUT_REBASE_JOB)
    .bind(now)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Marks the latest published version of each affected LUT Look.
pub async fn mark_lut_dependents(db: &SqlitePool, now: i64) -> Result<Vec<VersionRef>, AdminError> {
    let mut conn = db.acquire().await?;
    mark_lut_dependents_conn(&mut conn, now, None).await
}

/// `republished` names a base version a revert just wrote; it counts as that Look's newest layered version whatever its status.
async fn mark_lut_dependents_conn(
    conn: &mut SqliteConnection,
    now: i64,
    republished: Option<&VersionRef>,
) -> Result<Vec<VersionRef>, AdminError> {
    let pins = pins_conn(&mut *conn).await?;
    let invalid: HashSet<(String, u32)> = invalidated_versions_conn(&mut *conn)
        .await?
        .into_iter()
        .map(|version| (version.look.as_str().to_owned(), version.version))
        .collect();
    let latest_rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT look, MAX(version) FROM look_versions
         WHERE status IN ('published', 'rooted') GROUP BY look",
    )
    .fetch_all(&mut *conn)
    .await?;
    let latest: HashMap<String, u32> = latest_rows
        .into_iter()
        .filter_map(|(look, version)| u32::try_from(version).ok().map(|version| (look, version)))
        .collect();
    let layered_rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT v.look, v.version, m.artifacts FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.status IN ('published', 'rooted')",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut layered_latest: HashMap<String, u32> = HashMap::new();
    for (look, version, artifacts) in layered_rows {
        let Ok(version) = u32::try_from(version) else {
            continue;
        };
        let Ok(artifacts) = serde_json::from_str::<Vec<ManifestArtifact>>(&artifacts) else {
            continue;
        };
        if !artifacts
            .iter()
            .any(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        {
            continue;
        }
        layered_latest
            .entry(look)
            .and_modify(|newest| *newest = (*newest).max(version))
            .or_insert(version);
    }
    if let Some(base) = republished {
        let newest = layered_latest
            .entry(base.look.as_str().to_owned())
            .or_insert(base.version);
        *newest = (*newest).max(base.version);
    }
    let renderings: Vec<(String, String)> =
        sqlx::query_as("SELECT maker, rendering FROM lut_native_renderings")
            .fetch_all(&mut *conn)
            .await?;

    let mut marks = Vec::new();
    for (look, version, base, status, preview_base) in &pins {
        if latest.get(look.as_str()) != Some(version) {
            continue;
        }
        if invalid.contains(&(look.as_str().to_owned(), *version)) {
            marks.push((look.clone(), *version, LutRebaseReason::BaseRevoked));
            continue;
        }
        match base {
            BaseReference::Look {
                slug,
                version: pinned,
            } => {
                let declared = version_number(pinned).unwrap_or(u32::MAX);
                let Some(newest) = layered_latest.get(slug.as_str()).copied() else {
                    continue;
                };
                let already_previewed = preview_base.as_ref().is_some_and(|preview| {
                    matches!(
                        preview,
                        BaseReference::Look {
                            slug: preview_slug,
                            version: preview_version,
                        } if preview_slug.as_str() == slug.as_str()
                            && version_number(preview_version).ok() == Some(newest)
                    )
                });
                if !already_previewed
                    && (newest > declared || (newest == declared && !status.is_pinned()))
                {
                    marks.push((look.clone(), *version, LutRebaseReason::BasePublished));
                }
            }
            BaseReference::FamilyRendering {
                maker,
                rendering,
                dynamic_range,
                ..
            } => {
                if dynamic_range.is_some() {
                    continue;
                }
                let family = LutEmulation::CameraRendering {
                    maker: maker.clone(),
                    rendering: rendering.clone(),
                };
                if renderings.iter().any(|(native_maker, native_rendering)| {
                    looklab_model::file::emulation_matches_camera_rendering(
                        &family,
                        native_maker,
                        native_rendering,
                    )
                }) {
                    marks.push((look.clone(), *version, LutRebaseReason::FamilyResolved));
                }
            }
            BaseReference::BodyRealization { .. } => {}
        }
    }

    let mut marked = Vec::new();
    for (look, version, reason) in marks {
        let inserted = sqlx::query(
            "INSERT INTO lut_rebase_pending (look, version, reason, marked_at)
             VALUES (?, ?, ?, ?) ON CONFLICT (look, version) DO NOTHING",
        )
        .bind(look.as_str())
        .bind(i64::from(version))
        .bind(reason.as_str())
        .bind(now)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if inserted > 0 {
            marked.push(VersionRef { look, version });
        }
    }
    Ok(marked)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingState {
    Listed,
    Hidden,
    Removed,
}

impl ListingState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Listed => "listed",
            Self::Hidden => "hidden",
            Self::Removed => "removed",
        }
    }

    fn parse(value: &str) -> Result<Self, AdminError> {
        match value {
            "listed" => Ok(Self::Listed),
            "hidden" => Ok(Self::Hidden),
            "removed" => Ok(Self::Removed),
            _ => Err(AdminError::Corrupt),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LookListingRecord {
    pub look: LookSlug,
    pub listing: ListingState,
    pub superseded_by: Option<LookPin>,
}

fn parse_pin(value: &str) -> Result<LookPin, AdminError> {
    let (slug, version) = value.split_once('@').ok_or(AdminError::Corrupt)?;
    LookPin::new(slug, version).map_err(|_| AdminError::Corrupt)
}

fn listing_record(
    look: &LookSlug,
    listing: &str,
    superseded_by: Option<String>,
) -> Result<LookListingRecord, AdminError> {
    Ok(LookListingRecord {
        look: look.clone(),
        listing: ListingState::parse(listing)?,
        superseded_by: superseded_by.as_deref().map(parse_pin).transpose()?,
    })
}

pub async fn look_listings(db: &SqlitePool) -> Result<Vec<LookListingRecord>, AdminError> {
    let rows: Vec<(String, String, Option<String>)> =
        sqlx::query_as("SELECT look, listing, superseded_by FROM look_listings ORDER BY look")
            .fetch_all(db)
            .await?;
    rows.into_iter()
        .map(|(look, listing, superseded_by)| {
            let look = LookSlug::try_from(look).map_err(|_| AdminError::Corrupt)?;
            listing_record(&look, &listing, superseded_by)
        })
        .collect()
}

/// Returns whether the stored pointer changed.
pub async fn set_superseded_by(
    db: &SqlitePool,
    look: &LookSlug,
    superseded_by: &LookPin,
) -> Result<bool, AdminError> {
    let pin = superseded_by.to_string();
    let result = sqlx::query(
        "INSERT INTO look_listings (look, listing, superseded_by, updated_at)
         VALUES (?, 'listed', ?, ?)
         ON CONFLICT (look) DO UPDATE
         SET superseded_by = excluded.superseded_by, updated_at = excluded.updated_at
         WHERE look_listings.superseded_by IS NOT excluded.superseded_by",
    )
    .bind(look.as_str())
    .bind(&pin)
    .bind(now_unix())
    .execute(db)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn hide_look(
    db: &SqlitePool,
    moderator: &CredentialId,
    look: &LookSlug,
    reason: &str,
) -> Result<LookListingRecord, AdminError> {
    set_listing(db, moderator, look, reason, ListingState::Hidden).await
}

pub async fn remove_look(
    db: &SqlitePool,
    moderator: &CredentialId,
    look: &LookSlug,
    reason: &str,
) -> Result<LookListingRecord, AdminError> {
    set_listing(db, moderator, look, reason, ListingState::Removed).await
}

pub async fn relist_look(
    db: &SqlitePool,
    moderator: &CredentialId,
    look: &LookSlug,
    reason: &str,
) -> Result<LookListingRecord, AdminError> {
    set_listing(db, moderator, look, reason, ListingState::Listed).await
}

async fn set_listing(
    db: &SqlitePool,
    moderator: &CredentialId,
    look: &LookSlug,
    reason: &str,
    target: ListingState,
) -> Result<LookListingRecord, AdminError> {
    let reason = normalize_reason(reason)?;
    let mut conn = begin_write(db).await?;
    let result = async {
        let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
            .bind(look.as_str())
            .fetch_one(&mut *conn)
            .await?;
        if exists == 0 {
            return Err(AdminError::UnknownLook);
        }
        let current: Option<String> =
            sqlx::query_scalar("SELECT listing FROM look_listings WHERE look = ?")
                .bind(look.as_str())
                .fetch_optional(&mut *conn)
                .await?;
        let current = match current.as_deref() {
            Some(value) => ListingState::parse(value)?,
            None => ListingState::Listed,
        };
        let allowed = match target {
            ListingState::Listed => current != ListingState::Listed,
            ListingState::Hidden => current == ListingState::Listed,
            ListingState::Removed => current != ListingState::Removed,
        };
        if !allowed {
            return Err(AdminError::LookState(current.as_str()));
        }
        sqlx::query(
            "INSERT INTO look_listings (look, listing, superseded_by, updated_at)
             VALUES (?, ?, NULL, ?)
             ON CONFLICT (look) DO UPDATE
             SET listing = excluded.listing, updated_at = excluded.updated_at",
        )
        .bind(look.as_str())
        .bind(target.as_str())
        .bind(now_unix())
        .execute(&mut *conn)
        .await?;
        let (kind, event) = match target {
            ListingState::Listed => (AuditKind::LookRelisted, "look_relisted"),
            ListingState::Hidden => (AuditKind::LookHidden, "look_hidden"),
            ListingState::Removed => (AuditKind::LookRemoved, "look_removed"),
        };
        let payload = json!({
            "event": event,
            "actor": moderator,
            "look": look,
            "reason": reason,
        });
        audit::insert(&mut conn, kind, &payload).await?;
        crate::store::request_publication(&mut conn).await?;
        let (listing, superseded_by): (String, Option<String>) =
            sqlx::query_as("SELECT listing, superseded_by FROM look_listings WHERE look = ?")
                .bind(look.as_str())
                .fetch_one(&mut *conn)
                .await?;
        listing_record(look, &listing, superseded_by)
    }
    .await;
    finish(&mut conn, result).await
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditLogEntry {
    pub seq: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub payload: Value,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditLogPage {
    pub entries: Vec<AuditLogEntry>,
    pub next_after: Option<i64>,
}

pub async fn audit_log(
    db: &SqlitePool,
    after: i64,
    limit: i64,
) -> Result<AuditLogPage, AdminError> {
    let limit = limit.clamp(1, MAX_AUDIT_PAGE);
    let limit_usize = usize::try_from(limit).unwrap_or(0);
    let rows = sqlx::query(
        "SELECT seq, type, payload, ts FROM audit_log WHERE seq > ? ORDER BY seq LIMIT ?",
    )
    .bind(after)
    .bind(limit + 1)
    .fetch_all(db)
    .await?;
    let mut entries = Vec::with_capacity(rows.len().min(limit_usize));
    for row in rows {
        let payload: String = row.get("payload");
        entries.push(AuditLogEntry {
            seq: row.get("seq"),
            kind: row.get("type"),
            payload: serde_json::from_str(&payload).map_err(|_| AdminError::Corrupt)?,
            ts: row.get("ts"),
        });
    }
    let has_more = i64::try_from(entries.len()).unwrap_or(i64::MAX) > limit;
    if has_more {
        entries.truncate(limit_usize);
    }
    let next_after = if has_more {
        entries.last().map(|entry| entry.seq)
    } else {
        None
    };
    Ok(AuditLogPage {
        entries,
        next_after,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Body,
    SchemaChange,
    Mapping,
    InterfaceProfile,
    LutLook,
    EpochSplit,
    LookSplit,
    GateOverride,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProposalStatus {
    Pending,
    NeedsInfo,
    Approved,
    Rejected,
}

fn parse_proposal_status(value: &str) -> Result<ProposalStatus, AdminError> {
    match value {
        "pending" => Ok(ProposalStatus::Pending),
        "needs_info" => Ok(ProposalStatus::NeedsInfo),
        "approved" => Ok(ProposalStatus::Approved),
        "rejected" => Ok(ProposalStatus::Rejected),
        _ => Err(AdminError::Corrupt),
    }
}

fn parse_proposal_kind(value: &str) -> Result<ProposalKind, AdminError> {
    match value {
        "body" => Ok(ProposalKind::Body),
        "schema_change" => Ok(ProposalKind::SchemaChange),
        "mapping" => Ok(ProposalKind::Mapping),
        "interface_profile" => Ok(ProposalKind::InterfaceProfile),
        "lut_look" => Ok(ProposalKind::LutLook),
        "epoch_split" => Ok(ProposalKind::EpochSplit),
        "look_split" => Ok(ProposalKind::LookSplit),
        "gate_override" => Ok(ProposalKind::GateOverride),
        _ => Err(AdminError::Corrupt),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QueueProposal {
    pub id: ProposalId,
    pub kind: ProposalKind,
    pub summary: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedGate {
    pub look: LookSlug,
    pub version: u32,
    pub report: GateReport,
    pub created_at: i64,
}

/// The worker owns the report's fields.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct GateReport(pub Value);

/// A Look whose native fit keeps failing to converge ([docs/02 §4.3]).
#[derive(Debug, Clone, Serialize)]
pub struct FitHold {
    pub look: LookSlug,
    pub consecutive: u32,
    pub first_attempt: i64,
    pub last_attempt: i64,
    /// Worker-owned diagnostics of the last non-converged batch.
    pub diagnostics: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct SerialReuseFlag {
    pub flag: String,
    pub contributions: Vec<ContributionId>,
    pub credentials: Vec<CredentialId>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Nomination {
    pub credential: CredentialId,
    pub display_handle: Option<String>,
    pub reputation: f64,
    pub accepted_contributions: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModQueue {
    pub proposals: Vec<QueueProposal>,
    pub failed_gates: Vec<FailedGate>,
    pub fit_holds: Vec<FitHold>,
    pub serial_reuse: Vec<SerialReuseFlag>,
    pub nominations: Vec<Nomination>,
    pub manual_checks: Vec<ManualCheckReview>,
}

/// The queue a session at `tier` may read; endorsement nominations are seeder-only
/// (docs/08 §6).
pub async fn mod_queue(
    db: &SqlitePool,
    tier: Tier,
    manual_catalog: &looklab_schema::ManualCatalog,
) -> Result<ModQueue, AdminError> {
    let proposals = sqlx::query(
        "SELECT id, type, human_summary, created_at FROM proposals
         WHERE status IN ('pending', 'needs_info') ORDER BY created_at, id",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| {
        Ok(QueueProposal {
            id: ProposalId::try_from(row.get::<String, _>("id"))
                .map_err(|_| AdminError::Corrupt)?,
            kind: parse_proposal_kind(row.get::<String, _>("type").as_str())?,
            summary: row.get("human_summary"),
            created_at: row.get("created_at"),
        })
    })
    .collect::<Result<Vec<_>, AdminError>>()?;

    let failed_gates = sqlx::query(
        "SELECT look, version, report, created_at FROM gate_failures
         WHERE status = 'pending' ORDER BY created_at, id",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| {
        let report: String = row.get("report");
        Ok(FailedGate {
            look: LookSlug::try_from(row.get::<String, _>("look"))
                .map_err(|_| AdminError::Corrupt)?,
            version: u32::try_from(row.get::<i64, _>("version"))
                .map_err(|_| AdminError::Corrupt)?,
            report: GateReport(serde_json::from_str(&report).map_err(|_| AdminError::Corrupt)?),
            created_at: row.get("created_at"),
        })
    })
    .collect::<Result<Vec<_>, AdminError>>()?;

    let fit_holds = sqlx::query(
        "SELECT s.family AS look, h.consecutive, h.first_attempt, h.last_attempt, h.diagnostics
         FROM model_fit_holds h
         JOIN model_subjects s ON s.key = h.subject_key
         WHERE h.status = 'pending' AND s.kind = 'shared_family'
         ORDER BY h.last_attempt, s.family",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|row| {
        let diagnostics: String = row.get("diagnostics");
        Ok(FitHold {
            look: LookSlug::try_from(row.get::<String, _>("look"))
                .map_err(|_| AdminError::Corrupt)?,
            consecutive: u32::try_from(row.get::<i64, _>("consecutive"))
                .map_err(|_| AdminError::Corrupt)?,
            first_attempt: row.get("first_attempt"),
            last_attempt: row.get("last_attempt"),
            diagnostics: serde_json::from_str(&diagnostics).map_err(|_| AdminError::Corrupt)?,
        })
    })
    .collect::<Result<Vec<_>, AdminError>>()?;

    let groups: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT serial_hmac, COUNT(DISTINCT credential_id) FROM contributions
         WHERE serial_hmac IS NOT NULL GROUP BY serial_hmac
         HAVING COUNT(DISTINCT credential_id) > 1",
    )
    .fetch_all(db)
    .await?;
    let mut serial_reuse = Vec::with_capacity(groups.len());
    for (hmac, _) in groups {
        let rows = sqlx::query(
            "SELECT id, credential_id FROM contributions
             WHERE serial_hmac = ? ORDER BY created_at, id",
        )
        .bind(&hmac)
        .fetch_all(db)
        .await?;
        let mut contributions = Vec::with_capacity(rows.len());
        let mut credentials = BTreeSet::new();
        for row in rows {
            contributions.push(
                ContributionId::try_from(row.get::<String, _>("id"))
                    .map_err(|_| AdminError::Corrupt)?,
            );
            credentials.insert(
                CredentialId::try_from(row.get::<String, _>("credential_id"))
                    .map_err(|_| AdminError::Corrupt)?,
            );
        }
        serial_reuse.push(SerialReuseFlag {
            flag: hex(&Sha256::digest(&hmac)[..8]),
            contributions,
            credentials: credentials.into_iter().collect(),
        });
    }

    let mut nominations = Vec::new();
    if tier == Tier::Seeder {
        let state = {
            let mut conn = db.acquire().await?;
            trust_state(&mut conn).await?
        };
        for nominated in state.nominations(&TrustParams::default()) {
            let credential = local_credential(nominated)?;
            let row: Option<(Option<String>, f64)> =
                sqlx::query_as("SELECT display_handle, reputation FROM credentials WHERE id = ?")
                    .bind(credential.as_str())
                    .fetch_optional(db)
                    .await?;
            let Some((display_handle, reputation)) = row else {
                continue;
            };
            let accepted: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM contributions
                 WHERE credential_id = ? AND status = 'accepted' AND render_class = 'colour'",
            )
            .bind(credential.as_str())
            .fetch_one(db)
            .await?;
            nominations.push(Nomination {
                credential,
                display_handle,
                reputation,
                accepted_contributions: accepted,
            });
        }
    }

    Ok(ModQueue {
        proposals,
        failed_gates,
        fit_holds,
        serial_reuse,
        nominations,
        manual_checks: crate::manual_confirm::reviews(db, manual_catalog).await?,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModDecision {
    Approve,
    Reject,
    RequestInfo,
}

impl ModDecision {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::RequestInfo => "request_info",
        }
    }

    const fn proposal_status(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::Reject => "rejected",
            Self::RequestInfo => "needs_info",
        }
    }
}

/// The snapshot an approval publishes, recorded in the approval's transaction.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Publication<'a> {
    pub version: u64,
    pub content_hash: &'a str,
}

pub async fn decide(
    db: &SqlitePool,
    actor: Actor,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
) -> Result<(), AdminError> {
    decide_recording(db, actor, proposal, decision, reason, None).await
}

pub(crate) async fn decide_recording(
    db: &SqlitePool,
    actor: Actor,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
    publication: Option<Publication<'_>>,
) -> Result<(), AdminError> {
    decide_recording_job(db, actor, proposal, decision, reason, publication, None).await
}

pub(crate) struct DecisionJob {
    pub kind: crate::store::JobKind,
    pub payload: String,
}

pub(crate) async fn decide_recording_job(
    db: &SqlitePool,
    actor: Actor,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
    publication: Option<Publication<'_>>,
    job: Option<DecisionJob>,
) -> Result<(), AdminError> {
    let mut conn = begin_write(db).await?;
    let result = decide_recording_job_in(
        &mut conn,
        actor,
        proposal,
        decision,
        reason,
        publication,
        job,
    )
    .await;
    finish(&mut conn, result).await
}

pub(crate) async fn decide_recording_job_in(
    conn: &mut SqliteConnection,
    actor: Actor,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
    publication: Option<Publication<'_>>,
    job: Option<DecisionJob>,
) -> Result<(), AdminError> {
    let reason = normalize_reason(reason)?;
    let actor = actor_text(actor)?;
    let row: Option<(String,)> = sqlx::query_as("SELECT status FROM proposals WHERE id = ?")
        .bind(proposal.as_str())
        .fetch_optional(&mut *conn)
        .await?;
    let Some((status,)) = row else {
        return Err(AdminError::UnknownProposal);
    };
    match parse_proposal_status(&status)? {
        ProposalStatus::Pending | ProposalStatus::NeedsInfo => {}
        ProposalStatus::Approved | ProposalStatus::Rejected => {
            return Err(AdminError::ProposalDecided);
        }
    }
    sqlx::query(
        "INSERT INTO moderation_decisions
         (id, proposal_id, moderator_credential, decision, reason, decided_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(ModerationDecisionId::new().as_str())
    .bind(proposal.as_str())
    .bind(actor.as_str())
    .bind(decision.as_str())
    .bind(&reason)
    .bind(now_unix())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE proposals SET status = ? WHERE id = ?")
        .bind(decision.proposal_status())
        .bind(proposal.as_str())
        .execute(&mut *conn)
        .await?;
    if let Some(publication) = publication {
        let version = i64::try_from(publication.version).map_err(|_| AdminError::Corrupt)?;
        sqlx::query(
            "UPDATE proposals
             SET publication = 'pending', published_version = ?, published_hash = ?
             WHERE id = ?",
        )
        .bind(version)
        .bind(publication.content_hash)
        .bind(proposal.as_str())
        .execute(&mut *conn)
        .await?;
    }
    let payload = json!({
        "event": "moderation_decision",
        "actor": actor,
        "proposal": proposal,
        "decision": decision,
        "reason": reason,
    });
    audit::insert(&mut *conn, AuditKind::ModerationDecision, &payload).await?;
    if let Some(job) = job {
        crate::store::enqueue_job(&mut *conn, job.kind, &job.payload, now_unix()).await?;
    }
    Ok(())
}
