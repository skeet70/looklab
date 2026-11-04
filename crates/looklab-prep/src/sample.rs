// SPDX-License-Identifier: AGPL-3.0-or-later
//! Density sampling in the model's `(s, h_in, √C_in)` coordinates and the shuffle, [docs/03 §2.1 steps 6 and 8](../../../docs/03-data-model.md). A re-render group calls [`select_pixels`] once on its RAW.

use looklab_color::Xyz;
use looklab_color::linalg::Mat3;
use looklab_model::LookInput;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::PrepError;
use crate::image::Image3;
use crate::mask::Mask;

pub const MAX_DENSITY_SAMPLES: u32 = 50_000;

const EXPOSURE_MIN: f64 = -12.0;
const EXPOSURE_MAX: f64 = 6.0;
/// CAM16-UCS hue runs `[0, 360)` degrees.
const HUE_MAX: f64 = 360.0;
/// `√C_in` for UCS chroma `C_in` in `0..=100`.
const ROOT_CHROMA_MAX: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleParams {
    /// At most [`MAX_DENSITY_SAMPLES`].
    pub max_samples: u32,
    pub exposure_bins: u32,
    pub hue_bins: u32,
    pub root_chroma_bins: u32,
    /// Upper cap on a pixel's inverse-occupancy sampling weight, in `(0, 1]`.
    pub max_bin_weight: f32,
}

impl Default for SampleParams {
    fn default() -> Self {
        Self {
            max_samples: MAX_DENSITY_SAMPLES,
            exposure_bins: 18,
            hue_bins: 36,
            root_chroma_bins: 20,
            max_bin_weight: 0.5,
        }
    }
}

impl SampleParams {
    pub fn validate(&self) -> Result<(), PrepError> {
        if self.max_samples == 0 || self.max_samples > MAX_DENSITY_SAMPLES {
            return Err(PrepError::InvalidParams {
                field: "max_samples",
            });
        }
        for (field, bins) in [
            ("exposure_bins", self.exposure_bins),
            ("hue_bins", self.hue_bins),
            ("root_chroma_bins", self.root_chroma_bins),
        ] {
            if bins == 0 || bins > 256 {
                return Err(PrepError::InvalidParams { field });
            }
        }
        if !(self.max_bin_weight.is_finite()
            && self.max_bin_weight > 0.0
            && self.max_bin_weight <= 1.0)
        {
            return Err(PrepError::InvalidParams {
                field: "max_bin_weight",
            });
        }
        Ok(())
    }
}

/// Pixels chosen for one image or re-render group, in shuffled order.
#[derive(Clone, Debug, PartialEq)]
pub struct PixelSelection {
    indices: Vec<usize>,
    density_w: Vec<f32>,
}

impl PixelSelection {
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Indices into the source image's data, in shuffled order.
    pub fn indices(&self) -> &[usize] {
        &self.indices
    }

    /// Inverse inclusion probability of each selected pixel, at least 1.
    pub fn density_w(&self) -> &[f32] {
        &self.density_w
    }
}

struct Grid {
    exposure: u32,
    hue: u32,
    root_chroma: u32,
}

impl Grid {
    fn new(params: &SampleParams) -> Self {
        Self {
            exposure: params.exposure_bins,
            hue: params.hue_bins,
            root_chroma: params.root_chroma_bins,
        }
    }

    fn len(&self) -> usize {
        (self.exposure * self.hue * self.root_chroma) as usize
    }

    fn index(&self, exposure: f64, hue: f64, root_chroma: f64) -> usize {
        let s = bin(exposure, EXPOSURE_MIN, EXPOSURE_MAX, self.exposure);
        let h = bin(hue, 0.0, HUE_MAX, self.hue);
        let c = bin(root_chroma, 0.0, ROOT_CHROMA_MAX, self.root_chroma);
        (s * self.hue as usize + h) * self.root_chroma as usize + c
    }
}

fn bin(value: f64, min: f64, max: f64, count: u32) -> usize {
    let scaled = (value - min) / (max - min) * f64::from(count);
    let index = scaled.floor();
    if !index.is_finite() || index <= 0.0 {
        0
    } else if index >= f64::from(count - 1) {
        count as usize - 1
    } else {
        index as usize
    }
}

/// Includes each candidate independently, with probability proportional to the inverse occupancy of its `(s, h_in, √C_in)` bin capped at `max_bin_weight`.
pub fn select_pixels(
    image: &Image3,
    mask: &Mask,
    xyz_from_camera: &Mat3,
    params: &SampleParams,
    rng: &mut ChaCha20Rng,
) -> Result<PixelSelection, PrepError> {
    params.validate()?;
    debug_assert_eq!(mask.keep().len(), image.data.len());

    let grid = Grid::new(params);
    let mut occupancy = vec![0u32; grid.len()];
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for index in mask.indices() {
        let Some(bin) = bin_of(&image.data[index], xyz_from_camera, &grid) else {
            continue;
        };
        occupancy[bin] += 1;
        candidates.push((index, bin));
    }
    if candidates.is_empty() {
        return Err(PrepError::NoSamples);
    }

    let cap = f64::from(params.max_bin_weight);
    let weight_of = |bin: usize| (1.0 / f64::from(occupancy[bin])).min(cap);
    let total: f64 = candidates.iter().map(|(_, bin)| weight_of(*bin)).sum();

    let max_samples = params.max_samples as usize;
    let mut selected: Vec<(usize, f32)> = Vec::new();
    if candidates.len() <= max_samples {
        selected.extend(candidates.iter().map(|(index, _)| (*index, 1.0)));
    } else {
        let budget = max_samples as f64;
        for (index, bin) in &candidates {
            let probability = (budget * weight_of(*bin) / total).clamp(f64::MIN_POSITIVE, 1.0);
            if uniform_open_closed(rng) < probability {
                let weight = (1.0 / probability) as f32;
                selected.push((
                    *index,
                    weight.clamp(1.0, looklab_samples::MAX_DENSITY_WEIGHT),
                ));
            }
        }
    }
    if selected.is_empty() {
        return Err(PrepError::NoSamples);
    }
    selected.sort_by_key(|(index, _)| *index);

    let mut indices = Vec::with_capacity(selected.len());
    let mut density_w = Vec::with_capacity(selected.len());
    for (index, weight) in selected {
        indices.push(index);
        density_w.push(weight);
    }
    shuffle(&mut indices, &mut density_w, rng);
    Ok(PixelSelection { indices, density_w })
}

fn bin_of(rgb: &[f32; 3], xyz_from_camera: &Mat3, grid: &Grid) -> Option<usize> {
    if !rgb.iter().all(|channel| channel.is_finite()) {
        return None;
    }
    let camera = [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])];
    let xyz = Xyz::from_array(xyz_from_camera.mul_vec(camera));
    if ![xyz.x, xyz.y, xyz.z].iter().all(|value| value.is_finite()) {
        return None;
    }
    let input = LookInput::new(xyz);
    if !input.s.is_finite() || !input.h_in.is_finite() || !input.c_in.is_finite() {
        return None;
    }
    Some(grid.index(input.s, input.h_in, input.c_in.sqrt()))
}

/// Uniform in `(0, 1]`.
fn uniform_open_closed(rng: &mut ChaCha20Rng) -> f64 {
    const SCALE: f64 = 1.0 / 9_007_199_254_740_992.0;
    let mantissa = rng.next_u64() >> 11;
    (mantissa as f64 + 0.5) * SCALE
}

fn shuffle(indices: &mut [usize], density_w: &mut [f32], rng: &mut ChaCha20Rng) {
    for i in (1..indices.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        indices.swap(i, j);
        density_w.swap(i, j);
    }
}
