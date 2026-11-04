// SPDX-License-Identifier: AGPL-3.0-or-later
//! Baking: `F = Downstream⁻¹ ∘ OutputEncode ∘ CAM16UCS⁻¹ ∘ L ∘ ToSceneXYZ ∘ Decode_P`
//! (docs/04 §7).
//!
//! An artifact value is linear RGB in the downstream output primaries. The
//! firmware applies the output transform's transfer and the declared gamut clip,
//! which [`DownstreamModel`] models for the simulator and inverts for the bake.

use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_model::probes;
use looklab_model::{Look, Tone};
use looklab_schema::{
    AcceptedArtifact, InsertionPoint, InterfaceProfile, Interpolation, Precision, Primaries,
    TransferFunction,
};
use serde::{Deserialize, Serialize};

use crate::artifact::{Artifact, Curve1d, Lut3d, Matrix3x3, Shaper1d};
use crate::camera::InputTransform;
use crate::cct::{BAKE_CCTS, CctK};
use crate::downstream::DownstreamModel;
use crate::error::{ExportError, Result};
use crate::writer;

/// The shaper point count used for the emitted 1D LUT.
pub const SHAPER_POINTS: u32 = 4096;

/// Number of knots in a `curve1d` artifact at most.
const CURVE_POINTS: u32 = 65;

/// Log2 shaper parameters: `s = log2(linear / mid_gray)` is mapped from
/// `min_ev` to `max_ev` onto the artifact's `[0, 1]` input domain.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShaperParams {
    pub min_ev: f64,
    pub max_ev: f64,
    pub mid_gray: f64,
}

impl ShaperParams {
    pub const DEFAULT: Self = Self {
        min_ev: -12.0,
        max_ev: 6.0,
        mid_gray: 0.18,
    };

    pub fn from_schema(shaper: &looklab_schema::Shaper) -> Self {
        Self {
            min_ev: shaper.min_ev.to_f64(),
            max_ev: shaper.max_ev.to_f64(),
            mid_gray: shaper.mid_gray.to_f64(),
        }
    }

    /// The artifact coordinate of a linear value; `-inf` maps to the bottom.
    pub fn to_code(self, linear: f64) -> f64 {
        if linear <= 0.0 || !linear.is_finite() {
            return 0.0;
        }
        let ev = libm::log2(linear / self.mid_gray);
        (ev - self.min_ev) / (self.max_ev - self.min_ev)
    }

    pub fn from_code(self, code: f64) -> f64 {
        self.mid_gray * libm::exp2(self.min_ev + code * (self.max_ev - self.min_ev))
    }
}

impl ShaperParams {
    fn is_finite(self) -> bool {
        self.min_ev.is_finite()
            && self.max_ev.is_finite()
            && self.mid_gray.is_finite()
            && self.max_ev > self.min_ev
            && self.mid_gray > 0.0
    }
}

/// The insertion point and encoding read from a profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InsertionSpec {
    #[serde(with = "crate::serde_names::point")]
    pub point: InsertionPoint,
    #[serde(with = "crate::serde_names::transfer")]
    pub transfer: TransferFunction,
    #[serde(with = "crate::serde_names::primaries")]
    pub primaries: Primaries,
    /// The declared code range. Only a `linear` transfer scales values by it; the
    /// other transfers are defined on `[0, 1]` codes.
    pub range: [f64; 2],
    pub shaper: Option<ShaperParams>,
    /// `custom_1d` samples, uniformly spaced over the normalized code in `[0, 1]`.
    pub custom_curve: Option<Vec<f32>>,
}

impl InsertionSpec {
    /// # Errors
    /// [`ExportError::InvalidInsertion`] for a camera-native encoding outside
    /// `camera_rgb_wb_linear`, [`ExportError::MissingCustomCurve`] for `custom_1d`.
    pub fn from_profile(profile: &InterfaceProfile, custom_curve: Option<&[f64]>) -> Result<Self> {
        let insertion = &profile.insertion;
        let encoding = &insertion.encoding;
        if (encoding.primaries == Primaries::CameraNative)
            != (insertion.point == InsertionPoint::CameraRgbWbLinear)
        {
            return Err(ExportError::InvalidInsertion {
                point: crate::serde_names::point::name(insertion.point),
                detail: "camera-native primaries",
            });
        }
        let range = [encoding.range.min.to_f64(), encoding.range.max.to_f64()];
        if range[1] <= range[0] || !(range[1] - range[0]).is_finite() {
            return Err(ExportError::Profile("insertion range is empty"));
        }
        let shaper = encoding.shaper.as_ref().map(ShaperParams::from_schema);
        if let Some(params) = shaper
            && !params.is_finite()
        {
            return Err(ExportError::Profile("log2 shaper parameters are invalid"));
        }
        let custom_curve = match encoding.transfer {
            TransferFunction::Custom1d => Some(
                custom_curve
                    .ok_or(ExportError::MissingCustomCurve)?
                    .iter()
                    .map(|value| *value as f32)
                    .collect(),
            ),
            _ => None,
        };
        Ok(Self {
            point: insertion.point,
            transfer: encoding.transfer,
            primaries: encoding.primaries,
            range,
            shaper,
            custom_curve,
        })
    }

    pub fn needs_camera(&self) -> bool {
        self.primaries == Primaries::CameraNative
    }

    fn shaper_params(&self) -> ShaperParams {
        self.shaper.unwrap_or(ShaperParams::DEFAULT)
    }

    pub fn decode_channel(&self, code: f64) -> f64 {
        match self.transfer {
            TransferFunction::Linear => self.range[0] + code * (self.range[1] - self.range[0]),
            TransferFunction::Srgb => looklab_color::trc::srgb_decode(code),
            TransferFunction::Log2Shaper => self.shaper_params().from_code(code),
            TransferFunction::Custom1d => {
                let curve = self.custom_curve.as_deref().unwrap_or(&[0.0, 1.0]);
                interpolate_samples(curve, code)
            }
        }
    }

    pub fn encode_channel(&self, linear: f64) -> f64 {
        match self.transfer {
            TransferFunction::Linear => (linear - self.range[0]) / (self.range[1] - self.range[0]),
            TransferFunction::Srgb => looklab_color::trc::srgb_encode(linear),
            TransferFunction::Log2Shaper => self.shaper_params().to_code(linear),
            TransferFunction::Custom1d => {
                invert_samples(self.custom_curve.as_deref().unwrap_or(&[0.0, 1.0]), linear)
            }
        }
    }

    pub fn decode(&self, code: [f64; 3]) -> [f64; 3] {
        code.map(|value| self.decode_channel(value))
    }

    pub fn encode(&self, linear: [f64; 3]) -> [f64; 3] {
        linear.map(|value| self.encode_channel(value))
    }

    /// Linear insertion primaries to adapted scene XYZ.
    ///
    /// # Errors
    /// [`ExportError::TransformUndefined`] for a camera-native encoding without a
    /// body matrix.
    pub fn to_xyz(&self, linear: [f64; 3], camera: Option<Mat3>) -> Result<Xyz> {
        Ok(match self.primaries {
            Primaries::CameraNative => {
                let matrix = camera.ok_or(ExportError::Profile(
                    "camera-native insertion needs a body matrix",
                ))?;
                Xyz::from_array(matrix.mul_vec(linear))
            }
            Primaries::Rec2020 => RgbSpace::Rec2020.rgb_linear_to_xyz(Rgb::from_array(linear)),
            Primaries::Srgb => RgbSpace::Srgb.rgb_linear_to_xyz(Rgb::from_array(linear)),
            Primaries::XyzD65 => Xyz::from_array(linear),
        })
    }

    /// Adapted scene XYZ to linear insertion primaries.
    ///
    /// # Errors
    /// [`ExportError::TransformUndefined`] when the body matrix is missing and
    /// the encoding is camera-native.
    pub fn from_xyz(&self, xyz: Xyz, camera: Option<Mat3>) -> Result<[f64; 3]> {
        Ok(match self.primaries {
            Primaries::CameraNative => {
                let matrix = camera.ok_or(ExportError::Profile(
                    "camera-native insertion needs a body matrix",
                ))?;
                let inverse = matrix
                    .inverse()
                    .ok_or(ExportError::Profile("the body matrix is singular"))?;
                inverse.mul_vec(xyz.to_array())
            }
            Primaries::Rec2020 => RgbSpace::Rec2020.xyz_to_rgb_linear(xyz).to_array(),
            Primaries::Srgb => RgbSpace::Srgb.xyz_to_rgb_linear(xyz).to_array(),
            Primaries::XyzD65 => xyz.to_array(),
        })
    }
}

fn interpolate_samples(samples: &[f32], code: f64) -> f64 {
    if samples.len() < 2 {
        return code;
    }
    let x = code.clamp(0.0, 1.0) * (samples.len() - 1) as f64;
    let lower = x.floor() as usize;
    let index = lower.min(samples.len() - 2);
    let fraction = x - index as f64;
    let a = samples[index] as f64;
    let b = samples[index + 1] as f64;
    a + fraction * (b - a)
}

/// Inverse of a monotone 1D curve by bisection on the segment that brackets `value`.
fn invert_samples(samples: &[f32], value: f64) -> f64 {
    if samples.len() < 2 {
        return value;
    }
    let last = samples.len() - 1;
    let ascending = samples[last] as f64 >= samples[0] as f64;
    if (value < samples[0] as f64) == ascending {
        return 0.0;
    }
    if (value > samples[last] as f64) == ascending {
        return 1.0;
    }
    let mut low = 0.0_f64;
    let mut high = 1.0_f64;
    for _ in 0..48 {
        let middle = 0.5 * (low + high);
        let sample = interpolate_samples(samples, middle);
        if (sample < value) == ascending {
            low = middle;
        } else {
            high = middle;
        }
    }
    0.5 * (low + high)
}

/// Which illuminant(s) a bake targets.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BakeIlluminant {
    /// One artifact at one CCT; the firmware does not adapt.
    Single { cct: CctK },
    /// Two artifacts at StdA and D65; the firmware interpolates in 1/T.
    DualInterpolated,
}

impl BakeIlluminant {
    pub fn ccts(self) -> Vec<CctK> {
        match self {
            Self::Single { cct } => vec![cct],
            Self::DualInterpolated => vec![CctK::STD_A, CctK::D65],
        }
    }

    /// The CCT the validation report is keyed on.
    pub fn primary(self) -> CctK {
        match self {
            Self::Single { cct } => cct,
            Self::DualInterpolated => CctK::D65,
        }
    }

    /// The CCTs validation evaluates: the bake CCTs plus the other single-illuminant
    /// reporting CCTs.
    pub fn report_ccts(self) -> Vec<CctK> {
        match self {
            Self::Single { cct } => {
                let mut all = vec![cct];
                all.extend(BAKE_CCTS.iter().copied().filter(|other| *other != cct));
                all
            }
            Self::DualInterpolated => vec![CctK::STD_A, CctK::D65, CctK::DAYLIGHT],
        }
    }
}

/// One artifact and the CCT it was baked at.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactAtCct {
    pub cct: CctK,
    pub artifact: Artifact,
}

/// Everything a baked export carries besides the bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct BakeOutput {
    pub profile_id: looklab_schema::InterfaceId,
    pub profile_version: u32,
    pub bake: BakeIlluminant,
    pub insertion: InsertionSpec,
    pub downstream: DownstreamModel,
    pub interpolation: Option<Interpolation>,
    pub precision: Option<Precision>,
    pub artifacts: Vec<ArtifactAtCct>,
}

impl BakeOutput {
    pub fn artifact(&self, cct: CctK) -> Option<&Artifact> {
        self.artifacts
            .iter()
            .find(|entry| entry.cct == cct)
            .map(|entry| &entry.artifact)
    }

    pub fn primary_artifact(&self) -> Option<&Artifact> {
        self.artifact(self.bake.primary())
    }

    pub fn kind(&self) -> &'static str {
        match self.primary_artifact() {
            Some(artifact) => artifact.kind(),
            None => "none",
        }
    }
}

/// The look a bake can realize at `point`. A `display_encoded_post_tone` insertion
/// sits after the camera's own tone mapping, so only the colour layers survive.
pub fn baked_look(look: &Look, point: InsertionPoint) -> Look {
    if point == InsertionPoint::DisplayEncodedPostTone {
        let mut partial = look.clone();
        partial.tone = Tone::Colorimetric;
        partial
    } else {
        look.clone()
    }
}

struct LutPlan {
    size: u32,
    interpolation: Interpolation,
    precision: Precision,
    shaper: Option<ShaperParams>,
}

pub struct Baker<'a> {
    profile: &'a InterfaceProfile,
    look: &'a Look,
    transform: &'a dyn InputTransform,
    custom_curve: Option<&'a [f64]>,
}

impl<'a> Baker<'a> {
    pub fn new(
        profile: &'a InterfaceProfile,
        look: &'a Look,
        transform: &'a dyn InputTransform,
    ) -> Self {
        Self {
            profile,
            look,
            transform,
            custom_curve: None,
        }
    }

    pub fn with_custom_curve(mut self, curve: &'a [f64]) -> Self {
        self.custom_curve = Some(curve);
        self
    }

    /// # Errors
    /// [`ExportError::InvalidClip`] for out-of-range clip settings, and the
    /// selection errors of [`Self::bake_with`].
    pub fn bake(&self, illuminant: BakeIlluminant) -> Result<BakeOutput> {
        if !self.look.clip.validate() {
            return Err(ExportError::InvalidClip);
        }
        let insertion = InsertionSpec::from_profile(self.profile, self.custom_curve)?;
        let downstream = DownstreamModel::from_profile(self.profile)?;
        let look = baked_look(self.look, insertion.point);

        let main = self.bake_main(&insertion, &downstream, &look, illuminant)?;
        Ok(match main {
            MainArtifact::Lut(plan, artifacts) => BakeOutput {
                profile_id: self.profile.id.clone(),
                profile_version: self.profile.version,
                bake: illuminant,
                insertion,
                downstream,
                interpolation: Some(plan.interpolation),
                precision: Some(plan.precision),
                artifacts,
            },
            MainArtifact::Fit(artifacts) => BakeOutput {
                profile_id: self.profile.id.clone(),
                profile_version: self.profile.version,
                bake: illuminant,
                insertion,
                downstream,
                interpolation: None,
                precision: None,
                artifacts,
            },
        })
    }

    fn bake_main(
        &self,
        insertion: &InsertionSpec,
        downstream: &DownstreamModel,
        look: &Look,
        illuminant: BakeIlluminant,
    ) -> Result<MainArtifact> {
        let luts: Vec<(&looklab_schema::Lut3d, u32)> = self
            .profile
            .accepts
            .iter()
            .filter_map(|accepted| match accepted {
                AcceptedArtifact::Lut3d(lut) => Some(lut),
                AcceptedArtifact::Matrix3x3(_) | AcceptedArtifact::Curve1d(_) => None,
            })
            .flat_map(|lut| lut.sizes.iter().map(move |size| (lut, *size)))
            .collect();

        if !luts.is_empty() {
            let with_shaper = luts.iter().any(|(lut, _)| lut.with_shaper);
            let shaper = (with_shaper && insertion.transfer != TransferFunction::Log2Shaper)
                .then(|| insertion.shaper_params());
            let mut sizes: Vec<u32> = luts.iter().map(|(_, size)| *size).collect();
            sizes.sort_unstable_by(|a, b| b.cmp(a));
            sizes.dedup();

            let mut needed = u64::MAX;
            for size in sizes {
                let spec = luts
                    .iter()
                    .find(|(_, candidate)| *candidate == size)
                    .map(|(lut, _)| *lut)
                    .ok_or(ExportError::NoAcceptedArtifact)?;
                let plan = LutPlan {
                    size,
                    interpolation: spec.interpolation,
                    precision: spec.precision,
                    shaper,
                };
                let artifacts = self.bake_luts(&plan, insertion, downstream, look, illuminant)?;
                let bytes = self.measure(&artifacts, insertion, illuminant, plan.precision)?;
                needed = needed.min(bytes);
                if bytes <= self.profile.limits.max_bytes {
                    return Ok(MainArtifact::Lut(plan, artifacts));
                }
            }
            return Err(ExportError::ByteLimit {
                needed,
                limit: self.profile.limits.max_bytes,
            });
        }

        let kind = self
            .profile
            .accepts
            .iter()
            .find(|accepted| matches!(accepted, AcceptedArtifact::Matrix3x3(_)))
            .map(|_| ArtifactKind::Matrix)
            .or_else(|| {
                self.profile
                    .accepts
                    .iter()
                    .find(|accepted| matches!(accepted, AcceptedArtifact::Curve1d(_)))
                    .map(|accepted| match accepted {
                        AcceptedArtifact::Curve1d(curve) => ArtifactKind::Curve {
                            per_channel: curve.per_channel,
                            points_max: curve.points_max,
                        },
                        _ => ArtifactKind::Matrix,
                    })
            })
            .ok_or(ExportError::NoAcceptedArtifact)?;

        let artifacts = self.bake_fit(&kind, insertion, downstream, look, illuminant)?;
        let bytes = self.measure(&artifacts, insertion, illuminant, Precision::F32)?;
        if bytes > self.profile.limits.max_bytes {
            return Err(ExportError::ByteLimit {
                needed: bytes,
                limit: self.profile.limits.max_bytes,
            });
        }
        Ok(MainArtifact::Fit(artifacts))
    }

    fn bake_luts(
        &self,
        plan: &LutPlan,
        insertion: &InsertionSpec,
        downstream: &DownstreamModel,
        look: &Look,
        illuminant: BakeIlluminant,
    ) -> Result<Vec<ArtifactAtCct>> {
        let shaper = plan.shaper.map(|params| {
            Shaper1d::from_fn(SHAPER_POINTS, |code| {
                params
                    .to_code(insertion.decode_channel(code))
                    .clamp(0.0, 1.0)
            })
        });
        let mut out = Vec::new();
        for cct in illuminant.ccts() {
            let camera = self.camera(insertion, cct)?;
            let mut lut = Lut3d {
                size: plan.size,
                data: Vec::with_capacity((plan.size as usize).pow(3)),
                shaper: shaper.clone(),
            };
            for b in 0..plan.size {
                for g in 0..plan.size {
                    for r in 0..plan.size {
                        let scale = (plan.size - 1) as f64;
                        let at = [r as f64 / scale, g as f64 / scale, b as f64 / scale];
                        let linear = match plan.shaper {
                            Some(params) => at.map(|value| params.from_code(value)),
                            None => at.map(|value| insertion.decode_channel(value)),
                        };
                        let xyz = insertion.to_xyz(linear, camera)?;
                        let target = downstream.target(look, xyz);
                        let value = downstream.decode(target.codes).map(|channel| {
                            let channel = if channel.is_finite() { channel } else { 0.0 };
                            crate::quantize::quantize(channel as f32, plan.precision)
                        });
                        lut.data.push(value);
                    }
                }
            }
            out.push(ArtifactAtCct {
                cct,
                artifact: Artifact::Lut3d(lut),
            });
        }
        Ok(out)
    }

    fn bake_fit(
        &self,
        kind: &ArtifactKind,
        insertion: &InsertionSpec,
        downstream: &DownstreamModel,
        look: &Look,
        illuminant: BakeIlluminant,
    ) -> Result<Vec<ArtifactAtCct>> {
        let probe_set = probes::v1();
        let mut out = Vec::new();
        for cct in illuminant.ccts() {
            let camera = self.camera(insertion, cct)?;
            let mut samples: Vec<(f64, [f64; 3], [f64; 3])> = Vec::new();
            for probe in &probe_set.probes {
                let linear = insertion.from_xyz(probe.xyz, camera)?;
                let target = downstream.target(look, probe.xyz);
                let output = downstream.decode(target.codes);
                if linear.iter().all(|value| value.is_finite())
                    && output.iter().all(|value| value.is_finite())
                {
                    samples.push((probe.weight, linear, output));
                }
            }
            let artifact = match kind {
                ArtifactKind::Matrix => {
                    let fitted = fit_matrix(&samples)
                        .ok_or(ExportError::SingularMatrix { cct: cct.kelvin() })?;
                    Artifact::Matrix3x3(fitted)
                }
                ArtifactKind::Curve {
                    per_channel,
                    points_max,
                } => Artifact::Curve1d(fit_curve(&samples, *per_channel, *points_max)),
            };
            out.push(ArtifactAtCct { cct, artifact });
        }
        Ok(out)
    }

    fn camera(&self, insertion: &InsertionSpec, cct: CctK) -> Result<Option<Mat3>> {
        if !insertion.needs_camera() {
            return Ok(None);
        }
        self.transform
            .camera_to_xyz(cct)
            .map(Some)
            .ok_or(ExportError::TransformUndefined { cct: cct.kelvin() })
    }

    fn measure(
        &self,
        artifacts: &[ArtifactAtCct],
        insertion: &InsertionSpec,
        illuminant: BakeIlluminant,
        precision: Precision,
    ) -> Result<u64> {
        let output = BakeOutput {
            profile_id: self.profile.id.clone(),
            profile_version: self.profile.version,
            bake: illuminant,
            insertion: insertion.clone(),
            downstream: DownstreamModel::from_profile(self.profile)?,
            interpolation: None,
            precision: Some(precision),
            artifacts: artifacts.to_vec(),
        };
        let mut largest = 0;
        for format in &self.profile.formats {
            largest = largest.max(writer::written_bytes(&output, *format)?);
        }
        Ok(largest)
    }
}

enum MainArtifact {
    Lut(LutPlan, Vec<ArtifactAtCct>),
    Fit(Vec<ArtifactAtCct>),
}

#[derive(Clone, Copy)]
enum ArtifactKind {
    Matrix,
    Curve { per_channel: bool, points_max: u32 },
}

/// Least squares over the probe samples: `output = M · linear`.
fn fit_matrix(samples: &[(f64, [f64; 3], [f64; 3])]) -> Option<Matrix3x3> {
    let mut normal = [[0.0_f64; 3]; 3];
    let mut target = [[0.0_f64; 3]; 3];
    for (weight, input, output) in samples {
        for row in 0..3 {
            for column in 0..3 {
                normal[row][column] += weight * input[row] * input[column];
            }
        }
        for row in 0..3 {
            for channel in 0..3 {
                target[channel][row] += weight * input[row] * output[channel];
            }
        }
    }
    let inverse = Mat3(normal).inverse()?;
    let mut rows = [[0.0_f32; 3]; 3];
    for channel in 0..3 {
        let coefficients = inverse.mul_vec(target[channel]);
        for (column, value) in coefficients.iter().enumerate() {
            rows[channel][column] = *value as f32;
        }
    }
    Some(Matrix3x3 { rows })
}

fn fit_curve(samples: &[(f64, [f64; 3], [f64; 3])], per_channel: bool, points_max: u32) -> Curve1d {
    let points = CURVE_POINTS.min(points_max.max(2)) as usize;
    let domain_max = samples
        .iter()
        .flat_map(|(_, input, _)| input.iter())
        .copied()
        .fold(1.0_f64, f64::max);

    let groups: Vec<Vec<usize>> = if per_channel {
        vec![vec![0], vec![1], vec![2]]
    } else {
        vec![vec![0, 1, 2]]
    };
    let mut solutions = Vec::with_capacity(groups.len());
    for group in &groups {
        let mut off = vec![0.0_f64; points];
        let mut right = vec![0.0_f64; points];
        let mut diag = vec![1.0e-6_f64; points];
        for (weight, input, output) in samples {
            for channel in group {
                let (index, fraction) = curve_weights(input[*channel] / domain_max, points);
                let left = 1.0 - fraction;
                let right_weight = fraction;
                right[index] += weight * left * output[*channel];
                right[index + 1] += weight * right_weight * output[*channel];
                diag[index] += weight * left * left;
                diag[index + 1] += weight * right_weight * right_weight;
                off[index] += weight * left * right_weight;
            }
        }
        solutions.push(solve_tridiagonal(&diag, &off, &right));
    }

    let mut knots = vec![[0.0_f32; 3]; points];
    for index in 0..points {
        let solved = |channel: usize| {
            if per_channel {
                solutions[channel][index]
            } else {
                solutions[0][index]
            }
        };
        knots[index] = [
            solved(0).clamp(0.0, 1.0) as f32,
            solved(1).clamp(0.0, 1.0) as f32,
            solved(2).clamp(0.0, 1.0) as f32,
        ];
    }
    Curve1d {
        per_channel,
        domain_max: domain_max as f32,
        points: knots,
    }
}

fn curve_weights(value: f64, points: usize) -> (usize, f64) {
    let x = value.clamp(0.0, 1.0) * (points - 1) as f64;
    let lower = (x.floor() as usize).min(points - 2);
    (lower, x - lower as f64)
}

/// A zero pivot leaves that knot at 0.
fn solve_tridiagonal(diag: &[f64], off: &[f64], right: &[f64]) -> Vec<f64> {
    let n = diag.len();
    let mut c = vec![0.0_f64; n];
    let mut d = vec![0.0_f64; n];
    let mut b = diag[0];
    for index in 0..n {
        let a = if index > 0 { off[index - 1] } else { 0.0 };
        if index > 0 {
            let previous = b;
            b = if previous.abs() > 1.0e-12 {
                diag[index] - a * c[index - 1]
            } else {
                diag[index]
            };
        }
        c[index] = if index + 1 < n && b.abs() > 1.0e-12 {
            off[index] / b
        } else {
            0.0
        };
        d[index] = if b.abs() > 1.0e-12 {
            (right[index] - a * if index > 0 { d[index - 1] } else { 0.0 }) / b
        } else {
            0.0
        };
    }
    let mut x = vec![0.0_f64; n];
    x[n - 1] = d[n - 1];
    for index in (0..n - 1).rev() {
        x[index] = d[index] - c[index] * x[index + 1];
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log2_shaper_is_invertible() {
        let params = ShaperParams::DEFAULT;
        for linear in [0.001, 0.18, 1.0, 11.0] {
            let code = params.to_code(linear);
            let back = params.from_code(code);
            assert!(
                (back - linear).abs() <= linear * 1.0e-12,
                "{linear} -> {back}"
            );
        }
        assert_eq!(params.to_code(0.0), 0.0);
        assert_eq!(params.to_code(-1.0), 0.0);
    }

    #[test]
    fn custom_curve_inverse_round_trips() {
        let samples: Vec<f32> = (0..=32)
            .map(|index| {
                let x = index as f64 / 32.0;
                (x * x * x) as f32
            })
            .collect();
        for code in [0.1, 0.5, 0.9] {
            let linear = interpolate_samples(&samples, code);
            let back = invert_samples(&samples, linear);
            assert!((back - code).abs() < 1.0e-6, "{code} -> {back}");
        }
    }

    #[test]
    fn curve_fit_recovers_a_per_channel_tone() {
        let mut samples = Vec::new();
        for index in 0..64 {
            let input = index as f64 / 64.0;
            let linear = [input, input * 0.5, input * 2.0];
            let output = [linear[0].sqrt(), linear[1].sqrt(), linear[2].sqrt()];
            samples.push((1.0, linear, output));
        }
        let curve = fit_curve(&samples, true, 33);
        for input in [0.1, 0.3, 0.6] {
            let linear = [input, input * 0.5, input * 2.0];
            let out = curve.eval(linear);
            assert!((out[0] - input.sqrt()).abs() < 0.02, "{out:?}");
            assert!((out[1] - (input * 0.5).sqrt()).abs() < 0.02, "{out:?}");
        }
    }

    #[test]
    fn matrix_fit_recovers_a_mixing() {
        let mixing = Mat3([[0.5, 0.2, 0.0], [0.1, 0.9, 0.0], [0.0, 0.0, 1.2]]);
        let mut samples = Vec::new();
        for r in 0..4 {
            for g in 0..4 {
                for b in 0..3 {
                    let input = [r as f64 / 3.0, g as f64 / 3.0, b as f64 / 2.0];
                    samples.push((1.0, input, mixing.mul_vec(input)));
                }
            }
        }
        let fitted = fit_matrix(&samples).expect("full rank");
        let out = fitted.eval([0.3, 0.6, 0.2]);
        let expected = mixing.mul_vec([0.3, 0.6, 0.2]);
        for channel in 0..3 {
            assert!((out[channel] - expected[channel]).abs() < 1.0e-5);
        }
    }
}
