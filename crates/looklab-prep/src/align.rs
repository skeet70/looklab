// SPDX-License-Identifier: AGPL-3.0-or-later
//! Similarity, radial-distortion and radial-gain alignment of a JPEG against
//! its RAW.
//!
//! Multi-scale phase correlation and a Fourier-Mellin estimate seed a
//! Levenberg-Marquardt refinement on normalized cross-correlation. The radial
//! gain `g(r) = 1 + a1 r^2 + a2 r^4 + a3 r^6` is fitted jointly with a
//! temporary monotone 12-knot tone map on luminance
//! ([docs/03 §2.1](../../../docs/03-data-model.md)).

use serde::{Deserialize, Serialize};

use crate::error::PrepError;
use crate::image::Image3;
use crate::jpeg::DecodedJpeg;

const LUMINANCE: [f64; 3] = [0.2126, 0.7152, 0.0722];
const TONE_KNOTS: usize = 12;

/// Parameters that affect alignment and are part of the hashed prep settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlignParams {
    pub working_pixels: u32,
    pub min_central_correlation: f64,
    pub max_iterations: usize,
    pub min_overlap_fraction: f64,
}

impl Default for AlignParams {
    fn default() -> Self {
        Self {
            working_pixels: 1_500_000,
            min_central_correlation: 0.98,
            max_iterations: 30,
            min_overlap_fraction: 0.2,
        }
    }
}

/// The fitted JPEG-to-RAW mapping and shading correction.
///
/// `Alignment::gain_at` takes the radius of a RAW pixel in normalized
/// coordinates: `hypot((x - cx) / cx, (y - cy) / cy)` with
/// `(cx, cy) = ((w - 1) / 2, (h - 1) / 2)` and `(x, y)` in pixel units.
/// Corners are at about `sqrt(2)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Alignment {
    pub scale: f64,
    pub rotation_rad: f64,
    pub tx: f64,
    pub ty: f64,
    pub k1: f64,
    pub k2: f64,
    pub gain: [f64; 3],
    pub central_correlation: f64,
    pub residual_px: f64,
}

impl Alignment {
    fn linear(&self, u: f64, v: f64) -> (f64, f64) {
        let (sin, cos) = (libm::sin(self.rotation_rad), libm::cos(self.rotation_rad));
        (
            self.scale * (cos * u - sin * v) + self.tx,
            self.scale * (sin * u + cos * v) + self.ty,
        )
    }

    fn distort(&self, a: f64, b: f64) -> (f64, f64) {
        let r2 = a * a + b * b;
        let factor = 1.0 + self.k1 * r2 + self.k2 * r2 * r2;
        (a * factor, b * factor)
    }

    fn undistort(&self, a: f64, b: f64) -> (f64, f64) {
        let target = libm::hypot(a, b);
        let mut radius = target;
        for _ in 0..10 {
            let r2 = radius * radius;
            let value = radius + self.k1 * radius * r2 + self.k2 * radius * r2 * r2 - target;
            let slope = 1.0 + 3.0 * self.k1 * r2 + 5.0 * self.k2 * r2 * r2;
            if slope.abs() < 1e-12 {
                break;
            }
            radius -= value / slope;
            if radius < 0.0 {
                radius = 0.0;
            }
            if value.abs() < 1e-12 {
                break;
            }
        }
        if target == 0.0 {
            (0.0, 0.0)
        } else {
            let ratio = radius / target;
            (a * ratio, b * ratio)
        }
    }

    pub fn jpeg_to_raw(
        &self,
        x: f64,
        y: f64,
        jpeg_w: u32,
        jpeg_h: u32,
        raw_w: u32,
        raw_h: u32,
    ) -> (f64, f64) {
        let (jpeg_cx, jpeg_cy) = centre(jpeg_w, jpeg_h);
        let (u, v) = (ratio(x, jpeg_cx, jpeg_w), ratio(y, jpeg_cy, jpeg_h));
        let (a, b) = self.linear(u, v);
        let (ad, bd) = self.distort(a, b);
        to_pixels(ad, bd, raw_w, raw_h)
    }

    pub fn raw_to_jpeg(
        &self,
        x: f64,
        y: f64,
        raw_w: u32,
        raw_h: u32,
        jpeg_w: u32,
        jpeg_h: u32,
    ) -> (f64, f64) {
        let (raw_cx, raw_cy) = centre(raw_w, raw_h);
        let (u, v) = (ratio(x, raw_cx, raw_w), ratio(y, raw_cy, raw_h));
        let (a, b) = self.undistort(u, v);
        let (qx, qy) = (a - self.tx, b - self.ty);
        let (sin, cos) = (libm::sin(self.rotation_rad), libm::cos(self.rotation_rad));
        let inverse_scale = if self.scale.abs() > 1e-12 {
            1.0 / self.scale
        } else {
            0.0
        };
        let uj = inverse_scale * (cos * qx + sin * qy);
        let vj = inverse_scale * (-sin * qx + cos * qy);
        to_pixels(uj, vj, jpeg_w, jpeg_h)
    }

    pub fn gain_at(&self, r_norm: f64) -> f64 {
        let r2 = r_norm * r_norm;
        1.0 + self.gain[0] * r2 + self.gain[1] * r2 * r2 + self.gain[2] * r2 * r2 * r2
    }
}

fn centre(width: u32, height: u32) -> (f64, f64) {
    let cx = (f64::from(width) - 1.0) * 0.5;
    let cy = (f64::from(height) - 1.0) * 0.5;
    (cx, cy)
}

fn ratio(value: f64, centre: f64, extent: u32) -> f64 {
    if extent > 1 {
        (value - centre) / centre
    } else {
        0.0
    }
}

fn to_pixels(u: f64, v: f64, width: u32, height: u32) -> (f64, f64) {
    let (cx, cy) = centre(width, height);
    (cx + u * cx, cy + v * cy)
}

pub fn align(
    raw_wb_linear: &Image3,
    jpeg: &DecodedJpeg,
    params: &AlignParams,
) -> Result<Alignment, PrepError> {
    if raw_wb_linear.data.is_empty() || jpeg.image.data.is_empty() {
        return Err(PrepError::ImageEmpty);
    }
    let estimate_size = 512.min(next_power_of_two(
        raw_wb_linear.width.max(raw_wb_linear.height) as usize,
    ));
    let estimate_size = estimate_size.max(64);
    let raw_grid = normalized_luma(raw_wb_linear, estimate_size, estimate_size);
    let jpeg_grid = normalized_luma(&jpeg.image, estimate_size, estimate_size);
    if variance(&raw_grid) < 1e-12 || variance(&jpeg_grid) < 1e-12 {
        return Err(PrepError::AlignNoOverlap);
    }

    let initial = seed_alignment(raw_wb_linear, jpeg, &raw_grid, &jpeg_grid, estimate_size);

    let grid = 192;
    let jpeg_luma = normalized_luma(&jpeg.image, grid, grid);
    let mut alignment = initial;
    alignment.gain = fit_gain(&gain_samples(raw_wb_linear, jpeg, &alignment));
    for _ in 0..2 {
        refine_geometry(raw_wb_linear, &jpeg_luma, &mut alignment, params)?;
        alignment.gain = fit_gain(&gain_samples(raw_wb_linear, jpeg, &alignment));
    }
    refine_geometry(raw_wb_linear, &jpeg_luma, &mut alignment, params)?;
    alignment.gain = fit_gain(&gain_samples(raw_wb_linear, jpeg, &alignment));

    let (u, v) = normalized_coords(grid, grid);
    let (central_correlation, central_count) =
        correlation_at(raw_wb_linear, &jpeg_luma, &alignment, &u, &v)
            .ok_or(PrepError::AlignNoOverlap)?;
    let total = u.len();
    if central_count == 0 || (central_count as f64) < params.min_overlap_fraction * total as f64 {
        return Err(PrepError::AlignNoOverlap);
    }
    if central_correlation < params.min_central_correlation {
        return Err(PrepError::AlignLowCorrelation {
            correlation: central_correlation,
            minimum: params.min_central_correlation,
        });
    }

    let samples = gain_samples(raw_wb_linear, jpeg, &alignment);
    alignment.gain = fit_gain(&samples);
    alignment.central_correlation = central_correlation;
    alignment.residual_px = block_residual(raw_wb_linear, &jpeg.image, &alignment);
    Ok(alignment)
}

fn refine_geometry(
    raw: &Image3,
    jpeg_luma: &[f64],
    alignment: &mut Alignment,
    params: &AlignParams,
) -> Result<(), PrepError> {
    let grid = 192;
    let (u, v) = normalized_coords(grid, grid);
    let mask = base_mask(raw, alignment, &u, &v);
    let mut cost = evaluate_cost(raw, jpeg_luma, alignment, &u, &v, &mask)
        .map(|(cost, _)| cost)
        .ok_or(PrepError::AlignNoOverlap)?;
    let mut lambda = 1e-3_f64;
    for _ in 0..params.max_iterations {
        let base = evaluate_residual(raw, jpeg_luma, alignment, &u, &v, &mask)
            .ok_or(PrepError::AlignNoOverlap)?;
        let mut jacobian = vec![vec![0.0_f64; base.len()]; 6];
        for (parameter, row) in jacobian.iter_mut().enumerate() {
            let step = parameter_step(parameter, *alignment);
            let mut plus = *alignment;
            let mut minus = *alignment;
            add_parameter(&mut plus, parameter, step);
            add_parameter(&mut minus, parameter, -step);
            let Some(e_plus) = evaluate_residual(raw, jpeg_luma, &plus, &u, &v, &mask) else {
                continue;
            };
            let Some(e_minus) = evaluate_residual(raw, jpeg_luma, &minus, &u, &v, &mask) else {
                continue;
            };
            let scale = 0.5 / step;
            for (slot, (plus, minus)) in row.iter_mut().zip(e_plus.iter().zip(&e_minus)) {
                *slot = (plus - minus) * scale;
            }
        }
        let mut normal = [[0.0_f64; 6]; 6];
        let mut gradient = [0.0_f64; 6];
        for (parameter, row) in jacobian.iter().enumerate() {
            for (other, column) in jacobian.iter().enumerate() {
                normal[parameter][other] = row.iter().zip(column).map(|(a, b)| a * b).sum();
            }
            gradient[parameter] = row.iter().zip(&base).map(|(a, b)| a * b).sum();
        }
        for (index, row) in normal.iter_mut().enumerate() {
            if row[index] <= 0.0 {
                row[index] = 1e-12;
            }
            row[index] += lambda * row[index];
        }
        let mut rhs = [0.0_f64; 6];
        for (index, value) in rhs.iter_mut().enumerate() {
            *value = -gradient[index];
        }
        let Some(delta) = solve6(normal, rhs) else {
            break;
        };
        let mut trial = *alignment;
        for (parameter, value) in delta.iter().enumerate() {
            add_parameter(&mut trial, parameter, *value);
        }
        if let Some((trial_cost, _)) = evaluate_cost(raw, jpeg_luma, &trial, &u, &v, &mask) {
            if trial_cost < cost {
                let improvement = cost - trial_cost;
                *alignment = trial;
                cost = trial_cost;
                lambda = (lambda * 0.3).max(1e-10);
                if improvement < 1e-12 * cost.max(1.0) {
                    break;
                }
            } else {
                lambda *= 10.0;
                if lambda > 1e8 {
                    break;
                }
            }
        } else {
            lambda *= 10.0;
            if lambda > 1e8 {
                break;
            }
        }
    }
    Ok(())
}

fn next_power_of_two(value: usize) -> usize {
    value.max(1).next_power_of_two()
}

fn normalized_luma(image: &Image3, width: usize, height: usize) -> Vec<f64> {
    let (u, v) = normalized_coords(width, height);
    let mut out = Vec::with_capacity(u.len());
    let (cx, cy) = centre(image.width, image.height);
    for (u, v) in u.into_iter().zip(v) {
        let x = (cx + u * cx).clamp(0.0, f64::from(image.width - 1));
        let y = (cy + v * cy).clamp(0.0, f64::from(image.height - 1));
        out.push(image.sample_bilinear(x, y).map(luminance).unwrap_or(0.0));
    }
    out
}

fn normalized_coords(width: usize, height: usize) -> (Vec<f64>, Vec<f64>) {
    let mut us = Vec::with_capacity(width * height);
    let mut vs = Vec::with_capacity(width * height);
    for y in 0..height {
        let v = if height > 1 {
            2.0 * y as f64 / (height - 1) as f64 - 1.0
        } else {
            0.0
        };
        for x in 0..width {
            us.push(if width > 1 {
                2.0 * x as f64 / (width - 1) as f64 - 1.0
            } else {
                0.0
            });
            vs.push(v);
        }
    }
    (us, vs)
}

fn luminance(rgb: [f32; 3]) -> f64 {
    let mut value = 0.0;
    for channel in 0..3 {
        value += LUMINANCE[channel] * f64::from(rgb[channel]);
    }
    value
}

fn variance(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = mean(values);
    values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / values.len() as f64
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn seed_alignment(
    raw: &Image3,
    jpeg: &DecodedJpeg,
    raw_grid: &[f64],
    jpeg_grid: &[f64],
    size: usize,
) -> Alignment {
    let (mut scale, mut rotation) = log_polar_similarity(jpeg_grid, raw_grid, size);
    if !scale.is_finite() || scale <= 0.0 {
        scale = 1.0;
    }
    if !rotation.is_finite() {
        rotation = 0.0;
    }
    rotation = rotation.clamp(-0.35, 0.35);
    scale = scale.clamp(0.5, 2.0);

    let candidates = [
        (1.0_f64, 0.0_f64),
        (scale, rotation),
        (1.0 / scale, rotation),
        (scale, -rotation),
        (1.0 / scale, -rotation),
        (1.0, rotation),
        (1.0, -rotation),
        (scale, 0.0),
        (1.0 / scale, 0.0),
    ];
    let coarse = 64;
    let (u, v) = normalized_coords(coarse, coarse);
    let jpeg_coarse = normalized_luma(&jpeg.image, coarse, coarse);
    let raw_gray = normalized_luma(raw, coarse, coarse);
    let mut best = Alignment {
        scale: 1.0,
        rotation_rad: 0.0,
        tx: 0.0,
        ty: 0.0,
        k1: 0.0,
        k2: 0.0,
        gain: [0.0; 3],
        central_correlation: f64::NEG_INFINITY,
        residual_px: 0.0,
    };
    for (candidate_scale, candidate_rotation) in candidates {
        let warped = warped_grid(&jpeg.image, candidate_scale, candidate_rotation, coarse);
        let (dx, dy, _) = phase_translation_multiscale(&raw_gray, &warped, coarse);
        let mut candidate = Alignment {
            scale: candidate_scale,
            rotation_rad: candidate_rotation,
            tx: 2.0 * dx / (coarse as f64 - 1.0),
            ty: 2.0 * dy / (coarse as f64 - 1.0),
            k1: 0.0,
            k2: 0.0,
            gain: [0.0; 3],
            central_correlation: f64::NEG_INFINITY,
            residual_px: 0.0,
        };
        if let Some((correlation, count)) = correlation_at(raw, &jpeg_coarse, &candidate, &u, &v)
            && count > 0
            && correlation > best.central_correlation
        {
            candidate.central_correlation = correlation;
            best = candidate;
        }
    }
    best
}

fn warped_grid(jpeg: &Image3, scale: f64, rotation: f64, size: usize) -> Vec<f64> {
    let (u, v) = normalized_coords(size, size);
    let (sin, cos) = (libm::sin(rotation), libm::cos(rotation));
    let inverse_scale = if scale.abs() > 1e-12 {
        1.0 / scale
    } else {
        0.0
    };
    let (cx, cy) = centre(jpeg.width, jpeg.height);
    let mut out = Vec::with_capacity(u.len());
    for (u, v) in u.into_iter().zip(v) {
        let uj = inverse_scale * (cos * u + sin * v);
        let vj = inverse_scale * (-sin * u + cos * v);
        let x = cx + uj * cx;
        let y = cy + vj * cy;
        out.push(jpeg.sample_bilinear(x, y).map(luminance).unwrap_or(0.0));
    }
    out
}

fn base_mask(raw: &Image3, alignment: &Alignment, u: &[f64], v: &[f64]) -> Vec<bool> {
    let (max_x, max_y) = (f64::from(raw.width - 1), f64::from(raw.height - 1));
    u.iter()
        .zip(v)
        .map(|(u, v)| {
            let (a, b) = alignment.linear(*u, *v);
            let (ad, bd) = alignment.distort(a, b);
            let (x, y) = to_pixels(ad, bd, raw.width, raw.height);
            x > 2.0 && y > 2.0 && x < max_x - 2.0 && y < max_y - 2.0
        })
        .collect()
}

fn evaluate_cost(
    raw: &Image3,
    jpeg_luma: &[f64],
    alignment: &Alignment,
    u: &[f64],
    v: &[f64],
    mask: &[bool],
) -> Option<(f64, f64)> {
    let residual = evaluate_residual(raw, jpeg_luma, alignment, u, v, mask)?;
    let count = residual.len();
    if count < 16 {
        return None;
    }
    let cost = residual.iter().map(|value| value * value).sum::<f64>();
    Some((cost, count as f64))
}

fn evaluate_residual(
    raw: &Image3,
    jpeg_luma: &[f64],
    alignment: &Alignment,
    u: &[f64],
    v: &[f64],
    mask: &[bool],
) -> Option<Vec<f64>> {
    let (cx, cy) = centre(raw.width, raw.height);
    let (max_x, max_y) = (f64::from(raw.width - 1), f64::from(raw.height - 1));
    let mut raw_values = Vec::with_capacity(mask.len());
    let mut jpeg_values = Vec::with_capacity(mask.len());
    let mut indices = Vec::with_capacity(mask.len());
    for (index, selected) in mask.iter().enumerate() {
        if !selected {
            continue;
        }
        let (a, b) = alignment.linear(u[index], v[index]);
        let (ad, bd) = alignment.distort(a, b);
        let x = cx + ad * cx;
        let y = cy + bd * cy;
        if x < 0.0 || y < 0.0 || x > max_x || y > max_y {
            return None;
        }
        let value = raw.sample_bilinear(x, y).map(luminance)?;
        let radius = libm::hypot(ad, bd);
        raw_values.push(value * alignment.gain_at(radius));
        jpeg_values.push(
            jpeg_luma
                .get(index)
                .copied()
                .expect("jpeg grid has the base mask's length"),
        );
        indices.push(index);
    }
    if raw_values.len() < 16 {
        return None;
    }
    let mean_raw = mean(&raw_values);
    let mean_jpeg = mean(&jpeg_values);
    let mut covariance = 0.0;
    let mut variance_raw = 0.0;
    for index in 0..raw_values.len() {
        let dr = raw_values[index] - mean_raw;
        let dj = jpeg_values[index] - mean_jpeg;
        covariance += dr * dj;
        variance_raw += dr * dr;
    }
    if variance_raw < 1e-18 {
        return None;
    }
    let gain = covariance / variance_raw;
    let mut residual = Vec::with_capacity(raw_values.len());
    for index in 0..raw_values.len() {
        let modelled = mean_jpeg + gain * (raw_values[index] - mean_raw);
        residual.push(modelled - jpeg_values[index]);
    }
    Some(residual)
}

fn correlation_at(
    raw: &Image3,
    jpeg_luma: &[f64],
    alignment: &Alignment,
    u: &[f64],
    v: &[f64],
) -> Option<(f64, usize)> {
    let (cx, cy) = centre(raw.width, raw.height);
    let (max_x, max_y) = (f64::from(raw.width - 1), f64::from(raw.height - 1));
    let mut raw_values = Vec::new();
    let mut jpeg_values = Vec::new();
    for index in 0..u.len() {
        if u[index].abs() > 0.5 || v[index].abs() > 0.5 {
            continue;
        }
        let (a, b) = alignment.linear(u[index], v[index]);
        let (ad, bd) = alignment.distort(a, b);
        let x = cx + ad * cx;
        let y = cy + bd * cy;
        if x < 0.0 || y < 0.0 || x > max_x || y > max_y {
            continue;
        }
        let Some(value) = raw.sample_bilinear(x, y).map(luminance) else {
            continue;
        };
        raw_values.push(value);
        jpeg_values.push(jpeg_luma[index]);
    }
    let count = raw_values.len();
    if count < 16 {
        return None;
    }
    let mean_raw = mean(&raw_values);
    let mean_jpeg = mean(&jpeg_values);
    let mut covariance = 0.0;
    let mut var_raw = 0.0;
    let mut var_jpeg = 0.0;
    for index in 0..count {
        let dr = raw_values[index] - mean_raw;
        let dj = jpeg_values[index] - mean_jpeg;
        covariance += dr * dj;
        var_raw += dr * dr;
        var_jpeg += dj * dj;
    }
    if var_raw <= 0.0 || var_jpeg <= 0.0 {
        return None;
    }
    Some((covariance / (var_raw * var_jpeg).sqrt(), count))
}

fn parameter_step(parameter: usize, alignment: Alignment) -> f64 {
    let base: f64 = match parameter {
        0 => 1e-6,
        1 => 1e-6,
        2 => 1e-5,
        3 => 1e-5,
        4 => 1e-4,
        _ => 1e-4,
    };
    base.max(1e-9 * add_scale(parameter, alignment))
}

fn add_scale(parameter: usize, alignment: Alignment) -> f64 {
    match parameter {
        0 => alignment.scale.abs(),
        1 => alignment.rotation_rad.abs(),
        2 | 3 => 1.0,
        _ => 1.0,
    }
}

fn add_parameter(alignment: &mut Alignment, parameter: usize, delta: f64) {
    match parameter {
        0 => alignment.scale += delta,
        1 => alignment.rotation_rad += delta,
        2 => alignment.tx += delta,
        3 => alignment.ty += delta,
        4 => alignment.k1 += delta,
        _ => alignment.k2 += delta,
    }
}

fn solve6(mut a: [[f64; 6]; 6], mut b: [f64; 6]) -> Option<[f64; 6]> {
    for column in 0..6 {
        let mut pivot = column;
        for row in (column + 1)..6 {
            if a[row][column].abs() > a[pivot][column].abs() {
                pivot = row;
            }
        }
        if a[pivot][column].abs() < 1e-15 {
            return None;
        }
        a.swap(column, pivot);
        b.swap(column, pivot);
        let denominator = a[column][column];
        let pivot_row = a[column];
        for row in (column + 1)..6 {
            let factor = a[row][column] / denominator;
            for (index, value) in a[row].iter_mut().enumerate().skip(column) {
                *value -= factor * pivot_row[index];
            }
            b[row] -= factor * b[column];
        }
    }
    let mut out = [0.0_f64; 6];
    for row in (0..6).rev() {
        let mut sum = b[row];
        for column in (row + 1)..6 {
            sum -= a[row][column] * out[column];
        }
        out[row] = sum / a[row][row];
    }
    Some(out)
}

fn downsample_grid(values: &[f64], size: usize, target: usize) -> Vec<f64> {
    if target == size {
        return values.to_vec();
    }
    let factor = size / target;
    let mut out = vec![0.0; target * target];
    for y in 0..target {
        for x in 0..target {
            let mut sum = 0.0;
            for dy in 0..factor {
                for dx in 0..factor {
                    sum += values[(y * factor + dy) * size + x * factor + dx];
                }
            }
            out[y * target + x] = sum / (factor * factor) as f64;
        }
    }
    out
}

fn phase_translation_multiscale(a: &[f64], b: &[f64], size: usize) -> (f64, f64, f64) {
    let mut prediction = (0.0_f64, 0.0_f64);
    let mut quality = 0.0;
    for target in [size / 4, size / 2, size] {
        if target < 16 {
            continue;
        }
        let aa = downsample_grid(a, size, target);
        let bb = downsample_grid(b, size, target);
        let (dx, dy, q) = phase_correlation(&aa, &bb, target, target);
        let scale = size as f64 / target as f64;
        let scaled = (dx * scale, dy * scale);
        let close =
            (scaled.0 - prediction.0).abs() + (scaled.1 - prediction.1).abs() <= 3.0 * scale;
        if target == size / 4 || close {
            prediction = scaled;
        }
        quality = q;
    }
    (prediction.0, prediction.1, quality)
}

fn phase_correlation(a: &[f64], b: &[f64], width: usize, height: usize) -> (f64, f64, f64) {
    let count = width * height;
    if count == 0 || a.len() != count || b.len() != count {
        return (0.0, 0.0, 0.0);
    }
    let window = hann2(width, height);
    let mean_a = mean(a);
    let mean_b = mean(b);
    let mut re_a: Vec<f64> = a
        .iter()
        .zip(&window)
        .map(|(value, window)| (value - mean_a) * window)
        .collect();
    let mut im_a = vec![0.0; count];
    let mut re_b: Vec<f64> = b
        .iter()
        .zip(&window)
        .map(|(value, window)| (value - mean_b) * window)
        .collect();
    let mut im_b = vec![0.0; count];
    fft_2d(&mut re_a, &mut im_a, width, height, false);
    fft_2d(&mut re_b, &mut im_b, width, height, false);
    for index in 0..count {
        let (ar, ai) = (re_a[index], im_a[index]);
        let (br, bi) = (re_b[index], -im_b[index]);
        let (r, i) = (ar * br - ai * bi, ar * bi + ai * br);
        let magnitude = libm::hypot(r, i);
        let norm = if magnitude > 1e-12 {
            1.0 / magnitude
        } else {
            0.0
        };
        re_a[index] = r * norm;
        im_a[index] = i * norm;
    }
    fft_2d(&mut re_a, &mut im_a, width, height, true);

    let mut peak = 0usize;
    let mut peak_value = f64::NEG_INFINITY;
    for (index, value) in re_a.iter().enumerate() {
        if *value > peak_value {
            peak_value = *value;
            peak = index;
        }
    }
    let px = peak % width;
    let py = peak / width;
    let (dx, dy) = subpixel(re_a.as_slice(), width, height, px, py);
    let mut shift_x = px as f64 + dx;
    let mut shift_y = py as f64 + dy;
    if shift_x > width as f64 * 0.5 {
        shift_x -= width as f64;
    }
    if shift_y > height as f64 * 0.5 {
        shift_y -= height as f64;
    }
    (shift_x, shift_y, peak_value)
}

fn subpixel(values: &[f64], width: usize, height: usize, x: usize, y: usize) -> (f64, f64) {
    let at = |xx: isize, yy: isize| -> f64 {
        let xx = xx.rem_euclid(width as isize) as usize;
        let yy = yy.rem_euclid(height as isize) as usize;
        values[yy * width + xx]
    };
    let xm = at(x as isize - 1, y as isize);
    let xc = at(x as isize, y as isize);
    let xp = at(x as isize + 1, y as isize);
    let ym = at(x as isize, y as isize - 1);
    let yp = at(x as isize, y as isize + 1);
    let denominator_x = xm - 2.0 * xc + xp;
    let denominator_y = ym - 2.0 * xc + yp;
    let dx = if denominator_x.abs() > 1e-12 {
        (0.5 * (xm - xp) / denominator_x).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let dy = if denominator_y.abs() > 1e-12 {
        (0.5 * (ym - yp) / denominator_y).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    (dx, dy)
}

fn hann2(width: usize, height: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let wy = if height > 1 {
            0.5 - 0.5 * libm::cos(2.0 * std::f64::consts::PI * y as f64 / (height - 1) as f64)
        } else {
            1.0
        };
        for x in 0..width {
            let wx = if width > 1 {
                0.5 - 0.5 * libm::cos(2.0 * std::f64::consts::PI * x as f64 / (width - 1) as f64)
            } else {
                1.0
            };
            out.push(wx * wy);
        }
    }
    out
}

fn fft_2d(re: &mut [f64], im: &mut [f64], width: usize, height: usize, inverse: bool) {
    let mut row_re = vec![0.0; width];
    let mut row_im = vec![0.0; width];
    for y in 0..height {
        let start = y * width;
        row_re.copy_from_slice(&re[start..start + width]);
        row_im.copy_from_slice(&im[start..start + width]);
        fft_1d(&mut row_re, &mut row_im, inverse);
        re[start..start + width].copy_from_slice(&row_re);
        im[start..start + width].copy_from_slice(&row_im);
    }
    let mut col_re = vec![0.0; height];
    let mut col_im = vec![0.0; height];
    for x in 0..width {
        for y in 0..height {
            col_re[y] = re[y * width + x];
            col_im[y] = im[y * width + x];
        }
        fft_1d(&mut col_re, &mut col_im, inverse);
        for y in 0..height {
            re[y * width + x] = col_re[y];
            im[y * width + x] = col_im[y];
        }
    }
}

fn fft_1d(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    if n <= 1 {
        return;
    }
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut length = 2;
    while length <= n {
        let angle = if inverse {
            2.0 * std::f64::consts::PI / length as f64
        } else {
            -2.0 * std::f64::consts::PI / length as f64
        };
        let wlen_re = libm::cos(angle);
        let wlen_im = libm::sin(angle);
        let half = length / 2;
        let mut start = 0;
        while start < n {
            let mut w_re = 1.0;
            let mut w_im = 0.0;
            for offset in 0..half {
                let even = start + offset;
                let odd = even + half;
                let tre = w_re * re[odd] - w_im * im[odd];
                let tim = w_re * im[odd] + w_im * re[odd];
                re[odd] = re[even] - tre;
                im[odd] = im[even] - tim;
                re[even] += tre;
                im[even] += tim;
                let next_re = w_re * wlen_re - w_im * wlen_im;
                w_im = w_re * wlen_im + w_im * wlen_re;
                w_re = next_re;
            }
            start += length;
        }
        length <<= 1;
    }
    if inverse {
        let scale = 1.0 / n as f64;
        for value in re.iter_mut() {
            *value *= scale;
        }
        for value in im.iter_mut() {
            *value *= scale;
        }
    }
}

fn log_polar_similarity(jpeg_grid: &[f64], raw_grid: &[f64], size: usize) -> (f64, f64) {
    let crop = size.min(256).next_power_of_two().min(size);
    if crop < 32 {
        return (1.0, 0.0);
    }
    let start = (size - crop) / 2;
    let mut jpeg_crop = vec![0.0; crop * crop];
    let mut raw_crop = vec![0.0; crop * crop];
    for y in 0..crop {
        for x in 0..crop {
            jpeg_crop[y * crop + x] = jpeg_grid[(y + start) * size + x + start];
            raw_crop[y * crop + x] = raw_grid[(y + start) * size + x + start];
        }
    }
    let jpeg_spectrum = magnitude_spectrum(&jpeg_crop, crop);
    let raw_spectrum = magnitude_spectrum(&raw_crop, crop);
    let theta_count = 256;
    let radius_count = 128;
    let jpeg_polar = log_polar(&jpeg_spectrum, crop, theta_count, radius_count);
    let raw_polar = log_polar(&raw_spectrum, crop, theta_count, radius_count);
    let (theta_shift, radius_shift, quality) =
        phase_correlation(&raw_polar, &jpeg_polar, theta_count, radius_count);
    if quality < 0.02 {
        return (1.0, 0.0);
    }
    let rotation = 2.0 * std::f64::consts::PI * theta_shift / theta_count as f64;
    let radius_min = 2.0_f64;
    let radius_max = crop as f64 * 0.5 - 2.0;
    let log_range = libm::log(radius_max / radius_min);
    let scale = libm::exp(radius_shift * log_range / (radius_count - 1) as f64);
    if !scale.is_finite() || scale <= 0.0 {
        (1.0, 0.0)
    } else {
        (scale, rotation)
    }
}

fn magnitude_spectrum(values: &[f64], size: usize) -> Vec<f64> {
    let window = hann2(size, size);
    let mean_value = mean(values);
    let mut re: Vec<f64> = values
        .iter()
        .zip(&window)
        .map(|(value, window)| (value - mean_value) * window)
        .collect();
    let mut im = vec![0.0; size * size];
    fft_2d(&mut re, &mut im, size, size, false);
    let mut out = vec![0.0; size * size];
    let half = size / 2;
    for y in 0..size {
        let shifted_y = (y + half) % size;
        for x in 0..size {
            let shifted_x = (x + half) % size;
            let index = y * size + x;
            out[shifted_y * size + shifted_x] = libm::log1p(libm::hypot(re[index], im[index]));
        }
    }
    out
}

fn log_polar(spectrum: &[f64], size: usize, theta_count: usize, radius_count: usize) -> Vec<f64> {
    let centre_value = size as f64 * 0.5;
    let radius_min = 2.0_f64;
    let radius_max = centre_value - 2.0;
    let log_min = libm::log(radius_min);
    let log_max = libm::log(radius_max);
    let mut out = vec![0.0; theta_count * radius_count];
    for radius_index in 0..radius_count {
        let fraction = radius_index as f64 / (radius_count - 1) as f64;
        let radius = libm::exp(log_min + fraction * (log_max - log_min));
        for theta_index in 0..theta_count {
            let theta = 2.0 * std::f64::consts::PI * theta_index as f64 / theta_count as f64;
            let x = centre_value + radius * libm::cos(theta);
            let y = centre_value + radius * libm::sin(theta);
            out[radius_index * theta_count + theta_index] = sample_grid(spectrum, size, size, x, y);
        }
    }
    out
}

fn sample_grid(values: &[f64], width: usize, height: usize, x: f64, y: f64) -> f64 {
    if x < 0.0 || y < 0.0 || x > (width - 1) as f64 || y > (height - 1) as f64 {
        return 0.0;
    }
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y1 = (y0 + 1).min(height - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let p00 = values[y0 * width + x0];
    let p10 = values[y0 * width + x1];
    let p01 = values[y1 * width + x0];
    let p11 = values[y1 * width + x1];
    let top = p00 + fx * (p10 - p00);
    let bottom = p01 + fx * (p11 - p01);
    top + fy * (bottom - top)
}

#[derive(Clone, Copy, Debug)]
struct GainSample {
    radius: f64,
    raw: f64,
    jpeg: f64,
}

fn gain_samples(raw: &Image3, jpeg: &DecodedJpeg, alignment: &Alignment) -> Vec<GainSample> {
    let size = 160;
    let (u, v) = normalized_coords(size, size);
    let (cx, cy) = centre(raw.width, raw.height);
    let mut samples = Vec::with_capacity(size * size);
    for (u, v) in u.into_iter().zip(v) {
        let (a, b) = alignment.linear(u, v);
        let (ad, bd) = alignment.distort(a, b);
        let x = cx + ad * cx;
        let y = cy + bd * cy;
        let Some(raw_pixel) = raw.sample_bilinear(x, y) else {
            continue;
        };
        let raw_luma = luminance(raw_pixel);
        if !raw_luma.is_finite() || raw_luma <= 1e-7 {
            continue;
        }
        let (jpeg_x, jpeg_y) = alignment.raw_to_jpeg(
            x,
            y,
            raw.width,
            raw.height,
            jpeg.image.width,
            jpeg.image.height,
        );
        let Some(jpeg_pixel) = jpeg.image.sample_bilinear(jpeg_x, jpeg_y) else {
            continue;
        };
        let jpeg_luma = luminance(jpeg_pixel);
        if !jpeg_luma.is_finite() || jpeg_luma <= 1e-7 || jpeg_luma >= 0.999 {
            continue;
        }
        let radius = libm::hypot(ad, bd);
        samples.push(GainSample {
            radius,
            raw: raw_luma,
            jpeg: jpeg_luma,
        });
    }
    samples
}

fn fit_gain(samples: &[GainSample]) -> [f64; 3] {
    fit_gain_with_tone(samples).0
}

fn fit_gain_with_tone(samples: &[GainSample]) -> ([f64; 3], Option<ToneMap>) {
    if samples.len() < 64 {
        return ([0.0_f64; 3], None);
    }
    let reduced: Vec<GainSample> = samples.iter().step_by(4).copied().collect();
    let mut gain = [0.0_f64; 3];
    let mut best = spline_tone_sse(&reduced, &gain);
    let mut step = 0.2_f64;
    for _ in 0..60 {
        let mut improved = false;
        for index in 0..3 {
            for sign in [1.0_f64, -1.0] {
                let mut trial = gain;
                trial[index] = (trial[index] + sign * step).clamp(-0.9, 0.9);
                let sse = spline_tone_sse(&reduced, &trial);
                if sse < best {
                    gain = trial;
                    best = sse;
                    improved = true;
                }
            }
        }
        if !improved {
            step *= 0.5;
            if step < 1e-4 {
                break;
            }
        }
    }
    let mut ordered: Vec<(f64, f64)> = samples
        .iter()
        .map(|sample| (sample.raw * gain_factor(sample.radius, &gain), sample.jpeg))
        .collect();
    ordered.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    (gain, Some(ToneMap::fit(&ordered)))
}

fn spline_tone_sse(samples: &[GainSample], gain: &[f64; 3]) -> f64 {
    let mut ordered: Vec<(f64, f64)> = samples
        .iter()
        .map(|sample| (sample.raw * gain_factor(sample.radius, gain), sample.jpeg))
        .collect();
    ordered.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let tone = ToneMap::fit(&ordered);
    let mut sum = 0.0;
    for sample in samples {
        let t = sample.raw * gain_factor(sample.radius, gain);
        let (value, _) = tone.evaluate_with_derivative(t);
        let residual = sample.jpeg - value;
        sum += residual * residual;
    }
    sum
}

fn gain_factor(radius: f64, gain: &[f64; 3]) -> f64 {
    let r2 = radius * radius;
    1.0 + gain[0] * r2 + gain[1] * r2 * r2 + gain[2] * r2 * r2 * r2
}

#[derive(Clone, Debug)]
struct ToneMap {
    positions: [f64; TONE_KNOTS],
    values: [f64; TONE_KNOTS],
}

impl ToneMap {
    fn fit(ordered: &[(f64, f64)]) -> Self {
        let mut positions = [0.0_f64; TONE_KNOTS];
        let mut values = [0.0_f64; TONE_KNOTS];
        let count = ordered.len();
        for knot in 0..TONE_KNOTS {
            let start = knot * count / TONE_KNOTS;
            let end = ((knot + 1) * count / TONE_KNOTS).max(start + 1).min(count);
            let slice = &ordered[start.min(count - 1)..end];
            let weight = slice.len().max(1);
            let mut position_sum = 0.0;
            let mut value_sum = 0.0;
            for (position, value) in slice {
                position_sum += *position;
                value_sum += *value;
            }
            positions[knot] = position_sum / weight as f64;
            values[knot] = value_sum / weight as f64;
        }
        for index in 1..TONE_KNOTS {
            if positions[index] <= positions[index - 1] {
                positions[index] = if positions[index - 1] > 0.0 {
                    positions[index - 1] * 1.000_001
                } else {
                    positions[index - 1] + 1e-9
                };
            }
        }
        let mut blocks: Vec<(f64, usize)> = Vec::with_capacity(TONE_KNOTS);
        for value in values {
            blocks.push((value, 1));
            while blocks.len() >= 2 {
                let last = blocks[blocks.len() - 1];
                let previous = blocks[blocks.len() - 2];
                if previous.0 <= last.0 {
                    break;
                }
                let weight = previous.1 + last.1;
                let value =
                    (previous.0 * previous.1 as f64 + last.0 * last.1 as f64) / weight as f64;
                blocks.pop();
                blocks.pop();
                blocks.push((value, weight));
            }
        }
        let mut fitted = [0.0_f64; TONE_KNOTS];
        let mut filled = 0usize;
        for (value, weight) in blocks {
            let end = (filled + weight).min(TONE_KNOTS);
            for slot in fitted.iter_mut().take(end).skip(filled) {
                *slot = value;
            }
            filled = end;
        }
        while filled < TONE_KNOTS {
            fitted[filled] = fitted[filled.saturating_sub(1)];
            filled += 1;
        }
        Self {
            positions,
            values: fitted,
        }
    }

    fn evaluate_with_derivative(&self, position: f64) -> (f64, f64) {
        if position <= self.positions[0] {
            return (self.values[0], 0.0);
        }
        if position >= self.positions[TONE_KNOTS - 1] {
            return (self.values[TONE_KNOTS - 1], 0.0);
        }
        for index in 1..TONE_KNOTS {
            if position <= self.positions[index] {
                let span = self.positions[index] - self.positions[index - 1];
                if span <= 0.0 {
                    return (self.values[index], 0.0);
                }
                let fraction = (position - self.positions[index - 1]) / span;
                let slope = (self.values[index] - self.values[index - 1]) / span;
                let value = self.values[index - 1]
                    + fraction * (self.values[index] - self.values[index - 1]);
                return (value, slope);
            }
        }
        (self.values[TONE_KNOTS - 1], 0.0)
    }
}

#[allow(dead_code)]
fn solve3(mut a: [[f64; 3]; 3], mut b: [f64; 3]) -> Option<[f64; 3]> {
    for column in 0..3 {
        let mut pivot = column;
        for row in (column + 1)..3 {
            if a[row][column].abs() > a[pivot][column].abs() {
                pivot = row;
            }
        }
        if a[pivot][column].abs() < 1e-18 {
            return None;
        }
        a.swap(column, pivot);
        b.swap(column, pivot);
        let denominator = a[column][column];
        let pivot_row = a[column];
        for row in (column + 1)..3 {
            let factor = a[row][column] / denominator;
            for (index, value) in a[row].iter_mut().enumerate().skip(column) {
                *value -= factor * pivot_row[index];
            }
            b[row] -= factor * b[column];
        }
    }
    let mut out = [0.0_f64; 3];
    for row in (0..3).rev() {
        let mut sum = b[row];
        for column in (row + 1)..3 {
            sum -= a[row][column] * out[column];
        }
        out[row] = sum / a[row][row];
    }
    Some(out)
}

fn block_residual(raw: &Image3, jpeg: &Image3, alignment: &Alignment) -> f64 {
    const BLOCK: usize = 32;
    const STEP: usize = 24;
    let width = raw.width as usize;
    let height = raw.height as usize;
    let mut sum = 0.0;
    let mut count = 0usize;
    let mut y = 0;
    while y + BLOCK <= height {
        let mut x = 0;
        while x + BLOCK <= width {
            let mut a = Vec::with_capacity(BLOCK * BLOCK);
            let mut b = Vec::with_capacity(BLOCK * BLOCK);
            let mut valid = true;
            for py in 0..BLOCK {
                for px in 0..BLOCK {
                    let sx = (x + px) as f64;
                    let sy = (y + py) as f64;
                    let raw_pixel = raw
                        .pixel((x + px) as u32, (y + py) as u32)
                        .expect("block is inside the image");
                    let (jx, jy) = alignment.raw_to_jpeg(
                        sx,
                        sy,
                        raw.width,
                        raw.height,
                        jpeg.width,
                        jpeg.height,
                    );
                    let Some(jpeg_pixel) = jpeg.sample_bilinear(jx, jy) else {
                        valid = false;
                        break;
                    };
                    let radius = libm::hypot(
                        (sx - f64::from(raw.width - 1) * 0.5) / (f64::from(raw.width - 1) * 0.5),
                        (sy - f64::from(raw.height - 1) * 0.5) / (f64::from(raw.height - 1) * 0.5),
                    );
                    a.push(luminance(raw_pixel) * alignment.gain_at(radius));
                    b.push(luminance(jpeg_pixel));
                }
                if !valid {
                    break;
                }
            }
            if valid {
                let variance_a = variance(&a);
                let variance_b = variance(&b);
                if variance_a > 1e-10 && variance_b > 1e-10 {
                    let (dx, dy, quality) = phase_correlation(&a, &b, BLOCK, BLOCK);
                    if quality > 0.02 && dx.abs() <= 4.0 && dy.abs() <= 4.0 {
                        sum += dx * dx + dy * dy;
                        count += 1;
                    }
                }
            }
            x += STEP;
        }
        y += STEP;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_color::RgbSpace;
    use looklab_samples::OutputSpace;

    use crate::jpeg::decode_jpeg;

    const RAW_WIDTH: u32 = 900;
    const RAW_HEIGHT: u32 = 600;
    const JPEG_WIDTH: u32 = 720;
    const JPEG_HEIGHT: u32 = 480;
    const TRUE_GAIN: [f64; 3] = [0.25, 0.05, 0.0];

    fn scene(u: f64, v: f64) -> [f64; 3] {
        let base = 0.42
            + 0.10 * libm::sin(5.0 * u + 1.3)
            + 0.07 * libm::cos(7.0 * v - 0.4)
            + 0.05 * libm::sin(11.0 * (u + v));
        let blocks = if ((u * 4.0).floor() as i32 + (v * 3.0).floor() as i32) % 2 == 0 {
            0.05
        } else {
            -0.03
        };
        let edge = if libm::sin(17.0 * u) * libm::cos(13.0 * v) > 0.25 {
            0.04
        } else {
            0.0
        };
        [
            (base + blocks + edge + 0.03 * libm::sin(3.0 * v)).clamp(0.02, 0.98),
            (base + 0.7 * blocks + 0.02 * libm::cos(9.0 * u)).clamp(0.02, 0.98),
            (base - blocks + 0.04 * libm::sin(6.0 * v + 0.7)).clamp(0.02, 0.98),
        ]
    }

    fn tone(value: f64) -> f64 {
        (0.02 + 0.96 * libm::pow(value.clamp(0.0, 1.0), 0.82)).clamp(0.0, 1.0)
    }

    fn ground_truth() -> Alignment {
        Alignment {
            scale: 1.0,
            rotation_rad: 0.004,
            tx: 0.012,
            ty: -0.009,
            k1: -0.02,
            k2: 0.006,
            gain: TRUE_GAIN,
            central_correlation: 1.0,
            residual_px: 0.0,
        }
    }

    fn true_gain(radius: f64) -> f64 {
        let r2 = radius * radius;
        let r4 = r2 * r2;
        1.0 + TRUE_GAIN[0] * r2 + TRUE_GAIN[1] * r4 + TRUE_GAIN[2] * r4 * r2
    }

    fn make_pair(width: u32, height: u32, compress: bool) -> (Image3, DecodedJpeg) {
        let truth = ground_truth();
        let (cx, cy) = centre(width, height);
        let mut raw_data = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let u = (f64::from(x) - cx) / cx;
                let v = (f64::from(y) - cy) / cy;
                let radius = libm::hypot(u, v);
                let color = scene(u, v);
                let shading = 1.0 / true_gain(radius);
                raw_data.push([
                    (color[0] * shading) as f32,
                    (color[1] * shading) as f32,
                    (color[2] * shading) as f32,
                ]);
            }
        }
        let raw = Image3::new(width, height, raw_data).expect("raw image");

        let jpeg_width = width * 4 / 5;
        let jpeg_height = height * 4 / 5;
        let mut linear = Vec::with_capacity((jpeg_width * jpeg_height) as usize);
        for y in 0..jpeg_height {
            for x in 0..jpeg_width {
                let (rx, ry) = truth.jpeg_to_raw(
                    f64::from(x),
                    f64::from(y),
                    jpeg_width,
                    jpeg_height,
                    width,
                    height,
                );
                let u = (rx - cx) / cx;
                let v = (ry - cy) / cy;
                let color = scene(u, v);
                linear.push([
                    tone(color[0]) as f32,
                    tone(color[1]) as f32,
                    tone(color[2]) as f32,
                ]);
            }
        }
        if compress {
            (raw, encode_jpeg(linear, jpeg_width, jpeg_height))
        } else {
            (
                raw,
                DecodedJpeg {
                    image: Image3::new(jpeg_width, jpeg_height, linear).expect("jpeg image"),
                    space: OutputSpace::Srgb,
                    space_assumed: true,
                    code_values_near_clip: vec![false; (jpeg_width * jpeg_height) as usize],
                },
            )
        }
    }

    fn encode_jpeg(data: Vec<[f32; 3]>, width: u32, height: u32) -> DecodedJpeg {
        let mut bytes = Vec::with_capacity(data.len() * 3);
        for pixel in data {
            for channel in pixel {
                let encoded = RgbSpace::Srgb.encode(f64::from(channel).clamp(0.0, 1.0));
                bytes.push((encoded * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
        let mut encoded = Vec::new();
        jpeg_encoder::Encoder::new(&mut encoded, 95)
            .encode(
                &bytes,
                u16::try_from(width).expect("width fits"),
                u16::try_from(height).expect("height fits"),
                jpeg_encoder::ColorType::Rgb,
            )
            .expect("encode JPEG");
        decode_jpeg(&encoded).expect("decode JPEG")
    }

    fn mapping_error(fit: &Alignment) -> f64 {
        let truth = ground_truth();
        let mut sum = 0.0;
        let mut count = 0usize;
        for iy in 2..=14 {
            for ix in 2..=14 {
                let x = f64::from(ix) * f64::from(JPEG_WIDTH - 1) / 16.0;
                let y = f64::from(iy) * f64::from(JPEG_HEIGHT - 1) / 16.0;
                let (tx, ty) =
                    truth.jpeg_to_raw(x, y, JPEG_WIDTH, JPEG_HEIGHT, RAW_WIDTH, RAW_HEIGHT);
                let (ax, ay) =
                    fit.jpeg_to_raw(x, y, JPEG_WIDTH, JPEG_HEIGHT, RAW_WIDTH, RAW_HEIGHT);
                sum += (tx - ax) * (tx - ax) + (ty - ay) * (ty - ay);
                count += 1;
            }
        }
        (sum / count as f64).sqrt()
    }

    #[test]
    fn raw_to_jpeg_inverts_jpeg_to_raw() {
        let truth = ground_truth();
        for y in [0.0, 100.0, 239.5, 479.0] {
            for x in [0.0, 120.0, 359.5, 719.0] {
                let (rx, ry) =
                    truth.jpeg_to_raw(x, y, JPEG_WIDTH, JPEG_HEIGHT, RAW_WIDTH, RAW_HEIGHT);
                let (jx, jy) =
                    truth.raw_to_jpeg(rx, ry, RAW_WIDTH, RAW_HEIGHT, JPEG_WIDTH, JPEG_HEIGHT);
                assert!(
                    (jx - x).abs() < 1e-6 && (jy - y).abs() < 1e-6,
                    "({x}, {y}) -> ({rx}, {ry}) -> ({jx}, {jy})"
                );
            }
        }
    }

    #[test]
    fn recovers_geometry_and_gain_from_a_synthetic_pair() {
        let (raw, jpeg) = make_pair(RAW_WIDTH, RAW_HEIGHT, true);
        let alignment = align(&raw, &jpeg, &AlignParams::default()).expect("alignment");
        assert!(
            mapping_error(&alignment) < 0.3,
            "mapping error {}, alignment {alignment:?}",
            mapping_error(&alignment)
        );
        assert!(alignment.central_correlation >= 0.98);
        assert!((alignment.gain[0] - TRUE_GAIN[0]).abs() < 0.08);
        assert!((alignment.gain[1] - TRUE_GAIN[1]).abs() < 0.08);
    }

    #[test]
    fn gain_residual_is_flat_across_radius() {
        let (raw, jpeg) = make_pair(RAW_WIDTH, RAW_HEIGHT, false);
        let alignment = ground_truth();
        let samples = gain_samples(&raw, &jpeg, &alignment);
        let (gain, tone) = fit_gain_with_tone(&samples);
        let tone = tone.expect("tone map");
        let max_radius = samples
            .iter()
            .map(|sample| sample.radius)
            .fold(0.0_f64, f64::max)
            .max(1e-6);
        let mut bins: [Vec<f64>; 12] = std::array::from_fn(|_| Vec::new());
        for sample in &samples {
            let corrected = sample.raw * gain_factor(sample.radius, &gain);
            let (value, _) = tone.evaluate_with_derivative(corrected);
            if value > 1e-7 {
                let index = ((sample.radius / max_radius) * 11.0)
                    .round()
                    .clamp(0.0, 11.0) as usize;
                bins[index].push(sample.jpeg / value);
            }
        }
        for (index, bin) in bins.iter_mut().enumerate() {
            if bin.len() < 100 {
                continue;
            }
            bin.sort_unstable_by(f64::total_cmp);
            let median = bin[bin.len() / 2];
            assert!(
                (median - 1.0).abs() < 0.01,
                "radius bin {index} median ratio {median}, gain {gain:?}"
            );
        }
    }

    #[test]
    #[ignore = "reports release timing"]
    #[allow(clippy::disallowed_types)]
    fn align_on_1_5_megapixels() {
        let (raw, jpeg) = make_pair(1500, 1000, true);
        let started = std::time::Instant::now();
        let alignment = align(&raw, &jpeg, &AlignParams::default()).expect("alignment");
        let elapsed = started.elapsed();
        println!(
            "align 1.5 MP: {elapsed:?}, correlation {}, residual {} px",
            alignment.central_correlation, alignment.residual_px
        );
        assert!(alignment.central_correlation >= 0.98);
    }
}
