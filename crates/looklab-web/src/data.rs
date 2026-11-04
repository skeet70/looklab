// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fetching site data and checking every artifact against its version manifest.

use std::sync::Arc;

use gloo_net::http::Request;
use looklab_model::LookDocument;
use looklab_publish::{
    ArtifactHash, AuditLog, BodyPage, LUT_BYTES, LookArtifact, LookPage, PreparedScene, Scene,
    SceneIndex, SceneName, SiteIndex, VersionManifest, VersionSummary, verify_artifact,
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

/// The site index, verified against the digest embedded in this build.
pub async fn index() -> Result<SiteIndex, String> {
    let bytes = get("/data/index.json").await?;
    let expected = crate::site_root();
    if expected.is_empty() {
        return Err("this build has no embedded site root digest".to_owned());
    }
    let actual = ArtifactHash::of(&bytes);
    if actual.as_str() != expected {
        return Err(format!(
            "site index does not match the embedded root: expected {expected}, got {actual}"
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("/data/index.json: {error}"))
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
    let index = index().await?;
    let summary = index
        .scenes
        .iter()
        .find(|scene| scene.name.as_str() == name)
        .ok_or_else(|| format!("unknown scene `{name}`"))?;
    let bytes = get(&format!("/{}", summary.path.as_str())).await?;
    let digest = ArtifactHash::of(&bytes);
    if digest != summary.sha256 || bytes.len() as u64 != summary.bytes {
        return Err(format!(
            "scene `{name}` does not match the site index: expected {}, got {digest}",
            summary.sha256
        ));
    }
    let scene = Scene::from_f16_bytes(
        SceneName::new(name).map_err(|error| error.to_string())?,
        summary.width,
        summary.height,
        &bytes,
    )
    .map_err(|error| error.to_string())?;
    Ok(PreparedScene::new(scene))
}

/// A look version whose manifest and every downloaded artifact matched the site index.
#[derive(Clone, Debug)]
pub struct LoadedVersion {
    pub manifest: VersionManifest,
    pub document: LookDocument,
    pub lut: Arc<Vec<u8>>,
    pub diagnostics: looklab_report::LookDiagnostics,
}

fn version_summary<'a>(
    index: &'a SiteIndex,
    slug: &str,
    version: &str,
) -> Result<&'a VersionSummary, String> {
    index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == slug)
        .and_then(|look| {
            look.versions
                .iter()
                .find(|entry| entry.version.as_str() == version)
        })
        .ok_or_else(|| format!("{slug} {version} is not listed in the site index"))
}

async fn verified_manifest(
    slug: &str,
    version: &str,
    summary: &VersionSummary,
) -> Result<VersionManifest, String> {
    let bytes = get(&format!("/data/looks/{slug}/{version}/manifest.json")).await?;
    let digest = ArtifactHash::of(&bytes);
    if digest != summary.manifest_sha256 || bytes.len() as u64 != summary.manifest_bytes {
        return Err(format!(
            "{slug} {version}: manifest does not match the site index: expected {}, got {digest}",
            summary.manifest_sha256
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("{slug} {version}: {error}"))
}

/// Downloads and verifies only the preview LUT.
///
/// # Errors
/// Reports a missing manifest entry, a hash mismatch or a wrong LUT length.
pub async fn preview(slug: &str, version: &str) -> Result<Arc<Vec<u8>>, String> {
    let index = index().await?;
    let summary = version_summary(&index, slug, version)?;
    let manifest = verified_manifest(slug, version, summary).await?;
    let entry = manifest
        .artifacts
        .iter()
        .find(|entry| matches!(LookArtifact::of(&entry.name), LookArtifact::PreviewLut))
        .ok_or_else(|| format!("{slug} {version}: no preview LUT in the manifest"))?;
    let bytes = get(&format!(
        "/data/looks/{slug}/{version}/{}",
        entry.name.as_str()
    ))
    .await?;
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
    let index = index().await?;
    let summary = version_summary(&index, slug, version)?;
    let manifest = verified_manifest(slug, version, summary).await?;
    if manifest.look.as_str() != slug || manifest.version.as_str() != version {
        return Err(format!(
            "manifest names {} {}, page requested {slug} {version}",
            manifest.look, manifest.version
        ));
    }
    let mut look_bytes = None;
    let mut lut = None;
    for entry in &manifest.artifacts {
        let bytes = get(&format!(
            "/data/looks/{slug}/{version}/{}",
            entry.name.as_str()
        ))
        .await?;
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
    let adapter = crate::diagnostics::DocumentLook::new(&document, &probes);
    let diagnostics = looklab_report::look_diagnostics(&adapter, &probes);
    Ok(LoadedVersion {
        manifest,
        document,
        lut: Arc::new(lut),
        diagnostics,
    })
}
