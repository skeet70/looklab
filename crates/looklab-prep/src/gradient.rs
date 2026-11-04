// SPDX-License-Identifier: AGPL-3.0-or-later
//! Defocused tone-gradient detection, [docs/07 §3.2](../../../docs/07-contribution.md)
//! rung 4.
//!
//! The samples of a gradient session follow the ordinary preparation path; this
//! module supplies the verdict and the protocol's required RAW clip report.

use looklab_raw::DecodedRaw;
use serde::{Deserialize, Serialize};

use crate::error::PrepError;

const LUMA: [f64; 3] = [0.2126, 0.7152, 0.0722];

/// Working resolution the statistics are computed at.
const WORKING_PIXELS: u32 = 250_000;

/// Pixels read for the clip fraction, strided over the frame.
const CLIP_SAMPLES: u32 = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GradientParams {
    /// Box radius of the low-pass filter, in working pixels.
    pub blur_radius: u32,
    /// Bins of the luminance profile along the fitted gradient direction.
    pub profile_bins: u32,
    /// High-frequency energy over tone span below which the frame counts as
    /// defocused.
    pub max_detail_ratio: f64,
    /// Fraction of profile steps that must follow the fitted trend.
    pub min_monotone_fraction: f64,
    /// Luminance range, in stops, below which the frame is not a gradient.
    pub min_tone_span_stops: f64,
    /// RAW channel level at or above which a pixel is clipped.
    pub clip_level: f64,
    /// Clipped fraction of pixels a gradient session needs.
    pub min_clip_fraction: f64,
}

impl Default for GradientParams {
    fn default() -> Self {
        Self {
            blur_radius: 6,
            profile_bins: 64,
            max_detail_ratio: 0.02,
            min_monotone_fraction: 0.95,
            min_tone_span_stops: 4.0,
            clip_level: 0.98,
            min_clip_fraction: 0.001,
        }
    }
}

impl GradientParams {
    fn validate(&self) -> Result<(), PrepError> {
        if self.profile_bins < 4 || self.blur_radius == 0 {
            return Err(PrepError::InvalidParams {
                field: "gradient_profile",
            });
        }
        if !(0.0 < self.clip_level && self.clip_level <= 1.0) {
            return Err(PrepError::InvalidParams {
                field: "gradient_clip_level",
            });
        }
        for value in [
            self.max_detail_ratio,
            self.min_monotone_fraction,
            self.min_tone_span_stops,
            self.min_clip_fraction,
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(PrepError::InvalidParams {
                    field: "gradient_thresholds",
                });
            }
        }
        Ok(())
    }
}

/// The kind tag a caller records for the contribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientVerdict {
    Gradient,
    Other,
}

impl GradientVerdict {
    pub fn is_gradient(self) -> bool {
        self == Self::Gradient
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientReport {
    pub verdict: GradientVerdict,
    /// Mean absolute deviation from the low-pass luminance over tone span.
    pub detail_ratio: f64,
    /// Fraction of profile steps that follow the fitted direction.
    pub monotone_fraction: f64,
    /// Luminance range between the 1st and 99th percentiles, in stops.
    pub tone_span_stops: f64,
    /// Fraction of RAW pixels with any channel at or above `clip_level`.
    pub clip_fraction: f64,
    /// The protocol requires the gradient to clip in the RAW.
    pub raw_clipped: bool,
}

/// Decides whether `raw` is a defocused tone gradient and reports its clipping.
pub fn detect_gradient(
    raw: &DecodedRaw,
    params: &GradientParams,
) -> Result<GradientReport, PrepError> {
    params.validate()?;
    let pixels = raw
        .width
        .checked_mul(raw.height)
        .ok_or(PrepError::InvalidParams {
            field: "raw_dimensions",
        })?;
    if raw.rgb.len() < pixels * 3 {
        return Err(PrepError::InvalidParams {
            field: "raw_rgb_len",
        });
    }

    let clip_fraction = clip_fraction(raw, pixels, params)?;
    let (width, height, luma) = working_luma(raw, pixels)?;
    if width < 8 || height < 8 {
        return Err(PrepError::InvalidParams {
            field: "raw_dimensions",
        });
    }
    let low_pass = box_blur(&luma, width, height, params.blur_radius as usize);

    let span = percentile_span(&luma);
    let tone_span_stops = libm::log2(span.1 / span.0.max(1e-6));
    let mut detail = 0.0f64;
    for (value, smoothed) in luma.iter().zip(&low_pass) {
        detail += (value - smoothed).abs();
    }
    detail /= luma.len() as f64;
    let detail_ratio = if span.1 > span.0 {
        detail / (span.1 - span.0)
    } else {
        detail
    };

    let monotone_fraction = monotone_fraction(&low_pass, width, height, params.profile_bins);
    let verdict = if detail_ratio <= params.max_detail_ratio
        && monotone_fraction >= params.min_monotone_fraction
        && tone_span_stops >= params.min_tone_span_stops
    {
        GradientVerdict::Gradient
    } else {
        GradientVerdict::Other
    };
    Ok(GradientReport {
        verdict,
        detail_ratio,
        monotone_fraction,
        tone_span_stops,
        clip_fraction,
        raw_clipped: clip_fraction >= params.min_clip_fraction,
    })
}

fn clip_fraction(
    raw: &DecodedRaw,
    pixels: usize,
    params: &GradientParams,
) -> Result<f64, PrepError> {
    let step = (pixels / CLIP_SAMPLES as usize).max(1);
    let mut clipped = 0usize;
    let mut sampled = 0usize;
    let mut index = 0usize;
    while index < pixels {
        let base = index * 3;
        let channels = [
            f64::from(raw.rgb[base]),
            f64::from(raw.rgb[base + 1]),
            f64::from(raw.rgb[base + 2]),
        ];
        if channels.iter().any(|channel| *channel >= params.clip_level) {
            clipped += 1;
        }
        sampled += 1;
        index += step;
    }
    if sampled == 0 {
        return Err(PrepError::InvalidParams {
            field: "raw_dimensions",
        });
    }
    Ok(clipped as f64 / sampled as f64)
}

/// White-balanced luminance at a working resolution that keeps the aspect ratio.
fn working_luma(raw: &DecodedRaw, pixels: usize) -> Result<(usize, usize, Vec<f64>), PrepError> {
    let scale = (f64::from(WORKING_PIXELS) / pixels as f64).sqrt();
    let width = ((raw.width as f64 * scale).floor() as usize).max(1);
    let height = ((raw.height as f64 * scale).floor() as usize).max(1);
    let mut luma = Vec::with_capacity(width * height);
    for y in 0..height {
        let y0 = y * raw.height / height;
        let y1 = ((y + 1) * raw.height / height).max(y0 + 1);
        for x in 0..width {
            let x0 = x * raw.width / width;
            let x1 = ((x + 1) * raw.width / width).max(x0 + 1);
            let mut sum = 0.0;
            let mut count = 0.0;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let base = (sy * raw.width + sx) * 3;
                    let r = f64::from(raw.rgb[base]) * raw.as_shot_wb[0];
                    let g = f64::from(raw.rgb[base + 1]) * raw.as_shot_wb[1];
                    let b = f64::from(raw.rgb[base + 2]) * raw.as_shot_wb[2];
                    let value = r * LUMA[0] + g * LUMA[1] + b * LUMA[2];
                    if value.is_finite() {
                        sum += value;
                        count += 1.0;
                    }
                }
            }
            luma.push(if count > 0.0 { sum / count } else { 0.0 });
        }
    }
    Ok((width, height, luma))
}

fn box_blur(image: &[f64], width: usize, height: usize, radius: usize) -> Vec<f64> {
    let mut horizontal = vec![0.0f64; image.len()];
    for y in 0..height {
        let row = &image[y * width..(y + 1) * width];
        let mut sum = 0.0;
        let mut count = 0usize;
        for value in row.iter().take(radius.min(width)) {
            sum += value;
            count += 1;
        }
        for x in 0..width {
            horizontal[y * width + x] = sum / count as f64;
            let entering = x + radius;
            let leaving = x as isize - radius as isize;
            if let Some(value) = row.get(entering) {
                sum += value;
                count += 1;
            }
            if leaving >= 0
                && let Some(value) = row.get(leaving as usize)
            {
                sum -= value;
                count -= 1;
            }
        }
    }
    let mut out = vec![0.0f64; image.len()];
    for x in 0..width {
        let mut sum = 0.0;
        let mut count = 0usize;
        for y in 0..radius.min(height) {
            sum += horizontal[y * width + x];
            count += 1;
        }
        for y in 0..height {
            out[y * width + x] = sum / count as f64;
            let entering = y + radius;
            let leaving = y as isize - radius as isize;
            if entering < height {
                sum += horizontal[entering * width + x];
                count += 1;
            }
            if leaving >= 0 {
                sum -= horizontal[leaving as usize * width + x];
                count -= 1;
            }
        }
    }
    out
}

/// 1st and 99th percentiles by rank over a strided subsample.
fn percentile_span(luma: &[f64]) -> (f64, f64) {
    let step = (luma.len() / 20_000).max(1);
    let mut values: Vec<f64> = luma
        .iter()
        .step_by(step)
        .copied()
        .filter(|value| value.is_finite() && *value > 0.0)
        .collect();
    if values.is_empty() {
        return (0.0, 0.0);
    }
    values.sort_by(f64::total_cmp);
    let low = values[values.len() / 100];
    let high = values[values.len() * 99 / 100];
    (low, high.max(low))
}

/// Bins the low-pass luminance along the principal axis of a least-squares
/// plane fit.
fn monotone_fraction(low_pass: &[f64], width: usize, height: usize, bins: u32) -> f64 {
    let mut count = 0.0;
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut syy = 0.0;
    let mut sl = 0.0;
    let mut slx = 0.0;
    let mut sly = 0.0;
    for y in 0..height {
        let ny = normalized(y, height);
        for x in 0..width {
            let nx = normalized(x, width);
            let value = low_pass[y * width + x];
            count += 1.0;
            sx += nx;
            sy += ny;
            sxx += nx * nx;
            sxy += nx * ny;
            syy += ny * ny;
            sl += value;
            slx += value * nx;
            sly += value * ny;
        }
    }
    if count < 16.0 {
        return 0.0;
    }
    // Normal equations of `L = a + b x + c y`, solved by Cramer's rule.
    let m = [[count, sx, sy], [sx, sxx, sxy], [sy, sxy, syy]];
    let rhs = [sl, slx, sly];
    let determinant = determinant3(&m);
    if !determinant.is_finite() || determinant.abs() <= 1e-12 {
        return 0.0;
    }
    let b = determinant3(&[
        [rhs[0], m[0][1], m[0][2]],
        [rhs[1], m[1][1], m[1][2]],
        [rhs[2], m[2][1], m[2][2]],
    ]) / determinant;
    let c = determinant3(&[
        [m[0][0], rhs[0], m[0][2]],
        [m[1][0], rhs[1], m[1][2]],
        [m[2][0], rhs[2], m[2][2]],
    ]) / determinant;
    let magnitude = libm::hypot(b, c);
    if !magnitude.is_finite() || magnitude <= 1e-9 {
        return 0.0;
    }
    let direction = (b / magnitude, c / magnitude);

    let bin_count = bins as usize;
    let mut totals = vec![0.0f64; bin_count];
    let mut counts = vec![0.0f64; bin_count];
    for y in 0..height {
        let ny = normalized(y, height);
        for x in 0..width {
            let nx = normalized(x, width);
            let projection = nx * direction.0 + ny * direction.1;
            let bin = ((projection.clamp(-1.0, 1.0) + 1.0) * 0.5 * bin_count as f64)
                .floor()
                .clamp(0.0, (bin_count - 1) as f64) as usize;
            totals[bin] += low_pass[y * width + x];
            counts[bin] += 1.0;
        }
    }
    let profile: Vec<f64> = totals
        .iter()
        .zip(&counts)
        .filter(|(_, count)| **count > 0.0)
        .map(|(total, count)| total / count)
        .collect();
    if profile.len() < 2 {
        return 0.0;
    }
    let trend = profile[profile.len() - 1] - profile[0];
    if !trend.is_finite() || trend.abs() <= 1e-9 {
        return 0.0;
    }
    let sign = if trend > 0.0 { 1.0 } else { -1.0 };
    let mut following = 0usize;
    for pair in profile.windows(2) {
        let step = (pair[1] - pair[0]) * sign;
        if step >= 0.0 {
            following += 1;
        }
    }
    following as f64 / (profile.len() - 1) as f64
}

fn normalized(index: usize, extent: usize) -> f64 {
    if extent <= 1 {
        0.0
    } else {
        2.0 * index as f64 / (extent - 1) as f64 - 1.0
    }
}

fn determinant3(m: &[[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

#[cfg(test)]
mod tests {
    use super::{GradientParams, GradientVerdict, detect_gradient};
    use looklab_raw::{DecodedRaw, Orientation};

    fn raw(width: usize, height: usize, pixel: impl Fn(f64, f64) -> [f32; 3]) -> DecodedRaw {
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                let value = pixel(
                    (x as f64 + 0.5) / width as f64,
                    (y as f64 + 0.5) / height as f64,
                );
                rgb.extend_from_slice(&value);
            }
        }
        DecodedRaw {
            width,
            height,
            rgb,
            as_shot_wb: [1.0, 1.0, 1.0],
            orientation: Orientation::Normal,
            make: "FUJIFILM".to_owned(),
            model: "X-S10".to_owned(),
            color_matrices: None,
        }
    }

    #[test]
    fn a_smooth_clipped_ramp_is_a_gradient() {
        let image = raw(200, 150, |u, _| {
            let level = 1.2 - 1.19 * u;
            [level as f32, level as f32, level as f32]
        });
        let report = detect_gradient(&image, &GradientParams::default()).expect("detects");
        assert!(report.verdict.is_gradient());
        assert_eq!(report.verdict, GradientVerdict::Gradient);
        assert!(report.raw_clipped);
        assert!(report.clip_fraction > 0.01);
    }

    #[test]
    fn a_textured_ramp_is_not_a_gradient() {
        let image = raw(200, 150, |u, v| {
            let base = 1.2 - 1.19 * u;
            let texture = if (u * 40.0).floor() % 2.0 == 0.0 {
                0.2
            } else {
                -0.2
            };
            let value = (base + texture * (1.0 - v * 0.2)).max(0.0);
            [value as f32, value as f32, value as f32]
        });
        let report = detect_gradient(&image, &GradientParams::default()).expect("detects");
        assert_eq!(report.verdict, GradientVerdict::Other);
    }

    #[test]
    fn an_unclipped_ramp_reports_no_clipping() {
        let image = raw(200, 150, |u, _| {
            let level = 0.8 - 0.79 * u;
            [level as f32, level as f32, level as f32]
        });
        let report = detect_gradient(&image, &GradientParams::default()).expect("detects");
        assert!(!report.raw_clipped);
        assert_eq!(report.clip_fraction, 0.0);
        assert!(report.tone_span_stops > 4.0);
    }
}
