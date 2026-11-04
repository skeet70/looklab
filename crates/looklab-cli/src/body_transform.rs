// SPDX-License-Identifier: AGPL-3.0-or-later
//! The body colour transform file: DNG calibration matrices or one fixed
//! camera-to-XYZ matrix, plus optional per-image matrices.
//!
//! `color_matrices`/`forward_matrices` are a DNG colour profile
//! ([DNG 1.6 chapter 6](https://www.adobe.com/go/dng_spec)).

use std::path::Path;

use anyhow::{Context, Result, bail};
use looklab_color::Mat3;
use looklab_color::dng::{CalibrationMatrix, DngColorSpec, calibration_temperature};
use looklab_export::{CctK, DngInputTransform, InputTransform};
use looklab_prep::PriorTransform;
use looklab_samples::PriorTransformVersion;
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "looklab-transform/1";

fn format_id() -> String {
    FORMAT.to_owned()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformFile {
    #[serde(default = "format_id")]
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera_to_xyz: Option<Mat3>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub color_matrices: Vec<XyzToCamera>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forward_matrices: Vec<CameraToXyz>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub per_image: Vec<PerImageTransform>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XyzToCamera {
    pub cct: f64,
    pub xyz_to_camera: Mat3,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraToXyz {
    pub cct: f64,
    pub camera_to_xyz: Mat3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerImageTransform {
    pub file: String,
    pub camera_to_xyz: Mat3,
}

impl TransformFile {
    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = serde_norway::from_str(text).context("parsing transform YAML")?;
        file.validate()?;
        Ok(file)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| path.display().to_string())
    }

    pub fn to_yaml(&self) -> Result<String> {
        Ok(serde_norway::to_string(self)?)
    }

    fn validate(&self) -> Result<()> {
        if self.format != FORMAT {
            bail!("transform format {:?}, expected {FORMAT:?}", self.format);
        }
        if self.version == 0 {
            bail!("transform version must be at least 1");
        }
        if let Some(cct) = self.cct
            && !(cct.is_finite() && cct > 0.0)
        {
            bail!("transform cct {cct} is not a positive temperature");
        }
        for cct in self
            .color_matrices
            .iter()
            .map(|entry| entry.cct)
            .chain(self.forward_matrices.iter().map(|entry| entry.cct))
        {
            if !(cct.is_finite() && cct > 0.0) {
                bail!("calibration cct {cct} is not a positive temperature");
            }
        }
        if self.camera_to_xyz.is_none()
            && self.color_matrices.is_empty()
            && self.forward_matrices.is_empty()
            && self.per_image.is_empty()
        {
            bail!("transform file has no matrices");
        }
        Ok(())
    }

    pub fn prior_transform(&self) -> Result<PriorTransform> {
        let version = PriorTransformVersion::new(self.version);
        if let Some(matrix) = self.camera_to_xyz {
            return Ok(PriorTransform::new(
                matrix,
                self.cct.unwrap_or(0.0),
                version,
            ));
        }
        let spec = self.spec()?.context(
            "a body prior needs `camera_to_xyz` when no DNG colour matrices are present",
        )?;
        let cct = self.cct.unwrap_or(CctK::D65.kelvin());
        let matrix = spec
            .camera_to_xyz_d65(cct)
            .with_context(|| format!("no camera-to-XYZ matrix at {cct}K"))?;
        Ok(PriorTransform::new(matrix, cct, version))
    }

    pub fn input_transform(&self) -> Result<BodyTransform> {
        if let Some(spec) = self.spec()? {
            return Ok(BodyTransform(TransformSource::Dng(DngInputTransform::new(
                spec,
            )?)));
        }
        match self.camera_to_xyz {
            Some(matrix) => Ok(BodyTransform(TransformSource::Fixed(matrix))),
            None => bail!("transform file has no camera matrices usable for a bake"),
        }
    }

    /// Camera-to-XYZ of one image. A DNG profile resolves the matrix at the
    /// sample's CCT and scales it by the inverse white balance, matching the
    /// prior that prep used for density sampling.
    pub fn image_matrix(&self, file: &str, wb: [f32; 3]) -> Result<Mat3> {
        if let Some(entry) = self.per_image.iter().find(|entry| entry.file == file) {
            return Ok(entry.camera_to_xyz);
        }
        if let Some(spec) = self.spec()? {
            return matrix_at_wb(&spec, wb);
        }
        match self.camera_to_xyz {
            Some(matrix) => Ok(matrix),
            None => bail!("no transform for {}", file),
        }
    }

    pub fn spec(&self) -> Result<Option<DngColorSpec>> {
        if self.color_matrices.is_empty() && self.forward_matrices.is_empty() {
            return Ok(None);
        }
        let color = self
            .color_matrices
            .iter()
            .filter_map(|entry| CalibrationMatrix::new(entry.cct, entry.xyz_to_camera))
            .collect();
        let forward = self
            .forward_matrices
            .iter()
            .filter_map(|entry| CalibrationMatrix::new(entry.cct, entry.camera_to_xyz))
            .collect();
        let spec = DngColorSpec::new(color, forward)
            .context("building the DNG colour profile from the transform file")?;
        Ok(Some(spec))
    }

    /// The transform file for a decoded RAW's colour matrices.
    pub fn from_color_matrices(
        matrices: &looklab_raw::ColorMatrices,
        body: Option<String>,
        version: u32,
    ) -> Result<Self> {
        let color_matrices = matrices
            .matrices()
            .iter()
            .filter_map(|matrix| {
                let cct = calibration_temperature(matrix.illuminant.code())?;
                Some(XyzToCamera {
                    cct,
                    xyz_to_camera: matrix.xyz_to_camera,
                })
            })
            .collect();
        let forward_matrices = matrices
            .forward_matrices()
            .iter()
            .filter_map(|matrix| {
                let cct = calibration_temperature(matrix.illuminant.code())?;
                Some(CameraToXyz {
                    cct,
                    camera_to_xyz: matrix.camera_to_xyz,
                })
            })
            .collect();
        let file = Self {
            format: format_id(),
            version,
            body,
            cct: None,
            camera_to_xyz: None,
            color_matrices,
            forward_matrices,
            per_image: Vec::new(),
        };
        if file.color_matrices.is_empty() && file.forward_matrices.is_empty() {
            bail!("the RAW carries no usable colour matrices");
        }
        Ok(file)
    }
}

/// `DngColorSpec::from_neutral` plus prep's white-balance scaling.
fn matrix_at_wb(spec: &DngColorSpec, wb: [f32; 3]) -> Result<Mat3> {
    let wb = wb.map(f64::from);
    for value in wb {
        if !(value.is_finite() && value > 0.0) {
            bail!("white-balance multiplier {value} is not positive");
        }
    }
    let neutral = [1.0 / wb[0], 1.0, 1.0 / wb[2]];
    let d65 = spec
        .from_neutral(neutral)
        .context("solving the camera white point from the sample's WB multipliers")?;
    let balance = Mat3::diag([1.0 / wb[0], 1.0 / wb[1], 1.0 / wb[2]]);
    Ok(d65.matrix.mul(&balance))
}

#[derive(Debug)]
pub struct BodyTransform(TransformSource);

#[derive(Debug)]
enum TransformSource {
    Fixed(Mat3),
    Dng(DngInputTransform),
}

impl InputTransform for BodyTransform {
    fn camera_to_xyz(&self, cct: CctK) -> Option<Mat3> {
        match &self.0 {
            TransformSource::Fixed(matrix) => Some(*matrix),
            TransformSource::Dng(transform) => transform.camera_to_xyz(cct),
        }
    }
}
