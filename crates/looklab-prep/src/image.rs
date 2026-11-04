// SPDX-License-Identifier: AGPL-3.0-or-later
//! Planar linear RGB image used between decode and sampling.

use looklab_meta::Orientation;

use crate::error::PrepError;

/// Linear RGB, row-major, `width * height` entries.
#[derive(Clone, Debug, PartialEq)]
pub struct Image3 {
    pub width: u32,
    pub height: u32,
    pub data: Vec<[f32; 3]>,
}

impl Image3 {
    pub fn new(width: u32, height: u32, data: Vec<[f32; 3]>) -> Result<Self, PrepError> {
        if width == 0 || height == 0 {
            return Err(PrepError::ImageEmpty);
        }
        let expected = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
            .ok_or(PrepError::ImageTooLarge)?;
        if data.len() != expected {
            return Err(PrepError::ImageTooLarge);
        }
        Ok(Self {
            width,
            height,
            data,
        })
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[f32; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let index = usize::try_from(y).ok()? * usize::try_from(self.width).ok()?
            + usize::try_from(x).ok()?;
        self.data.get(index).copied()
    }

    pub fn downsample_area(&self, max_pixels: u32) -> Image3 {
        let total = u64::from(self.width) * u64::from(self.height);
        if total <= u64::from(max_pixels) {
            return self.clone();
        }
        let max_pixels = max_pixels.max(1);
        let scale = (f64::from(max_pixels) / total as f64).sqrt();
        let mut out_width = ((f64::from(self.width) * scale).floor() as u32).max(1);
        let mut out_height = ((f64::from(self.height) * scale).floor() as u32).max(1);
        while u64::from(out_width) * u64::from(out_height) > u64::from(max_pixels) {
            if out_width >= out_height && out_width > 1 {
                out_width -= 1;
            } else if out_height > 1 {
                out_height -= 1;
            } else {
                break;
            }
        }

        let x_taps = axis_taps(self.width, out_width);
        let y_taps = axis_taps(self.height, out_height);
        let width = usize::try_from(self.width).expect("width fits usize");
        let mut row_sum = vec![[0.0_f64; 3]; width];
        let mut data = Vec::with_capacity(
            usize::try_from(out_width).expect("width fits usize")
                * usize::try_from(out_height).expect("height fits usize"),
        );

        for taps_y in &y_taps {
            row_sum.iter_mut().for_each(|cell| *cell = [0.0; 3]);
            let mut weight_y = 0.0_f64;
            for &(sy, wy) in taps_y {
                weight_y += wy;
                let start = usize::try_from(sy).expect("row fits usize") * width;
                let row = &self.data[start..start + width];
                for (sum, value) in row_sum.iter_mut().zip(row) {
                    sum[0] += wy * f64::from(value[0]);
                    sum[1] += wy * f64::from(value[1]);
                    sum[2] += wy * f64::from(value[2]);
                }
            }
            for taps_x in &x_taps {
                let mut sum = [0.0_f64; 3];
                let mut weight_x = 0.0_f64;
                for &(sx, wx) in taps_x {
                    weight_x += wx;
                    let value = row_sum[usize::try_from(sx).expect("column fits usize")];
                    sum[0] += wx * value[0];
                    sum[1] += wx * value[1];
                    sum[2] += wx * value[2];
                }
                let norm = weight_x * weight_y;
                data.push([
                    (sum[0] / norm) as f32,
                    (sum[1] / norm) as f32,
                    (sum[2] / norm) as f32,
                ]);
            }
        }

        Image3 {
            width: out_width,
            height: out_height,
            data,
        }
    }

    /// Bilinear sample at pixel coordinates; `(0, 0)` is the first pixel centre.
    pub fn sample_bilinear(&self, x: f64, y: f64) -> Option<[f32; 3]> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        let max_x = f64::from(self.width - 1);
        let max_y = f64::from(self.height - 1);
        if x < 0.0 || y < 0.0 || x > max_x || y > max_y {
            return None;
        }
        let x0 = x.floor() as u32;
        let y0 = y.floor() as u32;
        let x1 = (x0 + 1).min(self.width - 1);
        let y1 = (y0 + 1).min(self.height - 1);
        let fx = x - f64::from(x0);
        let fy = y - f64::from(y0);
        let p00 = self.pixel(x0, y0)?;
        let p10 = self.pixel(x1, y0)?;
        let p01 = self.pixel(x0, y1)?;
        let p11 = self.pixel(x1, y1)?;
        let mut out = [0.0_f32; 3];
        for channel in 0..3 {
            let top =
                f64::from(p00[channel]) + fx * (f64::from(p10[channel]) - f64::from(p00[channel]));
            let bottom =
                f64::from(p01[channel]) + fx * (f64::from(p11[channel]) - f64::from(p01[channel]));
            out[channel] = (top + fy * (bottom - top)) as f32;
        }
        Some(out)
    }

    pub fn apply_orientation(&self, orientation: Orientation) -> Image3 {
        let (out_width, out_height) = orientation_size(self.width, self.height, orientation);
        let last_x = self.width - 1;
        let last_y = self.height - 1;
        let mut data =
            Vec::with_capacity(usize::try_from(out_width * out_height).expect("fits usize"));
        for y in 0..out_height {
            for x in 0..out_width {
                let (source_x, source_y) = orientation_source(x, y, last_x, last_y, orientation);
                data.push(
                    self.pixel(source_x, source_y)
                        .expect("orientation mapping stays in bounds"),
                );
            }
        }
        Image3 {
            width: out_width,
            height: out_height,
            data,
        }
    }
}

pub(crate) fn orientation_size(width: u32, height: u32, orientation: Orientation) -> (u32, u32) {
    match orientation {
        Orientation::Normal
        | Orientation::FlipHorizontal
        | Orientation::Rotate180
        | Orientation::FlipVertical => (width, height),
        _ => (height, width),
    }
}

pub(crate) fn orientation_source(
    x: u32,
    y: u32,
    last_x: u32,
    last_y: u32,
    orientation: Orientation,
) -> (u32, u32) {
    match orientation {
        Orientation::Normal => (x, y),
        Orientation::FlipHorizontal => (last_x - x, y),
        Orientation::Rotate180 => (last_x - x, last_y - y),
        Orientation::FlipVertical => (x, last_y - y),
        Orientation::Transpose => (y, x),
        Orientation::Rotate90Clockwise => (y, last_y - x),
        Orientation::Transverse => (last_x - y, last_y - x),
        Orientation::Rotate270Clockwise => (last_x - y, x),
    }
}

fn axis_taps(src: u32, dst: u32) -> Vec<Vec<(u32, f64)>> {
    let mut taps = Vec::with_capacity(usize::try_from(dst).expect("destination fits usize"));
    for index in 0..dst {
        let start = f64::from(index) * f64::from(src) / f64::from(dst);
        let end = f64::from(index + 1) * f64::from(src) / f64::from(dst);
        let first = start.floor() as u32;
        let last = (end.ceil() as u32).min(src);
        let mut row = Vec::with_capacity(usize::try_from(last - first).expect("range fits usize"));
        for source in first..last {
            let low = start.max(f64::from(source));
            let high = end.min(f64::from(source + 1));
            if high > low {
                row.push((source, high - low));
            }
        }
        taps.push(row);
    }
    taps
}
