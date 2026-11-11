// SPDX-License-Identifier: AGPL-3.0-or-later
//! P4.3 known-reference engine qualification from a persisted workload.

use std::collections::BTreeSet;

use looklab_schema::{BodyId, CellId, DependencyGeneration, RuleRevision, SourceRef};
use looklab_server::model_store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::assessment::{EngineTrial, EngineVersion, qualify_engine};
use crate::config::WorkerConfig;
use crate::error::{Result, WorkerError};
use crate::jobs::{JobOwnership, JobPayload};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EngineReferenceFormat {
    #[serde(rename = "looklab-engine-reference/1")]
    V1,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EngineReference {
    format: EngineReferenceFormat,
    engine: SourceRef,
    rule_revision: RuleRevision,
    bodies: Vec<BodyId>,
    trials: Vec<EngineTrialWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EngineTrialWire {
    body: BodyId,
    cell: CellId,
    true_error_de: f64,
    band_90_de: f64,
    recovered: bool,
    training_bodies: Vec<BodyId>,
    held_out_body: bool,
    out_of_gamut: bool,
    marked_unsupported: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    pool: &SqlitePool,
    config: &WorkerConfig,
    engine: &SourceRef,
    rule_revision: &RuleRevision,
    reference: Option<looklab_schema::Sha256Hex>,
    sources: &crate::source_identity::NumericalSources,
    observed: &[DependencyGeneration],
    ownership: &JobOwnership,
) -> Result<()> {
    let version = EngineVersion::new(
        sources.fitter.clone(),
        sources.evaluator.clone(),
        sources.model.clone(),
        rule_revision.clone(),
    )
    .map_err(WorkerError::Data)?;
    if version.source() != engine {
        return Err(WorkerError::Data(
            "engine assessment names a different engine version".to_owned(),
        ));
    }
    let Some(hash) = reference else {
        record_request(pool, engine, rule_revision).await?;
        return Ok(());
    };
    let path = config.blobs.join(hash.to_hex());
    let bytes = std::fs::read(&path)?;
    let digest = looklab_schema::Sha256Hex::from_bytes(Sha256::digest(&bytes).into());
    if digest != hash {
        return Err(WorkerError::Data(
            "known-reference workload bytes do not match the requested digest".to_owned(),
        ));
    }
    let reference: EngineReference = serde_json::from_slice(&bytes)?;
    if !matches!(reference.format, EngineReferenceFormat::V1)
        || reference.engine != *engine
        || reference.rule_revision != *rule_revision
    {
        return Err(WorkerError::Data(
            "known-reference workload names a different engine version".to_owned(),
        ));
    }
    validate_partition(&reference)?;
    let trials: Vec<EngineTrial> = reference
        .trials
        .into_iter()
        .map(|trial| EngineTrial {
            body: trial.body,
            cell: trial.cell,
            true_error_de: trial.true_error_de,
            band_90_de: trial.band_90_de,
            recovered: trial.recovered,
            held_out_body: trial.held_out_body,
            out_of_gamut: trial.out_of_gamut,
            marked_unsupported: trial.marked_unsupported,
        })
        .collect();
    let assessment = qualify_engine(&version, &trials);
    let Some(prerequisite) = assessment.prerequisite else {
        record_request(pool, engine, rule_revision).await?;
        return Ok(());
    };
    let now = crate::clock::now_unix();
    let mut conn = model_store::begin_immediate(pool).await?;
    if !lease_owned(&mut conn, ownership).await? {
        return Ok(());
    }
    let current = model_store::dependency_snapshot(&mut conn, &sources.dependencies()).await?;
    if !observed.iter().all(|expected| {
        current.iter().any(|actual| {
            actual.dependency == expected.dependency && actual.generation == expected.generation
        })
    }) {
        return Ok(());
    }
    let existing = model_store::engine_prerequisite(&mut conn, &prerequisite.id()).await?;
    if let Some(existing) = existing {
        if existing == prerequisite {
            return Ok(());
        }
        return Err(WorkerError::Data(
            "engine prerequisite already records a different outcome".to_owned(),
        ));
    }
    model_store::insert_engine_prerequisite(&mut conn, &prerequisite, now).await?;
    sqlx::query("UPDATE publication SET generation = generation + 1 WHERE id = 1")
        .execute(&mut *conn)
        .await?;
    enqueue_publish_conn(&mut conn, now).await?;
    conn.commit().await?;
    Ok(())
}

fn validate_partition(reference: &EngineReference) -> Result<()> {
    let bodies: BTreeSet<&BodyId> = reference.bodies.iter().collect();
    if bodies.len() < 3 {
        return Err(WorkerError::Data(
            "known-reference workload needs at least three bodies".to_owned(),
        ));
    }
    let mut held_out_bodies = BTreeSet::new();
    for trial in &reference.trials {
        let training: BTreeSet<&BodyId> = trial.training_bodies.iter().collect();
        if !bodies.contains(&trial.body)
            || !trial.held_out_body
            || training.is_empty()
            || training.contains(&trial.body)
            || !training.iter().all(|body| bodies.contains(body))
        {
            return Err(WorkerError::Data(
                "known-reference trial body partition is invalid".to_owned(),
            ));
        }
        held_out_bodies.insert(&trial.body);
    }
    if held_out_bodies.len() < 2 {
        return Err(WorkerError::Data(
            "known-reference workload needs at least two held-out bodies".to_owned(),
        ));
    }
    Ok(())
}

async fn record_request(
    pool: &SqlitePool,
    engine: &SourceRef,
    rule_revision: &RuleRevision,
) -> Result<()> {
    let payload = serde_json::json!({
        "kind": "missing_engine_qualification",
        "engine": engine.as_str(),
        "rule_revision": rule_revision.as_str(),
    })
    .to_string();
    sqlx::query(
        "INSERT INTO audit_log (type, payload, ts)
         SELECT 'engine_evidence_request', ?, ?
         WHERE NOT EXISTS (
             SELECT 1 FROM audit_log WHERE type = 'engine_evidence_request' AND payload = ?)",
    )
    .bind(&payload)
    .bind(crate::clock::now_unix())
    .bind(&payload)
    .execute(pool)
    .await?;
    Ok(())
}

async fn lease_owned(conn: &mut sqlx::SqliteConnection, ownership: &JobOwnership) -> Result<bool> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE id = ? AND attempts = ? AND status = 'running'",
    )
    .bind(ownership.id.as_str())
    .bind(ownership.attempts)
    .fetch_one(&mut *conn)
    .await?;
    Ok(count == 1)
}

async fn enqueue_publish_conn(conn: &mut sqlx::SqliteConnection, now: i64) -> Result<()> {
    let (kind, payload) = JobPayload::PublishSite.encode()?;
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = ? AND status IN ('queued', 'running')",
    )
    .bind(kind.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if pending == 0 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
             VALUES (?, ?, ?, 'queued', 0, ?, ?)",
        )
        .bind(crate::jobs::JobId::new().as_str())
        .bind(kind.as_str())
        .bind(payload)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}
