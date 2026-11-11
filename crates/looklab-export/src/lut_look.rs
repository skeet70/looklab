// SPDX-License-Identifier: AGPL-3.0-or-later
//! Import a community LUT as a `lut_defined` Look (docs/03 §3, docs/02 §3.8).
//! The approximation is fitted to `scene XYZ → encoding → LUT → output → appearance`
//! on deterministic noise-free samples; the stored LUT keeps evaluation base-free.

#![allow(clippy::needless_range_loop)]

use looklab_color::cam16::ViewingConditions;
use looklab_color::{Mat3, Rgb, RgbSpace, Xyz};
use looklab_fit::{FitImage, FitOptions, ProbeUncertainty, fit};
use looklab_model::baseline::{is_neutral_baseline, neutral_baseline_srgb_v1};
use looklab_model::domain_penalty::{DomainViolation, check_published_inverse};
use looklab_model::file::{
    BaseReference, DeclaredEncoding, Gamut, LogTransfer, LutData, LutDefinition, LutRecord,
    OutputEncoding,
};
use looklab_model::probes;
use looklab_model::{
    ClipFunction, ClipSettings, Density, LookDocument, LookIdentity, ModelError, ProbeCovariance,
    ProbeScalars, UncertaintyData,
};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_schema::Interpolation;
use thiserror::Error;

use crate::artifact::Lut3d;
use crate::downstream::{checked_appearance, ucs_to_xyz};
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
    #[error("the published rendering inverse is undefined: {0}")]
    PublishedInverse(DomainViolation),
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
    /// The final refinement stage met its objective and step tolerances.
    pub converged: bool,
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
    let mut document = LookDocument::from_lut_definition(identity, fitted.look, definition, lut)?;
    let base = pinned_base_document(&document, base_look);
    document = compose_uncertainty(
        document,
        base,
        &ApproximationUncertainty {
            probes: &fitted.probes,
            support: &fitted.support,
        },
        SUPPORT_SIGMA,
    )?;
    let report = approximation_error(&document, base_look, &fitted.support)?;
    Ok(LutLookImport {
        document,
        approximation: report.summary,
        by_group: report.by_group,
        supported: report.summary.probes,
        pre_clip_identified: report.pre_clip_identified,
        converged: fitted.converged,
    })
}

/// The pre-existing LUT publication gate: finite p95 at or below 1.0 ΔE on supported probes.
pub fn approximation_publishable(report: &DeltaSummary) -> bool {
    report.probes > 0 && report.p95.is_finite() && report.p95 <= 1.0
}

/// Support threshold on inherited post-clip σ, in ΔE.
const SUPPORT_SIGMA: f64 = 1.5;

struct ComposedFit {
    look: looklab_model::Look,
    support: Vec<bool>,
    probes: ProbeUncertainty,
    converged: bool,
}

struct ApproximationUncertainty<'a> {
    probes: &'a ProbeUncertainty,
    support: &'a [bool],
}

/// The measured base a pinned display-referred definition evaluates against.
/// The neutral baseline is a fixed standard with zero uncertainty.
fn pinned_base_document<'a>(
    document: &LookDocument,
    base_look: Option<&'a LookDocument>,
) -> Option<&'a LookDocument> {
    let definition = document.file.definition.as_ref()?;
    let DeclaredEncoding::DisplayReferred { base, status } = &definition.encoding else {
        return None;
    };
    if !status.is_pinned() || is_neutral_baseline(base) {
        return None;
    }
    base_look
}

struct FitSamples {
    result: looklab_fit::FitResult,
    informative: Vec<Xyz>,
    encoded: usize,
}

fn fit_composed(
    lut: &Lut3d,
    definition: &LutDefinition,
    base_look: Option<&LookDocument>,
    options: LutLookOptions,
) -> Result<ComposedFit> {
    let points = sample_points(options.seed);
    let samples = fit_result(&points, lut, definition, base_look, options)?;
    let FitSamples {
        result,
        informative,
        encoded,
    } = samples;
    let looklab_fit::FitResult {
        mut look,
        realization,
        probes,
        diagnostics,
        ..
    } = result;
    look.clip = ClipSettings {
        function: ClipFunction::TanhSoftV1,
        k: realization.clip_k,
    };
    look.validate()?;
    let support = probe_support(&informative, encoded, options.density_floor);
    Ok(ComposedFit {
        look,
        support,
        probes,
        converged: diagnostics.converged,
    })
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
        let encoded = match composed_encoded(lut, definition, base_look, *xyz) {
            Ok(encoded) => encoded,
            Err(LutLookError::PublishedInverse(_)) => continue,
            Err(error) => return Err(error),
        };
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
    let encoded = in_rgb.len();
    let samples = SampleSet::new(
        SampleHeader {
            n: encoded as u32,
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
        support_sigma: SUPPORT_SIGMA,
        identity_scale: 0.0,
        prior_floor: false,
        run_gcv: false,
        initial_lambdas: looklab_fit::StageLambdas {
            tone: 0.1,
            tint: 0.01,
            grl: 0.1,
            residual: 1.0,
            ..Default::default()
        },
        fit_residual: true,
        recipe_sweeps: 0,
        ..FitOptions::default()
    };
    let result = fit(std::slice::from_ref(&image), &fit_options)?;
    Ok(FitSamples {
        result,
        informative,
        encoded,
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
    composed_encoded_parts(
        lut,
        &definition.encoding,
        definition.output,
        &definition.lut,
        base_look,
        xyz,
    )
}

fn composed_encoded_parts(
    lut: &Lut3d,
    encoding: &DeclaredEncoding,
    output: OutputEncoding,
    record: &LutRecord,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<Rgb> {
    if output != OutputEncoding::Srgb {
        return Err(LutLookError::Unsupported("output encoding"));
    }
    let input = match encoding {
        DeclaredEncoding::SceneReferred { transfer, gamut } => {
            scene_referred_encoded(*transfer, *gamut, xyz)?
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
    let code = normalize_domain(input, record);
    Ok(Rgb::from_array(
        lut.eval(code.to_array(), Interpolation::Tetrahedral),
    ))
}

fn scene_referred_encoded(transfer: LogTransfer, gamut: Gamut, xyz: Xyz) -> Result<Rgb> {
    if transfer != LogTransfer::Flog2 || gamut != Gamut::Fgamut {
        return Err(LutLookError::Unsupported("scene-referred encoding"));
    }
    let linear = looklab_color::spaces::fgamut_from_xyz_matrix().mul_vec([xyz.x, xyz.y, xyz.z]);
    Ok(Rgb::new(
        looklab_color::flog2::encode(linear[0]),
        looklab_color::flog2::encode(linear[1]),
        looklab_color::flog2::encode(linear[2]),
    ))
}

/// Composed encoded output of a LUT on an explicit preview baseline.
pub fn preview_composed_encoded(
    lut: &Lut3d,
    encoding: &DeclaredEncoding,
    output: OutputEncoding,
    record: &LutRecord,
    baseline: &BaseReference,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<Rgb> {
    if output != OutputEncoding::Srgb {
        return Err(LutLookError::Unsupported("output encoding"));
    }
    let input = match encoding {
        DeclaredEncoding::SceneReferred { transfer, gamut } => {
            scene_referred_encoded(*transfer, *gamut, xyz)?
        }
        DeclaredEncoding::DisplayReferred { .. } => preview_base_encoded(baseline, base_look, xyz)?,
    };
    let code = normalize_domain(input, record);
    Ok(Rgb::from_array(
        lut.eval(code.to_array(), Interpolation::Tetrahedral),
    ))
}

fn preview_base_encoded(
    base: &BaseReference,
    base_look: Option<&LookDocument>,
    xyz: Xyz,
) -> Result<Rgb> {
    if is_neutral_baseline(base) {
        return Ok(neutral_baseline_srgb_v1(
            RgbSpace::Srgb.xyz_to_rgb_linear(xyz),
        ));
    }
    if matches!(base, BaseReference::Look { .. }) {
        return base_encoded(base, base_look, xyz);
    }
    Err(LutLookError::Unsupported("preview baseline"))
}

fn base_encoded(base: &BaseReference, base_look: Option<&LookDocument>, xyz: Xyz) -> Result<Rgb> {
    if is_neutral_baseline(base) {
        let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
        return Ok(neutral_baseline_srgb_v1(linear));
    }
    match base {
        BaseReference::Look { .. } => {
            let document = base_look.ok_or(LutLookError::MissingBase)?;
            let appearance = checked_appearance(&document.look, xyz)
                .map_err(LutLookError::PublishedInverse)?
                .appearance();
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
        let target = match composed_ucs(&lut, definition, base_look, probe.xyz) {
            Ok(target) => target,
            Err(LutLookError::PublishedInverse(_)) => continue,
            Err(error) => return Err(error),
        };
        let pre = match checked_appearance(&document.look, probe.xyz) {
            Ok(checked) => checked.appearance(),
            Err(_) => continue,
        };
        let reliable = match document
            .uncertainty
            .probe_pre_reliable
            .as_ref()
            .and_then(|values| values.values.get(index))
        {
            Some(value) => *value > 0.5,
            None => clip_derivative_min(pre, document.look.clip.k) >= 0.25,
        };
        if reliable {
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
        let ucs_a = match composed_ucs(&lut_a, definition_a, base_a, probe.xyz) {
            Ok(value) => value,
            Err(LutLookError::PublishedInverse(_)) => continue,
            Err(error) => return Err(error),
        };
        let ucs_b = match composed_ucs(&lut_b, definition_b, base_b, probe.xyz) {
            Ok(value) => value,
            Err(LutLookError::PublishedInverse(_)) => continue,
            Err(error) => return Err(error),
        };
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

/// Encoded-code perturbation for the composed Jacobians.
const COVARIANCE_EPSILON: f64 = 1.0e-4;

/// Variance stored for a probe whose uncertainty is missing or non-finite.
/// The support mask keeps it out of matching.
const UNKNOWN_VARIANCE: f64 = 1.0e30;

/// Composes the fit's per-probe uncertainty with an optional measured base.
fn compose_uncertainty(
    document: LookDocument,
    base: Option<&LookDocument>,
    approximation: &ApproximationUncertainty<'_>,
    support_sigma: f64,
) -> Result<LookDocument> {
    let definition = document
        .file
        .definition
        .as_ref()
        .ok_or(LutLookError::Unsupported("missing definition"))?;
    let lut = lut3d(&document)?;
    let probe_set = probes::v1();
    let count = probe_set.probes.len();
    let mut covariance = Vec::with_capacity(count * 9);
    let mut sigma_post = Vec::with_capacity(count);
    let mut support = Vec::with_capacity(count);
    let mut pre_reliable = Vec::with_capacity(count);
    let mut density = Vec::with_capacity(count);
    for (index, probe) in probe_set.probes.iter().enumerate() {
        let render_feasible = checked_appearance(&document.look, probe.xyz).is_ok();
        let estimate = approximation.probes.probes.get(index);
        let fit_covariance = estimate.and_then(|estimate| {
            covariance_is_finite(estimate.post_covariance).then_some(estimate.post_covariance)
        });
        let fit_supported = estimate.is_some_and(|estimate| estimate.supported)
            && approximation.support.get(index).copied().unwrap_or(false);
        let fit_reliable = estimate.is_some_and(|estimate| estimate.pre_clip_reliable);
        let fit_density = estimate
            .map(|estimate| estimate.density)
            .filter(|value| value.is_finite() && *value >= 0.0);
        let inherited = base.and_then(|base| {
            base_probe_covariance(base, index).and_then(|(values, post_clip)| {
                let checked = checked_appearance(&base.look, probe.xyz).ok()?;
                let appearance = if post_clip {
                    base.look
                        .output_encode(checked.appearance(), RgbSpace::Srgb)
                } else {
                    checked.appearance()
                };
                let encoded = encoded_jacobian(base, appearance, post_clip)?;
                let encoded_covariance = sandwich(encoded, values)?;
                let center = base_encoded_for(definition, Some(base), probe.xyz).ok()?;
                let jacobian = lut_jacobian(&lut, definition, center)?;
                sandwich(jacobian, encoded_covariance)
            })
        });
        let total = match (base.is_some(), inherited, fit_covariance) {
            (true, Some(inherited), Some(fit)) => sum_covariance(inherited, fit),
            (false, _, Some(fit)) => Some(fit),
            _ => None,
        };
        let known = total.is_some();
        let total = total.unwrap_or_else(unknown_covariance);
        let sigma = sigma_of(&total).unwrap_or(f64::MAX);
        let base_supported = base.is_none_or(|base| {
            base.uncertainty
                .support
                .as_ref()
                .and_then(|mask| mask.values.get(index))
                .is_none_or(|value| *value > 0.5)
        });
        let accepted =
            render_feasible && known && fit_supported && base_supported && sigma <= support_sigma;
        support.push(if accepted { 1.0 } else { 0.0 });
        sigma_post.push(sigma);
        let base_reliable = base.is_none_or(|base| {
            base.uncertainty
                .probe_pre_reliable
                .as_ref()
                .and_then(|values| values.values.get(index))
                .is_some_and(|value| *value > 0.5)
        });
        pre_reliable.push(if render_feasible && base_reliable && fit_reliable {
            1.0
        } else {
            0.0
        });
        density.push(
            base.and_then(|base| {
                base.uncertainty
                    .density
                    .as_ref()
                    .and_then(|density| density.values.get(index))
                    .copied()
            })
            .or(fit_density)
            .unwrap_or(0.0),
        );
        for row in &total {
            covariance.extend_from_slice(row);
        }
    }
    let uncertainty = UncertaintyData {
        probe_cov_post: Some(ProbeCovariance::new(count, covariance)?),
        probe_sigma_post: Some(ProbeScalars::new(sigma_post)?),
        probe_pre_reliable: Some(ProbeScalars::new(pre_reliable)?),
        support: Some(ProbeScalars::new(support)?),
        density: Some(Density::new(density)?),
        ..UncertaintyData::default()
    };
    Ok(document.with_uncertainty(uncertainty)?)
}

fn sum_covariance(left: [[f64; 3]; 3], right: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let mut out = left;
    add_covariance(&mut out, right);
    covariance_is_finite(out).then_some(out)
}

/// Whether the covariance describes post-clip appearance; a pre-clip covariance
/// is used only where the base marks the pre-clip value identifiable.
fn base_probe_covariance(base: &LookDocument, index: usize) -> Option<([[f64; 3]; 3], bool)> {
    let uncertainty = &base.uncertainty;
    if let Some(covariance) = &uncertainty.probe_cov_post {
        return covariance_at(covariance, index).map(|values| (values, true));
    }
    let reliable = uncertainty
        .probe_pre_reliable
        .as_ref()
        .and_then(|values| values.values.get(index))
        .is_some_and(|value| *value > 0.5);
    if !reliable {
        return None;
    }
    uncertainty
        .probe_cov
        .as_ref()
        .and_then(|covariance| covariance_at(covariance, index))
        .map(|values| (values, false))
}

fn covariance_at(covariance: &ProbeCovariance, index: usize) -> Option<[[f64; 3]; 3]> {
    let start = index.checked_mul(9)?;
    let values = covariance.values.get(start..start + 9)?;
    let mut out = [[0.0_f64; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            out[row][column] = values[row * 3 + column];
        }
    }
    covariance_is_finite(out).then_some(out)
}

/// Derivative of the base's encoded RGB output with respect to its appearance,
/// before or after the base's output clip.
fn encoded_jacobian(
    base: &LookDocument,
    appearance: looklab_color::ucs::Ucs,
    post_clip: bool,
) -> Option<[[f64; 3]; 3]> {
    let mut jacobian = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        let mut low = [appearance.j, appearance.a, appearance.b];
        let mut high = low;
        low[axis] -= COVARIANCE_EPSILON;
        high[axis] += COVARIANCE_EPSILON;
        let low = looklab_color::ucs::Ucs::new(low[0], low[1], low[2]);
        let high = looklab_color::ucs::Ucs::new(high[0], high[1], high[2]);
        if !post_clip
            && (check_published_inverse(low).is_err() || check_published_inverse(high).is_err())
        {
            return None;
        }
        let low = encoded_code(base, low, post_clip);
        let high = encoded_code(base, high, post_clip);
        if !low.iter().chain(high.iter()).all(|value| value.is_finite()) {
            return None;
        }
        for row in 0..3 {
            jacobian[row][axis] = (high[row] - low[row]) / (2.0 * COVARIANCE_EPSILON);
        }
    }
    covariance_is_finite(jacobian).then_some(jacobian)
}

fn encoded_code(
    base: &LookDocument,
    appearance: looklab_color::ucs::Ucs,
    post_clip: bool,
) -> [f64; 3] {
    let appearance = if post_clip {
        appearance
    } else {
        base.look.output_encode(appearance, RgbSpace::Srgb)
    };
    let linear = RgbSpace::Srgb.xyz_to_rgb_linear(ucs_to_xyz(appearance));
    [
        RgbSpace::Srgb.encode(linear.r),
        RgbSpace::Srgb.encode(linear.g),
        RgbSpace::Srgb.encode(linear.b),
    ]
}

/// Derivative of the composed appearance with respect to the LUT's input code.
fn lut_jacobian(lut: &Lut3d, definition: &LutDefinition, center: Rgb) -> Option<[[f64; 3]; 3]> {
    let mut jacobian = [[0.0_f64; 3]; 3];
    for axis in 0..3 {
        let mut low = center.to_array();
        let mut high = center.to_array();
        low[axis] -= COVARIANCE_EPSILON;
        high[axis] += COVARIANCE_EPSILON;
        let low = composed_from_code(lut, definition, Rgb::from_array(low)).ok()?;
        let high = composed_from_code(lut, definition, Rgb::from_array(high)).ok()?;
        let low = [low.j, low.a, low.b];
        let high = [high.j, high.a, high.b];
        if !low.iter().chain(high.iter()).all(|value| value.is_finite()) {
            return None;
        }
        for row in 0..3 {
            jacobian[row][axis] = (high[row] - low[row]) / (2.0 * COVARIANCE_EPSILON);
        }
    }
    covariance_is_finite(jacobian).then_some(jacobian)
}

fn sandwich(jacobian: [[f64; 3]; 3], covariance: [[f64; 3]; 3]) -> Option<[[f64; 3]; 3]> {
    let mut out = [[0.0_f64; 3]; 3];
    for row in 0..3 {
        for column in 0..3 {
            let mut total = 0.0;
            for i in 0..3 {
                for j in 0..3 {
                    total += jacobian[row][i] * covariance[i][j] * jacobian[column][j];
                }
            }
            out[row][column] = total;
        }
    }
    covariance_is_finite(out).then_some(out)
}

fn add_covariance(target: &mut [[f64; 3]; 3], addition: [[f64; 3]; 3]) {
    for row in 0..3 {
        for column in 0..3 {
            target[row][column] += addition[row][column];
        }
    }
}

fn covariance_is_finite(covariance: [[f64; 3]; 3]) -> bool {
    covariance
        .iter()
        .all(|row| row.iter().all(|value| value.is_finite()))
}

fn sigma_of(covariance: &[[f64; 3]; 3]) -> Option<f64> {
    let trace = covariance[0][0] + covariance[1][1] + covariance[2][2];
    trace.is_finite().then(|| trace.max(0.0).sqrt())
}

fn unknown_covariance() -> [[f64; 3]; 3] {
    let mut covariance = [[0.0_f64; 3]; 3];
    for (axis, row) in covariance.iter_mut().enumerate() {
        row[axis] = UNKNOWN_VARIANCE;
    }
    covariance
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
