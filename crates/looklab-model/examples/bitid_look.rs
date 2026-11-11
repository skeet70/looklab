// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native/wasm bit-identity workload for Look evaluation; `cargo xtask bitid`
//! compares it with a `wasm32-wasip1` run.

use looklab_color::Xyz;
use looklab_model::look::{ColorLayer, Look, Residual, Tint};
use looklab_model::{ClipSettings, Tone};

const SAMPLES: usize = 100_000;

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
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

fn random_look(rng: &mut SplitMix64) -> Look {
    let mut beta = [0.0_f64; looklab_model::basis::TONE_COEFS];
    for value in beta.iter_mut() {
        *value = rng.range(0.0, 0.4);
    }
    let tone = Tone::spline(rng.range(-1.0, 1.0), beta).expect("non-negative betas");
    let tint = Tint {
        a: (0..looklab_model::basis::TINT_COEFS)
            .map(|_| rng.range(-2.0, 2.0))
            .collect(),
        b: (0..looklab_model::basis::TINT_COEFS)
            .map(|_| rng.range(-2.0, 2.0))
            .collect(),
    };
    let mut color = ColorLayer::zeros();
    for component in [&mut color.g, &mut color.r, &mut color.lambda] {
        for coefficient in component
            .plane
            .iter_mut()
            .chain(component.volume.iter_mut())
            .chain(component.core.iter_mut())
        {
            *coefficient = rng.range(-0.3, 0.3);
        }
    }
    let residual = Residual {
        values: (0..looklab_model::basis::RESIDUAL_COEFS)
            .map(|_| rng.range(-0.2, 0.2))
            .collect(),
    };
    Look::new(tone, tint, color, residual, ClipSettings::default_v1()).expect("zero clip k")
}

fn main() {
    let mut rng = SplitMix64(0x5eed_1000_0000_0001);
    let look = random_look(&mut rng);
    let mut eval = Fnv::new();
    let mut encoded = Fnv::new();

    for sample in 0..SAMPLES {
        let y = rng.range(0.005, 4.0);
        let white = looklab_model::solve::reference_white();
        let xyz = if sample % 8 == 0 {
            white.scale(y)
        } else if sample % 4 == 0 {
            let angle = 2.0 * core::f64::consts::PI * rng.unit();
            let radius = 0.6 * rng.unit() * rng.unit();
            Xyz::new(
                y * (white.x + radius * libm::cos(angle)),
                y,
                y * (white.z + radius * libm::sin(angle)),
            )
        } else {
            Xyz::new(y * rng.range(0.4, 1.6), y, y * rng.range(0.3, 1.8))
        };
        let ucs = look.eval(xyz);
        eval.mix(ucs.j);
        eval.mix(ucs.a);
        eval.mix(ucs.b);
        let out = look.output_encode(ucs, looklab_color::RgbSpace::Srgb);
        encoded.mix(out.j);
        encoded.mix(out.a);
        encoded.mix(out.b);
    }

    println!("look_eval {:016x}", eval.finish());
    println!("output_encode_clip {:016x}", encoded.finish());
}
