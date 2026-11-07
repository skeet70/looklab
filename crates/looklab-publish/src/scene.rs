// SPDX-License-Identifier: AGPL-3.0-or-later
//! Preview scenes stored as 12-bit-quantized shaper coordinates in 16-bit RGB PNG.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::SitePath;

pub const SCENE_FORMAT: &str = "looklab-scene/rec2020-shaper-png16/1";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SceneName(String);

impl SceneName {
    /// # Errors
    /// Rejects names outside `[a-z0-9_-]`.
    pub fn new(value: impl Into<String>) -> Result<Self, PublishError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(PublishError::Path(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SceneName {
    type Error = PublishError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SceneName> for String {
    fn from(value: SceneName) -> Self {
        value.0
    }
}

impl fmt::Display for SceneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SceneKind {
    Photo,
    #[default]
    TestChart,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneCredit {
    pub text: String,
    pub source_url: String,
    pub license: String,
    pub source_sha256: crate::ArtifactHash,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneArtifact {
    pub width: u32,
    pub height: u32,
    pub path: SitePath,
    pub sha256: crate::ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSummary {
    pub name: SceneName,
    pub kind: SceneKind,
    pub title: String,
    pub credit: Option<SceneCredit>,
    pub thumbnail: SceneArtifact,
    pub width: u32,
    pub height: u32,
    pub path: SitePath,
    pub sha256: crate::ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneIndex {
    pub format: String,
    pub scenes: Vec<SceneSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub name: SceneName,
    pub kind: SceneKind,
    pub title: String,
    pub credit: Option<SceneCredit>,
    pub width: u32,
    pub height: u32,
    pub xyz: Vec<[f32; 3]>,
}

impl Scene {
    /// # Errors
    /// Rejects mismatched dimensions and non-finite XYZ values.
    pub fn new(
        name: SceneName,
        width: u32,
        height: u32,
        xyz: Vec<[f32; 3]>,
    ) -> Result<Self, PublishError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .ok_or(PublishError::SceneDimensions)?;
        if xyz.len() != expected || expected == 0 {
            return Err(PublishError::SceneDimensions);
        }
        if xyz.iter().flatten().any(|value| !value.is_finite()) {
            return Err(PublishError::SceneValue);
        }
        Ok(Self {
            title: name.as_str().replace('-', " "),
            name,
            kind: SceneKind::TestChart,
            credit: None,
            width,
            height,
            xyz,
        })
    }

    /// # Errors
    /// Rejects edges above 1600 pixels.
    pub fn to_png(&self) -> Result<Vec<u8>, PublishError> {
        use looklab_color::{RgbSpace, Xyz};
        if self.width > 1600 || self.height > 1600 {
            return Err(PublishError::SceneDimensions);
        }
        let mut data = Vec::with_capacity(self.xyz.len() * 6);
        for pixel in &self.xyz {
            let rgb = RgbSpace::Rec2020.xyz_to_rgb_linear(Xyz::from_array(pixel.map(f64::from)));
            for value in [rgb.r, rgb.g, rgb.b] {
                let coordinate = (crate::preview_shaper(value) * 4095.0).round();
                let value = (coordinate * (65535.0 / 4095.0)).round() as u16;
                data.extend_from_slice(&value.to_be_bytes());
            }
        }
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder
                .write_header()
                .map_err(|e| PublishError::ScenePng(e.to_string()))?;
            writer
                .write_image_data(&data)
                .map_err(|e| PublishError::ScenePng(e.to_string()))?;
        }
        Ok(out)
    }

    /// # Errors
    /// Rejects malformed PNGs and formats other than RGB16.
    pub fn from_png(name: SceneName, bytes: &[u8]) -> Result<Self, PublishError> {
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder
            .read_info()
            .map_err(|e| PublishError::ScenePng(e.to_string()))?;
        let info = reader.info();
        if info.color_type != png::ColorType::Rgb
            || info.bit_depth != png::BitDepth::Sixteen
            || info.width == 0
            || info.height == 0
            || info.width > 1600
            || info.height > 1600
        {
            return Err(PublishError::SceneDimensions);
        }
        let mut data = vec![
            0;
            reader
                .output_buffer_size()
                .ok_or(PublishError::SceneDimensions)?
        ];
        let frame = reader
            .next_frame(&mut data)
            .map_err(|e| PublishError::ScenePng(e.to_string()))?;
        let xyz = data[..frame.buffer_size()]
            .as_chunks::<6>()
            .0
            .iter()
            .map(|pixel| {
                let channel = |i| {
                    crate::preview_unshaper(
                        f64::from(u16::from_be_bytes([pixel[i], pixel[i + 1]])) / 65535.0,
                    )
                };
                let xyz = looklab_color::RgbSpace::Rec2020
                    .rgb_linear_to_xyz(looklab_color::Rgb::new(channel(0), channel(2), channel(4)));
                xyz.to_array().map(|v| v as f32)
            })
            .collect();
        Self::new(name, frame.width, frame.height, xyz)
    }

    pub fn downscale(&self, long_edge: u32) -> Self {
        let scale = (long_edge as f64 / self.width.max(self.height) as f64).min(1.0);
        let width = (self.width as f64 * scale).round().max(1.0) as u32;
        let height = (self.height as f64 * scale).round().max(1.0) as u32;
        let mut xyz = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let x0 = x * self.width / width;
                let x1 = ((x + 1) * self.width / width).max(x0 + 1);
                let y0 = y * self.height / height;
                let y1 = ((y + 1) * self.height / height).max(y0 + 1);
                let mut sum = [0.0_f64; 3];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        for (c, value) in sum.iter_mut().enumerate() {
                            *value += f64::from(self.xyz[(sy * self.width + sx) as usize][c]);
                        }
                    }
                }
                let count = f64::from((x1 - x0) * (y1 - y0));
                xyz.push(sum.map(|v| (v / count) as f32));
            }
        }
        Self {
            width,
            height,
            xyz,
            name: self.name.clone(),
            kind: self.kind,
            title: self.title.clone(),
            credit: self.credit.clone(),
        }
    }
}

/// Neutral/hue gradient, hue wheel and memory-colour patches in scene-linear
/// D65 XYZ.
///
/// # Errors
/// Rejects a size outside `3..=1600` and scenes with non-finite values.
pub fn test_chart_scenes(size: u32) -> Result<Vec<Scene>, PublishError> {
    if !(3..=1600).contains(&size) {
        return Err(PublishError::ChartSize(size));
    }
    Ok(vec![
        gradient_scene(size)?,
        hue_wheel_scene(size)?,
        patches_scene(size)?,
    ])
}

fn gradient_scene(size: u32) -> Result<Scene, PublishError> {
    let colours = [
        [0.82, 0.18, 0.16],
        [0.88, 0.55, 0.18],
        [0.86, 0.82, 0.24],
        [0.28, 0.62, 0.30],
        [0.22, 0.44, 0.78],
        [0.48, 0.28, 0.66],
    ];
    let mut xyz = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        let v = f64::from(y) / f64::from(size - 1);
        for x in 0..size {
            let u = f64::from(x) / f64::from(size - 1);
            let encoded = if u < 0.5 {
                let level = 0.002 + (u / 0.5);
                [level, level, level]
            } else {
                let band =
                    (((u - 0.5) * colours.len() as f64).floor() as usize).min(colours.len() - 1);
                let shade = 0.10 + 0.90 * (1.0 - v);
                [
                    colours[band][0] * shade,
                    colours[band][1] * shade,
                    colours[band][2] * shade,
                ]
            };
            xyz.push(encoded_xyz(encoded));
        }
    }
    Scene::new(SceneName::new("gradient")?, size, size, xyz)
}

fn hue_wheel_scene(size: u32) -> Result<Scene, PublishError> {
    let mut xyz = Vec::with_capacity((size * size) as usize);
    let centre = f64::from(size - 1) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = f64::from(x) - centre;
            let dy = f64::from(y) - centre;
            let distance = (dx * dx + dy * dy).sqrt() / centre;
            let angle = libm::atan2(dy, dx);
            let turns = angle / (2.0 * core::f64::consts::PI);
            let hue = turns - turns.floor();
            let saturation = distance.clamp(0.0, 1.0);
            let value = 1.0 - 0.55 * saturation;
            xyz.push(encoded_xyz(hsv(hue, saturation, value)));
        }
    }
    Scene::new(SceneName::new("hue-wheel")?, size, size, xyz)
}

fn patches_scene(size: u32) -> Result<Scene, PublishError> {
    let patches = [
        [0.91, 0.76, 0.66],
        [0.55, 0.36, 0.26],
        [0.22, 0.42, 0.16],
        [0.44, 0.66, 0.92],
        [0.18, 0.18, 0.18],
        [0.92, 0.92, 0.92],
        [0.04, 0.04, 0.04],
        [0.78, 0.12, 0.10],
        [0.16, 0.60, 0.22],
        [0.12, 0.32, 0.78],
        [0.96, 0.72, 0.28],
        [0.10, 0.09, 0.12],
    ];
    let cols = 4_u32;
    let rows = 3_u32;
    let mut xyz = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        let row = (y * rows / size).min(rows - 1) as usize;
        let v = f64::from(y % (size / rows)) / f64::from(size / rows);
        for x in 0..size {
            let col = (x * cols / size).min(cols - 1) as usize;
            let patch = patches[(row * cols as usize + col).min(patches.len() - 1)];
            let shade = 0.72 + 0.28 * v;
            xyz.push(encoded_xyz([
                patch[0] * shade,
                patch[1] * shade,
                patch[2] * shade,
            ]));
        }
    }
    Scene::new(SceneName::new("patches")?, size, size, xyz)
}

fn encoded_xyz(encoded: [f64; 3]) -> [f32; 3] {
    let xyz = looklab_color::RgbSpace::Srgb
        .rgb_encoded_to_xyz(looklab_color::Rgb::new(encoded[0], encoded[1], encoded[2]));
    [xyz.x as f32, xyz.y as f32, xyz.z as f32]
}

fn hsv(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h6 = h * 6.0;
    let i = h6.floor();
    let f = h6 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match (i as i64).rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_scene_values_are_rejected() {
        assert!(matches!(
            Scene::new(
                SceneName::new("invalid").expect("name"),
                1,
                1,
                vec![[f32::NAN, 0.18, 0.18]]
            ),
            Err(PublishError::SceneValue)
        ));
    }

    #[test]
    fn oversized_intermediates_must_be_downscaled_before_png_encoding() {
        for (width, height) in [(1601, 1), (1, 1601)] {
            let scene = Scene::new(
                SceneName::new("raw-intermediate").expect("name"),
                width,
                height,
                vec![[0.18; 3]; (width * height) as usize],
            )
            .expect("RAW intermediate");
            assert!(matches!(scene.to_png(), Err(PublishError::SceneDimensions)));
            let downscaled = scene.downscale(1600);
            let bytes = downscaled.to_png().expect("downscaled PNG");
            let decoded = Scene::from_png(scene.name.clone(), &bytes).expect("decode");
            assert_eq!(
                (decoded.width, decoded.height),
                (downscaled.width, downscaled.height)
            );
        }
    }

    #[test]
    fn compact_scene_preserves_negative_and_highlight_colors() {
        let scene = Scene::new(
            SceneName::new("colors").expect("name"),
            3,
            1,
            vec![[-0.02, 0.01, 0.05], [0.18, 0.18, 0.18], [2.0, 3.0, 4.0]],
        )
        .expect("scene");
        let decoded =
            Scene::from_png(scene.name.clone(), &scene.to_png().expect("encode")).expect("decode");
        assert_eq!((decoded.width, decoded.height), (3, 1));
        for (expected, actual) in scene.xyz.iter().zip(&decoded.xyz) {
            for (expected, actual) in expected.iter().zip(actual) {
                assert!((expected - actual).abs() < 0.01);
            }
        }
    }

    #[test]
    fn thumbnail_averages_scene_linear_light() {
        let scene = Scene::new(
            SceneName::new("light").expect("name"),
            2,
            1,
            vec![[0.0; 3], [1.0; 3]],
        )
        .expect("scene");
        let thumbnail = scene.downscale(1);
        assert_eq!((thumbnail.width, thumbnail.height), (1, 1));
        assert_eq!(thumbnail.xyz, vec![[0.5; 3]]);
    }
}
