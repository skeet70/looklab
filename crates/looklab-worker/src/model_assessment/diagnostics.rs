// SPDX-License-Identifier: AGPL-3.0-or-later
//! Independent camera-local diagnostic predictions consumed by P1.9.

use looklab_model::ProbeId;
use looklab_schema::{BodyRealizationId, FamilyTargetId, Sha256Hex, SourceRef, SubjectKey};
use looklab_server::model_store::{self, EvidenceGroupId, StoredRealizationRevision};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::config::WorkerConfig;

pub(crate) const DIAGNOSTIC_FORMAT: &str = "looklab-camera-local-diagnostics/1";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CameraLocalDiagnostics {
    pub format: String,
    pub subject: BodyRealizationId,
    pub target: FamilyTargetId,
    pub realization: SubjectKey,
    pub realization_version: u32,
    pub source: DiagnosticSource,
    pub input_transform: Sha256Hex,
    pub realization_params: Sha256Hex,
    pub gauge: Sha256Hex,
    pub control_response: Sha256Hex,
    pub groups: Vec<EvidenceGroupId>,
    pub probes: Vec<DiagnosticProbe>,
    pub budget: DiagnosticBudget,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiagnosticSource {
    pub fitter: SourceRef,
    pub evaluator: SourceRef,
    pub model: SourceRef,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiagnosticProbe {
    pub probe: ProbeId,
    pub post_clip: [f64; 3],
    pub supported: bool,
    pub sigma_de: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiagnosticBudget {
    pub evaluations: u64,
    pub sweeps: usize,
    pub converged: bool,
}

pub(crate) async fn load(
    pool: &SqlitePool,
    config: &WorkerConfig,
    revision: &StoredRealizationRevision,
) -> Result<Option<CameraLocalDiagnostics>, String> {
    let mut conn = pool.acquire().await.map_err(|error| error.to_string())?;
    let artifacts = model_store::artifacts(&mut conn, "realization_revision", revision.id.as_str())
        .await
        .map_err(|error| error.to_string())?;
    drop(conn);
    let Some(artifact) = artifacts
        .iter()
        .find(|artifact| artifact.role == looklab_schema::ArtifactRole::FitDiagnostic)
    else {
        return Ok(None);
    };
    let path = config.blobs.join(artifact.sha256.to_hex());
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let diagnostics = serde_json::from_slice::<CameraLocalDiagnostics>(&bytes)
        .map_err(|error| format!("fit diagnostics do not decode: {error}"))?;
    if diagnostics.format != DIAGNOSTIC_FORMAT {
        return Ok(None);
    }
    Ok(Some(diagnostics))
}

pub(crate) fn validate(
    diagnostics: &CameraLocalDiagnostics,
    revision: &StoredRealizationRevision,
    sources: &crate::source_identity::NumericalSources,
) -> Result<(), String> {
    if diagnostics.subject != revision.subject
        || diagnostics.target != revision.target
        || diagnostics.realization != SubjectKey::for_realization(&revision.subject)
        || diagnostics.realization_version != revision.version.get()
        || diagnostics.source.fitter != sources.fitter
        || diagnostics.source.evaluator != sources.evaluator
        || diagnostics.source.model != sources.model
        || !diagnostics.budget.converged
        || diagnostics.groups.len() < 2
    {
        return Err("camera-local diagnostics do not bind this revision".to_owned());
    }
    let binding = |role: looklab_schema::ArtifactRole| {
        revision
            .artifacts
            .iter()
            .find(|artifact| artifact.role == role)
            .map(|artifact| artifact.sha256)
    };
    let realization_params = binding(looklab_schema::ArtifactRole::RealizationParams)
        .or_else(|| binding(looklab_schema::ArtifactRole::BodyRealizationParams));
    if binding(looklab_schema::ArtifactRole::InputTransform)
        .is_some_and(|sha| sha != diagnostics.input_transform)
        || realization_params.is_some_and(|sha| sha != diagnostics.realization_params)
        || binding(looklab_schema::ArtifactRole::ControlResponse)
            .is_some_and(|sha| sha != diagnostics.control_response)
        || binding(looklab_schema::ArtifactRole::Gauge).is_some_and(|sha| sha != diagnostics.gauge)
    {
        return Err("camera-local diagnostics name different pinned artifacts".to_owned());
    }
    Ok(())
}
