// SPDX-License-Identifier: AGPL-3.0-or-later
//! Density sampling in CAM16 `(s, h, √C)` and the shuffle, [docs/03 §2.1 steps 6 and 8](../../../docs/03-data-model.md). A re-render group calls [`select_pixels`] once on its RAW.

use looklab_color::Xyz;
use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::linalg::Mat3;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::PrepError;
use crate::image::Image3;
use crate::mask::Mask;

pub const MAX_DENSITY_SAMPLES: u32 = 50_000;

/// CAM16 saturation range used for binning; correlates above 100 clamp to the
/// last bin.
const SATURATION_MAX: f64 = 100.0;
/// CAM16 hue runs `[0, 360)` degrees.
const HUE_MAX: f64 = 360.0;
/// `√C` for CAM16 chroma `C` in `0..=100`.
const ROOT_CHROMA_MAX: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SampleParams {
    /// At most [`MAX_DENSITY_SAMPLES`].
    pub max_samples: u32,
    pub saturation_bins: u32,
    pub hue_bins: u32,
    pub root_chroma_bins: u32,
    /// Lower bound on a bin's sampling weight.
    pub bin_weight_floor: f32,
}

impl Default for SampleParams {
    fn default() -> Self {
        Self {
            max_samples: MAX_DENSITY_SAMPLES,
            saturation_bins: 20,
            hue_bins: 36,
            root_chroma_bins: 20,
            bin_weight_floor: 0.1,
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
            ("saturation_bins", self.saturation_bins),
            ("hue_bins", self.hue_bins),
            ("root_chroma_bins", self.root_chroma_bins),
        ] {
            if bins == 0 || bins > 512 {
                return Err(PrepError::InvalidParams { field });
            }
        }
        if !(self.bin_weight_floor.is_finite()
            && self.bin_weight_floor > 0.0
            && self.bin_weight_floor <= 1.0)
        {
            return Err(PrepError::InvalidParams {
                field: "bin_weight_floor",
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

    /// Inverse selection probability, normalised to mean 1 over the selection.
    pub fn density_w(&self) -> &[f32] {
        &self.density_w
    }
}

struct Grid {
    saturation: u32,
    hue: u32,
    root_chroma: u32,
}

impl Grid {
    fn new(params: &SampleParams) -> Self {
        Self {
            saturation: params.saturation_bins,
            hue: params.hue_bins,
            root_chroma: params.root_chroma_bins,
        }
    }

    fn len(&self) -> usize {
        (self.saturation * self.hue * self.root_chroma) as usize
    }

    fn index(&self, saturation: f64, hue: f64, root_chroma: f64) -> usize {
        let s = bin(saturation, SATURATION_MAX, self.saturation);
        let h = bin(hue, HUE_MAX, self.hue);
        let c = bin(root_chroma, ROOT_CHROMA_MAX, self.root_chroma);
        (s * self.hue as usize + h) * self.root_chroma as usize + c
    }
}

fn bin(value: f64, max: f64, count: u32) -> usize {
    let scaled = value / max * f64::from(count);
    let index = scaled.floor();
    if !index.is_finite() || index <= 0.0 {
        0
    } else if index >= f64::from(count - 1) {
        count as usize - 1
    } else {
        index as usize
    }
}

/// Samples up to `max_samples` kept pixels with probability proportional to the inverse occupancy of their `(s, h, √C)` bin, then shuffles. `xyz_from_camera` is the prior transform at the image's CCT.
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
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut occupancy = vec![0u32; grid.len()];
    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for index in mask.indices() {
        let Some(bin) = bin_of(&image.data[index], xyz_from_camera, &model, &grid) else {
            continue;
        };
        occupancy[bin] += 1;
        candidates.push((index, bin));
    }
    if candidates.is_empty() {
        return Err(PrepError::NoSamples);
    }

    let mut keys: Vec<(f64, usize, usize)> = Vec::with_capacity(candidates.len());
    for (index, bin) in candidates {
        let weight = (1.0 / f64::from(occupancy[bin])).max(f64::from(params.bin_weight_floor));
        // Efraimidis-Spirakis weighted sampling without replacement, IPL 97(5), 2006.
        let key = libm::pow(uniform_open_closed(rng), 1.0 / weight);
        keys.push((key, index, bin));
    }
    keys.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    keys.truncate(params.max_samples as usize);
    keys.sort_by_key(|(_, index, _)| *index);

    let mut mean = 0.0;
    for (_, _, bin) in &keys {
        mean += f64::from(occupancy[*bin]);
    }
    mean /= keys.len() as f64;
    let mut indices = Vec::with_capacity(keys.len());
    let mut density_w = Vec::with_capacity(keys.len());
    for (_, index, bin) in keys {
        indices.push(index);
        let weight = (f64::from(occupancy[bin]) / mean) as f32;
        density_w.push(weight.clamp(f32::MIN_POSITIVE, looklab_samples::MAX_DENSITY_WEIGHT));
    }

    shuffle(&mut indices, &mut density_w, rng);
    Ok(PixelSelection { indices, density_w })
}

fn bin_of(
    rgb: &[f32; 3],
    xyz_from_camera: &Mat3,
    model: &Cam16Model,
    grid: &Grid,
) -> Option<usize> {
    if !rgb.iter().all(|channel| channel.is_finite()) {
        return None;
    }
    let camera = [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])];
    let xyz = Xyz::from_array(xyz_from_camera.mul_vec(camera));
    if ![xyz.x, xyz.y, xyz.z].iter().all(|value| value.is_finite()) {
        return None;
    }
    let cam = model.xyz_to_cam16(xyz);
    if !cam.s.is_finite() || !cam.h.is_finite() || !cam.c.is_finite() || cam.c < 0.0 {
        return None;
    }
    Some(grid.index(cam.s, cam.h, cam.c.sqrt()))
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
