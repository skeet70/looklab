// SPDX-License-Identifier: AGPL-3.0-or-later
//! Grants, endorsements, root admissions, moderation state and version manifests,
//! shared by the HTTP API and `looklab-cli admin` ([docs/05 §3 §7 §11], [docs/08 §6]).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::str::FromStr;

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
use crate::db::now_unix;
use crate::ids::{
    ContributionId, CredentialId, EndorsementId, GrantId, LookVersionId, ModerationDecisionId,
    ProposalId, RootAdmissionId,
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
    #[error("the contribution kind cannot root data")]
    IneligibleContribution,
    #[error("no active {0} record")]
    NoActiveRecord(&'static str),
    #[error("the root admission is unknown")]
    UnknownRootAdmission,
    #[error("the proposal payload is invalid: {0}")]
    InvalidProposal(String),
    #[error("the version is unknown")]
    UnknownVersion,
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
    #[error("the stored record is not readable")]
    Corrupt,
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

fn normalize_reason(reason: &str) -> Result<String, AdminError> {
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
        let row: Option<(String, String, String, String, Option<String>, String)> = sqlx::query_as(
            "SELECT credential_id, kind, status, body_id, target_look_id, intent
             FROM contributions WHERE id = ?",
        )
        .bind(contribution_id.as_str())
        .fetch_optional(&mut *conn)
        .await?;
        let Some((credential, kind, status, body_id, target_look_id, intent)) = row else {
            return Err(AdminError::UnknownContribution);
        };
        let kind = parse_contribution_kind(&kind)?;
        match ContributionStatus::from_db(&status).ok_or(AdminError::Corrupt)? {
            ContributionStatus::Pending | ContributionStatus::Accepted => {}
            ContributionStatus::Quarantined | ContributionStatus::Withdrawn => {
                return Err(AdminError::IneligibleContribution);
            }
        }
        let contribution_body =
            looklab_upload::BodyId::new(&body_id).map_err(|_| AdminError::Corrupt)?;
        let target_look = target_look_id
            .map(|look| LookSlug::try_from(look.as_str()).map_err(|_| AdminError::Corrupt))
            .transpose()?;
        let intent = match intent.as_str() {
            "native" => Intent::Native,
            "recipe" => Intent::Recipe {
                target_look: target_look.clone().ok_or(AdminError::Corrupt)?,
            },
            "card" => return Err(AdminError::IneligibleContribution),
            _ => return Err(AdminError::Corrupt),
        };
        let subject = CredentialId::try_from(credential).map_err(|_| AdminError::Corrupt)?;
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
        let payload = json!({
            "event": "root_revoked",
            "actor": actor_text(actor)?,
            "root_admission": root_admission,
            "scope": scope,
            "reason": reason,
        });
        audit::insert(&mut conn, AuditKind::RootRevoked, &payload).await?;

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
        Ok(RootRevocation {
            root_admission: root_admission.clone(),
            new_versions,
            regating,
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

/// Opens a moderation proposal from the worker. An identical pending worker
/// proposal is returned as `None` instead of being duplicated.
pub async fn submit_worker_proposal(
    db: &SqlitePool,
    proposal: WorkerProposal,
) -> Result<Option<ProposalId>, AdminError> {
    let validated = looklab_schema::validate_proposal(proposal.kind, &proposal.payload)
        .map_err(|errors| AdminError::InvalidProposal(format!("{errors:?}")))?;
    let summary = validated.summary().text();
    let evidence = serde_json::to_string(&proposal.evidence).map_err(|_| AdminError::Corrupt)?;
    let mut conn = begin_write(db).await?;
    let result = async {
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
        .bind(&summary)
        .bind(&evidence)
        .bind(now)
        .execute(&mut *conn)
        .await?;
        let payload = json!({
            "event": "proposal_opened",
            "actor": "worker",
            "proposal": id,
            "kind": proposal.kind.as_str(),
            "summary": summary,
        });
        audit::insert(&mut conn, AuditKind::ProposalOpened, &payload).await?;
        Ok(Some(id))
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

pub async fn write_version_manifest(
    db: &SqlitePool,
    look: &LookSlug,
    version: VersionId,
    artifacts: &[ManifestArtifact],
    depends_on_root: &[RootAdmissionId],
) -> Result<ManifestRecord, AdminError> {
    let artifacts = normalize_artifacts(artifacts)?;
    let mut depends_on_root = depends_on_root.to_vec();
    depends_on_root.sort();
    depends_on_root.dedup();

    let mut conn = begin_write(db).await?;
    let result = async {
        for root in &depends_on_root {
            let exists: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM root_admissions WHERE id = ?")
                    .bind(root.as_str())
                    .fetch_one(&mut *conn)
                    .await?;
            if exists == 0 {
                return Err(AdminError::UnknownRootAdmission);
            }
        }
        let now = now_unix();
        sqlx::query(
            "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
             VALUES (?, ?, ?, 'published', NULL, ?)
             ON CONFLICT (look, version) DO NOTHING",
        )
        .bind(LookVersionId::new().as_str())
        .bind(look.as_str())
        .bind(version.get())
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
        let (version_id, status, created_at) = row;
        let status = parse_version_status(&status)?;
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
        let canonical = json!({
            "look": look,
            "version": version.get(),
            "artifacts": artifacts,
        });
        let canonical = serde_json::to_vec(&canonical).map_err(|_| AdminError::Corrupt)?;
        let manifest_sha256 = hex(&Sha256::digest(&canonical));
        let payload = json!({
            "event": "release_manifest",
            "look": look,
            "version": version.get(),
            "manifest_sha256": manifest_sha256,
        });
        audit::insert(&mut conn, AuditKind::ReleaseManifest, &payload).await?;
        Ok(ManifestRecord {
            look: look.clone(),
            version: version.get(),
            status,
            artifacts: artifacts.clone(),
            created_at,
        })
    }
    .await;
    finish(&mut conn, result).await
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
    pub serial_reuse: Vec<SerialReuseFlag>,
    pub nominations: Vec<Nomination>,
}

/// The queue a session at `tier` may read; endorsement nominations are seeder-only
/// (docs/08 §6).
pub async fn mod_queue(db: &SqlitePool, tier: Tier) -> Result<ModQueue, AdminError> {
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
            "SELECT id, credential_id FROM contributions WHERE serial_hmac = ? ORDER BY created_at, id",
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
                "SELECT COUNT(*) FROM contributions WHERE credential_id = ? AND status = 'accepted'",
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
        serial_reuse,
        nominations,
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
    moderator: &CredentialId,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
) -> Result<(), AdminError> {
    decide_recording(db, moderator, proposal, decision, reason, None).await
}

pub(crate) async fn decide_recording(
    db: &SqlitePool,
    moderator: &CredentialId,
    proposal: &ProposalId,
    decision: ModDecision,
    reason: &str,
    publication: Option<Publication<'_>>,
) -> Result<(), AdminError> {
    let reason = normalize_reason(reason)?;
    let mut conn = begin_write(db).await?;
    let result = async {
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
        .bind(moderator.as_str())
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
            "actor": moderator,
            "proposal": proposal,
            "decision": decision,
            "reason": reason,
        });
        audit::insert(&mut conn, AuditKind::ModerationDecision, &payload).await?;
        Ok(())
    }
    .await;
    finish(&mut conn, result).await
}
