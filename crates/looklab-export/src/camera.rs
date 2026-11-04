// SPDX-License-Identifier: AGPL-3.0-or-later
//! The body input transform `A_B(T)`: camera RGB to adapted scene XYZ (docs/02 §2).

use looklab_color::Mat3;
use looklab_color::dng::DngColorSpec;

use crate::cct::{BAKE_CCTS, CctK};
use crate::error::{ExportError, Result};

/// Camera-to-XYZ for a body at a scene CCT. `None` when the transform is undefined
/// at that temperature.
///
/// `camera_rgb_wb_linear` insertion values are white-balanced: a D65 neutral maps
/// to equal camera channels.
pub trait InputTransform {
    fn camera_to_xyz(&self, cct: CctK) -> Option<Mat3>;
}

/// `A_B(T)` from a DNG colour profile: DNG 1.6 chapter 6 interpolation in 1/T
/// and the camera-to-XYZ(D65) forward or inverted colour matrix.
#[derive(Clone, Debug, PartialEq)]
pub struct DngInputTransform {
    spec: DngColorSpec,
}

impl DngInputTransform {
    /// # Errors
    /// [`ExportError::TransformUndefined`] or [`ExportError::SingularMatrix`] when
    /// the profile has no usable transform at one of the bake CCTs.
    pub fn new(spec: DngColorSpec) -> Result<Self> {
        let transform = Self { spec };
        let mut ccts = BAKE_CCTS.to_vec();
        ccts.push(CctK::STD_A);
        ccts.push(CctK::D65);
        for cct in ccts {
            let matrix = transform
                .camera_to_xyz(cct)
                .ok_or(ExportError::TransformUndefined { cct: cct.kelvin() })?;
            if matrix.inverse().is_none() {
                return Err(ExportError::SingularMatrix { cct: cct.kelvin() });
            }
        }
        Ok(transform)
    }

    pub fn spec(&self) -> &DngColorSpec {
        &self.spec
    }
}

impl InputTransform for DngInputTransform {
    fn camera_to_xyz(&self, cct: CctK) -> Option<Mat3> {
        let matrix = self.spec.camera_to_xyz_d65(cct.kelvin()).ok()?;
        let inverse = matrix.inverse()?;
        let neutral = inverse.mul_vec(looklab_color::Illuminant::D65.xyz().to_array());
        if neutral
            .iter()
            .any(|value| !value.is_finite() || *value == 0.0)
        {
            return None;
        }
        Some(matrix.mul(&Mat3::diag(neutral)))
    }
}
