// SPDX-License-Identifier: AGPL-3.0-or-later
//! Owner confirmations of documented camera controls.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;

use looklab_schema::{
    Availability, BodyId, CapabilityDomain, CapabilityKey, ConfirmationState, DocumentedCapability,
    Finding, IdentityGroupId, ManualCamera, ManualCatalog, ManualCheckReview,
    ManualCheckReviewStatus, ManualConfirmation, ManualConfirmationSummary, ManualControlIntake,
    ManualEvidence, ManualOwnerLinkage, ManualUrl, ModeratorEvidence, ModeratorEvidenceKind,
    ModeratorFirmwareScope, ModeratorRecord, ReportedFirmware, Sha256Hex, evaluate_camera,
};
use looklab_upload::Nonce;
use serde_json::json;
use sqlx::{Row, SqlitePool};

use crate::audit::{self, AuditKind};
use crate::ids::CredentialId;
use crate::store;

/// Two credentials that reported the same serial HMAC for this body share a
/// group; the representative is the smallest credential id, never the HMAC.
pub async fn identity_groups(
    db: &SqlitePool,
) -> Result<HashMap<CredentialId, CredentialId>, sqlx::Error> {
    let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
        "SELECT DISTINCT credential_id, serial_hmac FROM contributions
         WHERE serial_hmac IS NOT NULL",
    )
    .fetch_all(db)
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

struct LatestObservation {
    camera: BodyId,
    control: CapabilityKey,
    occurrence: i64,
    received_at: i64,
    withdrawn: bool,
    linked: bool,
    group: Option<CredentialId>,
    firmware: ReportedFirmware,
    digest: Sha256Hex,
    finding: Finding,
}

type ObservationKey = (
    BodyId,
    CredentialId,
    CapabilityKey,
    Sha256Hex,
    ReportedFirmware,
);
type GroupClaimKey = (CredentialId, CapabilityKey, Sha256Hex, ReportedFirmware);

async fn linked_credentials(
    db: &SqlitePool,
    camera: Option<&BodyId>,
) -> Result<HashMap<BodyId, HashSet<CredentialId>>, sqlx::Error> {
    let rows: Vec<(String, String)> = match camera {
        Some(camera) => {
            sqlx::query_as(
                "SELECT DISTINCT body_id, credential_id FROM contributions
                 WHERE serial_hmac IS NOT NULL AND body_id = ?",
            )
            .bind(camera.as_str())
            .fetch_all(db)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT DISTINCT body_id, credential_id FROM contributions
                 WHERE serial_hmac IS NOT NULL",
            )
            .fetch_all(db)
            .await?
        }
    };
    let mut linked: HashMap<BodyId, HashSet<CredentialId>> = HashMap::new();
    for (body, credential) in rows {
        let body = BodyId::new(&body)
            .map_err(|_| sqlx::Error::Protocol("camera id is not a body id".to_owned()))?;
        let credential = CredentialId::try_from(credential)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))?;
        linked.entry(body).or_default().insert(credential);
    }
    Ok(linked)
}

/// The latest non-withdrawn occurrence per profile, control, definition and firmware.
async fn latest_observations(
    db: &SqlitePool,
    camera: Option<&BodyId>,
) -> Result<Vec<LatestObservation>, sqlx::Error> {
    let groups = identity_groups(db).await?;
    let linked = linked_credentials(db, camera).await?;
    let rows = match camera {
        Some(camera) => {
            sqlx::query(
                "SELECT c.credential_id, c.camera_id, c.firmware,
                        c.withdrawn_at AS confirmation_withdrawn,
                        o.capability_key, o.definition_digest, o.finding_json,
                        occ.id AS occurrence_id, occ.received_at AS occurrence_received,
                        occ.withdrawn_at AS occurrence_withdrawn
                 FROM manual_confirmations c
                 JOIN manual_observations o ON o.confirmation_id = c.id
                 JOIN manual_confirmation_occurrences occ ON occ.confirmation_id = c.id
                 WHERE c.camera_id = ?
                 ORDER BY occ.id",
            )
            .bind(camera.as_str())
            .fetch_all(db)
            .await?
        }
        None => {
            sqlx::query(
                "SELECT c.credential_id, c.camera_id, c.firmware,
                        c.withdrawn_at AS confirmation_withdrawn,
                        o.capability_key, o.definition_digest, o.finding_json,
                        occ.id AS occurrence_id, occ.received_at AS occurrence_received,
                        occ.withdrawn_at AS occurrence_withdrawn
                 FROM manual_confirmations c
                 JOIN manual_observations o ON o.confirmation_id = c.id
                 JOIN manual_confirmation_occurrences occ ON occ.confirmation_id = c.id
                 ORDER BY occ.id",
            )
            .fetch_all(db)
            .await?
        }
    };
    let mut latest: HashMap<ObservationKey, LatestObservation> = HashMap::new();
    for row in rows {
        let camera_text: String = row.get("camera_id");
        let camera = BodyId::new(&camera_text)
            .map_err(|_| sqlx::Error::Protocol("camera id is not a body id".to_owned()))?;
        let credential: String = row.get("credential_id");
        let credential = CredentialId::try_from(credential)
            .map_err(|_| sqlx::Error::Protocol("credential id is not a UUID".to_owned()))?;
        let group = groups.get(&credential).cloned();
        let linked = linked
            .get(&camera)
            .is_some_and(|credentials| credentials.contains(&credential))
            && group.is_some();
        let key_text: String = row.get("capability_key");
        let control = CapabilityKey::from_str(&key_text)
            .map_err(|()| sqlx::Error::Protocol(format!("unknown control {key_text}")))?;
        let occurrence: i64 = row.get("occurrence_id");
        let received_at: i64 = row.get("occurrence_received");
        let occurrence_withdrawn: Option<i64> = row.get("occurrence_withdrawn");
        let confirmation_withdrawn: Option<i64> = row.get("confirmation_withdrawn");
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
        let observation_key = (
            camera.clone(),
            credential,
            control,
            digest,
            firmware.clone(),
        );
        let entry = LatestObservation {
            camera,
            control,
            occurrence,
            received_at,
            withdrawn: occurrence_withdrawn.is_some() || confirmation_withdrawn.is_some(),
            linked,
            group,
            firmware,
            digest,
            finding,
        };
        match latest.get(&observation_key) {
            Some(existing) if existing.occurrence >= occurrence => {}
            _ => {
                latest.insert(observation_key, entry);
            }
        }
    }
    Ok(latest.into_values().collect())
}

fn finding_claim(
    capability: &DocumentedCapability,
    finding: &Finding,
) -> (Availability, CapabilityDomain) {
    match finding {
        Finding::Matches => (capability.availability, capability.domain.clone()),
        Finding::Corrected {
            availability,
            domain,
        } => (*availability, domain.clone()),
    }
}

fn counted_agreement(
    summary: &ManualConfirmationSummary,
    capability: &DocumentedCapability,
    observation: &LatestObservation,
) -> bool {
    let Some(control) = summary.control(capability.key) else {
        return false;
    };
    let Some(firmware) = control
        .firmware
        .iter()
        .find(|firmware| firmware.firmware == observation.firmware)
    else {
        return false;
    };
    if firmware.state != ConfirmationState::Confirmed {
        return false;
    }
    let Some(actual) = &firmware.confirmed_actual else {
        return false;
    };
    let (availability, domain) = finding_claim(capability, &observation.finding);
    actual.availability == availability && actual.domain == domain
}

fn group_evidence(observations: &[LatestObservation]) -> Result<Vec<ManualEvidence>, sqlx::Error> {
    let mut latest: HashMap<GroupClaimKey, &LatestObservation> = HashMap::new();
    for observation in observations {
        if !observation.linked {
            continue;
        }
        let Some(group) = &observation.group else {
            continue;
        };
        let key = (
            group.clone(),
            observation.control,
            observation.digest,
            observation.firmware.clone(),
        );
        match latest.get(&key) {
            Some(existing) if existing.occurrence >= observation.occurrence => {}
            _ => {
                latest.insert(key, observation);
            }
        }
    }
    let mut evidence = Vec::with_capacity(latest.len());
    for ((group, control, digest, firmware), observation) in latest {
        if observation.withdrawn {
            continue;
        }
        evidence.push(ManualEvidence {
            identity_group: IdentityGroupId::new(group.as_str())
                .map_err(|_| sqlx::Error::Protocol("empty identity group".to_owned()))?,
            firmware,
            control,
            definition_digest: digest,
            finding: observation.finding.clone(),
            // Occurrence ids are the monotonic submission order.
            submitted_at: observation.occurrence,
        });
    }
    Ok(evidence)
}

// A check submitted after the decision watermark, or outside its firmware scope, stays pending.
fn reviewed_by_moderator(
    decisions: &BTreeMap<CapabilityKey, ModeratorDecision>,
    capability: &DocumentedCapability,
    observation: &LatestObservation,
) -> bool {
    let Some(decision) = decisions.get(&capability.key) else {
        return false;
    };
    let Some(watermark) = decision.watermark else {
        return false;
    };
    observation.occurrence <= watermark
        && decision
            .record
            .evidence
            .firmware_scope
            .covers(&observation.firmware)
}

fn latest_group_claims(observations: &[LatestObservation]) -> HashMap<GroupClaimKey, i64> {
    let mut latest = HashMap::new();
    for observation in observations {
        if !observation.linked {
            continue;
        }
        let Some(group) = &observation.group else {
            continue;
        };
        let key = (
            group.clone(),
            observation.control,
            observation.digest,
            observation.firmware.clone(),
        );
        let entry = latest.entry(key).or_insert(observation.occurrence);
        *entry = (*entry).max(observation.occurrence);
    }
    latest
}

fn is_latest_group_claim(
    observation: &LatestObservation,
    latest: &HashMap<GroupClaimKey, i64>,
) -> bool {
    let Some(group) = &observation.group else {
        return false;
    };
    latest.get(&(
        group.clone(),
        observation.control,
        observation.digest,
        observation.firmware.clone(),
    )) == Some(&observation.occurrence)
}

fn analyze_camera(
    camera: &ManualCamera,
    observations: &[LatestObservation],
    moderators: &BTreeMap<CapabilityKey, ModeratorDecision>,
) -> Result<(ManualConfirmationSummary, Vec<ManualCheckReview>), sqlx::Error> {
    let evidence = group_evidence(observations)?;
    let records: BTreeMap<CapabilityKey, ModeratorRecord> = moderators
        .iter()
        .map(|(key, decision)| (*key, decision.record.clone()))
        .collect();
    let mut summary = evaluate_camera(camera, &evidence, &records);
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
        for observation in observations {
            if observation.withdrawn || observation.control != capability.key {
                continue;
            }
            if observation.digest == current {
                intake.recorded += 1;
                if !observation.linked {
                    intake.unlinked += 1;
                }
            } else {
                intake.stale += 1;
            }
        }
        control.intake = intake;
    }
    let latest_group = latest_group_claims(observations);
    let mut reviews = Vec::new();
    for observation in observations {
        if observation.withdrawn {
            continue;
        }
        let Some(capability) = camera
            .capabilities
            .iter()
            .find(|capability| capability.key == observation.control)
        else {
            continue;
        };
        let status = if observation.digest != capability.definition_digest(camera) {
            ManualCheckReviewStatus::StaleDefinition
        } else if reviewed_by_moderator(moderators, capability, observation) {
            ManualCheckReviewStatus::Reviewed
        } else if is_latest_group_claim(observation, &latest_group)
            && counted_agreement(&summary, capability, observation)
        {
            ManualCheckReviewStatus::Counted
        } else {
            ManualCheckReviewStatus::Pending
        };
        reviews.push(ManualCheckReview {
            camera: camera.id.clone(),
            control: capability.key,
            label: capability.label.clone(),
            firmware: observation.firmware.clone(),
            finding: observation.finding.clone(),
            definition_digest: observation.digest,
            submitted_at: observation.received_at,
            owner_linkage: if observation.linked {
                ManualOwnerLinkage::Linked
            } else {
                ManualOwnerLinkage::Unlinked
            },
            status,
        });
    }
    reviews.sort_by(|left, right| {
        left.control
            .cmp(&right.control)
            .then_with(|| left.firmware.as_str().cmp(right.firmware.as_str()))
            .then_with(|| left.submitted_at.cmp(&right.submitted_at))
            .then_with(|| left.status.cmp(&right.status))
    });
    Ok((summary, reviews))
}

struct ModeratorDecision {
    record: ModeratorRecord,
    watermark: Option<i64>,
}

async fn moderator_decisions(
    db: &SqlitePool,
    camera: &ManualCamera,
) -> Result<BTreeMap<CapabilityKey, ModeratorDecision>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT m.id, m.capability_key, m.definition_digest, m.firmware_scope, m.confirmed,
                m.evidence_kind, m.evidence_url, m.evidence_description,
                (SELECT json_extract(a.payload, '$.occurrence_watermark')
                 FROM audit_log a
                 WHERE a.type = ?
                   AND json_extract(a.payload, '$.moderator_confirmation') = m.id
                 ORDER BY a.seq DESC LIMIT 1) AS occurrence_watermark
         FROM manual_moderator_confirmations m
         WHERE m.camera_id = ?
         ORDER BY m.created_at DESC, m.id DESC",
    )
    .bind(AuditKind::ManualModeratorConfirmation.as_str())
    .bind(camera.id.as_str())
    .fetch_all(db)
    .await?;
    let mut records = BTreeMap::new();
    for row in rows {
        let key_text: String = row.get("capability_key");
        let Ok(key) = CapabilityKey::from_str(&key_text) else {
            continue;
        };
        if records.contains_key(&key) {
            continue;
        }
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
        let watermark: Option<i64> = row.get("occurrence_watermark");
        let Ok(kind) = ModeratorEvidenceKind::from_str(&evidence_kind) else {
            continue;
        };
        let url = evidence_url
            .as_deref()
            .and_then(|value| ManualUrl::new(value).ok());
        records.insert(
            key,
            ModeratorDecision {
                record: ModeratorRecord {
                    confirmed: confirmed != 0,
                    evidence: ModeratorEvidence {
                        kind,
                        url,
                        description: evidence_description,
                        firmware_scope,
                    },
                },
                watermark,
            },
        );
    }
    Ok(records)
}

pub async fn summary(
    db: &SqlitePool,
    camera: &ManualCamera,
) -> Result<ManualConfirmationSummary, sqlx::Error> {
    let observations = latest_observations(db, Some(&camera.id)).await?;
    let moderators = moderator_decisions(db, camera).await?;
    let (summary, _) = analyze_camera(camera, &observations, &moderators)?;
    Ok(summary)
}

/// The latest active check per profile, control, definition and firmware.
pub async fn reviews(
    db: &SqlitePool,
    catalog: &ManualCatalog,
) -> Result<Vec<ManualCheckReview>, sqlx::Error> {
    let observations = latest_observations(db, None).await?;
    let mut by_camera: HashMap<BodyId, Vec<LatestObservation>> = HashMap::new();
    for observation in observations {
        by_camera
            .entry(observation.camera.clone())
            .or_default()
            .push(observation);
    }
    let mut reviews = Vec::new();
    for camera in &catalog.cameras {
        let Some(observations) = by_camera.get(&camera.id) else {
            continue;
        };
        let moderators = moderator_decisions(db, camera).await?;
        let (_, mut checks) = analyze_camera(camera, observations, &moderators)?;
        reviews.append(&mut checks);
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

pub async fn owner_linkage(
    db: &SqlitePool,
    camera: &BodyId,
    credential: &CredentialId,
) -> Result<ManualOwnerLinkage, sqlx::Error> {
    let linked: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM contributions
         WHERE credential_id = ? AND body_id = ? AND serial_hmac IS NOT NULL
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
    #[error("the credential is blocked from new contributions")]
    CredentialBlocked,
}

/// A repeated payload is stored once but still completes the attestation of the
/// send that authorized it.
pub async fn record(
    db: &SqlitePool,
    credential: &CredentialId,
    confirmation: &ManualConfirmation,
    payload_digest: Sha256Hex,
    assertion_hash: &[u8; 32],
    send_nonce: &Nonce,
) -> Result<(), RecordError> {
    let scope_json = serde_json::to_string(&confirmation.scope)
        .map_err(|_| sqlx::Error::Protocol("scope does not serialize".to_owned()))?;
    let scope_kind = confirmation.scope.as_str();
    let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
    if crate::store::intake_blocked_conn(&mut tx, credential).await? {
        return Err(RecordError::CredentialBlocked);
    }
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
            "confirmation": id,
            "camera": confirmation.camera.as_str(),
            "firmware": confirmation.firmware.as_str(),
            "controls": confirmation.observations.len(),
        });
        audit::insert(&mut tx, AuditKind::ManualConfirmation, &payload).await?;
    }
    store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Marks every live confirmation this credential made for the camera withdrawn.
pub async fn withdraw(
    db: &SqlitePool,
    credential: &CredentialId,
    camera: &BodyId,
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
    .bind(camera.as_str())
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
    .bind(camera.as_str())
    .execute(&mut *tx)
    .await?;
    let affected = sqlx::query(
        "UPDATE manual_confirmations SET withdrawn_at = ?
         WHERE credential_id = ? AND camera_id = ? AND withdrawn_at IS NULL",
    )
    .bind(now)
    .bind(credential.as_str())
    .bind(camera.as_str())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if affected > 0 {
        let payload = json!({
            "camera": camera.as_str(),
            "withdrawn": affected,
        });
        audit::insert(&mut tx, AuditKind::ManualConfirmationWithdrawal, &payload).await?;
    }
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
    let occurrence_watermark: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM manual_confirmation_occurrences")
            .fetch_one(&mut *tx)
            .await?;
    let payload = json!({
        "camera": camera.id.as_str(),
        "control": key.as_str(),
        "definition_digest": definition_digest.to_hex(),
        "decision": if record.confirmed { "confirm" } else { "reject" },
        "firmware_scope": &record.evidence.firmware_scope,
        "moderator": credential.as_str(),
        "moderator_confirmation": moderator_confirmation,
        "occurrence_watermark": occurrence_watermark,
        "evidence_kind": evidence_kind,
        "evidence_url": record.evidence.url.as_ref().map(ManualUrl::as_str),
    });
    audit::insert(&mut tx, AuditKind::ManualModeratorConfirmation, &payload).await?;
    store::request_publication(&mut tx).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn promotion_audit(
    db: &SqlitePool,
    camera: &ManualCamera,
    summary: &ManualConfirmationSummary,
) -> Result<(), sqlx::Error> {
    for control in &summary.controls {
        for firmware in &control.firmware {
            if firmware.state != ConfirmationState::Confirmed {
                continue;
            }
            let payload = json!({
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
                    .fetch_all(db)
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
            let mut conn = db.acquire().await?;
            audit::insert(&mut conn, AuditKind::ManualConfirmationPromotion, &payload).await?;
        }
    }
    Ok(())
}
