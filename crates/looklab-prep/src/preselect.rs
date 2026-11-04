// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rung-0 coverage-gain preselection from a cheap thumbnail, [docs/07 §1](../../../docs/07-contribution.md). Gain is the mass a thumbnail adds to bins the pool has not filled, so duplicates score near zero.

use std::borrow::Cow;

use looklab_color::Xyz;
use looklab_color::cam16::{Cam16Model, ViewingConditions};
use looklab_color::spaces::RgbSpace;
use serde::{Deserialize, Serialize};

use crate::image::Image3;

const SATURATION_MAX: f64 = 100.0;
const HUE_MAX: f64 = 360.0;
const ROOT_CHROMA_MAX: f64 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreselectOptions {
    /// Default 100 per body/sim group.
    pub max_selected: usize,
    pub saturation_bins: u32,
    pub hue_bins: u32,
    pub root_chroma_bins: u32,
    /// Larger thumbnails are area-averaged down to this many pixels first.
    pub thumbnail_max_pixels: u32,
    /// Primaries of the thumbnail.
    pub preview_space: RgbSpace,
}

impl Default for PreselectOptions {
    fn default() -> Self {
        Self {
            max_selected: 100,
            saturation_bins: 10,
            hue_bins: 12,
            root_chroma_bins: 10,
            thumbnail_max_pixels: 4096,
            preview_space: RgbSpace::Srgb,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoveragePool {
    options: PreselectOptions,
    occupied: Vec<bool>,
}

impl CoveragePool {
    pub fn new(options: PreselectOptions) -> Self {
        Self {
            occupied: vec![false; total_bins(&options)],
            options,
        }
    }

    pub fn total_bins(&self) -> usize {
        self.occupied.len()
    }

    pub fn filled_bins(&self) -> usize {
        self.occupied.iter().filter(|filled| **filled).count()
    }

    /// Expected reduction in empty coverage bins, in `0.0..=1.0`.
    pub fn gain(&self, thumbnail: &Image3) -> f64 {
        histogram(thumbnail, &self.options)
            .iter()
            .filter(|(bin, _)| !self.occupied[*bin])
            .map(|(_, weight)| weight)
            .sum()
    }

    pub fn add(&mut self, thumbnail: &Image3) {
        for (bin, _) in histogram(thumbnail, &self.options) {
            self.occupied[bin] = true;
        }
    }
}

/// Selects up to `options.max_selected` indices in selection order, taking the
/// best marginal gain against the bins chosen so far.
pub fn preselect(thumbnails: &[Image3], options: PreselectOptions) -> Vec<usize> {
    let histograms: Vec<Vec<(usize, f64)>> = thumbnails
        .iter()
        .map(|thumbnail| histogram(thumbnail, &options))
        .collect();
    let mut pool = CoveragePool::new(options);
    let mut available: Vec<usize> = (0..thumbnails.len()).collect();
    let mut chosen = Vec::new();
    while chosen.len() < pool.options.max_selected && !available.is_empty() {
        let mut best: Option<(usize, f64)> = None;
        for (position, candidate) in available.iter().enumerate() {
            let gain: f64 = histograms[*candidate]
                .iter()
                .filter(|(bin, _)| !pool.occupied[*bin])
                .map(|(_, weight)| weight)
                .sum();
            let better = match best {
                None => true,
                Some((best_position, best_gain)) => {
                    gain > best_gain || (gain == best_gain && *candidate < available[best_position])
                }
            };
            if better {
                best = Some((position, gain));
            }
        }
        let Some((position, gain)) = best else {
            break;
        };
        if gain <= 0.0 {
            break;
        }
        let candidate = available.remove(position);
        for (bin, _) in &histograms[candidate] {
            pool.occupied[*bin] = true;
        }
        chosen.push(candidate);
    }
    chosen
}

fn total_bins(options: &PreselectOptions) -> usize {
    options.saturation_bins as usize * options.hue_bins as usize * options.root_chroma_bins as usize
}

fn histogram(image: &Image3, options: &PreselectOptions) -> Vec<(usize, f64)> {
    let thumbnail = if image.data.len() as u64 > u64::from(options.thumbnail_max_pixels) {
        Cow::Owned(image.downsample_area(options.thumbnail_max_pixels))
    } else {
        Cow::Borrowed(image)
    };
    let model = Cam16Model::new(&ViewingConditions::reference());
    let to_xyz = options.preview_space.to_xyz_matrix();
    let bins = |saturation: f64, hue: f64, root_chroma: f64| {
        let s = bin(saturation, SATURATION_MAX, options.saturation_bins);
        let h = bin(hue, HUE_MAX, options.hue_bins);
        let c = bin(root_chroma, ROOT_CHROMA_MAX, options.root_chroma_bins);
        (s * options.hue_bins as usize + h) * options.root_chroma_bins as usize + c
    };

    let mut counts = vec![0u32; total_bins(options)];
    let mut touched = Vec::new();
    let mut total = 0u32;
    for rgb in &thumbnail.data {
        if !rgb.iter().all(|channel| channel.is_finite()) {
            continue;
        }
        let linear = [f64::from(rgb[0]), f64::from(rgb[1]), f64::from(rgb[2])];
        let xyz = Xyz::from_array(to_xyz.mul_vec(linear));
        if ![xyz.x, xyz.y, xyz.z].iter().all(|value| value.is_finite()) {
            continue;
        }
        let cam = model.xyz_to_cam16(xyz);
        if !cam.s.is_finite() || !cam.h.is_finite() || !cam.c.is_finite() || cam.c < 0.0 {
            continue;
        }
        let index = bins(cam.s, cam.h, cam.c.sqrt());
        if counts[index] == 0 {
            touched.push(index);
        }
        counts[index] += 1;
        total += 1;
    }
    if total == 0 {
        return Vec::new();
    }
    touched.sort_unstable();
    touched
        .into_iter()
        .map(|bin| (bin, f64::from(counts[bin]) / f64::from(total)))
        .collect()
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
