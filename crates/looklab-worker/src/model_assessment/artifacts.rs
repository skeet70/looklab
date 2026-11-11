// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact candidate and body-state artifacts an assessment evaluates.

use looklab_body::InputTransform;
use looklab_fit::RgbSpace;
use looklab_model::file::LookDocument;
use looklab_model::realization::decode_crm;
use looklab_publish::RELEASE_ARTIFACT;
use looklab_schema::{
    ArtifactBinding, ArtifactRole, BodyRealizationVersionId, MatchParams, MatchRealizationParams,
    OutputEncoding,
};
use looklab_server::model_store::{self, StoredRealizationRevision};
use sqlx::SqlitePool;

use crate::config::WorkerConfig;
use crate::db;
use crate::error::Result;
use crate::refit::{LOOK_ARTIFACT, ReleaseRecord};

use super::predict::ExactModel;

pub(crate) struct LoadedRealization {
    pub revision: StoredRealizationRevision,
    pub transform: InputTransform,
    pub model: ExactModel,
    pub release: ReleaseRecord,
    pub output_encoding: OutputEncoding,
}

pub(crate) async fn load_realization(
    pool: &SqlitePool,
    config: &WorkerConfig,
    candidate: &BodyRealizationVersionId,
) -> Result<LoadedRealization, String> {
    let id = looklab_schema::RealizationRevisionId::for_version(candidate);
    let mut conn = pool.acquire().await.map_err(|error| error.to_string())?;
    let revision = model_store::realization_revision(&mut conn, &id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("realization revision {} is not stored", id))?;
    if revision.subject != candidate.realization
        || revision.version != candidate.version
        || revision.target != candidate.target
    {
        return Err(format!(
            "realization revision {} does not name the candidate's subject and target",
            id
        ));
    }
    let state = model_store::body_state(
        &mut conn,
        &candidate.realization.body,
        revision.body_state_version,
    )
    .await
    .map_err(|error| error.to_string())?
    .ok_or_else(|| {
        format!(
            "body state {} is not stored",
            revision.body_state_version.get()
        )
    })?;
    let native = is_native_revision(&mut conn, &revision)
        .await
        .map_err(|error| error.to_string())?;
    drop(conn);

    let transform: InputTransform = serde_json::from_slice(&read_binding(
        config,
        &state.pins.input_transform,
        ArtifactRole::InputTransform,
    )?)
    .map_err(|error| format!("input transform is not valid JSON: {error}"))?;
    if transform.body().as_str() != candidate.realization.body.as_str() {
        return Err("input transform names another body".to_owned());
    }
    let params: MatchParams = serde_json::from_slice(&read_binding(
        config,
        &state.pins.body_params,
        ArtifactRole::BodyRealizationParams,
    )?)
    .map_err(|error| format!("body realization params are not valid JSON: {error}"))?;
    params
        .validate(&candidate.realization.body)
        .map_err(|error| format!("body realization params are invalid: {error}"))?;

    let look_bytes = match revision
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::Look)
    {
        Some(binding) => read_binding(config, binding, ArtifactRole::Look)?,
        None => read_target_artifact(config, pool, &candidate.target, LOOK_ARTIFACT).await?,
    };
    let look = LookDocument::decode(&look_bytes)
        .map_err(|error| format!("realization Look does not decode: {error}"))?
        .look;
    let crm = match revision
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::ControlResponse)
    {
        Some(binding) => Some(
            decode_crm(&read_binding(
                config,
                binding,
                ArtifactRole::ControlResponse,
            )?)
            .map_err(|error| format!("control response does not decode: {error}"))?,
        ),
        None if native => None,
        None => {
            return Err("realization revision has no control-response artifact".to_owned());
        }
    };
    let realization_binding = revision
        .artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::RealizationParams)
        .ok_or_else(|| "realization revision has no realization-params artifact".to_owned())?;
    let realization_params: MatchRealizationParams = serde_json::from_slice(&read_binding(
        config,
        realization_binding,
        ArtifactRole::RealizationParams,
    )?)
    .map_err(|error| format!("realization params are not valid JSON: {error}"))?;
    if native {
        realization_params.validate_native()
    } else {
        realization_params.validate()
    }
    .map_err(|error| format!("realization params are invalid: {error}"))?;
    if realization_params.realization != *candidate
        || realization_params.family != candidate.target
        || realization_params.version != candidate.version.get()
        || realization_params.control_response_version != crm.as_ref().map_or(0, |crm| crm.version)
        || (realization_params.clip_k - revision.clip_k).abs() > 1e-12
    {
        return Err("realization params do not bind the candidate revision".to_owned());
    }
    let release_bytes =
        read_target_artifact(config, pool, &candidate.target, RELEASE_ARTIFACT).await?;
    let release: ReleaseRecord = serde_json::from_slice(&release_bytes)
        .map_err(|error| format!("target release record is not valid JSON: {error}"))?;
    if release.version != candidate.target.version.get() {
        return Err("target release record names another version".to_owned());
    }
    let output_encoding = output_encoding(release.output_space);
    let model = ExactModel::new(
        look,
        crm,
        params,
        realization_params.clone(),
        revision.clip_k,
        release.output_space,
        revision.input_transform_version,
        revision.realization_params_version,
    )?;
    Ok(LoadedRealization {
        revision,
        transform,
        model,
        release,
        output_encoding,
    })
}

pub(crate) async fn read_target_artifact(
    config: &WorkerConfig,
    pool: &SqlitePool,
    target: &looklab_schema::FamilyTargetId,
    name: &str,
) -> Result<Vec<u8>, String> {
    let artifacts = db::manifest_artifacts(pool, target.family.as_str(), target.version.get())
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("target {} has no manifest", target.to_key()))?;
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .ok_or_else(|| format!("target {} has no `{name}` artifact", target.to_key()))?;
    let path = config.blobs.join(&artifact.sha256);
    std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))
}

pub(crate) fn read_binding(
    config: &WorkerConfig,
    binding: &ArtifactBinding,
    role: ArtifactRole,
) -> Result<Vec<u8>, String> {
    let path = config.blobs.join(binding.sha256.to_hex());
    std::fs::read(&path)
        .map_err(|error| format!("{} {}: {error}", role.as_str(), binding.sha256.to_hex()))
}

pub(crate) async fn private_membership(
    pool: &SqlitePool,
    config: &WorkerConfig,
    revision: &StoredRealizationRevision,
) -> Result<Option<model_store::EvidenceMembership>, String> {
    let mut conn = pool.acquire().await.map_err(|error| error.to_string())?;
    let artifacts = model_store::artifacts(&mut conn, "realization_revision", revision.id.as_str())
        .await
        .map_err(|error| error.to_string())?;
    drop(conn);
    let Some(binding) = artifacts
        .iter()
        .find(|artifact| artifact.role == ArtifactRole::EvidenceMembership)
    else {
        return Ok(None);
    };
    let bytes = read_binding(config, binding, ArtifactRole::EvidenceMembership)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("evidence membership does not decode: {error}"))
}

pub(crate) fn output_encoding(space: RgbSpace) -> OutputEncoding {
    match space {
        RgbSpace::Srgb => OutputEncoding::Srgb,
        RgbSpace::AdobeRgb => OutputEncoding::AdobeRgb,
        RgbSpace::Rec2020 => OutputEncoding::Rec2020,
        RgbSpace::DisplayP3 => OutputEncoding::DisplayP3,
        RgbSpace::ProPhoto => OutputEncoding::ProPhoto,
    }
}

/// Whether a revision carries the native-stage warm state, which only that stage writes.
pub(crate) async fn is_native_revision(
    conn: &mut sqlx::SqliteConnection,
    revision: &StoredRealizationRevision,
) -> Result<bool, String> {
    let artifacts = model_store::artifacts(conn, "realization_revision", revision.id.as_str())
        .await
        .map_err(|error| error.to_string())?;
    Ok(artifacts
        .iter()
        .any(|artifact| artifact.role == ArtifactRole::NativeWarmState))
}
