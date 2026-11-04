// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical preview scenes: scene-linear D65 XYZ in little-endian half floats (docs/08 §5).

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::PublishError;
use crate::hash::SitePath;

pub const SCENE_FORMAT: &str = "looklab-scene/xyz-f16le/1";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SceneName(String);

impl SceneName {
    /// # Errors
    /// Rejects names outside `[a-z0-9_-]`.
    pub fn new(value: impl Into<String>) -> Result<Self, PublishError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(PublishError::Path(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SceneName {
    type Error = PublishError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SceneName> for String {
    fn from(value: SceneName) -> Self {
        value.0
    }
}

impl fmt::Display for SceneName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSummary {
    pub name: SceneName,
    pub width: u32,
    pub height: u32,
    pub path: SitePath,
    pub sha256: crate::ArtifactHash,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SceneIndex {
    pub format: String,
    pub scenes: Vec<SceneSummary>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub name: SceneName,
    pub width: u32,
    pub height: u32,
    pub xyz: Vec<[f32; 3]>,
}

impl Scene {
    /// # Errors
    /// Rejects a `xyz` length that differs from `width * height`.
    pub fn new(
        name: SceneName,
        width: u32,
        height: u32,
        xyz: Vec<[f32; 3]>,
    ) -> Result<Self, PublishError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .ok_or(PublishError::SceneDimensions)?;
        if xyz.len() != expected || expected == 0 {
            return Err(PublishError::SceneDimensions);
        }
        Ok(Self {
            name,
            width,
            height,
            xyz,
        })
    }

    /// # Errors
    /// Rejects a byte length that does not match the scene dimensions or a
    /// value that is not finite.
    pub fn from_f16_bytes(
        name: SceneName,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<Self, PublishError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(3))
            .ok_or(PublishError::SceneDimensions)?;
        if bytes.len() != expected * 2 {
            return Err(PublishError::SceneDimensions);
        }
        let mut xyz = Vec::with_capacity(expected);
        for (index, chunk) in bytes.as_chunks::<2>().0.iter().enumerate() {
            let value = f16_to_f32(u16::from_le_bytes(*chunk));
            if !value.is_finite() {
                return Err(PublishError::SceneValue);
            }
            if index % 3 == 0 {
                xyz.push([0.0; 3]);
            }
            xyz[index / 3][index % 3] = value;
        }
        Self::new(name, width, height, xyz)
    }

    pub fn to_f16_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.xyz.len() * 6);
        for pixel in &self.xyz {
            for channel in pixel {
                out.extend_from_slice(&f32_to_f16(*channel).to_le_bytes());
            }
        }
        out
    }
}

/// IEEE 754 binary16 bits of a finite `f32`, rounded to nearest even.
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;

    if exponent == 0xff {
        return sign | 0x7c00 | if mantissa != 0 { 0x0200 } else { 0 };
    }
    let half_exponent = exponent - 127 + 15;
    if half_exponent >= 0x1f {
        return sign | 0x7c00;
    }
    if half_exponent <= 0 {
        if half_exponent < -10 {
            return sign;
        }
        let mantissa = mantissa | 0x0080_0000;
        let shift = (14 - half_exponent) as u32;
        let dropped = mantissa & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        let mut half = (mantissa >> shift) as u16;
        if dropped > halfway || (dropped == halfway && (half & 1) == 1) {
            half += 1;
        }
        return sign | half;
    }
    let mut half = (((half_exponent as u32) << 10) | (mantissa >> 13)) as u16;
    let dropped = mantissa & 0x1fff;
    if dropped > 0x1000 || (dropped == 0x1000 && (half & 1) == 1) {
        half = half.wrapping_add(1);
        if half & 0x7c00 == 0x7c00 {
            return sign | 0x7c00;
        }
    }
    sign | half
}

pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = ((bits & 0x8000) as u32) << 16;
    let exponent = (bits >> 10) & 0x1f;
    let mantissa = (bits & 0x03ff) as u32;
    let out = if exponent == 0 {
        if mantissa == 0 {
            sign
        } else {
            let mut exponent = -1i32;
            let mut mantissa = mantissa;
            loop {
                mantissa <<= 1;
                exponent += 1;
                if mantissa & 0x0400 != 0 {
                    break;
                }
            }
            sign | (((127 - 15 - exponent) as u32) << 23) | ((mantissa & 0x03ff) << 13)
        }
    } else if exponent == 0x1f {
        sign | 0x7f80_0000 | (mantissa << 13)
    } else {
        sign | (((exponent as u32) + 127 - 15) << 23) | (mantissa << 13)
    };
    f32::from_bits(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_float_round_trips_representable_values() {
        for value in [0.0, -0.0, 0.5, -2.5, 1.0, 65504.0, 1.0e-4] {
            let bits = f32_to_f16(value);
            let back = f16_to_f32(bits);
            if value == 0.0 {
                assert_eq!(back, value);
            } else {
                assert!((back - value).abs() <= value.abs() * 1.0e-3);
            }
        }
    }

    #[test]
    fn half_float_handles_subnormals_and_overflow() {
        assert_eq!(f32_to_f16(1.0e-8), 0);
        assert_eq!(f16_to_f32(1), 5.960_464_5e-8);
        assert_eq!(f32_to_f16(1.0e9) & 0x7fff, 0x7c00);
    }

    #[test]
    fn scene_round_trips_through_bytes() {
        let scene = Scene::new(
            SceneName::new("gradient").expect("name"),
            2,
            1,
            vec![[0.0, 0.18, 1.0], [0.5, 0.25, 0.0]],
        )
        .expect("scene");
        let bytes = scene.to_f16_bytes();
        assert_eq!(bytes.len(), 12);
        let decoded =
            Scene::from_f16_bytes(SceneName::new("gradient").expect("name"), 2, 1, &bytes)
                .expect("decode");
        assert_eq!(decoded.width, 2);
        assert_eq!(decoded.xyz.len(), 2);
        assert!((decoded.xyz[0][1] - 0.18).abs() < 1.0e-3);
    }
}
