// SPDX-License-Identifier: AGPL-3.0-or-later
//! Artifacts a firmware consumes: 3D LUT with optional shaper, 3x3 matrix, 1D curve.

use looklab_schema::{Interpolation, Precision};
use serde::{Deserialize, Serialize};

use crate::error::{ExportError, Result};
use crate::quantize::quantize;

/// Largest grid side a reader or a writer accepts.
pub const MAX_LUT_SIZE: u32 = 256;

/// Largest 1D table a reader or a writer accepts.
pub const MAX_CURVE1D_SIZE: u32 = 65_536;

fn check_side(size: u32) -> Result<()> {
    if !(2..=MAX_LUT_SIZE).contains(&size) {
        return Err(ExportError::InvalidArtifact("grid side out of range"));
    }
    Ok(())
}

fn check_curve(size: u32) -> Result<()> {
    if !(2..=MAX_CURVE1D_SIZE).contains(&size) {
        return Err(ExportError::InvalidArtifact("curve length out of range"));
    }
    Ok(())
}

fn check_finite(values: impl IntoIterator<Item = f32>) -> Result<()> {
    if values.into_iter().any(|value| !value.is_finite()) {
        return Err(ExportError::InvalidArtifact("non-finite sample"));
    }
    Ok(())
}

/// A 1D shaper: normalized insertion code in `[0, 1]` to the 3D LUT's grid
/// coordinate, applied per channel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Shaper1d {
    pub size: u32,
    pub values: Vec<f32>,
}

impl Shaper1d {
    /// # Panics
    /// When `size` is below 2.
    pub fn from_fn(size: u32, mut f: impl FnMut(f64) -> f64) -> Self {
        assert!(size >= 2, "a shaper needs at least two points");
        let mut values = Vec::with_capacity(size as usize);
        for index in 0..size {
            let code = index as f64 / (size - 1) as f64;
            values.push(f(code).clamp(0.0, 1.0) as f32);
        }
        Self { size, values }
    }

    pub fn eval(&self, code: f64) -> f64 {
        let x = code.clamp(0.0, 1.0) * (self.size - 1) as f64;
        let lower = x.floor() as usize;
        if lower + 1 >= self.values.len() {
            return self.values[self.values.len() - 1] as f64;
        }
        let fraction = x - lower as f64;
        let a = self.values[lower] as f64;
        let b = self.values[lower + 1] as f64;
        a + fraction * (b - a)
    }

    /// # Errors
    /// [`ExportError::InvalidArtifact`] when the shaper is malformed.
    pub fn validate(&self) -> Result<()> {
        check_curve(self.size)?;
        if self.values.len() != self.size as usize {
            return Err(ExportError::InvalidArtifact("shaper length mismatch"));
        }
        check_finite(self.values.iter().copied())?;
        if self
            .values
            .windows(2)
            .any(|pair| !(0.0..=1.0).contains(&pair[0]) || pair[1] < pair[0])
        {
            return Err(ExportError::InvalidArtifact("shaper is not non-decreasing"));
        }
        if !(0.0..=1.0).contains(&self.values[self.values.len() - 1]) {
            return Err(ExportError::InvalidArtifact("shaper leaves the unit range"));
        }
        Ok(())
    }
}

/// A 3D LUT whose grid coordinates are the shaped insertion code.
///
/// `data` is indexed red fastest: `r + size * (g + size * b)`. Values are stored
/// at the profile's precision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lut3d {
    pub size: u32,
    pub data: Vec<[f32; 3]>,
    pub shaper: Option<Shaper1d>,
}

impl Lut3d {
    /// # Panics
    /// When `size` is below 2.
    pub fn from_fn(
        size: u32,
        shaper: Option<Shaper1d>,
        precision: Precision,
        mut f: impl FnMut([f64; 3]) -> [f64; 3],
    ) -> Self {
        assert!(size >= 2, "a 3D LUT needs at least two points per axis");
        let mut data = Vec::with_capacity((size as usize).pow(3));
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let scale = (size - 1) as f64;
                    let at = [r as f64 / scale, g as f64 / scale, b as f64 / scale];
                    let value = f(at);
                    data.push(value.map(|channel| {
                        let channel = if channel.is_finite() { channel } else { 0.0 };
                        quantize(channel as f32, precision)
                    }));
                }
            }
        }
        Self { size, data, shaper }
    }

    pub fn index(&self, r: usize, g: usize, b: usize) -> usize {
        let size = self.size as usize;
        r + size * (g + size * b)
    }

    pub fn eval(&self, code: [f64; 3], interpolation: Interpolation) -> [f64; 3] {
        let at = match &self.shaper {
            Some(shaper) => code.map(|value| shaper.eval(value)),
            None => code.map(|value| value.clamp(0.0, 1.0)),
        };
        self.eval_grid(at, interpolation)
    }

    fn eval_grid(&self, at: [f64; 3], interpolation: Interpolation) -> [f64; 3] {
        let size = self.size;
        let scale = (size - 1) as f64;
        let mut base = [0_usize; 3];
        let mut fraction = [0.0_f64; 3];
        for axis in 0..3 {
            let x = at[axis].clamp(0.0, 1.0) * scale;
            let lower = (x.floor() as usize).min(size as usize - 1);
            let lower = if lower + 1 < size as usize {
                lower
            } else {
                size as usize - 2
            };
            base[axis] = lower;
            fraction[axis] = x - lower as f64;
        }
        let corner = |dr: usize, dg: usize, db: usize| {
            let value = self.data[self.index(base[0] + dr, base[1] + dg, base[2] + db)];
            [value[0] as f64, value[1] as f64, value[2] as f64]
        };
        match interpolation {
            Interpolation::Trilinear => trilinear(&corner, fraction),
            Interpolation::Tetrahedral => tetrahedral(&corner, fraction),
        }
    }

    /// Resamples the grid, keeping the shaper and the stored precision.
    pub fn resample(&self, size: u32, precision: Precision) -> Self {
        let source = self.clone();
        Self::from_fn(size, self.shaper.clone(), precision, |at| {
            source.eval_grid(at, Interpolation::Trilinear)
        })
    }

    /// # Errors
    /// [`ExportError::InvalidArtifact`] when the grid is malformed.
    pub fn validate(&self) -> Result<()> {
        check_side(self.size)?;
        let expected = (self.size as u64).pow(3);
        if self.data.len() as u64 != expected {
            return Err(ExportError::InvalidArtifact("node count mismatch"));
        }
        check_finite(self.data.iter().flat_map(|value| value.iter().copied()))?;
        if let Some(shaper) = &self.shaper {
            shaper.validate()?;
        }
        Ok(())
    }
}

fn trilinear(corner: &impl Fn(usize, usize, usize) -> [f64; 3], f: [f64; 3]) -> [f64; 3] {
    let [fr, fg, fb] = f;
    let c000 = corner(0, 0, 0);
    let c100 = corner(1, 0, 0);
    let c010 = corner(0, 1, 0);
    let c110 = corner(1, 1, 0);
    let c001 = corner(0, 0, 1);
    let c101 = corner(1, 0, 1);
    let c011 = corner(0, 1, 1);
    let c111 = corner(1, 1, 1);
    let mut out = [0.0_f64; 3];
    for channel in 0..3 {
        let c00 = c000[channel] * (1.0 - fr) + c100[channel] * fr;
        let c10 = c010[channel] * (1.0 - fr) + c110[channel] * fr;
        let c01 = c001[channel] * (1.0 - fr) + c101[channel] * fr;
        let c11 = c011[channel] * (1.0 - fr) + c111[channel] * fr;
        let c0 = c00 * (1.0 - fg) + c10 * fg;
        let c1 = c01 * (1.0 - fg) + c11 * fg;
        out[channel] = c0 * (1.0 - fb) + c1 * fb;
    }
    out
}

/// Six-case tetrahedral interpolation over the cube corner ladder.
fn tetrahedral(corner: &impl Fn(usize, usize, usize) -> [f64; 3], f: [f64; 3]) -> [f64; 3] {
    let [fr, fg, fb] = f;
    let c000 = corner(0, 0, 0);
    let (a, wa, b, wb, c, wc) = if fr > fg {
        if fg > fb {
            (
                corner(1, 0, 0),
                fr,
                corner(1, 1, 0),
                fg,
                corner(1, 1, 1),
                fb,
            )
        } else if fr > fb {
            (
                corner(1, 0, 0),
                fr,
                corner(1, 0, 1),
                fb,
                corner(1, 1, 1),
                fg,
            )
        } else {
            (
                corner(0, 0, 1),
                fb,
                corner(1, 0, 1),
                fr,
                corner(1, 1, 1),
                fg,
            )
        }
    } else if fb > fg {
        (
            corner(0, 0, 1),
            fb,
            corner(0, 1, 1),
            fg,
            corner(1, 1, 1),
            fr,
        )
    } else if fb > fr {
        (
            corner(0, 1, 0),
            fg,
            corner(0, 1, 1),
            fb,
            corner(1, 1, 1),
            fr,
        )
    } else {
        (
            corner(0, 1, 0),
            fg,
            corner(1, 1, 0),
            fr,
            corner(1, 1, 1),
            fb,
        )
    };
    let mut out = [0.0_f64; 3];
    for channel in 0..3 {
        out[channel] = c000[channel]
            + wa * (a[channel] - c000[channel])
            + wb * (b[channel] - a[channel])
            + wc * (c[channel] - b[channel]);
    }
    out
}

/// A 3x3 matrix applied to the decoded linear insertion RGB.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Matrix3x3 {
    pub rows: [[f32; 3]; 3],
}

impl Matrix3x3 {
    pub fn eval(&self, linear: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0_f64; 3];
        for (row, weights) in self.rows.iter().enumerate() {
            for (weight, value) in weights.iter().zip(linear.iter()) {
                out[row] += *weight as f64 * value;
            }
        }
        out
    }

    /// # Errors
    /// [`ExportError::InvalidArtifact`] when a coefficient is not finite.
    pub fn validate(&self) -> Result<()> {
        check_finite(self.rows.iter().flat_map(|row| row.iter().copied()))
    }
}

/// A per-channel 1D curve over the decoded linear insertion RGB in `[0, domain_max]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Curve1d {
    pub per_channel: bool,
    pub domain_max: f32,
    /// Piecewise-linear knots, uniformly spaced over the domain.
    pub points: Vec<[f32; 3]>,
}

impl Curve1d {
    pub fn eval(&self, linear: [f64; 3]) -> [f64; 3] {
        let mut out = [0.0_f64; 3];
        for channel in 0..3 {
            let x = (linear[channel] / self.domain_max as f64).clamp(0.0, 1.0)
                * (self.points.len() - 1) as f64;
            let lower = x.floor() as usize;
            let fraction = x - lower as f64;
            let value = |index: usize| {
                let point = self.points[index];
                if self.per_channel {
                    point[channel] as f64
                } else {
                    point[0] as f64
                }
            };
            let next = (lower + 1).min(self.points.len() - 1);
            out[channel] = value(lower) * (1.0 - fraction) + value(next) * fraction;
        }
        out
    }

    /// # Errors
    /// [`ExportError::InvalidArtifact`] when the curve is malformed.
    pub fn validate(&self) -> Result<()> {
        if self.points.len() < 2 {
            return Err(ExportError::InvalidArtifact("curve needs two knots"));
        }
        if self.points.len() > MAX_CURVE1D_SIZE as usize {
            return Err(ExportError::InvalidArtifact("curve has too many knots"));
        }
        if !(self.domain_max.is_finite() && self.domain_max > 0.0) {
            return Err(ExportError::InvalidArtifact("curve domain is invalid"));
        }
        check_finite(self.points.iter().flat_map(|point| point.iter().copied()))
    }
}

/// One artifact, in the form the firmware consumes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Artifact {
    Lut3d(Lut3d),
    Matrix3x3(Matrix3x3),
    Curve1d(Curve1d),
}

impl Artifact {
    /// `code` is the normalized insertion code; `linear` its decoded value. LUTs
    /// read `code`, matrices and curves read `linear`.
    pub fn eval(&self, code: [f64; 3], linear: [f64; 3], interpolation: Interpolation) -> [f64; 3] {
        match self {
            Self::Lut3d(lut) => lut.eval(code, interpolation),
            Self::Matrix3x3(matrix) => matrix.eval(linear),
            Self::Curve1d(curve) => curve.eval(linear),
        }
    }

    pub fn as_lut(&self) -> Option<&Lut3d> {
        match self {
            Self::Lut3d(lut) => Some(lut),
            Self::Matrix3x3(_) | Self::Curve1d(_) => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Lut3d(_) => "lut3d",
            Self::Matrix3x3(_) => "matrix3x3",
            Self::Curve1d(_) => "curve1d",
        }
    }

    /// # Errors
    /// [`ExportError::InvalidArtifact`] when the artifact is malformed.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Lut3d(lut) => lut.validate(),
            Self::Matrix3x3(matrix) => matrix.validate(),
            Self::Curve1d(curve) => curve.validate(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(size: u32) -> Lut3d {
        Lut3d::from_fn(size, None, Precision::F32, |at| at)
    }

    #[test]
    fn lut_reproduces_its_grid_points() {
        let lut = ramp(5);
        for index in 0..5 {
            let at = index as f64 / 4.0;
            for interpolation in [Interpolation::Trilinear, Interpolation::Tetrahedral] {
                let value = lut.eval([at, at, at], interpolation);
                assert!((value[0] - at).abs() < 1.0e-7, "{value:?} at {at}");
            }
        }
    }

    #[test]
    fn interpolation_methods_agree_on_an_affine_grid() {
        let lut = Lut3d::from_fn(9, None, Precision::F32, |at| {
            [0.2 + 0.1 * at[0], 0.4 * at[1], 0.9 * at[2] + 0.05]
        });
        let at = [0.34, 0.71, 0.22];
        let trilinear = lut.eval(at, Interpolation::Trilinear);
        let tetrahedral = lut.eval(at, Interpolation::Tetrahedral);
        for channel in 0..3 {
            assert!((trilinear[channel] - tetrahedral[channel]).abs() < 1.0e-7);
        }
    }

    #[test]
    fn shaper_is_applied_before_the_grid() {
        let shaper = Shaper1d::from_fn(3, |code| code * code);
        let lut = Lut3d::from_fn(3, Some(shaper), Precision::F32, |at| at);
        let value = lut.eval([0.5, 0.5, 0.5], Interpolation::Trilinear);
        assert!((value[0] - 0.25).abs() < 1.0e-6, "{value:?}");
    }

    #[test]
    fn resampling_keeps_a_smooth_grid() {
        let lut = Lut3d::from_fn(17, None, Precision::F32, |at| {
            [at[0] * at[1], at[1].sqrt(), 0.5]
        });
        let resampled = lut.resample(64, Precision::F32);
        assert_eq!(resampled.data.len(), 64 * 64 * 64);
        let at = [0.3, 0.6, 0.9];
        let a = lut.eval(at, Interpolation::Trilinear);
        let b = resampled.eval(at, Interpolation::Trilinear);
        for channel in 0..3 {
            assert!((a[channel] - b[channel]).abs() < 0.01, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn curve_is_piecewise_linear() {
        let curve = Curve1d {
            per_channel: true,
            domain_max: 2.0,
            points: vec![[0.0, 0.0, 0.0], [0.5, 0.25, 0.75], [1.0, 1.0, 1.0]],
        };
        let value = curve.eval([1.0, 1.0, 1.0]);
        assert!((value[0] - 0.5).abs() < 1.0e-6);
        assert!((value[1] - 0.25).abs() < 1.0e-6);
        assert!((value[2] - 0.75).abs() < 1.0e-6);
    }

    #[test]
    fn matrix_uses_row_times_column() {
        let matrix = Matrix3x3 {
            rows: [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, -1.0]],
        };
        assert_eq!(matrix.eval([0.5, 0.25, 0.125]), [0.5, 0.5, -0.125]);
    }
}
