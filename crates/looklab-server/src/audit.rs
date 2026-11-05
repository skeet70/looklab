// SPDX-License-Identifier: AGPL-3.0-or-later
//! Audit-log rows. The initial migration's triggers reject UPDATE and DELETE.

use looklab_trust::{Actor, AuditEvent};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqliteConnection;

use crate::admin::{AdminError, local_credential};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditKind {
    GrantCreated,
    GrantRevoked,
    EndorsementIssued,
    EndorsementRevoked,
    EndorsementSuperseded,
    RootAdmitted,
    RootRevoked,
    ModerationDecision,
    ProposalOpened,
    ReAnchor,
    Revert,
    Split,
    ReleaseManifest,
}

impl AuditKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::GrantCreated => "grant_created",
            Self::GrantRevoked => "grant_revoked",
            Self::EndorsementIssued => "endorsement_issued",
            Self::EndorsementRevoked => "endorsement_revoked",
            Self::EndorsementSuperseded => "endorsement_superseded",
            Self::RootAdmitted => "root_admitted",
            Self::RootRevoked => "root_revoked",
            Self::ModerationDecision => "moderation_decision",
            Self::ProposalOpened => "proposal_opened",
            Self::ReAnchor => "re_anchor",
            Self::Revert => "revert",
            Self::Split => "split",
            Self::ReleaseManifest => "release_manifest",
        }
    }
}

/// Who acted, as the public log may show it: an internal credential id or `cli`.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct ActorRef(String);

pub(crate) async fn insert(
    conn: &mut SqliteConnection,
    kind: AuditKind,
    payload: &Value,
) -> Result<(), sqlx::Error> {
    let payload = serde_json::to_string(payload)
        .map_err(|_| sqlx::Error::Protocol("audit payload does not serialize".to_owned()))?;
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES (?, ?, ?)")
        .bind(kind.as_str())
        .bind(payload)
        .bind(crate::db::now_unix())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn actor(actor: Actor) -> Result<ActorRef, AdminError> {
    match actor {
        Actor::Cli => Ok(ActorRef("cli".to_owned())),
        Actor::Credential(id) => Ok(ActorRef(local_credential(id)?.as_str().to_owned())),
    }
}

fn as_value<T: Serialize>(value: &T) -> Result<Value, AdminError> {
    serde_json::to_value(value).map_err(|_| AdminError::Corrupt)
}

/// Credential ids become the server's internal ids.
pub(crate) fn trust_event(event: &AuditEvent) -> Result<(AuditKind, Value), AdminError> {
    let (kind, payload) = match event {
        AuditEvent::GrantCreated {
            actor: who,
            subject,
            grant,
        } => (
            AuditKind::GrantCreated,
            json!({ "event": "grant_created", "actor": actor(*who)?, "subject": local_credential(*subject)?, "grant": as_value(grant)? }),
        ),
        AuditEvent::GrantRevoked {
            actor: who,
            subject,
            grant,
        } => (
            AuditKind::GrantRevoked,
            json!({ "event": "grant_revoked", "actor": actor(*who)?, "subject": local_credential(*subject)?, "grant": as_value(grant)? }),
        ),
        AuditEvent::EndorsementIssued {
            actor: who,
            subject,
            endorsement,
        } => (
            AuditKind::EndorsementIssued,
            json!({ "event": "endorsement_issued", "actor": actor(*who)?, "subject": local_credential(*subject)?, "endorsement": as_value(endorsement)? }),
        ),
        AuditEvent::EndorsementRevoked {
            actor: who,
            subject,
        } => (
            AuditKind::EndorsementRevoked,
            json!({ "event": "endorsement_revoked", "actor": actor(*who)?, "subject": local_credential(*subject)? }),
        ),
        AuditEvent::RootAdmitted {
            actor: who,
            subject,
            look,
            body,
            reason,
        } => (
            AuditKind::RootAdmitted,
            json!({ "event": "root_admitted", "actor": actor(*who)?, "subject": local_credential(*subject)?, "look": look, "body": body, "reason": reason }),
        ),
        AuditEvent::RootRevoked {
            actor: who,
            subject,
            look,
            body,
        } => (
            AuditKind::RootRevoked,
            json!({ "event": "root_revoked", "actor": actor(*who)?, "subject": local_credential(*subject)?, "look": look, "body": body }),
        ),
        AuditEvent::ModerationDecision {
            actor: who,
            proposal,
            outcome,
            reason,
        } => (
            AuditKind::ModerationDecision,
            json!({ "event": "moderation_decision", "actor": actor(*who)?, "proposal": as_value(proposal)?, "outcome": as_value(outcome)?, "reason": reason }),
        ),
        AuditEvent::ReAnchor {
            actor: who,
            look,
            version,
            reason,
        } => (
            AuditKind::ReAnchor,
            json!({ "event": "re_anchor", "actor": actor(*who)?, "look": look, "version": as_value(version)?, "reason": reason }),
        ),
        AuditEvent::Revert {
            actor: who,
            look,
            plan,
        } => (
            AuditKind::Revert,
            json!({ "event": "revert", "actor": actor(*who)?, "look": look, "plan": as_value(plan)? }),
        ),
        AuditEvent::Split {
            actor: who,
            look,
            from,
            into,
            reason,
        } => (
            AuditKind::Split,
            json!({ "event": "split", "actor": actor(*who)?, "look": look, "from": as_value(from)?, "into": as_value(into)?, "reason": reason }),
        ),
        AuditEvent::ReleaseManifest {
            look,
            version,
            artifact,
        } => (
            AuditKind::ReleaseManifest,
            json!({ "event": "release_manifest", "look": look, "version": as_value(version)?, "artifact": artifact }),
        ),
    };
    Ok((kind, payload))
}
