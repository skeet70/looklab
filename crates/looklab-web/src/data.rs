// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fetching site data and checking every artifact against its version manifest.

use std::sync::Arc;

use gloo_net::http::Request;
use looklab_model::LookDocument;
use looklab_publish::{
    AuditLog, BodyPage, LUT_BYTES, LookArtifact, LookPage, PreparedScene, Scene, SceneIndex,
    SceneName, SiteIndex, VersionManifest, verify_artifact,
};
use serde::de::DeserializeOwned;

#[derive(Clone, Debug, PartialEq)]
pub enum Load<T> {
    Loading,
    Failed(String),
    Ready(T),
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

async fn get(path: &str) -> Result<Vec<u8>, String> {
    let response = Request::get(path)
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if !response.ok() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .binary()
        .await
        .map_err(|error| format!("{path}: {error}"))
}

async fn get_json<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let bytes = get(path).await?;
    serde_json::from_slice(&bytes).map_err(|error| format!("{path}: {error}"))
}

pub async fn index() -> Result<SiteIndex, String> {
    get_json("/data/index.json").await
}

pub async fn body(make: &str, model: &str) -> Result<BodyPage, String> {
    get_json(&format!("/data/bodies/{make}/{model}.json")).await
}

pub async fn look(slug: &str) -> Result<LookPage, String> {
    get_json(&format!("/data/looks/{slug}.json")).await
}

pub async fn audit_log() -> Result<AuditLog, String> {
    get_json("/data/audit-log.json").await
}

pub async fn scenes() -> Result<SceneIndex, String> {
    get_json("/data/scenes/index.json").await
}

pub async fn scene(name: &str) -> Result<PreparedScene, String> {
    let index = scenes().await?;
    let summary = index
        .scenes
        .iter()
        .find(|scene| scene.name.as_str() == name)
        .ok_or_else(|| format!("unknown scene `{name}`"))?;
    let bytes = get(&format!("/{}", summary.path.as_str())).await?;
    let scene = Scene::from_f16_bytes(
        SceneName::new(name).map_err(|error| error.to_string())?,
        summary.width,
        summary.height,
        &bytes,
    )
    .map_err(|error| error.to_string())?;
    Ok(PreparedScene::new(scene))
}

/// A look version whose every downloaded artifact matched its manifest.
#[derive(Clone, Debug)]
pub struct LoadedVersion {
    pub manifest: VersionManifest,
    pub document: LookDocument,
    pub lut: Arc<Vec<u8>>,
    pub diagnostics: looklab_report::LookDiagnostics,
}

/// Downloads and verifies only the preview LUT.
///
/// # Errors
/// Reports a missing manifest entry, a hash mismatch or a wrong LUT length.
pub async fn preview(slug: &str, version: &str) -> Result<Arc<Vec<u8>>, String> {
    let base = format!("/data/looks/{slug}/{version}");
    let manifest: VersionManifest = get_json(&format!("{base}/manifest.json")).await?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::PreviewLut))
        .ok_or_else(|| format!("{slug} {version}: no preview LUT in the manifest"))?;
    let bytes = get(&format!("{base}/{}", entry.name.as_str())).await?;
    verify_artifact(&manifest, &entry.name, &bytes)
        .map_err(|error| format!("{slug} {version}: {error}"))?;
    if bytes.len() != LUT_BYTES {
        return Err(format!(
            "{slug} {version}: preview LUT has {} bytes",
            bytes.len()
        ));
    }
    Ok(Arc::new(bytes))
}

pub async fn version(slug: &str, version: &str) -> Result<LoadedVersion, String> {
    let base = format!("/data/looks/{slug}/{version}");
    let manifest: VersionManifest = get_json(&format!("{base}/manifest.json")).await?;
    if manifest.look.as_str() != slug || manifest.version.as_str() != version {
        return Err(format!(
            "manifest names {} {}, page requested {slug} {version}",
            manifest.look, manifest.version
        ));
    }
    let mut look_bytes = None;
    let mut lut = None;
    for entry in &manifest.artifacts {
        let bytes = get(&format!("{base}/{}", entry.name.as_str())).await?;
        verify_artifact(&manifest, &entry.name, &bytes)
            .map_err(|error| format!("{slug} {version}: {error}"))?;
        match LookArtifact::of(&entry.name) {
            LookArtifact::Document => look_bytes = Some(bytes),
            LookArtifact::PreviewLut => lut = Some(bytes),
            LookArtifact::Other => {}
        }
    }
    let look_bytes = look_bytes.ok_or_else(|| format!("{slug} {version}: no look artifact"))?;
    let lut = lut.ok_or_else(|| format!("{slug} {version}: no preview LUT"))?;
    if lut.len() != LUT_BYTES {
        return Err(format!(
            "{slug} {version}: preview LUT has {} bytes",
            lut.len()
        ));
    }
    let document =
        LookDocument::decode(&look_bytes).map_err(|error| format!("{slug} {version}: {error}"))?;
    let probes = looklab_model::probes::v1();
    let diagnostics = looklab_report::look_diagnostics(&document.look, &probes);
    Ok(LoadedVersion {
        manifest,
        document,
        lut: Arc::new(lut),
        diagnostics,
    })
}
