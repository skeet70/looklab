// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(dead_code)]

//! Synthetic photographs: a template warped by a homography into a scene, with
//! defocus, sensor noise, exposure and a display model.

use looklab_color::RgbSpace;
use looklab_prep::geom::{Homography, Point, Quad, UNIT_SQUARE};
use looklab_prep::{Alignment, DecodedJpeg, Image3};
use looklab_raw::{DecodedRaw, Orientation};
use looklab_samples::OutputSpace;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::{RngCore, SeedableRng};

pub fn rng(seed: u64) -> ChaCha20Rng {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&seed.to_le_bytes());
    ChaCha20Rng::from_seed(bytes)
}

pub fn uniform(rng: &mut ChaCha20Rng) -> f64 {
    (f64::from(rng.next_u32()) + 0.5) / 4_294_967_296.0
}

pub fn signed(rng: &mut ChaCha20Rng, magnitude: f64) -> f64 {
    (uniform(rng) - 0.5) * 2.0 * magnitude
}

pub fn blank(width: u32, height: u32, colour: [f32; 3]) -> Image3 {
    Image3::new(width, height, vec![colour; (width * height) as usize]).expect("dimensions match")
}

pub fn fill_rect(image: &mut Image3, x0: f64, y0: f64, x1: f64, y1: f64, colour: [f32; 3]) {
    let px0 = x0.floor().clamp(0.0, f64::from(image.width)) as u32;
    let py0 = y0.floor().clamp(0.0, f64::from(image.height)) as u32;
    let px1 = x1.ceil().clamp(0.0, f64::from(image.width)) as u32;
    let py1 = y1.ceil().clamp(0.0, f64::from(image.height)) as u32;
    for y in py0..py1 {
        for x in px0..px1 {
            image.data[(y * image.width + x) as usize] = colour;
        }
    }
}

pub fn map_colour(image: &mut Image3, mut f: impl FnMut(usize, usize, [f64; 3]) -> [f32; 3]) {
    let width = image.width as usize;
    for (index, pixel) in image.data.iter_mut().enumerate() {
        *pixel = f(
            index % width,
            index / width,
            [
                f64::from(pixel[0]),
                f64::from(pixel[1]),
                f64::from(pixel[2]),
            ],
        );
    }
}

pub fn scale(image: &Image3, factor: f64) -> Image3 {
    let data = image
        .data
        .iter()
        .map(|rgb| {
            [
                (f64::from(rgb[0]) * factor) as f32,
                (f64::from(rgb[1]) * factor) as f32,
                (f64::from(rgb[2]) * factor) as f32,
            ]
        })
        .collect();
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

/// Bilinear warp of `template` onto `quad`.
pub fn warp(
    template: &Image3,
    quad: &Quad,
    width: u32,
    height: u32,
    background: [f32; 3],
) -> Image3 {
    warp_sampled(template, quad, width, height, background, 1)
}

/// [`warp`] with `grid × grid` samples per pixel, which places the template's
/// edges at their geometric position rather than on the pixel lattice.
pub fn warp_fine(
    template: &Image3,
    quad: &Quad,
    width: u32,
    height: u32,
    background: [f32; 3],
) -> Image3 {
    warp_sampled(template, quad, width, height, background, 3)
}

fn warp_sampled(
    template: &Image3,
    quad: &Quad,
    width: u32,
    height: u32,
    background: [f32; 3],
    grid: u32,
) -> Image3 {
    let inverse = Homography::from_quad(quad, &UNIT_SQUARE).expect("the quad is projective");
    let max_x = f64::from(template.width - 1);
    let max_y = f64::from(template.height - 1);
    let grid = grid.max(1);
    let step = f64::from(grid);
    let mut data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0.0f64; 3];
            let mut count = 0.0f64;
            for gy in 0..grid {
                for gx in 0..grid {
                    let point = Point::new(
                        f64::from(x) + (f64::from(gx) + 0.5) / step - 0.5,
                        f64::from(y) + (f64::from(gy) + 0.5) / step - 0.5,
                    );
                    let rgb = inverse
                        .map(point)
                        .filter(|uv| (0.0..=1.0).contains(&uv.x) && (0.0..=1.0).contains(&uv.y))
                        .and_then(|uv| {
                            let source_x =
                                (uv.x * f64::from(template.width) - 0.5).clamp(0.0, max_x);
                            let source_y =
                                (uv.y * f64::from(template.height) - 0.5).clamp(0.0, max_y);
                            template.sample_bilinear(source_x, source_y)
                        })
                        .unwrap_or(background);
                    for channel in 0..3 {
                        sum[channel] += f64::from(rgb[channel]);
                    }
                    count += 1.0;
                }
            }
            data.push([
                (sum[0] / count) as f32,
                (sum[1] / count) as f32,
                (sum[2] / count) as f32,
            ]);
        }
    }
    Image3::new(width, height, data).expect("dimensions match")
}

/// Separable box blur, the defocus model.
pub fn blur(image: &Image3, radius: u32) -> Image3 {
    if radius == 0 {
        return image.clone();
    }
    let width = image.width as usize;
    let height = image.height as usize;
    let reach = radius as isize;
    let mut horizontal = vec![[0.0f32; 3]; width * height];
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0.0f32; 3];
            let mut count = 0.0f32;
            for offset in -reach..=reach {
                let source = x as isize + offset;
                if source < 0 || source >= width as isize {
                    continue;
                }
                let pixel = image.data[y * width + source as usize];
                for channel in 0..3 {
                    sum[channel] += pixel[channel];
                }
                count += 1.0;
            }
            horizontal[y * width + x] = [sum[0] / count, sum[1] / count, sum[2] / count];
        }
    }
    let mut data = vec![[0.0f32; 3]; width * height];
    for y in 0..height {
        for x in 0..width {
            let mut sum = [0.0f32; 3];
            let mut count = 0.0f32;
            for offset in -reach..=reach {
                let source = y as isize + offset;
                if source < 0 || source >= height as isize {
                    continue;
                }
                let pixel = horizontal[source as usize * width + x];
                for channel in 0..3 {
                    sum[channel] += pixel[channel];
                }
                count += 1.0;
            }
            data[y * width + x] = [sum[0] / count, sum[1] / count, sum[2] / count];
        }
    }
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

pub fn add_noise(image: &Image3, amplitude: f64, rng: &mut ChaCha20Rng) -> Image3 {
    let data = image
        .data
        .iter()
        .map(|rgb| {
            let mut out = [0.0f32; 3];
            for channel in 0..3 {
                let value = f64::from(rgb[channel]) + signed(rng, amplitude);
                out[channel] = value.max(0.0) as f32;
            }
            out
        })
        .collect();
    Image3::new(image.width, image.height, data).expect("dimensions match")
}

pub fn raw_from(scene: &Image3, wb: [f64; 3]) -> DecodedRaw {
    let rgb = scene
        .data
        .iter()
        .flat_map(|pixel| {
            [
                (f64::from(pixel[0]) / wb[0]) as f32,
                (f64::from(pixel[1]) / wb[1]) as f32,
                (f64::from(pixel[2]) / wb[2]) as f32,
            ]
        })
        .collect();
    DecodedRaw {
        width: scene.width as usize,
        height: scene.height as usize,
        rgb,
        as_shot_wb: wb,
        orientation: Orientation::Normal,
        make: "FUJIFILM".to_owned(),
        model: "X-S10".to_owned(),
        color_matrices: None,
    }
}

pub fn jpeg_from(image: Image3) -> DecodedJpeg {
    let code_values_near_clip = image
        .data
        .iter()
        .map(|rgb| {
            rgb.iter().any(|channel| {
                let encoded = RgbSpace::Srgb.encode(f64::from(*channel));
                !(0.008..=0.98).contains(&encoded)
            })
        })
        .collect();
    DecodedJpeg {
        image,
        space: OutputSpace::Srgb,
        space_assumed: false,
        code_values_near_clip,
    }
}

pub fn identity_alignment() -> Alignment {
    Alignment {
        scale: 1.0,
        rotation_rad: 0.0,
        tx: 0.0,
        ty: 0.0,
        k1: 0.0,
        k2: 0.0,
        gain: [0.0; 3],
        central_correlation: 1.0,
        residual_px: 0.0,
    }
}

/// A display's light for a linear sRGB pixel: the sRGB code value raised to the
/// panel gamma.
pub fn display_gamma(linear: f64, gamma: f64) -> f64 {
    libm::pow(RgbSpace::Srgb.encode(linear.clamp(0.0, 1.0)), gamma)
}

/// Moves each corner by up to `pixels` along both axes.
pub fn jitter_quad(quad: &Quad, pixels: f64, rng: &mut ChaCha20Rng) -> Quad {
    quad.map(|point| Point::new(point.x + signed(rng, pixels), point.y + signed(rng, pixels)))
}
