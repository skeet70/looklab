// SPDX-License-Identifier: AGPL-3.0-or-later
//! A prepared scene directory: one PNG per scene plus the provenance index that authenticates it.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::hash::ArtifactHash;
use crate::{PublishError, Scene, SceneCredit, SceneKind, SceneName};

pub const SCENE_SOURCE_FORMAT: &str = "looklab-scene-sources/1";
pub const SCENE_SOURCE_INDEX: &str = "index.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSource {
    pub name: SceneName,
    pub kind: SceneKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit: Option<SceneCredit>,
    pub width: u32,
    pub height: u32,
    pub sha256: ArtifactHash,
    pub bytes: u64,
}

impl SceneSource {
    fn of(scene: &Scene, png: &[u8]) -> Self {
        Self {
            name: scene.name.clone(),
            kind: scene.kind,
            title: scene.title.clone(),
            credit: scene.credit.clone(),
            width: scene.width,
            height: scene.height,
            sha256: ArtifactHash::of(png),
            bytes: png.len() as u64,
        }
    }

    /// # Errors
    /// Rejects bytes that do not match the recorded digest, size or dimensions.
    pub fn decode(&self, png: &[u8]) -> Result<Scene, PublishError> {
        if png.len() as u64 != self.bytes || ArtifactHash::of(png) != self.sha256 {
            return Err(PublishError::SceneSources(format!(
                "{}: bytes do not match the scene source index",
                self.name
            )));
        }
        let mut scene = Scene::from_png(self.name.clone(), png)?;
        if (scene.width, scene.height) != (self.width, self.height) {
            return Err(PublishError::SceneSources(format!(
                "{}: PNG dimensions do not match the scene source index",
                self.name
            )));
        }
        scene.kind = self.kind;
        scene.title.clone_from(&self.title);
        scene.credit.clone_from(&self.credit);
        Ok(scene)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSourceIndex {
    pub format: String,
    pub scenes: Vec<SceneSource>,
}

fn png_name(name: &SceneName) -> String {
    format!("{name}.png")
}

/// # Errors
/// Rejects scenes that cannot be encoded as a PNG.
pub fn write_scene_sources(directory: &Path, scenes: &[Scene]) -> Result<(), PublishError> {
    fs::create_dir_all(directory)?;
    let mut sources = Vec::with_capacity(scenes.len());
    for scene in scenes {
        let png = scene.to_png()?;
        fs::write(directory.join(png_name(&scene.name)), &png)?;
        sources.push(SceneSource::of(scene, &png));
    }
    let index = SceneSourceIndex {
        format: SCENE_SOURCE_FORMAT.to_owned(),
        scenes: sources,
    };
    fs::write(
        directory.join(SCENE_SOURCE_INDEX),
        serde_json::to_vec_pretty(&index)?,
    )?;
    Ok(())
}

/// # Errors
/// Rejects a missing or unknown index, unreadable scene files, and mismatched bytes.
pub fn read_scene_sources(directory: &Path) -> Result<Vec<Scene>, PublishError> {
    let index: SceneSourceIndex =
        serde_json::from_slice(&fs::read(directory.join(SCENE_SOURCE_INDEX))?)?;
    if index.format != SCENE_SOURCE_FORMAT {
        return Err(PublishError::SceneSources(format!(
            "unsupported scene source format `{}`",
            index.format
        )));
    }
    index
        .scenes
        .iter()
        .map(|source| {
            let png = fs::read(directory.join(png_name(&source.name)))?;
            source.decode(&png)
        })
        .collect()
}
