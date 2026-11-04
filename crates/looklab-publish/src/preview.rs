// SPDX-License-Identifier: AGPL-3.0-or-later
//! Preview LUTs: a 33³ grid over encoded sRGB, baked from a Look and applied to
//! canonical scenes (docs/08 §5).

use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{Rgb, RgbSpace, Xyz};
use looklab_model::LookDocument;

use crate::PublishError;
use crate::scene::Scene;

pub const LUT_SIZE: usize = 33;
/// `LUT_SIZE³` RGB entries of `f32`.
pub const LUT_BYTES: usize = LUT_SIZE * LUT_SIZE * LUT_SIZE * 3 * 4;

/// Encoded-sRGB output of [`bake_preview_lut`], little-endian `f32`, red slowest.
pub fn bake_preview_lut(document: &LookDocument) -> Vec<u8> {
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut out = Vec::with_capacity(LUT_BYTES);
    let scale = 1.0 / (LUT_SIZE - 1) as f64;
    for r in 0..LUT_SIZE {
        for g in 0..LUT_SIZE {
            for b in 0..LUT_SIZE {
                let input = RgbSpace::Srgb.rgb_encoded_to_xyz(Rgb::new(
                    r as f64 * scale,
                    g as f64 * scale,
                    b as f64 * scale,
                ));
                let clipped = document
                    .look
                    .output_encode(document.look.eval(input), RgbSpace::Srgb);
                let output = ucs_to_xyz(&model, clipped);
                let encoded = RgbSpace::Srgb.xyz_to_rgb_encoded(output);
                for channel in [encoded.r, encoded.g, encoded.b] {
                    out.extend_from_slice(&(channel as f32).to_le_bytes());
                }
            }
        }
    }
    out
}

fn ucs_to_xyz(model: &Cam16Model, ucs: Ucs) -> Xyz {
    model.jmh_to_xyz(ucs_to_jmh(ucs, UcsSpace::Ucs))
}

/// A canonical scene with its encoded-sRGB coordinates.
#[derive(Clone, Debug)]
pub struct PreparedScene {
    pub scene: Scene,
    samples: Vec<Sample>,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    corners: [u32; 4],
    weights: [f32; 4],
}

impl PreparedScene {
    pub fn new(mut scene: Scene) -> Self {
        let samples = scene.xyz.iter().map(|xyz| sample(*xyz)).collect::<Vec<_>>();
        scene.xyz = Vec::new();
        Self { scene, samples }
    }

    pub fn width(&self) -> u32 {
        self.scene.width
    }

    pub fn height(&self) -> u32 {
        self.scene.height
    }

    pub fn pixels(&self) -> usize {
        self.samples.len()
    }
}

/// Renders a scene to RGBA8 with a baked LUT.
///
/// # Errors
/// Rejects a LUT whose byte length differs from [`LUT_BYTES`].
pub fn render_preview(lut: &[u8], scene: &PreparedScene) -> Result<Vec<u8>, PublishError> {
    if lut.len() != LUT_BYTES {
        return Err(PublishError::PreviewLut(lut.len()));
    }
    let values = lut
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect::<Vec<_>>();
    let mut out = Vec::with_capacity(scene.samples.len() * 4);
    for sample in &scene.samples {
        let [c0, c1, c2, c3] = sample.corners.map(|corner| corner as usize);
        let [w0, w1, w2, w3] = sample.weights;
        for channel in 0..3 {
            let value = w0 * values[c0 + channel]
                + w1 * values[c1 + channel]
                + w2 * values[c2 + channel]
                + w3 * values[c3 + channel];
            out.push((value.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
        out.push(255);
    }
    Ok(out)
}

fn sample(xyz: [f32; 3]) -> Sample {
    let rgb = RgbSpace::Srgb.xyz_to_rgb_encoded(Xyz::new(
        f64::from(xyz[0]),
        f64::from(xyz[1]),
        f64::from(xyz[2]),
    ));
    let rgb = [
        (rgb.r as f32).clamp(0.0, 1.0),
        (rgb.g as f32).clamp(0.0, 1.0),
        (rgb.b as f32).clamp(0.0, 1.0),
    ];
    let max = (LUT_SIZE - 1) as f32;
    let pos = [rgb[0] * max, rgb[1] * max, rgb[2] * max];
    let low = [
        (pos[0].floor() as usize).min(LUT_SIZE - 1),
        (pos[1].floor() as usize).min(LUT_SIZE - 1),
        (pos[2].floor() as usize).min(LUT_SIZE - 1),
    ];
    let frac = [
        (pos[0] - low[0] as f32).clamp(0.0, 1.0),
        (pos[1] - low[1] as f32).clamp(0.0, 1.0),
        (pos[2] - low[2] as f32).clamp(0.0, 1.0),
    ];
    let step = |r: usize, g: usize, b: usize| (((r * LUT_SIZE + g) * LUT_SIZE + b) * 3) as u32;
    let c000 = step(low[0], low[1], low[2]);
    let c100 = step((low[0] + 1).min(LUT_SIZE - 1), low[1], low[2]);
    let c010 = step(low[0], (low[1] + 1).min(LUT_SIZE - 1), low[2]);
    let c001 = step(low[0], low[1], (low[2] + 1).min(LUT_SIZE - 1));
    let c110 = step(
        (low[0] + 1).min(LUT_SIZE - 1),
        (low[1] + 1).min(LUT_SIZE - 1),
        low[2],
    );
    let c101 = step(
        (low[0] + 1).min(LUT_SIZE - 1),
        low[1],
        (low[2] + 1).min(LUT_SIZE - 1),
    );
    let c011 = step(
        low[0],
        (low[1] + 1).min(LUT_SIZE - 1),
        (low[2] + 1).min(LUT_SIZE - 1),
    );
    let c111 = step(
        (low[0] + 1).min(LUT_SIZE - 1),
        (low[1] + 1).min(LUT_SIZE - 1),
        (low[2] + 1).min(LUT_SIZE - 1),
    );

    let [r, g, b] = frac;
    let (corners, weights) = if r > g {
        if g > b {
            ([c000, c100, c110, c111], [1.0 - r, r - g, g - b, b])
        } else if r > b {
            ([c000, c100, c101, c111], [1.0 - r, r - b, b - g, g])
        } else {
            ([c000, c001, c101, c111], [1.0 - b, b - r, r - g, g])
        }
    } else if b > g {
        ([c000, c001, c011, c111], [1.0 - b, b - g, g - r, r])
    } else if b > r {
        ([c000, c010, c011, c111], [1.0 - g, g - b, b - r, r])
    } else {
        ([c000, c010, c110, c111], [1.0 - g, g - r, r - b, b])
    };
    Sample { corners, weights }
}

#[cfg(test)]
mod tests {
    use looklab_model::Look;
    use looklab_model::file::identity;

    use super::*;
    use crate::scene::SceneName;

    #[test]
    fn identity_lut_reproduces_srgb() {
        let document = LookDocument::from_look(
            identity("identity", "1.0.0").expect("id"),
            Look::colorimetric(),
        )
        .expect("document");
        let lut = bake_preview_lut(&document);
        assert_eq!(lut.len(), LUT_BYTES);
        let scene = PreparedScene::new(
            Scene::new(
                SceneName::new("patch").expect("name"),
                1,
                1,
                vec![[0.2, 0.2, 0.2]],
            )
            .expect("scene"),
        );
        let rgba = render_preview(&lut, &scene).expect("render");
        assert_eq!(rgba.len(), 4);
        let expected = RgbSpace::Srgb.xyz_to_rgb_encoded(Xyz::new(0.2, 0.2, 0.2));
        assert!((f64::from(rgba[0]) / 255.0 - expected.r).abs() < 0.02);
        assert!((f64::from(rgba[1]) / 255.0 - expected.g).abs() < 0.02);
        assert!((f64::from(rgba[2]) / 255.0 - expected.b).abs() < 0.02);
        assert_eq!(rgba[3], 255);
    }

    #[test]
    fn render_rejects_a_wrong_lut_length() {
        let scene = PreparedScene::new(
            Scene::new(
                SceneName::new("patch").expect("name"),
                1,
                1,
                vec![[0.2, 0.2, 0.2]],
            )
            .expect("scene"),
        );
        assert!(matches!(
            render_preview(&[0; 4], &scene),
            Err(PublishError::PreviewLut(4))
        ));
    }
}
