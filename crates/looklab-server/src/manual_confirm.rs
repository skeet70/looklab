// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owner confirmations of documented camera controls.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use looklab_schema::{
    BodyId, CapabilityKey, ConfirmationState, Finding, IdentityGroupId, ManualCamera,
    ManualConfirmation, ManualConfirmationSummary, ManualEvidence, ManualUrl, ModeratorEvidence,
    ModeratorEvidenceKind, ModeratorFirmwareScope, ModeratorRecord, ReportedFirmware, Sha256Hex,
    evaluate_camera,
};
use looklab_upload::Nonce;
use serde_json::json;
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::audit::{self, AuditKind};
use crate::ids::CredentialId;
use crate::store;

/// Two credentials that reported the same serial HMAC for this body share a
/// group; the representative is the smallest credential id, never the HMAC.
pub async fn identity_groups(
    conn: &mut SqliteConnection,
) -> Result<HashMap<CredentialId, CredentialId>, sqlx::Error> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT credential_id, serial_hmac FROM contributions
         WHERE serial_hmac IS NOT NULL",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut parent: HashMap<IdentityNode, IdentityNode> = HashMap::new();
    let mut credentials: Vec<CredentialId> = Vec::new();
    for (credential, hmac) in rows {
        let credential = CredentialId::try_from(credential)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))?;
        let hmac: [u8; 32] = hmac
            .try_into()
            .map_err(|_| sqlx::Error::Protocol("serial HMAC is not 32 bytes".to_owned()))?;
        let credential_node = IdentityNode::Credential(credential.clone());
        let serial_node = IdentityNode::Serial(hmac);
        parent
            .entry(credential_node.clone())
            .or_insert_with(|| credential_node.clone());
        parent
            .entry(serial_node.clone())
            .or_insert_with(|| serial_node.clone());
        let left = find(&mut parent, &credential_node);
        let right = find(&mut parent, &serial_node);
        if left != right {
            let (low, high) = if left < right {
                (left, right)
            } else {
                (right, left)
            };
            parent.insert(high, low);
        }
        credentials.push(credential);
    }
    let mut representatives: HashMap<IdentityNode, CredentialId> = HashMap::new();
    for credential in &credentials {
        let root = find(&mut parent, &IdentityNode::Credential(credential.clone()));
        let representative = representatives
            .entry(root)
            .or_insert_with(|| credential.clone());
        if credential < &*representative {
            *representative = credential.clone();
        }
    }
    let mut groups = HashMap::new();
    for credential in credentials {
        let root = find(&mut parent, &IdentityNode::Credential(credential.clone()));
        let representative = representatives
            .get(&root)
            .cloned()
            .unwrap_or_else(|| credential.clone());
        groups.insert(credential, representative);
    }
    Ok(groups)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum IdentityNode {
    Credential(CredentialId),
    Serial([u8; 32]),
}

fn find(parent: &mut HashMap<IdentityNode, IdentityNode>, key: &IdentityNode) -> IdentityNode {
    let mut root = key.clone();
    while let Some(next) = parent.get(&root) {
        if *next == root {
            break;
        }
        root = next.clone();
    }
    let mut current = key.clone();
    while current != root {
        let next = parent
            .get(&current)
            .cloned()
            .unwrap_or_else(|| root.clone());
        parent.insert(current.clone(), root.clone());
        current = next;
    }
    root
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Credentials with a serial link. Identity groups come from all known links;
/// a group counts for this camera only through this set.
pub async fn camera_credentials(
    conn: &mut SqliteConnection,
    camera: &BodyId,
) -> Result<HashSet<CredentialId>, sqlx::Error> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT credential_id FROM contributions
         WHERE serial_hmac IS NOT NULL AND body_id = ? AND status != 'withdrawn'",
    )
    .bind(camera.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(credential,)| {
            CredentialId::try_from(credential)
                .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))
        })
        .collect()
}

struct LatestObservation {
    occurrence: i64,
    withdrawn: bool,
    firmware: String,
    digest: Vec<u8>,
    finding: String,
}

/// A credential with no serial HMAC for this body contributes no evidence.
pub async fn evidence(
    conn: &mut SqliteConnection,
    camera: &BodyId,
    groups: &HashMap<CredentialId, CredentialId>,
    eligible: &HashSet<CredentialId>,
) -> Result<Vec<ManualEvidence>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.credential_id, c.firmware, c.withdrawn_at AS confirmation_withdrawn,
                o.capability_key, o.definition_digest, o.finding_json,
                occ.id AS occurrence_id, occ.withdrawn_at AS occurrence_withdrawn
         FROM manual_confirmations c
         JOIN manual_observations o ON o.confirmation_id = c.id
         JOIN manual_confirmation_occurrences occ ON occ.confirmation_id = c.id
         WHERE c.camera_id = ?
         ORDER BY occ.id",
    )
    .bind(camera.as_str())
    .fetch_all(&mut *conn)
    .await?;
    type EvidenceKey = (CredentialId, CapabilityKey, Vec<u8>, String);
    let mut latest: HashMap<EvidenceKey, LatestObservation> = HashMap::new();
    for row in rows {
        let credential: String = row.get("credential_id");
        let credential = CredentialId::try_from(credential)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))?;
        if !eligible.contains(&credential) {
            continue;
        }
        let Some(group) = groups.get(&credential) else {
            continue;
        };
        let key_text: String = row.get("capability_key");
        let key = CapabilityKey::from_str(&key_text)
            .map_err(|()| sqlx::Error::Protocol(format!("unknown control {key_text}")))?;
        let occurrence: i64 = row.get("occurrence_id");
        let occurrence_withdrawn: Option<i64> = row.get("occurrence_withdrawn");
        let confirmation_withdrawn: Option<i64> = row.get("confirmation_withdrawn");
        let firmware: String = row.get("firmware");
        let digest: Vec<u8> = row.get("definition_digest");
        let evidence_key = (group.clone(), key, digest.clone(), firmware.clone());
        let entry = LatestObservation {
            occurrence,
            withdrawn: occurrence_withdrawn.is_some() || confirmation_withdrawn.is_some(),
            firmware,
            digest,
            finding: row.get("finding_json"),
        };
        match latest.get(&evidence_key) {
            Some(existing) if existing.occurrence >= occurrence => {}
            _ => {
                latest.insert(evidence_key, entry);
            }
        }
    }
    let mut evidence = Vec::with_capacity(latest.len());
    for ((group, key, _, _), observation) in latest {
        if observation.withdrawn {
            continue;
        }
        evidence.push(ManualEvidence {
            identity_group: IdentityGroupId::new(group.as_str())
                .map_err(|_| sqlx::Error::Protocol("empty identity group".to_owned()))?,
            firmware: ReportedFirmware::new(&observation.firmware)
                .map_err(|_| sqlx::Error::Protocol("empty firmware".to_owned()))?,
            control: key,
            definition_digest: Sha256Hex::from_bytes(observation.digest.try_into().map_err(
                |_| sqlx::Error::Protocol("definition digest is not 32 bytes".to_owned()),
            )?),
            finding: serde_json::from_str::<Finding>(&observation.finding)
                .map_err(|_| sqlx::Error::Protocol("finding is not readable".to_owned()))?,
            // Occurrence ids are the monotonic submission order.
            submitted_at: observation.occurrence,
        });
    }
    Ok(evidence)
}

pub async fn moderator_records(
    conn: &mut SqliteConnection,
    camera: &ManualCamera,
) -> Result<BTreeMap<CapabilityKey, Vec<ModeratorRecord>>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT capability_key, definition_digest, firmware_scope, confirmed, evidence_kind,
                evidence_url, evidence_description
         FROM manual_moderator_confirmations
         WHERE camera_id = ?
         ORDER BY created_at DESC, id DESC",
    )
    .bind(camera.id.as_str())
    .fetch_all(&mut *conn)
    .await?;
    let mut records: BTreeMap<CapabilityKey, Vec<ModeratorRecord>> = BTreeMap::new();
    for row in rows {
        let key_text: String = row.get("capability_key");
        let Ok(key) = CapabilityKey::from_str(&key_text) else {
            continue;
        };
        let Some(capability) = camera
            .capabilities
            .iter()
            .find(|capability| capability.key == key)
        else {
            continue;
        };
        let digest: Vec<u8> = row.get("definition_digest");
        let Ok(digest) = <[u8; 32]>::try_from(digest) else {
            continue;
        };
        if Sha256Hex::from_bytes(digest) != capability.definition_digest(camera) {
            continue;
        }
        let scope: String = row.get("firmware_scope");
        let Ok(firmware_scope) = serde_json::from_str::<ModeratorFirmwareScope>(&scope) else {
            continue;
        };
        let confirmed: i64 = row.get("confirmed");
        let evidence_kind: String = row.get("evidence_kind");
        let evidence_url: Option<String> = row.get("evidence_url");
        let evidence_description: String = row.get("evidence_description");
        let Ok(kind) = ModeratorEvidenceKind::from_str(&evidence_kind) else {
            continue;
        };
        let url = evidence_url
            .as_deref()
            .and_then(|value| ManualUrl::new(value).ok());
        let record = ModeratorRecord {
            confirmed: confirmed != 0,
            evidence: ModeratorEvidence {
                kind,
                url,
                description: evidence_description,
                firmware_scope,
            },
        };
        let entries = records.entry(key).or_default();
        if !entries
            .iter()
            .any(|existing| existing.evidence.firmware_scope == record.evidence.firmware_scope)
        {
            entries.push(record);
        }
    }
    Ok(records)
}

pub async fn summary(
    db: &SqlitePool,
    camera: &ManualCamera,
) -> Result<ManualConfirmationSummary, sqlx::Error> {
    let mut conn = db.acquire().await?;
    summary_with(&mut conn, camera).await
}

pub async fn summary_with(
    conn: &mut SqliteConnection,
    camera: &ManualCamera,
) -> Result<ManualConfirmationSummary, sqlx::Error> {
    let groups = identity_groups(&mut *conn).await?;
    let eligible = camera_credentials(&mut *conn, &camera.id).await?;
    let evidence = evidence(&mut *conn, &camera.id, &groups, &eligible).await?;
    let moderators = moderator_records(&mut *conn, camera).await?;
    Ok(evaluate_camera(camera, &evidence, &moderators))
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("the send does not list the confirmation as pending")]
    AttestationNotPending,
    #[error("the camera withdrew confirmations on this send")]
    Blocked,
}

/// A repeated payload is stored once but still completes the attestation of the
/// send that authorized it.
pub async fn record(
    db: &SqlitePool,
    credential: &CredentialId,
    camera: &ManualCamera,
    confirmation: &ManualConfirmation,
    payload_digest: Sha256Hex,
    assertion_hash: &[u8; 32],
    send_nonce: &Nonce,
) -> Result<(), RecordError> {
    let scope_json = serde_json::to_string(&confirmation.scope)
        .map_err(|_| sqlx::Error::Protocol("scope does not serialize".to_owned()))?;
    let scope_kind = confirmation.scope.as_str();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    let blocked: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM manual_confirmation_blocked_sends
         WHERE credential_id = ? AND camera_id = ? AND send_nonce = ?",
    )
    .bind(credential.as_str())
    .bind(confirmation.camera.as_str())
    .bind(hex(send_nonce.as_bytes()))
    .fetch_optional(&mut *tx)
    .await?;
    if blocked.is_some() {
        return Err(RecordError::Blocked);
    }
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO manual_confirmations
         (credential_id, camera_id, camera_digest, firmware, scope_kind, scope_json,
          payload_digest, assertion_hash, send_nonce, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(credential.as_str())
    .bind(confirmation.camera.as_str())
    .bind(confirmation.camera_digest.as_bytes().as_slice())
    .bind(confirmation.firmware.as_str())
    .bind(scope_kind)
    .bind(scope_json)
    .bind(payload_digest.as_bytes().as_slice())
    .bind(assertion_hash.as_slice())
    .bind(hex(send_nonce.as_bytes()))
    .bind(crate::db::now_unix())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let id: i64 = sqlx::query_scalar(
        "SELECT id FROM manual_confirmations WHERE credential_id = ? AND payload_digest = ?",
    )
    .bind(credential.as_str())
    .bind(payload_digest.as_bytes().as_slice())
    .fetch_one(&mut *tx)
    .await?;
    if inserted == 1 {
        for observation in &confirmation.observations {
            let finding = serde_json::to_string(&observation.finding)
                .map_err(|_| sqlx::Error::Protocol("finding does not serialize".to_owned()))?;
            sqlx::query(
                "INSERT INTO manual_observations
                 (confirmation_id, capability_key, definition_digest, finding_json)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(id)
            .bind(observation.key.as_str())
            .bind(observation.definition_digest.as_bytes().as_slice())
            .bind(finding)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "INSERT INTO manual_confirmation_occurrences (confirmation_id, send_nonce, received_at)
         VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(hex(send_nonce.as_bytes()))
    .bind(crate::db::now_unix())
    .execute(&mut *tx)
    .await?;
    if !store::mark_send_attestation_received(&mut tx, send_nonce, payload_digest.as_bytes(), id)
        .await?
    {
        tx.rollback().await?;
        return Err(RecordError::AttestationNotPending);
    }
    let reactivated = sqlx::query(
        "UPDATE manual_confirmations SET withdrawn_at = NULL
         WHERE id = ? AND withdrawn_at IS NOT NULL",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if inserted == 1 || reactivated {
        let payload = json!({
            "event": "manual_confirmation",
            "confirmation": id,
            "camera": confirmation.camera.as_str(),
            "firmware": confirmation.firmware.as_str(),
            "controls": confirmation.observations.len(),
        });
        audit::insert(&mut tx, AuditKind::ManualConfirmation, &payload).await?;
    }
    let summary = summary_with(&mut tx, camera).await?;
    promotion_audit_with(&mut tx, camera, &summary).await?;
    store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Marks every live confirmation this credential made for the camera withdrawn.
pub async fn withdraw(
    db: &SqlitePool,
    credential: &CredentialId,
    camera: &ManualCamera,
) -> Result<u64, sqlx::Error> {
    let mut tx = db.begin().await?;
    let now = crate::db::now_unix();
    sqlx::query(
        "INSERT OR IGNORE INTO manual_confirmation_blocked_sends
         (credential_id, camera_id, send_nonce)
         SELECT ?, ?, nonce FROM send_manifests
         WHERE credential_id = (SELECT credential_id FROM credentials WHERE id = ?)",
    )
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .bind(credential.as_str())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE manual_confirmation_occurrences SET withdrawn_at = ?
         WHERE withdrawn_at IS NULL
           AND confirmation_id IN (SELECT id FROM manual_confirmations
                                   WHERE credential_id = ? AND camera_id = ?)",
    )
    .bind(now)
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .execute(&mut *tx)
    .await?;
    let affected = sqlx::query(
        "UPDATE manual_confirmations SET withdrawn_at = ?
         WHERE credential_id = ? AND camera_id = ? AND withdrawn_at IS NULL",
    )
    .bind(now)
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected > 0 {
        let payload = json!({
            "event": "manual_confirmation_withdrawal",
            "camera": camera.id.as_str(),
            "withdrawn": affected,
        });
        audit::insert(&mut tx, AuditKind::ManualConfirmationWithdrawal, &payload).await?;
    }
    let summary = summary_with(&mut tx, camera).await?;
    promotion_audit_with(&mut tx, camera, &summary).await?;
    store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(affected)
}

pub async fn record_moderator(
    db: &SqlitePool,
    camera: &ManualCamera,
    key: CapabilityKey,
    definition_digest: &Sha256Hex,
    credential: &CredentialId,
    record: &ModeratorRecord,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    let evidence_kind = record.evidence.kind.as_str();
    let firmware_scope = serde_json::to_string(&record.evidence.firmware_scope).map_err(|_| {
        sqlx::Error::Protocol("moderator firmware scope does not serialize".to_owned())
    })?;
    sqlx::query(
        "INSERT INTO manual_moderator_confirmations
         (camera_id, capability_key, definition_digest, firmware_scope, moderator_credential,
          confirmed, evidence_kind, evidence_url, evidence_description, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(camera.id.as_str())
    .bind(key.as_str())
    .bind(definition_digest.as_bytes().as_slice())
    .bind(&firmware_scope)
    .bind(credential.as_str())
    .bind(record.confirmed)
    .bind(evidence_kind)
    .bind(record.evidence.url.as_ref().map(ManualUrl::as_str))
    .bind(&record.evidence.description)
    .bind(crate::db::now_unix())
    .execute(&mut *tx)
    .await?;
    let payload = json!({
        "event": "manual_moderator_confirmation",
        "camera": camera.id.as_str(),
        "control": key.as_str(),
        "definition_digest": definition_digest.to_hex(),
        "decision": if record.confirmed { "confirm" } else { "reject" },
        "firmware_scope": &record.evidence.firmware_scope,
        "moderator": credential.as_str(),
        "evidence_kind": evidence_kind,
        "evidence_url": record.evidence.url.as_ref().map(ManualUrl::as_str),
    });
    audit::insert(&mut tx, AuditKind::ManualModeratorConfirmation, &payload).await?;
    store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn promotion_audit_with(
    conn: &mut SqliteConnection,
    camera: &ManualCamera,
    summary: &ManualConfirmationSummary,
) -> Result<(), sqlx::Error> {
    for control in &summary.controls {
        for firmware in &control.firmware {
            if firmware.state != ConfirmationState::Confirmed {
                continue;
            }
            let payload = json!({
                "event": "manual_confirmation_promotion",
                "camera": camera.id.as_str(),
                "control": control.key.as_str(),
                "groups": firmware.agreeing_groups,
                "firmware": firmware.firmware.as_str(),
                "basis": &firmware.basis,
                "confirmed_actual": &firmware.confirmed_actual,
            });
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT payload FROM audit_log WHERE type = ?")
                    .bind(AuditKind::ManualConfirmationPromotion.as_str())
                    .fetch_all(&mut *conn)
                    .await?;
            let already = rows.iter().any(|(stored,)| {
                serde_json::from_str::<serde_json::Value>(stored)
                    .ok()
                    .is_some_and(|value| {
                        value.get("camera") == payload.get("camera")
                            && value.get("control") == payload.get("control")
                            && value.get("firmware") == payload.get("firmware")
                            && value.get("basis") == payload.get("basis")
                            && value.get("confirmed_actual") == payload.get("confirmed_actual")
                    })
            });
            if already {
                continue;
            }
            audit::insert(&mut *conn, AuditKind::ManualConfirmationPromotion, &payload).await?;
        }
    }
    Ok(())
}
