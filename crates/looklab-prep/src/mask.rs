// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sample exclusion masks, [docs/03 §2.1 step 5](../../../docs/03-data-model.md).
//! A chart session's region enters through [`Mask::exclude_region`].

use serde::{Deserialize, Serialize};

use crate::image::Image3;

/// Rec.709 luma coefficients, ITU-R BT.709-6 table 3.
const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaskOptions {
    /// Fraction of each image dimension excluded at every edge.
    pub border_fraction: f64,
    /// Gradient magnitude percentile above which a pixel is excluded, in
    /// `0.0..=1.0`.
    pub gradient_percentile: f64,
}

impl Default for MaskOptions {
    fn default() -> Self {
        Self {
            border_fraction: 0.03,
            gradient_percentile: 0.90,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskCounts {
    pub border: usize,
    /// Counted by [`Mask::exclude_clipped`] for pixels still kept when it runs.
    pub clipped: usize,
    /// Above the gradient percentile, or a non-finite gradient neighbourhood.
    pub gradient: usize,
    /// A non-finite channel or luminance.
    pub invalid: usize,
    /// Inside a chart region, counted by [`Mask::exclude_region`].
    pub chart: usize,
}

/// Per-pixel keep bits for one image. Reasons are exclusive: border and
/// non-finite channels first, then gradient, then clipping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    width: u32,
    height: u32,
    keep: Vec<bool>,
    counts: MaskCounts,
}

impl Mask {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn keep(&self) -> &[bool] {
        &self.keep
    }

    pub fn counts(&self) -> MaskCounts {
        self.counts
    }

    pub fn kept(&self) -> usize {
        self.keep.iter().filter(|keep| **keep).count()
    }

    pub fn is_kept(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.keep[(y * self.width + x) as usize]
    }

    /// Indices into [`Image3::data`], ascending.
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.keep
            .iter()
            .enumerate()
            .filter(|(_, keep)| **keep)
            .map(|(index, _)| index)
    }

    pub fn exclude_clipped(&mut self, clipped: &[bool]) {
        for (index, excluded) in clipped.iter().enumerate() {
            if *excluded && self.keep.get(index).copied().unwrap_or(false) {
                self.keep[index] = false;
                self.counts.clipped += 1;
            }
        }
    }

    pub fn exclude_region(&mut self, inside: &[bool]) {
        for (index, excluded) in inside.iter().enumerate() {
            if *excluded && self.keep.get(index).copied().unwrap_or(false) {
                self.keep[index] = false;
                self.counts.chart += 1;
            }
        }
    }
}

pub fn build_mask(image: &Image3, options: &MaskOptions) -> Mask {
    let width = image.width as usize;
    let height = image.height as usize;
    let n = width * height;
    let border = options.border_fraction.clamp(0.0, 0.5);
    let border_x = (width as f64 * border).ceil() as usize;
    let border_y = (height as f64 * border).ceil() as usize;

    let mut keep = vec![false; n];
    let mut counts = MaskCounts::default();
    let mut is_border = vec![false; n];
    for y in 0..height {
        for x in 0..width {
            if x < border_x || y < border_y || x >= width - border_x || y >= height - border_y {
                is_border[y * width + x] = true;
                counts.border += 1;
            }
        }
    }

    let mut luma = vec![f64::NAN; n];
    for (index, px) in image.data.iter().enumerate() {
        let value =
            f64::from(px[0]) * LUMA[0] + f64::from(px[1]) * LUMA[1] + f64::from(px[2]) * LUMA[2];
        if value.is_finite() && px.iter().all(|channel| channel.is_finite()) {
            luma[index] = value;
        } else if !is_border[index] {
            counts.invalid += 1;
        }
    }

    let mut interior_magnitudes = Vec::new();
    let mut magnitude = vec![f64::NAN; n];
    for y in 1..height.saturating_sub(1) {
        for x in 1..width.saturating_sub(1) {
            let index = y * width + x;
            if is_border[index] {
                continue;
            }
            let neighbours = [
                luma[index],
                luma[index - 1],
                luma[index + 1],
                luma[index - width],
                luma[index + width],
            ];
            if !neighbours.iter().all(|value| value.is_finite()) {
                continue;
            }
            let gx = (luma[index + 1] - luma[index - 1]) * 0.5;
            let gy = (luma[index + width] - luma[index - width]) * 0.5;
            let value = (gx * gx + gy * gy).sqrt();
            magnitude[index] = value;
            interior_magnitudes.push(value);
        }
    }
    interior_magnitudes.sort_by(f64::total_cmp);
    let threshold = percentile(&interior_magnitudes, options.gradient_percentile);

    for index in 0..n {
        if is_border[index] || !luma[index].is_finite() {
            continue;
        }
        if !magnitude[index].is_finite() || magnitude[index] > threshold {
            counts.gradient += 1;
            continue;
        }
        keep[index] = true;
    }

    Mask {
        width: image.width,
        height: image.height,
        keep,
        counts,
    }
}

/// The value at `percentile`, interpolated between the two neighbouring order
/// statistics; `+inf` when `values` is empty.
fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return f64::INFINITY;
    }
    let position = percentile.clamp(0.0, 1.0) * (values.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    values[lower] + (values[upper] - values[lower]) * weight
}
