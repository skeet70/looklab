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

    /// `ucs` compressed smoothly into the gamut: first lightness, then
    /// colourfulness, each over a band that starts inside its in-gamut ceiling.
    ///
    /// Lightness has to move for colours whose neutral at the target `J'` is
    /// already outside the output cube; compressing `M'` alone would collapse
    /// them onto white with a jump.
    fn gamut_map(&self, ucs: Ucs) -> Ucs {
        let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
        let linear = self.space.linear_from_xyz(ucs_to_xyz(ucs));
        let safe = in_gamut(ucs, self.space)
            && linear
                .iter()
                .all(|value| value.is_finite() && *value <= 0.9);
        if safe {
            return ucs;
        }
        let lightness = lightness_ceiling(jmh.j, self.space);
        let start = (lightness - LIGHTNESS_BAND).max(0.0);
        let j = if jmh.j <= start || lightness <= start {
            jmh.j.min(lightness)
        } else {
            let band = lightness - start;
            start + band * libm::tanh((jmh.j - start) / band)
        };
        let base = looklab_color::cam16::Jmh {
            j,
            m: jmh.m.max(0.0),
            h: jmh.h,
        };
        let m = base.m;
        let inside = in_gamut(chroma_at(base, m), self.space);
        let ceiling = if inside {
            upper_boundary(base, m, self.space)
        } else {
            lower_boundary(base, m, self.space)
        };
        let start = GAMUT_START * ceiling;
        if m <= start {
            return chroma_at(base, m);
        }
        let band = ceiling - start;
        if band <= 0.0 {
            return chroma_at(base, start);
        }
        chroma_at(base, start + band * libm::tanh((m - start) / band))
    }

    /// The output codes that reproduce `look` at `xyz`.
    pub fn target(&self, look: &Look, xyz: Xyz) -> TargetOutput {
        let ucs = look.eval(xyz);
        let clipped = self.hard_clip && !in_gamut(ucs, self.space);
        let ucs = if self.hard_clip {
            self.gamut_map(ucs)
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
    let ceiling = lower_boundary(jmh, jmh.m.max(0.0), space);
    chroma_at(jmh, ceiling)
}

/// Fraction of the in-gamut `M'` ceiling that passes through unchanged.
const GAMUT_START: f64 = 0.85;

/// Width of the lightness compression band, in CAM16-UCS `J'` units.
const LIGHTNESS_BAND: f64 = 4.0;

/// The largest `J'` whose neutral is inside the output cube.
fn lightness_ceiling(j: f64, space: OutputSpace) -> f64 {
    let neutral = |j: f64| ucs_from_jmh(looklab_color::cam16::Jmh { j, m: 0.0, h: 0.0 });
    let mut inside = 0.0_f64;
    let mut outside = j.max(0.0);
    if !in_gamut(neutral(outside), space) {
        for _ in 0..32 {
            let middle = 0.5 * (inside + outside);
            if in_gamut(neutral(middle), space) {
                inside = middle;
            } else {
                outside = middle;
            }
        }
        return inside;
    }
    inside = outside;
    outside = (2.0 * inside).max(inside + LIGHTNESS_BAND);
    for _ in 0..16 {
        if !in_gamut(neutral(outside), space) {
            for _ in 0..32 {
                let middle = 0.5 * (inside + outside);
                if in_gamut(neutral(middle), space) {
                    inside = middle;
                } else {
                    outside = middle;
                }
            }
            return inside;
        }
        inside = outside;
        outside *= 2.0;
    }
    inside
}

fn chroma_at(jmh: looklab_color::cam16::Jmh, m: f64) -> Ucs {
    ucs_from_jmh(looklab_color::cam16::Jmh {
        j: jmh.j,
        m,
        h: jmh.h,
    })
}

/// The largest colourfulness in gamut at `jmh`'s lightness and hue, at or below
/// `m`. `m` itself must be out of gamut.
fn lower_boundary(jmh: looklab_color::cam16::Jmh, m: f64, space: OutputSpace) -> f64 {
    let mut inside = 0.0_f64;
    let mut outside = m;
    if !in_gamut(chroma_at(jmh, inside), space) {
        return inside;
    }
    for _ in 0..32 {
        let middle = 0.5 * (inside + outside);
        if in_gamut(chroma_at(jmh, middle), space) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    inside
}

/// The largest colourfulness in gamut at `jmh`'s lightness and hue, at or above
/// `m`. `m` itself must be in gamut.
fn upper_boundary(jmh: looklab_color::cam16::Jmh, m: f64, space: OutputSpace) -> f64 {
    let mut inside = m;
    let mut outside = (2.0 * m).max(m + 10.0);
    let mut found = false;
    for _ in 0..16 {
        if !in_gamut(chroma_at(jmh, outside), space) {
            found = true;
            break;
        }
        inside = outside;
        outside *= 2.0;
    }
    if !found {
        return inside;
    }
    for _ in 0..32 {
        let middle = 0.5 * (inside + outside);
        if in_gamut(chroma_at(jmh, middle), space) {
            inside = middle;
        } else {
            outside = middle;
        }
    }
    inside
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
