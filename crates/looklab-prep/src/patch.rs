// SPDX-License-Identifier: AGPL-3.0-or-later
//! Patch sampling shared by chart and pattern extraction.

use looklab_samples::{MAX_RGB, MIN_RGB};

use crate::align::Alignment;
use crate::geom::{Homography, Point};
use crate::image::Image3;
use crate::jpeg::DecodedJpeg;

pub(crate) struct PatchSample {
    pub in_mean: [f32; 3],
    pub in_std: [f32; 3],
    pub out_mean: [f32; 3],
    pub out_std: [f32; 3],
}

/// The images and sampling settings one patch measurement uses.
pub(crate) struct PatchSampler<'a> {
    pub jpeg: &'a DecodedJpeg,
    /// White-balanced linear camera RGB after orientation and radial gain.
    pub raw: &'a Image3,
    pub alignment: &'a Alignment,
    pub inner_fraction: f64,
    pub taps: u32,
    pub max_relative_std: f64,
}

impl PatchSampler<'_> {
    /// Averages `centre ± half` in the coordinates `homography` maps into, and
    /// rejects the patch when a channel's spread over its mean exceeds
    /// `max_relative_std`.
    pub fn sample(
        &self,
        homography: &Homography,
        centre: Point,
        half: Point,
    ) -> Option<PatchSample> {
        let spread = self.inner_fraction;
        let taps = self.taps;
        let mut jpeg_stats = Accumulator::new();
        let mut raw_stats = Accumulator::new();
        for iy in 0..taps {
            let sy = (f64::from(iy) + 0.5) / f64::from(taps) - 0.5;
            for ix in 0..taps {
                let sx = (f64::from(ix) + 0.5) / f64::from(taps) - 0.5;
                let point = homography.map(Point::new(
                    centre.x + sx * 2.0 * half.x * spread,
                    centre.y + sy * 2.0 * half.y * spread,
                ))?;
                if let Some(rgb) = self.jpeg.image.sample_bilinear(point.x, point.y) {
                    jpeg_stats.push(rgb);
                }
                let (rx, ry) = self.alignment.jpeg_to_raw(
                    point.x,
                    point.y,
                    self.jpeg.image.width,
                    self.jpeg.image.height,
                    self.raw.width,
                    self.raw.height,
                );
                if let Some(rgb) = self.raw.sample_bilinear(rx, ry) {
                    raw_stats.push(rgb);
                }
            }
        }
        let (in_mean, in_std) = raw_stats.finish()?;
        let (out_mean, out_std) = jpeg_stats.finish()?;
        if relative_spread(&in_mean, &in_std) > self.max_relative_std
            || relative_spread(&out_mean, &out_std) > self.max_relative_std
        {
            return None;
        }
        Some(PatchSample {
            in_mean: checked(in_mean)?,
            in_std: checked_std(in_std)?,
            out_mean: checked(out_mean)?,
            out_std: checked_std(out_std)?,
        })
    }
}

struct Accumulator {
    count: usize,
    sum: [f64; 3],
    sum_of_squares: [f64; 3],
}

impl Accumulator {
    fn new() -> Self {
        Self {
            count: 0,
            sum: [0.0; 3],
            sum_of_squares: [0.0; 3],
        }
    }

    fn push(&mut self, rgb: [f32; 3]) {
        if !rgb.iter().all(|channel| channel.is_finite()) {
            return;
        }
        for (channel, value) in rgb.iter().enumerate() {
            let value = f64::from(*value);
            self.sum[channel] += value;
            self.sum_of_squares[channel] += value * value;
        }
        self.count += 1;
    }

    fn finish(self) -> Option<([f64; 3], [f64; 3])> {
        if self.count == 0 {
            return None;
        }
        let count = self.count as f64;
        let mut mean = [0.0; 3];
        let mut deviation = [0.0; 3];
        for channel in 0..3 {
            mean[channel] = self.sum[channel] / count;
            let variance = self.sum_of_squares[channel] / count - mean[channel] * mean[channel];
            deviation[channel] = variance.max(0.0).sqrt();
        }
        Some((mean, deviation))
    }
}

fn relative_spread(mean: &[f64; 3], deviation: &[f64; 3]) -> f64 {
    let mut worst = 0.0f64;
    for channel in 0..3 {
        let value = if mean[channel] > 1e-6 {
            deviation[channel] / mean[channel]
        } else {
            deviation[channel]
        };
        worst = worst.max(value);
    }
    worst
}

fn checked(rgb: [f64; 3]) -> Option<[f32; 3]> {
    let mut out = [0.0f32; 3];
    for channel in 0..3 {
        let value = rgb[channel];
        if !value.is_finite() || !(f64::from(MIN_RGB)..=f64::from(MAX_RGB)).contains(&value) {
            return None;
        }
        out[channel] = value as f32;
    }
    Some(out)
}

fn checked_std(rgb: [f64; 3]) -> Option<[f32; 3]> {
    let mut out = [0.0f32; 3];
    for channel in 0..3 {
        let value = rgb[channel];
        if !value.is_finite() || !(0.0..=f64::from(MAX_RGB)).contains(&value) {
            return None;
        }
        out[channel] = value as f32;
    }
    Some(out)
}
