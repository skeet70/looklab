// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owner confirmations of documented camera controls.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use looklab_schema::{
    BodyId, CapabilityKey, ConfirmationState, DocumentedCapability, Finding, IdentityGroupId,
    ManualCamera, ManualCatalog, ManualCheckReview, ManualCheckReviewStatus, ManualConfirmation,
    ManualConfirmationSummary, ManualControlIntake, ManualEvidence, ManualOwnerLinkage, ManualUrl,
    ModeratorEvidence, ModeratorEvidenceKind, ModeratorFirmwareScope, ModeratorRecord,
    ReportedFirmware, Sha256Hex, evaluate_camera,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct DecisionId(i64);

/// `manual_confirmation_occurrences.id`, the monotonic submission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct OccurrenceId(i64);

struct ModeratorDecision {
    id: DecisionId,
    key: CapabilityKey,
    record: ModeratorRecord,
}

/// Decisions for the current control definitions, newest first, one per control and firmware scope.
async fn moderator_decisions(
    conn: &mut SqliteConnection,
    camera: &ManualCamera,
) -> Result<Vec<ModeratorDecision>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, capability_key, definition_digest, firmware_scope, confirmed, evidence_kind,
                evidence_url, evidence_description
         FROM manual_moderator_confirmations
         WHERE camera_id = ?
         ORDER BY created_at DESC, id DESC",
    )
    .bind(camera.id.as_str())
    .fetch_all(&mut *conn)
    .await?;
    let mut decisions: Vec<ModeratorDecision> = Vec::new();
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
        let superseded = decisions.iter().any(|existing| {
            existing.key == key
                && existing.record.evidence.firmware_scope == record.evidence.firmware_scope
        });
        if !superseded {
            decisions.push(ModeratorDecision {
                id: DecisionId(row.get("id")),
                key,
                record,
            });
        }
    }
    Ok(decisions)
}

pub async fn moderator_records(
    conn: &mut SqliteConnection,
    camera: &ManualCamera,
) -> Result<BTreeMap<CapabilityKey, Vec<ModeratorRecord>>, sqlx::Error> {
    let mut records: BTreeMap<CapabilityKey, Vec<ModeratorRecord>> = BTreeMap::new();
    for decision in moderator_decisions(conn, camera).await? {
        records
            .entry(decision.key)
            .or_default()
            .push(decision.record);
    }
    Ok(records)
}

/// The occurrence watermark each moderator decision recorded in its audit row.
async fn moderator_watermarks(
    conn: &mut SqliteConnection,
) -> Result<HashMap<DecisionId, OccurrenceId>, sqlx::Error> {
    let rows: Vec<(Option<i64>, Option<i64>)> = sqlx::query_as(
        "SELECT json_extract(payload, '$.moderator_confirmation'),
                json_extract(payload, '$.occurrence_watermark')
         FROM audit_log
         WHERE type = ?",
    )
    .bind(AuditKind::ManualModeratorConfirmation.as_str())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(decision, watermark)| Some((DecisionId(decision?), OccurrenceId(watermark?))))
        .collect())
}

/// One profile's newest submission of a control check for a definition and firmware.
struct LatestCheck {
    camera: BodyId,
    credential: CredentialId,
    control: CapabilityKey,
    occurrence: OccurrenceId,
    received_at: i64,
    withdrawn: bool,
    firmware: ReportedFirmware,
    digest: Sha256Hex,
    finding: Finding,
}

/// Every profile's newest occurrence per control, definition and firmware, in submission order.
async fn latest_checks(
    conn: &mut SqliteConnection,
    camera: Option<&BodyId>,
) -> Result<Vec<LatestCheck>, sqlx::Error> {
    const SELECT: &str = "SELECT c.credential_id, c.camera_id, c.firmware,
                c.withdrawn_at AS confirmation_withdrawn,
                o.capability_key, o.definition_digest, o.finding_json,
                occ.id AS occurrence_id, occ.received_at AS occurrence_received,
                occ.withdrawn_at AS occurrence_withdrawn
         FROM manual_confirmations c
         JOIN manual_observations o ON o.confirmation_id = c.id
         JOIN manual_confirmation_occurrences occ ON occ.confirmation_id = c.id";
    let rows = match camera {
        Some(camera) => {
            sqlx::query(&format!("{SELECT} WHERE c.camera_id = ? ORDER BY occ.id"))
                .bind(camera.as_str())
                .fetch_all(&mut *conn)
                .await?
        }
        None => {
            sqlx::query(&format!("{SELECT} ORDER BY occ.id"))
                .fetch_all(&mut *conn)
                .await?
        }
    };
    type CheckKey = (
        BodyId,
        CredentialId,
        CapabilityKey,
        Sha256Hex,
        ReportedFirmware,
    );
    let mut latest: HashMap<CheckKey, LatestCheck> = HashMap::new();
    for row in rows {
        let camera: String = row.get("camera_id");
        let camera = BodyId::new(&camera)
            .map_err(|_| sqlx::Error::Protocol("camera id is not a body id".to_owned()))?;
        let credential: String = row.get("credential_id");
        let credential = CredentialId::try_from(credential)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))?;
        let key_text: String = row.get("capability_key");
        let control = CapabilityKey::from_str(&key_text)
            .map_err(|()| sqlx::Error::Protocol(format!("unknown control {key_text}")))?;
        let firmware: String = row.get("firmware");
        let firmware = ReportedFirmware::new(&firmware)
            .map_err(|_| sqlx::Error::Protocol("empty firmware".to_owned()))?;
        let digest: Vec<u8> = row.get("definition_digest");
        let digest =
            Sha256Hex::from_bytes(digest.try_into().map_err(|_| {
                sqlx::Error::Protocol("definition digest is not 32 bytes".to_owned())
            })?);
        let finding: String = row.get("finding_json");
        let finding = serde_json::from_str::<Finding>(&finding)
            .map_err(|_| sqlx::Error::Protocol("finding is not readable".to_owned()))?;
        let occurrence = OccurrenceId(row.get("occurrence_id"));
        let occurrence_withdrawn: Option<i64> = row.get("occurrence_withdrawn");
        let confirmation_withdrawn: Option<i64> = row.get("confirmation_withdrawn");
        let key = (
            camera.clone(),
            credential.clone(),
            control,
            digest,
            firmware.clone(),
        );
        if latest
            .get(&key)
            .is_some_and(|existing| existing.occurrence >= occurrence)
        {
            continue;
        }
        latest.insert(
            key,
            LatestCheck {
                camera,
                credential,
                control,
                occurrence,
                received_at: row.get("occurrence_received"),
                withdrawn: occurrence_withdrawn.is_some() || confirmation_withdrawn.is_some(),
                firmware,
                digest,
                finding,
            },
        );
    }
    let mut checks: Vec<LatestCheck> = latest.into_values().collect();
    checks.sort_by_key(|check| check.occurrence);
    Ok(checks)
}

type AnswerKey<'a> = (&'a CredentialId, CapabilityKey, &'a ReportedFirmware);

/// Each profile's newest occurrence per control and firmware, across definitions.
fn newest_answers(checks: &[LatestCheck]) -> HashMap<AnswerKey<'_>, OccurrenceId> {
    let mut newest = HashMap::new();
    for check in checks {
        let entry = newest
            .entry((&check.credential, check.control, &check.firmware))
            .or_insert(check.occurrence);
        *entry = (*entry).max(check.occurrence);
    }
    newest
}

/// Whether the profile later answered the same control and firmware under another definition.
fn superseded(check: &LatestCheck, newest: &HashMap<AnswerKey<'_>, OccurrenceId>) -> bool {
    newest
        .get(&(&check.credential, check.control, &check.firmware))
        .is_some_and(|newest| *newest > check.occurrence)
}

fn fill_intake(
    camera: &ManualCamera,
    summary: &mut ManualConfirmationSummary,
    checks: &[LatestCheck],
    eligible: &HashSet<CredentialId>,
) {
    let newest = newest_answers(checks);
    for control in &mut summary.controls {
        let Some(capability) = camera
            .capabilities
            .iter()
            .find(|capability| capability.key == control.key)
        else {
            continue;
        };
        let current = capability.definition_digest(camera);
        let mut intake = ManualControlIntake::default();
        for check in checks {
            if check.withdrawn || check.control != control.key {
                continue;
            }
            if check.digest != current {
                if !superseded(check, &newest) {
                    intake.stale += 1;
                }
                continue;
            }
            intake.recorded += 1;
            if !eligible.contains(&check.credential) {
                intake.unlinked += 1;
            }
        }
        control.intake = intake;
    }
}

fn counted_agreement(
    summary: &ManualConfirmationSummary,
    capability: &DocumentedCapability,
    check: &LatestCheck,
) -> bool {
    summary
        .control(capability.key)
        .and_then(|control| {
            control
                .firmware
                .iter()
                .find(|firmware| firmware.firmware == check.firmware)
        })
        .filter(|firmware| firmware.state == ConfirmationState::Confirmed)
        .and_then(|firmware| firmware.confirmed_actual.as_ref())
        .is_some_and(|actual| actual.agrees_with(capability, &check.finding))
}

fn check_reviews(
    camera: &ManualCamera,
    summary: &ManualConfirmationSummary,
    checks: &[LatestCheck],
    eligible: &HashSet<CredentialId>,
    groups: &HashMap<CredentialId, CredentialId>,
    decisions: &[ModeratorDecision],
    watermarks: &HashMap<DecisionId, OccurrenceId>,
) -> Vec<ManualCheckReview> {
    // `evidence` keeps one claim per identity group; a withdrawn newest claim retires the group's older ones.
    let mut group_latest: HashMap<
        (&CredentialId, CapabilityKey, Sha256Hex, &ReportedFirmware),
        OccurrenceId,
    > = HashMap::new();
    for check in checks {
        if !eligible.contains(&check.credential) {
            continue;
        }
        let Some(group) = groups.get(&check.credential) else {
            continue;
        };
        let newest = group_latest
            .entry((group, check.control, check.digest, &check.firmware))
            .or_insert(check.occurrence);
        *newest = (*newest).max(check.occurrence);
    }
    let newest = newest_answers(checks);
    let mut reviews = Vec::new();
    for check in checks {
        if check.withdrawn {
            continue;
        }
        let Some(capability) = camera
            .capabilities
            .iter()
            .find(|capability| capability.key == check.control)
        else {
            continue;
        };
        let stale = check.digest != capability.definition_digest(camera);
        if stale && superseded(check, &newest) {
            continue;
        }
        let linked = eligible.contains(&check.credential);
        let reviewed = decisions.iter().any(|decision| {
            let scope = &decision.record.evidence.firmware_scope;
            decision.key == check.control
                && watermarks
                    .get(&decision.id)
                    .is_some_and(|watermark| check.occurrence <= *watermark)
                && (scope.is_unspecified() || decision.record.applies_to(&check.firmware))
        });
        let counted = linked
            && groups.get(&check.credential).is_some_and(|group| {
                group_latest.get(&(group, check.control, check.digest, &check.firmware))
                    == Some(&check.occurrence)
            })
            && counted_agreement(summary, capability, check);
        let status = if stale {
            ManualCheckReviewStatus::StaleDefinition
        } else if reviewed {
            ManualCheckReviewStatus::Reviewed
        } else if counted {
            ManualCheckReviewStatus::Counted
        } else {
            ManualCheckReviewStatus::Pending
        };
        reviews.push(ManualCheckReview {
            camera: camera.id.clone(),
            control: capability.key,
            label: capability.label.clone(),
            firmware: check.firmware.clone(),
            finding: check.finding.clone(),
            definition_digest: check.digest,
            submitted_at: check.received_at,
            owner_linkage: if linked {
                ManualOwnerLinkage::Linked
            } else {
                ManualOwnerLinkage::Unlinked
            },
            status,
        });
    }
    reviews
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
    let mut summary = evaluate_camera(camera, &evidence, &moderators);
    let checks = latest_checks(&mut *conn, Some(&camera.id)).await?;
    fill_intake(camera, &mut summary, &checks, &eligible);
    Ok(summary)
}

/// Every profile's newest active check on a documented camera, for the moderator queue.
pub async fn reviews(
    db: &SqlitePool,
    catalog: &ManualCatalog,
) -> Result<Vec<ManualCheckReview>, sqlx::Error> {
    let mut tx = db.begin().await?;
    let groups = identity_groups(&mut tx).await?;
    let mut by_camera: HashMap<BodyId, Vec<LatestCheck>> = HashMap::new();
    for check in latest_checks(&mut tx, None).await? {
        by_camera
            .entry(check.camera.clone())
            .or_default()
            .push(check);
    }
    let watermarks = moderator_watermarks(&mut tx).await?;
    let mut reviews = Vec::new();
    for camera in &catalog.cameras {
        let Some(checks) = by_camera.get(&camera.id) else {
            continue;
        };
        let eligible = camera_credentials(&mut tx, &camera.id).await?;
        let evidence = evidence(&mut tx, &camera.id, &groups, &eligible).await?;
        let decisions = moderator_decisions(&mut tx, camera).await?;
        let mut records: BTreeMap<CapabilityKey, Vec<ModeratorRecord>> = BTreeMap::new();
        for decision in &decisions {
            records
                .entry(decision.key)
                .or_default()
                .push(decision.record.clone());
        }
        let summary = evaluate_camera(camera, &evidence, &records);
        reviews.extend(check_reviews(
            camera,
            &summary,
            checks,
            &eligible,
            &groups,
            &decisions,
            &watermarks,
        ));
    }
    reviews.sort_by(|left, right| {
        left.camera
            .as_str()
            .cmp(right.camera.as_str())
            .then_with(|| left.control.cmp(&right.control))
            .then_with(|| left.firmware.as_str().cmp(right.firmware.as_str()))
            .then_with(|| left.submitted_at.cmp(&right.submitted_at))
            .then_with(|| left.status.cmp(&right.status))
    });
    Ok(reviews)
}

/// Whether a non-withdrawn serial-linked photo ties the profile to the camera.
pub async fn owner_linkage(
    db: &SqlitePool,
    camera: &BodyId,
    credential: &CredentialId,
) -> Result<ManualOwnerLinkage, sqlx::Error> {
    let linked: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM contributions
         WHERE credential_id = ? AND body_id = ? AND serial_hmac IS NOT NULL
           AND status != 'withdrawn'
         LIMIT 1",
    )
    .bind(credential.as_str())
    .bind(camera.as_str())
    .fetch_optional(db)
    .await?;
    Ok(if linked.is_some() {
        ManualOwnerLinkage::Linked
    } else {
        ManualOwnerLinkage::Unlinked
    })
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
    let moderator_confirmation = sqlx::query(
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
    .await?
    .last_insert_rowid();
    // The insert above holds the write lock, so no check can land between it and this read.
    let occurrence_watermark: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM manual_confirmation_occurrences")
            .fetch_one(&mut *tx)
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
        "moderator_confirmation": moderator_confirmation,
        "occurrence_watermark": occurrence_watermark,
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
