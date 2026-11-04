// SPDX-License-Identifier: AGPL-3.0-or-later
//! The declared downstream of the insertion point, and its inverse.

use looklab_color::cam16::Chroma;
use looklab_color::ucs::{Ucs, UcsSpace, ucs_to_jmh};
use looklab_color::{RgbSpace, Xyz};
use looklab_model::Look;
use looklab_model::clip::output_encode_clip;
use looklab_model::solve;
use looklab_schema::{
    Downstream, GamutClip, InterfaceProfile, OutputTransform, Primaries, TransferFunction,
};
use serde::{Deserialize, Serialize};

use crate::bake::ShaperParams;
use crate::error::{ExportError, Result};

/// CAM16-UCS `J'a'b'` to XYZ under the reference viewing conditions.
pub fn ucs_to_xyz(ucs: Ucs) -> Xyz {
    let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
    solve::reference_model().cam16_to_xyz(jmh.j, Chroma::M(jmh.m), jmh.h)
}

/// The primaries the firmware's output transform writes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSpace {
    Srgb,
    Rec2020,
    XyzD65,
}

impl OutputSpace {
    fn from_schema(primaries: Primaries) -> Result<Self> {
        match primaries {
            Primaries::Srgb => Ok(Self::Srgb),
            Primaries::Rec2020 => Ok(Self::Rec2020),
            Primaries::XyzD65 => Ok(Self::XyzD65),
            Primaries::CameraNative => Err(ExportError::Profile(
                "a downstream output transform cannot write camera-native primaries",
            )),
        }
    }

    /// The `RgbSpace` for the spaces that have one; XYZ has no transfer function.
    pub fn rgb_space(self) -> Option<RgbSpace> {
        match self {
            Self::Srgb => Some(RgbSpace::Srgb),
            Self::Rec2020 => Some(RgbSpace::Rec2020),
            Self::XyzD65 => None,
        }
    }

    fn linear_from_xyz(self, xyz: Xyz) -> [f64; 3] {
        match self.rgb_space() {
            Some(space) => space.xyz_to_rgb_linear(xyz).to_array(),
            None => xyz.to_array(),
        }
    }

    fn xyz_from_linear(self, linear: [f64; 3]) -> Xyz {
        match self.rgb_space() {
            Some(space) => space.rgb_linear_to_xyz(looklab_color::Rgb::from_array(linear)),
            None => Xyz::from_array(linear),
        }
    }
}

/// The transfer function applied by the downstream output transform.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transfer {
    Linear,
    Srgb,
    Log2(ShaperParams),
}

impl Transfer {
    fn from_schema(transfer: TransferFunction) -> Result<Self> {
        match transfer {
            TransferFunction::Linear => Ok(Self::Linear),
            TransferFunction::Srgb => Ok(Self::Srgb),
            TransferFunction::Log2Shaper => Ok(Self::Log2(ShaperParams::DEFAULT)),
            TransferFunction::Custom1d => {
                Err(ExportError::Unsupported("custom_1d downstream transfer"))
            }
        }
    }

    pub fn encode(self, linear: f64) -> f64 {
        match self {
            Self::Linear => linear,
            Self::Srgb => looklab_color::trc::srgb_encode(linear),
            Self::Log2(params) => params.to_code(linear),
        }
    }

    pub fn decode(self, encoded: f64) -> f64 {
        match self {
            Self::Linear => encoded,
            Self::Srgb => looklab_color::trc::srgb_decode(encoded),
            Self::Log2(params) => params.from_code(encoded),
        }
    }
}

/// The downstream stages declared by the profile.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownstreamModel {
    pub space: OutputSpace,
    pub transfer: Transfer,
    pub hard_clip: bool,
}

impl DownstreamModel {
    /// # Errors
    /// [`ExportError::Profile`] when no output transform is declared, or when the
    /// one declared names camera-native primaries.
    pub fn from_profile(profile: &InterfaceProfile) -> Result<Self> {
        let mut output: Option<&OutputTransform> = None;
        let mut hard_clip = false;
        for stage in &profile.downstream {
            match stage {
                Downstream::OutputTransform(transform) => output = Some(transform),
                Downstream::GamutClip(GamutClip::Hard) => hard_clip = true,
            }
        }
        let output = output.ok_or(ExportError::Profile(
            "downstream declares no output transform",
        ))?;
        Ok(Self {
            space: OutputSpace::from_schema(output.primaries)?,
            transfer: Transfer::from_schema(output.transfer)?,
            hard_clip,
        })
    }

    pub fn encode(&self, linear: [f64; 3]) -> [f64; 3] {
        linear.map(|value| self.transfer.encode(value))
    }

    pub fn decode(&self, encoded: [f64; 3]) -> [f64; 3] {
        encoded.map(|value| self.transfer.decode(value))
    }

    /// Clamps to `[0, 1]` when the profile declares a hard gamut clip.
    pub fn clip(&self, encoded: [f64; 3]) -> [f64; 3] {
        if self.hard_clip {
            encoded.map(|value| value.clamp(0.0, 1.0))
        } else {
            encoded
        }
    }

    /// The appearance a firmware shows for output codes it already clipped.
    pub fn appearance(&self, encoded: [f64; 3]) -> Ucs {
        let xyz = self.space.xyz_from_linear(self.decode(encoded));
        solve::ucs_of_xyz(xyz)
    }

    /// The output codes that reproduce `look` at `xyz`.
    pub fn target(&self, look: &Look, xyz: Xyz) -> TargetOutput {
        let ucs = look.eval(xyz);
        let clipped = self.hard_clip && !in_gamut(ucs, self.space);
        let ucs = if clipped {
            perceptual_clip(ucs, self.space)
        } else {
            ucs
        };
        let ucs = match self.space.rgb_space() {
            Some(space) => output_encode_clip(ucs, space, look.clip.k),
            None => ucs,
        };
        let codes = self.encode(self.space.linear_from_xyz(ucs_to_xyz(ucs)));
        TargetOutput {
            codes: self.clip(codes),
            clipped,
        }
    }
}

/// The output codes a look asks for, and whether getting them needed perceptual
/// clipping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetOutput {
    pub codes: [f64; 3],
    pub clipped: bool,
}

/// Tolerance on the `[0, 1]` cube boundary.
const GAMUT_TOLERANCE: f64 = 1.0e-6;

/// Whether `ucs` maps inside the output space's `[0, 1]` cube.
pub fn in_gamut(ucs: Ucs, space: OutputSpace) -> bool {
    let linear = space.linear_from_xyz(ucs_to_xyz(ucs));
    linear.iter().all(|value| {
        value.is_finite() && (-GAMUT_TOLERANCE..=1.0 + GAMUT_TOLERANCE).contains(value)
    })
}

/// `ucs` with colourfulness reduced at constant `J'` and `h` until its linear
/// output RGB is inside `[0, 1]`, by bisection.
pub fn perceptual_clip(ucs: Ucs, space: OutputSpace) -> Ucs {
    if in_gamut(ucs, space) {
        return ucs;
    }
    let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
    let chroma = |m: f64| {
        ucs_from_jmh(looklab_color::cam16::Jmh {
            j: jmh.j,
            m,
            h: jmh.h,
        })
    };
    let mut inside = 0.0_f64;
    let mut outside = jmh.m.max(0.0);
    if !in_gamut(chroma(inside), space) {
        return chroma(inside);
    }
    for _ in 0..32 {
        let middle = 0.5 * (inside + outside);
        if in_gamut(chroma(middle), space) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    chroma(inside)
}

fn ucs_from_jmh(jmh: looklab_color::cam16::Jmh) -> Ucs {
    looklab_color::ucs::jmh_to_ucs(jmh, UcsSpace::Ucs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_of_gamut() -> Ucs {
        Ucs::new(55.0, 70.0, -45.0)
    }

    #[test]
    fn perceptual_clip_keeps_lightness_and_hue() {
        let original = out_of_gamut();
        assert!(!in_gamut(original, OutputSpace::Srgb));
        let clipped = perceptual_clip(original, OutputSpace::Srgb);
        assert!(in_gamut(clipped, OutputSpace::Srgb));

        let before = ucs_to_jmh(original, UcsSpace::Ucs);
        let after = ucs_to_jmh(clipped, UcsSpace::Ucs);
        assert!((before.j - after.j).abs() < 1.0e-9);
        let hue = |jmh: looklab_color::cam16::Jmh| {
            let mut h = libm::atan2(
                libm::sin(jmh.h * core::f64::consts::PI / 180.0),
                libm::cos(jmh.h * core::f64::consts::PI / 180.0),
            );
            if h < 0.0 {
                h += 2.0 * core::f64::consts::PI;
            }
            h
        };
        assert!((hue(before) - hue(after)).abs() < 1.0e-6);
        assert!(after.m < before.m);
    }

    #[test]
    fn in_gamut_values_are_unchanged() {
        let ucs = solve::ucs_of_xyz(Xyz::new(0.3, 0.4, 0.2));
        assert_eq!(perceptual_clip(ucs, OutputSpace::Srgb), ucs);
    }

    #[test]
    fn target_uses_the_declared_output_transform() {
        let profile = looklab_schema::validate_interface_profile(
            "id: t/p\nname: t\nversion: 1\ninsertion:\n  point: scene_linear_xyz_d65\n  encoding:\n    transfer: linear\n    primaries: xyz_d65\n    range: [0.0, 1.0]\n  illuminant_handling: single\naccepts:\n  - type: matrix3x3\ndownstream:\n  - output_transform: { primaries: srgb, transfer: srgb }\n  - gamut_clip: hard\nformats: [cube]\nlimits: { max_bytes: 1024 }\n",
        )
        .expect("profile");
        let model = DownstreamModel::from_profile(&profile).expect("downstream");
        assert_eq!(model.space, OutputSpace::Srgb);
        assert_eq!(model.transfer, Transfer::Srgb);
        assert!(model.hard_clip);
        let white = model.encode([1.0, 0.0, 0.0]);
        assert!((white[0] - 1.0).abs() < 1.0e-12);
        assert_eq!(white[1], 0.0);
        assert!((model.encode([0.18, 0.18, 0.18])[0] - 0.461_356).abs() < 1.0e-5);
    }
}
