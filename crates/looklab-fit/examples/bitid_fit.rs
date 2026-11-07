// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native/wasm bit-identity workload for a complete small fit; `cargo xtask bitid`
//! compares it with a `wasm32-wasip1` run.

use looklab_color::ucs::{UcsSpace, ucs_to_jmh};
use looklab_color::{Mat3, RgbSpace, Xyz};
use looklab_fit::{FitImage, FitOptions, fit};
use looklab_model::basis::{RESIDUAL_COEFS, TINT_COEFS};
use looklab_model::look::{ColorLayer, Residual, Tint};
use looklab_model::solve::{reference_model, reference_white};
use looklab_model::{ClipSettings, Look, Tone};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};

const IMAGES: usize = 2;
const SAMPLES: usize = 240;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64);
        lo + (hi - lo) * unit
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn mix(&mut self, value: f64) {
        for byte in value.to_bits().to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn truth_look(rng: &mut SplitMix64) -> Look {
    let beta = core::array::from_fn(|_| rng.range(0.0, 0.25));
    let tone = Tone::spline(rng.range(-0.6, 0.6), beta).expect("non-negative betas");
    let tint = Tint {
        a: (0..TINT_COEFS).map(|_| rng.range(-0.4, 0.4)).collect(),
        b: (0..TINT_COEFS).map(|_| rng.range(-0.4, 0.4)).collect(),
    };
    let mut color = ColorLayer::zeros();
    for component in [&mut color.g, &mut color.r, &mut color.lambda] {
        for coefficient in component
            .plane
            .iter_mut()
            .chain(component.volume.iter_mut())
            .chain(component.core.iter_mut())
        {
            *coefficient = rng.range(-0.02, 0.02);
        }
    }
    let residual = Residual {
        values: vec![0.0; RESIDUAL_COEFS],
    };
    Look::new(tone, tint, color, residual, ClipSettings::default_v1()).expect("valid clip")
}

fn scene(index: usize, sample: usize) -> Xyz {
    let t = sample as f64 / (SAMPLES - 1) as f64;
    let y = 0.004 * libm::pow(10.0, 2.4 * t);
    let white = reference_white();
    if sample.is_multiple_of(9) {
        return white.scale(y);
    }
    let hue = 2.0 * core::f64::consts::PI * ((index * 7 + sample % 17) as f64) / 17.0;
    let radius = 0.45 * ((sample % 5) as f64) / 5.0;
    Xyz::new(
        y * (white.x + radius * libm::cos(hue)),
        y,
        y * (white.z + radius * libm::sin(hue)),
    )
}

fn image(truth: &Look, index: usize, hint: f64) -> FitImage {
    let scale = libm::pow(2.0, -hint);
    let mut in_rgb = Vec::with_capacity(SAMPLES);
    let mut out_rgb = Vec::with_capacity(SAMPLES);
    for sample in 0..SAMPLES {
        let target = scene(index, sample);
        let ucs = truth.output_encode(truth.eval(target), RgbSpace::Srgb);
        let jmh = ucs_to_jmh(ucs, UcsSpace::Ucs);
        let (xyz, _) = reference_model().inverse_xyz_and_jacobian(jmh);
        let linear = RgbSpace::Srgb.xyz_to_rgb_linear(xyz);
        in_rgb.push([
            (target.x * scale) as f32,
            (target.y * scale) as f32,
            (target.z * scale) as f32,
        ]);
        out_rgb.push([linear.r as f32, linear.g as f32, linear.b as f32]);
    }
    let header = SampleHeader {
        n: SAMPLES as u32,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 0.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(0),
        prior_transform_version: PriorTransformVersion::new(0),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    };
    let samples = SampleSet::new(
        header,
        in_rgb,
        out_rgb,
        vec![1.0; SAMPLES],
        vec![SampleFlags::NONE; SAMPLES],
    )
    .expect("workload columns are valid");
    let mut image = FitImage::new(samples, Mat3::IDENTITY);
    image.exposure_hint = hint;
    image
}

fn main() {
    let mut rng = SplitMix64(0x5eed_0f17_0000_0001);
    let truth = truth_look(&mut rng);
    let images: Vec<FitImage> = (0..IMAGES)
        .map(|index| image(&truth, index, 0.15 * index as f64))
        .collect();
    let options = FitOptions {
        seed: 0x5eed_0f17_0000_0002,
        max_samples_per_image: SAMPLES,
        early_samples_per_image: 64,
        hessian_samples: 256,
        max_sweeps: 20,
        ..FitOptions::default()
    };
    let result = fit(&images, &options).expect("workload fit");

    let mut coefficients = Fnv::new();
    for value in result.look.coef_vector() {
        coefficients.mix(value);
    }
    coefficients.mix(result.realization.clip_k);
    for image in &result.images {
        coefficients.mix(image.exposure);
        coefficients.mix(image.wb_log_r);
        coefficients.mix(image.wb_log_b);
    }
    println!("fit_coefficients {:016x}", coefficients.finish());

    let mut path = Fnv::new();
    path.mix(f64::from(result.diagnostics.converged as u8));
    path.mix(result.diagnostics.sweeps as f64);
    path.mix(result.diagnostics.evaluations as f64);
    path.mix(result.diagnostics.sample_evaluations.get() as f64);
    path.mix(result.diagnostics.operator_rows as f64);
    path.mix(result.diagnostics.objective_samples as f64);
    path.mix(result.diagnostics.final_cost);
    for (_, sigma) in &result.diagnostics.sigma_v {
        path.mix(*sigma);
    }
    println!("fit_path {:016x}", path.finish());

    let mut inputs = Fnv::new();
    for image in &images {
        for rgb in image.samples.in_rgb() {
            for value in rgb {
                inputs.mix(f64::from(*value));
            }
        }
        for rgb in image.samples.out_rgb() {
            for value in rgb {
                inputs.mix(f64::from(*value));
            }
        }
    }
    println!("fit_inputs {:016x}", inputs.finish());
}
