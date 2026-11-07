// SPDX-License-Identifier: AGPL-3.0-or-later
//! CC0 photo scenes decoded from fetched RAW fixtures.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::hash::ArtifactHash;
use crate::{PublishError, Scene, SceneCredit, SceneKind, SceneName};

/// `(raw.pixls.us record, title)` of the photographic preview scenes.
pub const PHOTO_SCENES: [(&str, &str); 6] = [
    ("3115", "Coastal dunes"),
    ("1955", "Riverside light"),
    ("1824", "Autumn canopy"),
    ("1797", "Blue boat on a green waterway"),
    ("3094", "Frog among lily pads"),
    ("1603", "Sunlit brickwork"),
];

#[derive(Debug, thiserror::Error)]
pub enum PhotoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest: {0}")]
    Manifest(#[from] toml::de::Error),
    #[error("RAW: {0}")]
    Raw(#[from] looklab_raw::RawError),
    #[error("body: {0}")]
    Body(#[from] looklab_body::BodyError),
    #[error(transparent)]
    Publish(#[from] PublishError),
    #[error("{0}")]
    Source(String),
}

#[derive(Deserialize)]
struct FixtureSources {
    fixture: Vec<FixtureSource>,
}

#[derive(Deserialize)]
struct FixtureSource {
    file: String,
    make: String,
    model: String,
    source_url: String,
    license: String,
    sha256: ArtifactHash,
    size: u64,
}

impl FixtureSource {
    fn credit(&self) -> String {
        let normalize = |value: &str| {
            let mut normalized = String::with_capacity(value.len());
            for word in value.split_whitespace() {
                if !normalized.is_empty() {
                    normalized.push(' ');
                }
                normalized.push_str(word);
            }
            normalized
        };
        let make = normalize(&self.make);
        let model = normalize(&self.model);
        let camera = if model
            .get(..make.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&make))
        {
            model
        } else {
            format!("{make} {model}")
        };
        format!("{camera} · raw.pixls.us · CC0")
    }
}

/// Decodes `PHOTO_SCENES` from `cache`, verifying each source against the manifest digest.
///
/// # Errors
/// Rejects a digest or size mismatch, and RAW files with no color matrix or usable luminance.
pub fn prepare_photo_scenes(manifest: &str, cache: &Path) -> Result<Vec<Scene>, PhotoError> {
    use looklab_raw::{RawDecoder, RawlerDecoder};

    let sources: FixtureSources = toml::from_str(manifest)?;
    let mut scenes = Vec::with_capacity(PHOTO_SCENES.len());
    for (id, title) in PHOTO_SCENES {
        let filename = format!("scene-{id}.dng");
        let path = cache.join(&filename);
        let bytes = fs::read(&path).map_err(|error| {
            PhotoError::Source(format!(
                "{}: {error}. Run tools/fetch-fixtures.sh.",
                path.display()
            ))
        })?;
        let source = sources
            .fixture
            .iter()
            .find(|entry| entry.file == filename)
            .ok_or_else(|| {
                PhotoError::Source(format!("{filename} is missing from the fixture manifest"))
            })?;
        if source.sha256 != ArtifactHash::of(&bytes) || source.size != bytes.len() as u64 {
            return Err(PhotoError::Source(format!(
                "{}: source checksum mismatch. Run tools/fetch-fixtures.sh.",
                path.display()
            )));
        }

        let raw = RawlerDecoder.decode(&bytes)?;
        let profile =
            looklab_body::CameraProfile::from_raw(raw.color_matrices.as_ref().ok_or_else(
                || PhotoError::Source(format!("{filename}: RAW has no color matrix")),
            )?)?;
        let matrix =
            looklab_body::camera_to_xyz_at_wb(profile.spec(), raw.as_shot_wb.map(|v| v as f32))?;
        let mut xyz: Vec<[f32; 3]> = raw
            .apply_wb()
            .as_chunks::<3>()
            .0
            .iter()
            .map(|rgb| {
                matrix
                    .mul_vec([rgb[0] as f64, rgb[1] as f64, rgb[2] as f64])
                    .map(|v| v as f32)
            })
            .collect();
        let mut luminance: Vec<f32> = xyz
            .iter()
            .map(|p| p[1])
            .filter(|y| y.is_finite() && *y > 0.00001)
            .collect();
        if luminance.is_empty() {
            return Err(PhotoError::Source(format!(
                "{filename}: no usable luminance"
            )));
        }
        let middle = luminance.len() / 2;
        luminance.select_nth_unstable_by(middle, f32::total_cmp);
        let exposure = 0.18 / luminance[middle];
        for pixel in &mut xyz {
            for value in pixel {
                *value *= exposure;
            }
        }
        let mut scene = Scene::new(
            SceneName::new(format!("photo-{id}"))?,
            raw.width as u32,
            raw.height as u32,
            xyz,
        )?
        .downscale(1000);
        while scene.to_png()?.len() > 2_500_000 {
            scene = scene.downscale(scene.width.max(scene.height) * 9 / 10);
        }
        scene.kind = SceneKind::Photo;
        scene.title = title.to_owned();
        scene.credit = Some(SceneCredit {
            text: source.credit(),
            source_url: source.source_url.clone(),
            license: source.license.clone(),
            source_sha256: source.sha256.clone(),
        });
        scenes.push(scene);
    }
    Ok(scenes)
}
