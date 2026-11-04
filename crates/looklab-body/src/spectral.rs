// SPDX-License-Identifier: AGPL-3.0-or-later
//! Spectral sensitivity import and the spectral prior for a body input
//! transform (docs/02 §2, source 1).
//!
//! The training reflectances, CIE 1931 colour matching functions and the A and
//! D65 spectral power distributions come from
//! `data/charts/colorchecker24_spectral.json`, written by
//! `tools/golden/gen_spectral.py`.

use std::sync::LazyLock;

use looklab_color::xyy::Illuminant;
use looklab_color::{Mat3, Xyz};
use serde::Deserialize;

use crate::cct::Cct;
use crate::error::{BodyError, Result};
use crate::linalg;

const SPECTRAL_JSON: &str = include_str!("../../../data/charts/colorchecker24_spectral.json");

const CHANNELS: usize = 3;
const MIN_SSF_SAMPLES: usize = 3;

/// A calibration illuminant with tabulated spectra in the training data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SsfIlluminant {
    A,
    D65,
}

impl SsfIlluminant {
    pub const ALL: [Self; 2] = [Self::A, Self::D65];

    pub fn xy(self) -> (f64, f64) {
        match self {
            Self::A => Illuminant::A.xy(),
            Self::D65 => Illuminant::D65.xy(),
        }
    }

    pub fn xyz(self) -> Xyz {
        match self {
            Self::A => Illuminant::A.xyz(),
            Self::D65 => Illuminant::D65.xyz(),
        }
    }

    pub fn cct(self) -> Cct {
        match self {
            Self::A => Cct::A,
            Self::D65 => Cct::D65,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::A => 0,
            Self::D65 => 1,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SpectralFile {
    grid_nm: Vec<f64>,
    cmfs: CmfColumns,
    illuminants: IlluminantColumns,
    reflectances: ReflectanceTable,
}

#[derive(Debug, Deserialize)]
struct CmfColumns {
    x: Vec<f64>,
    y: Vec<f64>,
    z: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct IlluminantColumns {
    a: Vec<f64>,
    d65: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct ReflectanceTable {
    chart: String,
    patches: Vec<NamedSeries>,
}

#[derive(Debug, Deserialize)]
struct NamedSeries {
    name: String,
    values: Vec<f64>,
}

/// The spectral reference data, sampled on one uniform wavelength grid.
#[derive(Debug)]
pub struct SpectralData {
    chart: String,
    grid: Vec<f64>,
    cmfs: Vec<[f64; CHANNELS]>,
    illuminants: [Vec<f64>; 2],
    names: Vec<String>,
    reflectances: Vec<Vec<f64>>,
}

static DATA: LazyLock<SpectralData> = LazyLock::new(|| {
    SpectralData::parse(SPECTRAL_JSON)
        // The embedded file is written by tools/golden/gen_spectral.py and
        // checked by `embedded_training_data_is_well_formed`.
        .expect("embedded spectral training data is valid")
});

pub fn training_data() -> &'static SpectralData {
    &DATA
}

impl SpectralData {
    fn parse(json: &str) -> Result<Self> {
        let file: SpectralFile = serde_json::from_str(json).map_err(|_| BodyError::SpectralData)?;
        let n = file.grid_nm.len();
        if n < MIN_SSF_SAMPLES {
            return Err(BodyError::SsfTooFewSamples {
                min: MIN_SSF_SAMPLES,
                got: n,
            });
        }
        for row in 1..n {
            let previous = file.grid_nm[row - 1];
            let current = file.grid_nm[row];
            if !(current.is_finite() && previous.is_finite() && current > previous) {
                return Err(BodyError::SsfNotAscending { row });
            }
        }
        let columns = [
            file.cmfs.x.len(),
            file.cmfs.y.len(),
            file.cmfs.z.len(),
            file.illuminants.a.len(),
            file.illuminants.d65.len(),
        ];
        for length in columns {
            if length != n {
                return Err(BodyError::SpectralColumn {
                    expected: n,
                    got: length,
                });
            }
        }
        for patch in &file.reflectances.patches {
            if patch.values.len() != n {
                return Err(BodyError::SpectralColumn {
                    expected: n,
                    got: patch.values.len(),
                });
            }
        }
        let all_values = file
            .cmfs
            .x
            .iter()
            .chain(file.cmfs.y.iter())
            .chain(file.cmfs.z.iter())
            .chain(file.illuminants.a.iter())
            .chain(file.illuminants.d65.iter())
            .chain(
                file.reflectances
                    .patches
                    .iter()
                    .flat_map(|p| p.values.iter()),
            );
        if all_values.into_iter().any(|value| !value.is_finite()) {
            return Err(BodyError::NonFinite {
                what: "spectral value",
            });
        }
        let mut cmfs = Vec::with_capacity(n);
        for index in 0..n {
            cmfs.push([file.cmfs.x[index], file.cmfs.y[index], file.cmfs.z[index]]);
        }
        let names = file
            .reflectances
            .patches
            .iter()
            .map(|patch| patch.name.clone())
            .collect();
        let reflectances = file
            .reflectances
            .patches
            .into_iter()
            .map(|patch| patch.values)
            .collect();
        Ok(Self {
            chart: file.reflectances.chart,
            grid: file.grid_nm,
            cmfs,
            illuminants: [file.illuminants.a, file.illuminants.d65],
            names,
            reflectances,
        })
    }

    pub fn grid(&self) -> &[f64] {
        &self.grid
    }

    pub fn chart(&self) -> &str {
        &self.chart
    }

    pub fn step_nm(&self) -> f64 {
        self.grid[1] - self.grid[0]
    }

    pub fn illuminant(&self, illuminant: SsfIlluminant) -> &[f64] {
        &self.illuminants[illuminant.index()]
    }

    pub fn patch_count(&self) -> usize {
        self.reflectances.len()
    }

    pub fn patch_name(&self, index: usize) -> Option<&str> {
        self.names.get(index).map(String::as_str)
    }

    pub fn reflectance(&self, index: usize) -> Option<&[f64]> {
        self.reflectances.get(index).map(Vec::as_slice)
    }

    /// White-normalized: the illuminant's white has `Y = 1`.
    pub(crate) fn xyz_under(&self, spd: &[f64], reflectance: &[f64]) -> Xyz {
        self.integrate(spd, Some(reflectance))
    }

    /// The illuminant's own tristimulus values, `Y = 1`.
    pub(crate) fn white_xyz(&self, spd: &[f64]) -> Xyz {
        self.integrate(spd, None)
    }

    fn integrate(&self, spd: &[f64], reflectance: Option<&[f64]>) -> Xyz {
        let step = self.step_nm();
        let mut sum = [0.0_f64; 3];
        let mut white_y = 0.0;
        for index in 0..self.grid.len() {
            let weight = spd[index] * step;
            white_y += weight * self.cmfs[index][1];
            for channel in 0..3 {
                let mut response = weight * self.cmfs[index][channel];
                if let Some(values) = reflectance {
                    response *= values[index];
                }
                sum[channel] += response;
            }
        }
        let scale = 1.0 / white_y;
        Xyz::new(sum[0] * scale, sum[1] * scale, sum[2] * scale)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpectralSensitivity {
    wavelengths: Vec<f64>,
    channels: [Vec<f64>; CHANNELS],
}

impl SpectralSensitivity {
    pub fn from_csv(text: &str) -> Result<Self> {
        let mut lines = text.lines().enumerate();
        let (_, header) = lines.next().ok_or(BodyError::SsfHeader)?;
        let columns: Vec<String> = header
            .split(',')
            .map(|column| column.trim().to_ascii_lowercase())
            .collect();
        if columns != ["wavelength", "r", "g", "b"] {
            return Err(BodyError::SsfHeader);
        }
        let mut wavelengths = Vec::new();
        let mut channels: [Vec<f64>; CHANNELS] = [Vec::new(), Vec::new(), Vec::new()];
        for (index, line) in lines {
            let row = index + 1;
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() != 4 {
                return Err(BodyError::SsfColumnCount {
                    row,
                    got: fields.len(),
                });
            }
            let mut values = [0.0_f64; 4];
            for (field, value) in fields.iter().zip(&mut values) {
                *value = field.parse().map_err(|_| BodyError::SsfValue { row })?;
                if !value.is_finite() {
                    return Err(BodyError::NonFinite {
                        what: "spectral sensitivity value",
                    });
                }
            }
            let wavelength = values[0];
            if let Some(previous) = wavelengths.last()
                && wavelength <= *previous
            {
                return Err(BodyError::SsfNotAscending { row });
            }
            wavelengths.push(wavelength);
            for channel in 0..CHANNELS {
                let value = values[channel + 1];
                if value < 0.0 {
                    return Err(BodyError::SsfNegative { row, value });
                }
                channels[channel].push(value);
            }
        }
        if wavelengths.len() < MIN_SSF_SAMPLES {
            return Err(BodyError::SsfTooFewSamples {
                min: MIN_SSF_SAMPLES,
                got: wavelengths.len(),
            });
        }
        Ok(Self {
            wavelengths,
            channels,
        })
    }

    pub fn to_csv(&self) -> String {
        let mut text = String::from("wavelength,r,g,b\n");
        for (index, wavelength) in self.wavelengths.iter().enumerate() {
            text.push_str(&format!(
                "{wavelength},{},{},{}\n",
                self.channels[0][index], self.channels[1][index], self.channels[2][index]
            ));
        }
        text
    }

    pub fn wavelengths(&self) -> &[f64] {
        &self.wavelengths
    }

    pub fn channel(&self, index: usize) -> Option<&[f64]> {
        self.channels.get(index).map(Vec::as_slice)
    }

    /// Linear interpolation onto `grid`; zero outside the measured range.
    pub fn resample(&self, grid: &[f64]) -> [Vec<f64>; CHANNELS] {
        let mut out: [Vec<f64>; CHANNELS] = [
            Vec::with_capacity(grid.len()),
            Vec::with_capacity(grid.len()),
            Vec::with_capacity(grid.len()),
        ];
        let last = self.wavelengths.len() - 1;
        for wavelength in grid {
            let upper = self
                .wavelengths
                .partition_point(|measured| *measured <= *wavelength);
            for channel in 0..CHANNELS {
                let value = if upper == 0 {
                    0.0
                } else if upper > last {
                    if *wavelength > self.wavelengths[last] {
                        0.0
                    } else {
                        self.channels[channel][last]
                    }
                } else {
                    let low = self.wavelengths[upper - 1];
                    let high = self.wavelengths[upper];
                    let fraction = (wavelength - low) / (high - low);
                    let a = self.channels[channel][upper - 1];
                    let b = self.channels[channel][upper];
                    a + (b - a) * fraction
                };
                out[channel].push(value);
            }
        }
        out
    }
}

/// Camera-to-XYZ(D65) from a sensitivity and the training reflectances, fitted
/// with the neutral fixed.
pub fn ssf_matrix(
    ssf: &SpectralSensitivity,
    illuminant: SsfIlluminant,
    data: &SpectralData,
) -> Result<Mat3> {
    let spd = data.illuminant(illuminant);
    let (responses, _) = responses_and_white(ssf, illuminant, data)?;
    let white_xyz = data.white_xyz(spd);
    let mut normal = [[0.0_f64; 3]; 3];
    let mut rhs = [[0.0_f64; 3]; 3];
    for (index, response) in responses.iter().enumerate() {
        let reflectance = data.reflectance(index).ok_or(BodyError::EmptyChart)?;
        let reference = looklab_color::cat::adapt(
            data.xyz_under(spd, reflectance),
            looklab_color::Cat::Bradford,
            white_xyz,
            Illuminant::D65.xyz(),
        );
        let target = reference.to_array();
        for row in 0..3 {
            for column in 0..3 {
                normal[row][column] += response[row] * response[column];
            }
            rhs[row][0] += response[0] * target[row];
            rhs[row][1] += response[1] * target[row];
            rhs[row][2] += response[2] * target[row];
        }
    }

    let white = Illuminant::D65.xyz().to_array();
    let mut matrix = [[0.0_f64; 3]; 3];
    for row in 0..3 {
        matrix[row] = solve_row(normal, rhs[row], white[row])?;
    }
    Ok(Mat3(matrix))
}

/// White-balanced camera responses of the training reflectances.
pub fn camera_responses(
    ssf: &SpectralSensitivity,
    illuminant: SsfIlluminant,
    data: &SpectralData,
) -> Result<Vec<[f64; 3]>> {
    Ok(responses_and_white(ssf, illuminant, data)?.0)
}

fn responses_and_white(
    ssf: &SpectralSensitivity,
    illuminant: SsfIlluminant,
    data: &SpectralData,
) -> Result<(Vec<[f64; 3]>, [f64; 3])> {
    let spd = data.illuminant(illuminant);
    let step = data.step_nm();
    let channels = ssf.resample(data.grid());
    let mut white = [0.0_f64; CHANNELS];
    for (index, power) in spd.iter().enumerate() {
        for channel in 0..CHANNELS {
            white[channel] += channels[channel][index] * power * step;
        }
    }
    if white
        .iter()
        .any(|value| !(value.is_finite() && *value > 0.0))
    {
        return Err(BodyError::SsfWhiteResponse);
    }
    let mut responses = Vec::with_capacity(data.patch_count());
    for index in 0..data.patch_count() {
        let reflectance = data.reflectance(index).ok_or(BodyError::EmptyChart)?;
        let mut response = [0.0_f64; CHANNELS];
        for (channel, value) in response.iter_mut().enumerate() {
            for (sample, power) in spd.iter().enumerate() {
                *value += channels[channel][sample] * power * reflectance[sample] * step;
            }
            *value /= white[channel];
        }
        responses.push(response);
    }
    Ok((responses, white))
}

fn solve_row(normal: [[f64; 3]; 3], rhs: [f64; 3], white: f64) -> Result<[f64; 3]> {
    let mut kkt = [[0.0_f64; 4]; 4];
    for row in 0..3 {
        for column in 0..3 {
            kkt[row][column] = normal[row][column];
        }
        kkt[row][3] = 1.0;
        kkt[3][row] = 1.0;
    }
    let mut rhs = [rhs[0], rhs[1], rhs[2], white];
    let solution = linalg::solve(&mut kkt, &mut rhs).ok_or(BodyError::Singular {
        what: "spectral prior",
    })?;
    Ok([solution[0], solution[1], solution[2]])
}

/// A three-Gaussian sensitivity for tests and synthetic bodies.
pub fn synthetic_sensitivity(
    wavelengths: &[f64],
    peaks: [f64; 3],
    sigma_nm: f64,
) -> SpectralSensitivity {
    let mut channels: [Vec<f64>; CHANNELS] = [Vec::new(), Vec::new(), Vec::new()];
    for wavelength in wavelengths {
        for channel in 0..CHANNELS {
            let offset = (wavelength - peaks[channel]) / sigma_nm;
            channels[channel].push(libm::exp(-0.5 * offset * offset));
        }
    }
    SpectralSensitivity {
        wavelengths: wavelengths.to_vec(),
        channels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_color::dng::xy_to_cct_robertson;

    fn grid() -> Vec<f64> {
        let mut wavelengths = Vec::new();
        let mut wavelength = 380.0;
        while wavelength <= 730.0 {
            wavelengths.push(wavelength);
            wavelength += 5.0;
        }
        wavelengths
    }

    #[test]
    fn embedded_training_data_is_well_formed() {
        let data = training_data();
        assert!(data.grid().len() >= 60);
        assert_eq!(data.patch_count(), 24);
        assert_eq!(data.patch_name(0), Some("dark skin"));
        assert!(data.white_xyz(data.illuminant(SsfIlluminant::D65)).y == 1.0);
        let xy = looklab_color::xyy::xyz_to_xy(data.white_xyz(data.illuminant(SsfIlluminant::A)));
        let cct = xy_to_cct_robertson(xy.0, xy.1);
        assert!((cct - Cct::A.kelvin()).abs() < 100.0);
    }

    #[test]
    fn csv_round_trips_through_text() {
        let sensitivity = synthetic_sensitivity(&grid(), [600.0, 540.0, 450.0], 40.0);
        let text = sensitivity.to_csv();
        let parsed = SpectralSensitivity::from_csv(&text).expect("generated CSV");
        assert_eq!(parsed, sensitivity);
    }

    #[test]
    fn csv_rejects_a_bad_header_and_bad_rows() {
        assert!(matches!(
            SpectralSensitivity::from_csv("lambda,r,g,b\n400,1,2,3\n"),
            Err(BodyError::SsfHeader)
        ));
        assert!(matches!(
            SpectralSensitivity::from_csv("wavelength,r,g,b\n400,1,2\n"),
            Err(BodyError::SsfColumnCount { row: 2, got: 3 })
        ));
        assert!(matches!(
            SpectralSensitivity::from_csv("wavelength,r,g,b\n400,1,2,3\n400,1,2,3\n410,1,2,3\n"),
            Err(BodyError::SsfNotAscending { row: 3 })
        ));
    }

    #[test]
    fn resampling_interpolates_and_stops_at_the_measured_range() {
        let sensitivity =
            SpectralSensitivity::from_csv("wavelength,r,g,b\n400,1,0,0\n500,0,1,0\n600,0,0,1\n")
                .expect("valid CSV");
        let resampled = sensitivity.resample(&[400.0, 450.0, 500.0, 600.0, 700.0]);
        assert_eq!(resampled[0][0], 1.0);
        assert_eq!(resampled[0][1], 0.5);
        assert_eq!(resampled[2][3], 1.0);
        assert_eq!(resampled[0][4], 0.0);
    }

    #[test]
    fn spectral_prior_keeps_the_neutral_fixed() {
        let data = training_data();
        let sensitivity = synthetic_sensitivity(data.grid(), [610.0, 545.0, 455.0], 45.0);
        for illuminant in SsfIlluminant::ALL {
            let matrix = ssf_matrix(&sensitivity, illuminant, data).expect("well-formed");
            let white = matrix.mul_vec([1.0; 3]);
            let target = Illuminant::D65.xyz().to_array();
            for index in 0..3 {
                assert!((white[index] - target[index]).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn a_dark_channel_is_rejected() {
        let data = training_data();
        let mut sensitivity = synthetic_sensitivity(data.grid(), [610.0, 545.0, 455.0], 45.0);
        sensitivity.channels[2]
            .iter_mut()
            .for_each(|value| *value = 0.0);
        assert!(matches!(
            ssf_matrix(&sensitivity, SsfIlluminant::D65, data),
            Err(BodyError::SsfWhiteResponse)
        ));
    }
}
