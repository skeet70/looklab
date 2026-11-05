// SPDX-License-Identifier: AGPL-3.0-or-later
//! Import a community LUT as a `lut_defined` Look (docs/03 §3, docs/02 §3.8).
//! The approximation is fitted to `scene XYZ → encoding → LUT → output → appearance`
//! on deterministic noise-free samples; the stored LUT keeps evaluation base-free.

#![allow(clippy::needless_range_loop)]

use looklab_color::cam16::ViewingConditions;
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_fit::{FitImage, FitOptions, fit};
use looklab_model::baseline::{is_neutral_baseline, neutral_baseline_srgb_v1};
use looklab_model::file::{
    BaseReference, DeclaredEncoding, LutData, LutDefinition, LutRecord, OutputEncoding,
};
use looklab_model::probes;
use looklab_model::{
    ClipFunction, ClipSettings, LookDocument, LookIdentity, ModelError, ProbeCovariance,
    ProbeScalars, UncertaintyData,
};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_schema::Interpolation;
use thiserror::Error;

use crate::artifact::Lut3d;
use crate::downstream::ucs_to_xyz;
use crate::writer::CubeFile;

#[derive(Debug, Error)]
pub enum LutLookError {
    #[error("cube: {0}")]
    Cube(String),
    #[error("model: {0}")]
    Model(#[from] ModelError),
    #[error("fit: {0}")]
    Fit(#[from] looklab_fit::FitError),
    #[error("samples: {0}")]
    Samples(#[from] looklab_samples::Error),
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    #[error("a display-referred base Look is required")]
    MissingBase,
}

pub type Result<T, E = LutLookError> = core::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LutLookOptions {
    pub seed: u64,
    /// Soft-clip width of the stored approximation. A LUT output is hard-clipped by
    /// its `[0, 1]` code domain.
    pub clip_k: f64,
    pub max_sweeps: usize,
    pub hessian_samples: usize,
    /// Absolute KDE weight per `(s, h_in, sqrt(C_in))` bin at which a probe is supported.
    pub density_floor: f64,
}

impl Default for LutLookOptions {
    fn default() -> Self {
        Self {
            seed: 0x6c75_742d_6c6f_6f6b,
            clip_k: 0.005,
            max_sweeps: 64,
            hessian_samples: 8_000,
            density_floor: 0.01,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DeltaSummary {
    pub probes: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LutLookImport {
    pub document: LookDocument,
    pub approximation: DeltaSummary,
    pub by_group: Vec<(looklab_model::ProbeGroup, DeltaSummary)>,
    /// Probes with sample density at or above the fit's floor.
    pub supported: usize,
    /// Supported probes whose pre-clip appearance is identifiable.
    pub pre_clip_identified: usize,
}

pub fn lut_data_from_cube(cube: &CubeFile) -> Result<LutData> {
    let size = cube.lut.size;
    if size > looklab_model::MAX_LUT_SIDE {
        return Err(LutLookError::Cube(format!(
            "LUT_3D_SIZE {size} exceeds {}",
            looklab_model::MAX_LUT_SIDE
        )));
    }
    if cube.lut.shaper.is_some() {
        return Err(LutLookError::Cube(
            "shaped cubes are not supported".to_string(),
        ));
    }
    if !(cube.domain[0].is_finite() && cube.domain[1].is_finite())
        || cube.domain[0] >= cube.domain[1]
    {
        return Err(LutLookError::Cube("invalid DOMAIN_MIN/MAX".to_string()));
    }
    let mut values = Vec::with_capacity(cube.lut.data.len() * 3);
    for value in &cube.lut.data {
        for channel in value {
            if !channel.is_finite() {
                return Err(LutLookError::Cube("non-finite LUT value".to_string()));
            }
            values.push(*channel as f64);
        }
    }
    LutData::new(size, [cube.domain[0]; 3], [cube.domain[1]; 3], values)
        .map_err(LutLookError::Model)
}

/// Fits the layered approximation and stores the LUT in a Look document.
pub fn import_lut_look(
    identity: LookIdentity,
    definition: LutDefinition,
    cube: &CubeFile,
    base_look: Option<&LookDocument>,
    options: LutLookOptions,
) -> Result<LutLookImport> {
    definition.validate()?;
    let lut = lut_data_from_cube(cube)?;
    if lut.record() != definition.lut {
        return Err(LutLookError::Cube(
            "LUT grid does not match the definition".to_string(),
        ));
    }
    let fitted = fit_composed(&cube.lut, &definition, base_look, options)?;
    let document = LookDocument::from_lut_definition(identity, fitted.look, definition, lut)?;
    let report = approximation_error(&document, base_look, &fitted.support)?;
    Ok(LutLookImport {
        document,
        approximation: report.summary,
        by_group: report.by_group,
        supported: report.summary.probes,
        pre_clip_identified: report.pre_clip_identified,
    })
}

struct ComposedFit {
    look: looklab_model::Look,
    support: Vec<bool>,
}

struct FitSamples {
    result: looklab_fit::FitResult,
    informative: Vec<Xyz>,
}

fn fit_composed(
    lut: &Lut3d,
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    options: LutLookOptions,
) -> Result<ComposedFit> {
    let points = sample_points(options.seed);
    let samples = fit_result(&points, lut, definition, base_look, options)?;
    let mut look = samples.result.look.clone();
    look.clip = ClipSettings {
        function: ClipFunction::TanhSoftV1,
        k: samples.result.realization.clip_k,
    };
    look.validate()?;
    let support = probe_support(&samples.informative, points.len(), options.density_floor);
    Ok(ComposedFit { look, support })
}

/// Probe support from the sample density in `(s, h_in, sqrt(C_in))` (docs/02 §5).
/// Only samples whose output is away from the clip shoulders carry density.
fn probe_support(points: &[Xyz], total: usize, floor: f64) -> Vec<bool> {
    const N_EFF: f64 = 500.0;
    let weight = N_EFF / total.max(1) as f64;
    let density = looklab_fit::sample_density(points, weight, floor);
    probes::v1()
        .probes
        .iter()
        .map(|probe| {
            let input = looklab_model::LookInput::new(probe.xyz);
            density.at(input.s, input.h_in, input.c_in) >= floor
        })
        .collect()
}

fn fit_result(
    points: &[Xyz],
    lut: &Lut3d,
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    options: LutLookOptions,
) -> Result<FitSamples> {
    let mut in_rgb = Vec::with_capacity(points.len());
    let mut out_rgb = Vec::with_capacity(points.len());
    let mut density = Vec::with_capacity(points.len());
    let mut flags = Vec::with_capacity(points.len());
    let mut informative = Vec::new();
    for xyz in points {
        let encoded = composed_encoded(lut, definition, base_look, *xyz)?;
        let linear = linear_from_encoded(encoded);
        if [linear.r, linear.g, linear.b]
            .iter()
            .all(|value| *value > 0.06 && *value < 0.94)
        {
            informative.push(*xyz);
        }
        in_rgb.push([xyz.x as f32, xyz.y as f32, xyz.z as f32]);
        out_rgb.push([linear.r as f32, linear.g as f32, linear.b as f32]);
        density.push(1.0_f32);
        flags.push(SampleFlags::NONE);
    }
    let samples = SampleSet::new(
        SampleHeader {
            n: points.len() as u32,
            input_space: InputSpace::CameraRgbWbLinear,
            output_space: OutputSpace::Srgb,
            wb_multipliers: [1.0, 1.0, 1.0],
            cct_hint_k: 0.0,
            exposure_hint_ev: 0.0,
            rerender_group: None,
            prep_version: PrepVersion::new(0),
            prior_transform_version: PriorTransformVersion::new(0),
            prep_params_hash: PrepParamsHash::new([0; 32]),
        },
        in_rgb,
        out_rgb,
        density,
        flags,
    )?;
    let image = FitImage::new(samples, Mat3::IDENTITY);
    let fit_options = FitOptions {
        seed: options.seed,
        max_samples_per_image: points.len(),
        early_samples_per_image: 0,
        hessian_samples: options.hessian_samples.min(points.len()),
        max_sweeps: options.max_sweeps,
        sweep_budget: 20_000_000.0,
        clip_prior: (options.clip_k, 1.0e-8),
        exposure_prior_sd: 1.0e-6,
        wb_prior_sd: 1.0e-6,
        exposure_mode_prior_sd: 1.0e-6,
        density_floor: options.density_floor,
        min_probe_clusters: 1,
        support_sigma: 1.5,
        identity_scale: 0.0,
        prior_floor: false,
        run_gcv: true,
        fit_residual: true,
        recipe_sweeps: 0,
        ..FitOptions::default()
    };
    let result = fit(std::slice::from_ref(&image), &fit_options)?;
    Ok(FitSamples {
        result,
        informative,
    })
}

/// The composed target appearance of `xyz`, before the output clip.
pub fn composed_ucs(
    lut: &Lut3d,
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<looklab_color::ucs::Ucs> {
    let encoded = composed_encoded(lut, definition, base_look, xyz)?;
    Ok(looklab_ucs(encoded))
}

fn composed_encoded(
    lut: &Lut3d,
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<Rgb> {
    if definition.output != OutputEncoding::Srgb {
        return Err(LutLookError::Unsupported("output encoding"));
    }
    let input = match &definition.encoding {
        DeclaredEncoding::SceneReferred { transfer, gamut } => {
            if *transfer != looklab_model::LogTransfer::Flog2
                || *gamut != looklab_model::Gamut::Fgamut
            {
                return Err(LutLookError::Unsupported("scene-referred encoding"));
            }
            let linear =
                looklab_color::spaces::fgamut_from_xyz_matrix().mul_vec([xyz.x, xyz.y, xyz.z]);
            Rgb::new(
                looklab_color::flog2::encode(linear[0]),
                looklab_color::flog2::encode(linear[1]),
                looklab_color::flog2::encode(linear[2]),
            )
        }
        DeclaredEncoding::DisplayReferred { base, status } => {
            if !status.is_pinned()
                && matches!(
                    base,
                    BaseReference::BodyRealization { .. } | BaseReference::FamilyRendering { .. }
                )
            {
                let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
                neutral_baseline_srgb_v1(linear)
            } else {
                base_encoded(base, base_look, xyz)?
            }
        }
    };
    let code = normalize_domain(input, &definition.lut);
    Ok(Rgb::from_array(
        lut.eval(code.to_array(), Interpolation::Tetrahedral),
    ))
}

fn base_encoded(base: &BaseReference, base_look: Option<&LookDocument>, xyz: Xyz) -> Result<Rgb> {
    if is_neutral_baseline(base) {
        let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
        return Ok(neutral_baseline_srgb_v1(linear));
    }
    match base {
        BaseReference::Look { .. } => {
            let document = base_look.ok_or(LutLookError::MissingBase)?;
            let appearance = document.look.eval(xyz);
            let encoded = document.look.output_encode(appearance, RgbSpace::Srgb);
            let linear = RgbSpace::Srgb.xyz_to_rgb_linear(ucs_to_xyz(encoded));
            Ok(Rgb::new(
                RgbSpace::Srgb.encode(linear.r),
                RgbSpace::Srgb.encode(linear.g),
                RgbSpace::Srgb.encode(linear.b),
            ))
        }
        BaseReference::BodyRealization { .. } | BaseReference::FamilyRendering { .. } => {
            Err(LutLookError::Unsupported("unpinned base"))
        }
    }
}

fn normalize_domain(value: Rgb, record: &LutRecord) -> Rgb {
    let map = |value: f64, channel: usize| {
        (value - record.domain_min[channel])
            / (record.domain_max[channel] - record.domain_min[channel])
    };
    Rgb::new(map(value.r, 0), map(value.g, 1), map(value.b, 2))
}

fn linear_from_encoded(encoded: Rgb) -> Rgb {
    Rgb::new(
        RgbSpace::Srgb.decode(encoded.r),
        RgbSpace::Srgb.decode(encoded.g),
        RgbSpace::Srgb.decode(encoded.b),
    )
}

fn looklab_ucs(encoded: Rgb) -> looklab_color::ucs::Ucs {
    let linear = linear_from_encoded(encoded);
    looklab_model::solve::ucs_of_xyz(RgbSpace::Srgb.rgb_linear_to_xyz(linear))
}

fn soft_clip(value: f64, k: f64) -> f64 {
    if value > 1.0 - k {
        (1.0 - k) + k * libm::tanh((value - (1.0 - k)) / k)
    } else if value < k {
        k - k * libm::tanh((k - value) / k)
    } else {
        value
    }
}

fn sample_points(seed: u64) -> Vec<Xyz> {
    let white = ViewingConditions::reference().white;
    let mut points = Vec::new();

    for step in 0..=240 {
        let s = -10.0 + 15.5 * step as f64 / 240.0;
        let y = 0.18 * libm::pow(2.0, s);
        points.push(Xyz::new(white.x * y, y, white.z * y));
    }

    let patches = looklab_color::chart::colorchecker24_d65();
    for patch in patches {
        for s in [-2.0, -1.0, 0.0, 1.0, 2.0] {
            points.push(patch.xyz.scale(libm::pow(2.0, s)));
        }
    }

    let mut rng = Rng::new(seed);
    let to_xyz = looklab_color::spaces::fgamut_to_xyz_matrix();
    for _ in 0..4096 {
        let rgb = [rng.next_f64(), rng.next_f64(), rng.next_f64()];
        let xyz = Xyz::from_array(to_xyz.mul_vec(rgb));
        let s = -9.0 + 14.0 * rng.next_f64();
        let target = 0.18 * libm::pow(2.0, s);
        let scale = if xyz.y > 1.0e-9 { target / xyz.y } else { 1.0 };
        points.push(xyz.scale(scale));
    }

    points
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x as f64 / u64::MAX as f64
    }
}

struct ApproximationReport {
    summary: DeltaSummary,
    by_group: Vec<(looklab_model::ProbeGroup, DeltaSummary)>,
    pre_clip_identified: usize,
}

/// ΔE of the post-clip appearance on supported probes (docs/02 §3.4).
fn approximation_error(
    document: &LookDocument,
    base_look: Option<&LookDocument>,
    support: &[bool],
) -> Result<ApproximationReport> {
    let definition = document
        .file
        .definition
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing definition"))?;
    let lut = lut3d(document)?;
    let probe_set = probes::v1();
    let mut errors = Vec::new();
    let mut pre_clip_identified = 0;
    let mut groups: std::collections::BTreeMap<looklab_model::ProbeGroup, Vec<f64>> =
        std::collections::BTreeMap::new();
    for (index, probe) in probe_set.probes.into_iter().enumerate() {
        if !support.get(index).copied().unwrap_or(false) {
            continue;
        }
        let target = composed_ucs(&lut, definition, base_look, probe.xyz)?;
        let pre = document.look.eval(probe.xyz);
        if clip_derivative_min(pre, document.look.clip.k) >= 0.25 {
            pre_clip_identified += 1;
        }
        let actual = document.look.output_encode(pre, RgbSpace::Srgb);
        let dj = target.j - actual.j;
        let da = target.a - actual.a;
        let db = target.b - actual.b;
        let delta = (dj * dj + da * da + db * db).sqrt();
        if delta.is_finite() {
            errors.push(delta);
            groups.entry(probe.group).or_default().push(delta);
        }
    }
    let by_group = groups
        .into_iter()
        .map(|(group, values)| (group, summarize(values)))
        .collect();
    Ok(ApproximationReport {
        summary: summarize(errors),
        by_group,
        pre_clip_identified,
    })
}

/// Smallest per-channel derivative of the soft clip at the pre-clip prediction.
fn clip_derivative_min(pre: looklab_color::ucs::Ucs, k: f64) -> f64 {
    let jmh = looklab_color::ucs::ucs_to_jmh(pre, looklab_color::ucs::UcsSpace::Ucs);
    let xyz = looklab_model::solve::reference_model().cam16_to_xyz(
        jmh.j,
        looklab_color::cam16::Chroma::M(jmh.m),
        jmh.h,
    );
    let rgb = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
    [rgb.r, rgb.g, rgb.b]
        .iter()
        .map(|value| {
            if *value > 1.0 - k || *value < k {
                let u = if *value > 1.0 - k {
                    (value - (1.0 - k)) / k
                } else {
                    (k - value) / k
                };
                1.0 - libm::tanh(u) * libm::tanh(u)
            } else {
                1.0
            }
        })
        .fold(1.0_f64, f64::min)
}

/// Per-probe appearance ΔE between two LUT definitions.
pub fn definition_delta(
    a: &LookDocument,
    base_a: Option<&LookDocument>,
    b: &LookDocument,
    base_b: Option<&LookDocument>,
) -> Result<DeltaSummary> {
    let definition_a = a
        .file
        .definition
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing definition"))?;
    let definition_b = b
        .file
        .definition
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing definition"))?;
    let lut_a = lut3d(a)?;
    let lut_b = lut3d(b)?;
    let mut errors = Vec::new();
    for probe in probes::v1().probes {
        let ucs_a = composed_ucs(&lut_a, definition_a, base_a, probe.xyz)?;
        let ucs_b = composed_ucs(&lut_b, definition_b, base_b, probe.xyz)?;
        let dj = ucs_a.j - ucs_b.j;
        let da = ucs_a.a - ucs_b.a;
        let db = ucs_a.b - ucs_b.b;
        let delta = (dj * dj + da * da + db * db).sqrt();
        if delta.is_finite() {
            errors.push(delta);
        }
    }
    Ok(summarize(errors))
}

fn lut3d(document: &LookDocument) -> Result<Lut3d> {
    let lut = document
        .lut
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing stored LUT"))?;
    let mut data = Vec::with_capacity(lut.values.len() / 3);
    let (chunks, _) = lut.values.as_chunks::<3>();
    for chunk in chunks {
        data.push([chunk[0] as f32, chunk[1] as f32, chunk[2] as f32]);
    }
    Ok(Lut3d {
        size: lut.size,
        data,
        shaper: None,
    })
}

fn summarize(mut values: Vec<f64>) -> DeltaSummary {
    if values.is_empty() {
        return DeltaSummary::default();
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let at = |fraction: f64| {
        let index = ((values.len() as f64 - 1.0) * fraction).round() as usize;
        values[index]
    };
    DeltaSummary {
        probes: values.len(),
        median: at(0.5),
        p95: at(0.95),
        max: values[values.len() - 1],
    }
}

/// Propagates encoded-space base covariance through the stored LUT to appearance
/// covariance, and marks probes above the normal support threshold as unsupported.
pub fn with_base_covariance(
    document: &LookDocument,
    base_encoded_covariance: &[[f64; 9]],
    base_look: Option<&LookDocument>,
    support_sigma: f64,
) -> Result<LookDocument> {
    let definition = document
        .file
        .definition
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing definition"))?;
    let lut = lut3d(document)?;
    let probe_set = probes::v1();
    if base_encoded_covariance.len() != probe_set.probes.len() {
        return Err(LutLookError::Unsupported("base covariance length"));
    }
    let mut values = Vec::with_capacity(probe_set.probes.len() * 9);
    let mut support = Vec::with_capacity(probe_set.probes.len());
    const EPSILON: f64 = 1.0e-4;
    for (probe, covariance) in probe_set.probes.iter().zip(base_encoded_covariance) {
        let center = base_encoded_for(definition, base_look, probe.xyz)?;
        let mut jacobian = [[0.0_f64; 3]; 3];
        for axis in 0..3 {
            let mut low = center.to_array();
            let mut high = center.to_array();
            low[axis] -= EPSILON;
            high[axis] += EPSILON;
            let low = composed_from_code(&lut, definition, Rgb::from_array(low))?;
            let high = composed_from_code(&lut, definition, Rgb::from_array(high))?;
            jacobian[0][axis] = (high.j - low.j) / (2.0 * EPSILON);
            jacobian[1][axis] = (high.a - low.a) / (2.0 * EPSILON);
            jacobian[2][axis] = (high.b - low.b) / (2.0 * EPSILON);
        }
        let cov = [
            [covariance[0], covariance[1], covariance[2]],
            [covariance[3], covariance[4], covariance[5]],
            [covariance[6], covariance[7], covariance[8]],
        ];
        let mut out = [[0.0_f64; 3]; 3];
        for row in 0..3 {
            for column in 0..3 {
                let mut total = 0.0;
                for i in 0..3 {
                    for j in 0..3 {
                        total += jacobian[row][i] * cov[i][j] * jacobian[column][j];
                    }
                }
                out[row][column] = total;
            }
        }
        let variance = out[0][0].max(0.0) + out[1][1].max(0.0) + out[2][2].max(0.0);
        support.push(if variance.sqrt() <= support_sigma {
            1.0
        } else {
            0.0
        });
        for row in &out {
            values.extend_from_slice(row);
        }
    }
    let uncertainty = UncertaintyData {
        probe_cov: Some(ProbeCovariance::new(probe_set.probes.len(), values)?),
        support: Some(ProbeScalars::new(support)?),
        ..UncertaintyData::default()
    };
    Ok(document.clone().with_uncertainty(uncertainty)?)
}

fn composed_from_code(
    lut: &Lut3d,
    definition: &LutDefinition,
    code: Rgb,
) -> Result<looklab_color::ucs::Ucs> {
    let normalized = normalize_domain(code, &definition.lut);
    let encoded = Rgb::from_array(lut.eval(normalized.to_array(), Interpolation::Tetrahedral));
    Ok(looklab_ucs(encoded))
}

fn base_encoded_for(
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<Rgb> {
    match &definition.encoding {
        DeclaredEncoding::SceneReferred { .. } => {
            Err(LutLookError::Unsupported("scene-referred base covariance"))
        }
        DeclaredEncoding::DisplayReferred { base, status } => {
            if !status.is_pinned()
                && matches!(
                    base,
                    BaseReference::BodyRealization { .. } | BaseReference::FamilyRendering { .. }
                )
            {
                let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
                Ok(neutral_baseline_srgb_v1(linear))
            } else {
                base_encoded(base, base_look, xyz)
            }
        }
    }
}

#[cfg(test)]
mod measure {
    use super::*;
    use crate::read_cube;
    use looklab_model::BaseStatus;

    fn distance(a: looklab_color::ucs::Ucs, b: looklab_color::ucs::Ucs) -> f64 {
        let dj = a.j - b.j;
        let da = a.a - b.a;
        let db = a.b - b.b;
        (dj * dj + da * da + db * db).sqrt()
    }

    fn stats(mut values: Vec<f64>) -> (usize, f64, f64, f64) {
        if values.is_empty() {
            return (0, 0.0, 0.0, 0.0);
        }
        values.sort_by(|a, b| a.total_cmp(b));
        let at = |f: f64| values[((values.len() as f64 - 1.0) * f) as usize];
        (values.len(), at(0.5), at(0.95), values[values.len() - 1])
    }

    fn definition_for(name: &str, cube: &CubeFile) -> LutDefinition {
        let values: Vec<f64> = cube
            .lut
            .data
            .iter()
            .flat_map(|rgb| rgb.iter().map(|value| *value as f64))
            .collect();
        let lut = LutData::new(
            cube.lut.size,
            [cube.domain[0]; 3],
            [cube.domain[1]; 3],
            values,
        )
        .expect("lut");
        let encoding = if name.contains("FLog2") || name.contains("synthetic") {
            DeclaredEncoding::SceneReferred {
                transfer: looklab_model::LogTransfer::Flog2,
                gamut: looklab_model::Gamut::Fgamut,
            }
        } else {
            DeclaredEncoding::DisplayReferred {
                base: BaseReference::neutral_baseline(),
                status: BaseStatus::BaseIdentifiedUnmeasured,
            }
        };
        LutDefinition {
            name: name.to_string(),
            license: looklab_model::LicenseRecord {
                own_work: false,
                source_license: Some("CC0-1.0".to_string()),
                source_url: None,
                attribution: "donor".to_string(),
            },
            encoding,
            output: OutputEncoding::Srgb,
            lut: lut.record(),
            emulates: None,
            superseded_by: None,
            notes: Vec::new(),
        }
    }

    fn known_look() -> looklab_model::Look {
        let mut look = looklab_model::Look::colorimetric_spline().expect("tone spline");
        if let looklab_model::Tone::Spline { c0, beta } = &mut look.tone {
            *c0 *= 1.06;
            for coefficient in beta.iter_mut() {
                *coefficient *= 1.03;
            }
        }
        look.tint.a.fill(1.0);
        look.tint.b.fill(-0.6);
        look.color.g.plane.fill(0.04);
        look.color.g.volume.fill(0.01);
        look.color.r.plane.fill(1.5);
        look.color.lambda.plane.fill(0.3);
        look
    }

    fn cube_from_look(look: &looklab_model::Look) -> CubeFile {
        let size = 33usize;
        let mut data = Vec::with_capacity(size * size * size);
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let code = [
                        r as f64 / (size - 1) as f64,
                        g as f64 / (size - 1) as f64,
                        b as f64 / (size - 1) as f64,
                    ];
                    let linear = Rgb::new(
                        looklab_color::flog2::decode(code[0]),
                        looklab_color::flog2::decode(code[1]),
                        looklab_color::flog2::decode(code[2]),
                    );
                    let xyz = Xyz::from_array(
                        looklab_color::spaces::fgamut_to_xyz_matrix()
                            .mul_vec([linear.r, linear.g, linear.b]),
                    );
                    let post = look.output_encode(look.eval(xyz), RgbSpace::Srgb);
                    let srgb = RgbSpace::Srgb.xyz_to_rgb_linear(ucs_to_xyz(post));
                    data.push([
                        RgbSpace::Srgb.encode(srgb.r) as f32,
                        RgbSpace::Srgb.encode(srgb.g) as f32,
                        RgbSpace::Srgb.encode(srgb.b) as f32,
                    ]);
                }
            }
        }
        CubeFile {
            lut: Lut3d {
                size: size as u32,
                data,
                shaper: None,
            },
            domain: [0.0, 1.0],
        }
    }

    fn report(label: &str, cube: &CubeFile, definition: &LutDefinition, imported: &LutLookImport) {
        let lut = &cube.lut;
        let look = &imported.document.look;
        let k = look.clip.k;
        let options = LutLookOptions::default();
        let points = sample_points(options.seed);
        let mut informative = Vec::new();
        for xyz in &points {
            let linear = linear_from_encoded(
                composed_encoded(lut, definition, None, *xyz).expect("encoded"),
            );
            if [linear.r, linear.g, linear.b]
                .iter()
                .all(|value| *value > 0.06 && *value < 0.94)
            {
                informative.push(*xyz);
            }
        }
        let support = probe_support(&informative, points.len(), options.density_floor);
        let probe_set = probes::v1();
        let mut target_clipped = 0;
        let mut base_clipped = 0;
        let mut clipped_supported = 0;
        let mut groups_all: std::collections::BTreeMap<probes::ProbeGroup, Vec<f64>> =
            std::collections::BTreeMap::new();
        let mut worst: Vec<(f64, &str, probes::ProbeGroup)> = Vec::new();
        for (index, probe) in probe_set.probes.iter().enumerate() {
            let target = composed_ucs(lut, definition, None, probe.xyz).expect("target");
            let target_linear = linear_from_encoded(
                composed_encoded(lut, definition, None, probe.xyz).expect("encoded"),
            );
            let target_clips = [target_linear.r, target_linear.g, target_linear.b]
                .iter()
                .any(|value| *value < k || *value > 1.0 - k);
            if target_clips {
                target_clipped += 1;
            }
            let base_input_clips = match &definition.encoding {
                DeclaredEncoding::SceneReferred { .. } => {
                    let linear = looklab_color::spaces::fgamut_from_xyz_matrix().mul_vec([
                        probe.xyz.x,
                        probe.xyz.y,
                        probe.xyz.z,
                    ]);
                    linear.iter().any(|value| {
                        let code = looklab_color::flog2::encode(*value);
                        !(0.0..=1.0).contains(&code)
                    })
                }
                DeclaredEncoding::DisplayReferred { .. } => {
                    let linear = RgbSpace::Srgb.xyz_to_rgb_linear(probe.xyz);
                    [linear.r, linear.g, linear.b]
                        .iter()
                        .any(|value| *value < 0.0 || *value > 1.0)
                }
            };
            if base_input_clips {
                base_clipped += 1;
            }
            if support[index] {
                if target_clips {
                    clipped_supported += 1;
                }
                let post = look.output_encode(look.eval(probe.xyz), RgbSpace::Srgb);
                let delta = distance(post, target);
                groups_all.entry(probe.group).or_default().push(delta);
                worst.push((delta, probe.id.as_str(), probe.group));
            }
        }
        let flat: Vec<f64> = groups_all.values().flatten().copied().collect();
        let (n, m, p, x) = stats(flat);
        println!(
            "== {label}: k={k:.5} supported={} identified={} | post n={n} median={m:.3} p95={p:.3} max={x:.3} | target_clipped={target_clipped} clipped_supported={clipped_supported} base_clipped={base_clipped}",
            imported.supported, imported.pre_clip_identified
        );
        for (group, values) in &groups_all {
            let (n, m, p, x) = stats(values.clone());
            println!("   {group:?}: n={n} median={m:.3} p95={p:.3} max={x:.3}");
        }
        worst.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (delta, id, group) in worst.iter().take(5) {
            println!("   worst {id} {group:?} dE={delta:.3}");
            if let Some(probe) = probe_set
                .probes
                .iter()
                .find(|probe| probe.id.as_str() == *id)
            {
                let target = composed_ucs(lut, definition, None, probe.xyz).expect("target");
                let pre = look.eval(probe.xyz);
                let post = look.output_encode(pre, RgbSpace::Srgb);
                let code = composed_encoded(lut, definition, None, probe.xyz).expect("encoded");
                let input = looklab_model::LookInput::new(probe.xyz);
                println!(
                    "      input s={:.2} c={:.1} target {:.2},{:.2},{:.2} pre {:.2},{:.2},{:.2} post {:.2},{:.2},{:.2} code {:.3},{:.3},{:.3} y={:.6}",
                    input.s,
                    input.c_in,
                    target.j,
                    target.a,
                    target.b,
                    pre.j,
                    pre.a,
                    pre.b,
                    post.j,
                    post.a,
                    post.b,
                    code.r,
                    code.g,
                    code.b,
                    probe.xyz.y
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn tone_trace() {
        let text = std::fs::read_to_string("../../data/luts/Ricoh_GRIV_Cinema_Green_SRGB.cube")
            .expect("read");
        let cube = read_cube(&text).expect("cube");
        let definition = definition_for("Ricoh_GRIV_Cinema_Green_SRGB.cube", &cube);
        let imported = import_lut_look(
            LookIdentity::new(
                "trace",
                "1.0.0",
                looklab_model::LookKind::LutDefined,
                looklab_model::WbPolicy::Adaptive,
            )
            .expect("identity"),
            definition.clone(),
            &cube,
            None,
            LutLookOptions::default(),
        )
        .expect("import");
        let look = &imported.document.look;
        if let looklab_model::Tone::Spline { c0, beta } = &look.tone {
            println!("c0={c0:.3} beta={beta:?}");
        }
        let white = ViewingConditions::reference().white;
        for step in 0..=24 {
            let s = -10.0 + step as f64 * 0.25;
            let y = 0.18 * libm::pow(2.0, s);
            let xyz = Xyz::new(white.x * y, y, white.z * y);
            let target = composed_ucs(&cube.lut, &definition, None, xyz).expect("target");
            let pre = look.eval(xyz);
            let post = look.output_encode(pre, RgbSpace::Srgb);
            let tone = look.tone.eval(s);
            println!(
                "s={s:.2} tone={tone:.2} target={:.2},{:.2},{:.2} pre={:.2},{:.2},{:.2} post={:.2},{:.2},{:.2}",
                target.j, target.a, target.b, pre.j, pre.a, pre.b, post.j, post.a, post.b
            );
        }
    }

    #[test]
    #[ignore]
    fn measure_luts() {
        let files = match std::env::var("MEASURE_FILES") {
            Ok(value) => value.split(',').map(str::to_string).collect::<Vec<_>>(),
            Err(_) => vec![
                "Ricoh_GRIV_Cinema_Green_FLog2.cube".to_string(),
                "Ricoh_GRIV_Cinema_Green_SRGB.cube".to_string(),
                "Leica_Classic_Gold_200.cube".to_string(),
                "CineStill_800T_1600T_sRGB_V4.cube".to_string(),
            ],
        };
        for file in &files {
            let text = std::fs::read_to_string(format!("../../data/luts/{file}")).expect("read");
            let cube = read_cube(&text).expect("cube");
            let definition = definition_for(file, &cube);
            let imported = import_lut_look(
                LookIdentity::new(
                    file,
                    "1.0.0",
                    looklab_model::LookKind::LutDefined,
                    looklab_model::WbPolicy::Adaptive,
                )
                .expect("identity"),
                definition.clone(),
                &cube,
                None,
                LutLookOptions::default(),
            )
            .expect("import");
            report(file, &cube, &definition, &imported);
        }
    }

    fn tone_only_look() -> looklab_model::Look {
        let mut look = looklab_model::Look::colorimetric_spline().expect("tone spline");
        if let looklab_model::Tone::Spline { c0, beta } = &mut look.tone {
            *c0 *= 1.06;
            for coefficient in beta.iter_mut() {
                *coefficient *= 1.03;
            }
        }
        look
    }

    fn mild_look() -> looklab_model::Look {
        let mut look = tone_only_look();
        look.tint.a.fill(0.5);
        look.tint.b.fill(-0.3);
        look.color.g.plane.fill(0.02);
        look.color.r.plane.fill(0.5);
        look.color.lambda.plane.fill(0.15);
        look
    }

    fn bt709_decode(code: f64) -> f64 {
        if code < 0.081 {
            code / 4.5
        } else {
            libm::pow((code + 0.099) / 1.099, 1.0 / 0.45)
        }
    }

    fn cube_from_look_display(look: &looklab_model::Look) -> CubeFile {
        let size = 33usize;
        let mut data = Vec::with_capacity(size * size * size);
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let code = [
                        r as f64 / (size - 1) as f64,
                        g as f64 / (size - 1) as f64,
                        b as f64 / (size - 1) as f64,
                    ];
                    let linear = Rgb::new(
                        bt709_decode(code[0]),
                        bt709_decode(code[1]),
                        bt709_decode(code[2]),
                    );
                    let xyz = RgbSpace::Srgb.rgb_linear_to_xyz(linear);
                    let post = look.output_encode(look.eval(xyz), RgbSpace::Srgb);
                    let srgb = RgbSpace::Srgb.xyz_to_rgb_linear(ucs_to_xyz(post));
                    data.push([
                        RgbSpace::Srgb.encode(srgb.r) as f32,
                        RgbSpace::Srgb.encode(srgb.g) as f32,
                        RgbSpace::Srgb.encode(srgb.b) as f32,
                    ]);
                }
            }
        }
        CubeFile {
            lut: Lut3d {
                size: size as u32,
                data,
                shaper: None,
            },
            domain: [0.0, 1.0],
        }
    }

    fn display_definition(name: &str, cube: &CubeFile) -> LutDefinition {
        let mut definition = definition_for(name, cube);
        definition.encoding = DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status: BaseStatus::BaseIdentifiedUnmeasured,
        };
        definition
    }

    #[test]
    #[ignore]
    fn representation_display() {
        let variants: [(&str, looklab_model::Look); 4] = [
            (
                "colorimetric",
                looklab_model::Look::colorimetric_spline().expect("tone spline"),
            ),
            ("tone", tone_only_look()),
            ("mild", mild_look()),
            ("strong", known_look()),
        ];
        for (name, mut look) in variants {
            look.clip = ClipSettings {
                function: ClipFunction::TanhSoftV1,
                k: 0.005,
            };
            let cube = cube_from_look_display(&look);
            let definition = display_definition("display", &cube);
            let mut all = Vec::new();
            let mut unclipped = Vec::new();
            for probe in &probes::v1().probes {
                let target = composed_ucs(&cube.lut, &definition, None, probe.xyz).expect("target");
                let truth = look.output_encode(look.eval(probe.xyz), RgbSpace::Srgb);
                let delta = distance(truth, target);
                all.push(delta);
                let linear = linear_from_encoded(
                    composed_encoded(&cube.lut, &definition, None, probe.xyz).expect("encoded"),
                );
                if [linear.r, linear.g, linear.b]
                    .iter()
                    .all(|value| *value >= 0.005 && *value <= 0.995)
                {
                    unclipped.push(delta);
                }
            }
            for (label, values) in [("all", all), ("unclipped", unclipped)] {
                let (n, m, p, x) = stats(values);
                println!("== display {name} {label}: n={n} median={m:.3} p95={p:.3} max={x:.3}");
            }
        }
    }

    #[test]
    #[ignore]
    fn representation() {
        let variants: [(&str, looklab_model::Look); 4] = [
            (
                "colorimetric",
                looklab_model::Look::colorimetric_spline().expect("tone spline"),
            ),
            ("tone", tone_only_look()),
            ("mild", mild_look()),
            ("strong", known_look()),
        ];
        for (name, mut look) in variants {
            look.clip = ClipSettings {
                function: ClipFunction::TanhSoftV1,
                k: 0.005,
            };
            let cube = cube_from_look(&look);
            let definition = definition_for("synthetic", &cube);
            let mut all = Vec::new();
            let mut unclipped = Vec::new();
            let mut gray = Vec::new();
            for probe in &probes::v1().probes {
                let target = composed_ucs(&cube.lut, &definition, None, probe.xyz).expect("target");
                let truth = look.output_encode(look.eval(probe.xyz), RgbSpace::Srgb);
                let delta = distance(truth, target);
                all.push(delta);
                let linear = linear_from_encoded(
                    composed_encoded(&cube.lut, &definition, None, probe.xyz).expect("encoded"),
                );
                if [linear.r, linear.g, linear.b]
                    .iter()
                    .all(|value| *value >= 0.005 && *value <= 0.995)
                {
                    unclipped.push(delta);
                }
                if probe.group == probes::ProbeGroup::GrayRamp {
                    gray.push(delta);
                }
            }
            for (label, values) in [("all", all), ("unclipped", unclipped), ("gray", gray)] {
                let (n, m, p, x) = stats(values);
                println!("== {name} {label}: n={n} median={m:.3} p95={p:.3} max={x:.3}");
            }
        }
    }

    #[test]
    #[ignore]
    fn recovery() {
        for (name, mut known) in [("mild", mild_look())] {
            known.clip = ClipSettings {
                function: ClipFunction::TanhSoftV1,
                k: 0.005,
            };
            let cube = cube_from_look_display(&known);
            let definition = display_definition("display", &cube);
            let imported = import_lut_look(
                LookIdentity::new(
                    "synthetic",
                    "1.0.0",
                    looklab_model::LookKind::LutDefined,
                    looklab_model::WbPolicy::Adaptive,
                )
                .expect("identity"),
                definition.clone(),
                &cube,
                None,
                LutLookOptions::default(),
            )
            .expect("import");
            report(&format!("recovery-{name}"), &cube, &definition, &imported);
            let mut representation = Vec::new();
            for probe in &probes::v1().probes {
                let target = composed_ucs(&cube.lut, &definition, None, probe.xyz).expect("target");
                let truth = known.output_encode(known.eval(probe.xyz), RgbSpace::Srgb);
                representation.push(distance(truth, target));
            }
            let (n, m, p, x) = stats(representation);
            println!("   representation {name}: n={n} median={m:.3} p95={p:.3} max={x:.3}");
        }
    }
}
