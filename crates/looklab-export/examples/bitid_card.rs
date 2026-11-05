// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native/wasm bit-identity workload for the card writer and grid evaluator;
//! `cargo xtask bitid` compares it with a `wasm32-wasip1` run.

use looklab_export::card::{CardMeta, CardOptions, read_card, write_card};
use looklab_model::file::{LookIdentity, LookKind, WbPolicy};
use looklab_model::look::{ColorLayer, Look, Residual, Tint};
use looklab_model::{ClipSettings, LookDocument, Tone};

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
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn mix(&mut self, value: f64) {
        for byte in value.to_bits().to_le_bytes() {
            self.mix_byte(byte);
        }
    }

    fn mix_u32(&mut self, value: u32) {
        for byte in value.to_le_bytes() {
            self.mix_byte(byte);
        }
    }

    fn mix_byte(&mut self, byte: u8) {
        self.0 ^= u64::from(byte);
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

fn synthetic_document() -> LookDocument {
    let mut beta = [0.0_f64; looklab_model::basis::TONE_COEFS];
    for (index, value) in beta.iter_mut().enumerate() {
        *value = 0.01 + 0.02 * (index % 5) as f64;
    }
    let tone = Tone::spline(0.01, beta).expect("non-negative betas");
    let mut color = ColorLayer::zeros();
    for (index, coefficient) in color
        .g
        .plane
        .iter_mut()
        .chain(color.g.volume.iter_mut())
        .enumerate()
    {
        *coefficient = 0.01 * (index % 3) as f64;
    }
    let look = Look::new(
        tone,
        Tint::zeros(),
        color,
        Residual::zeros(),
        ClipSettings::default_v1(),
    )
    .expect("zero clip k");
    let identity = LookIdentity::new(
        "bitid-card",
        "1.0.0",
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("static identity");
    LookDocument::from_look(identity, look).expect("document")
}

fn main() {
    let document = synthetic_document();
    let meta = CardMeta::new("Bit Identity", "Bit ID")
        .with_description("Synthetic card for native/wasm comparison.");
    let bytes = write_card(&document, &meta, &CardOptions::default()).expect("card");
    let card = read_card(&bytes).expect("card reads");

    let mut content = Fnv::new();
    for byte in card.content_id() {
        content.mix_byte(byte);
    }
    println!("content-id {:016x}", content.finish());

    let mut rng = SplitMix64(0x5eed_ca7d_0000_0002);
    let mut linear = Fnv::new();
    let mut shaped = Fnv::new();
    for _ in 0..SAMPLES {
        let input = [rng.unit() * 8.0, rng.unit() * 8.0, rng.unit() * 8.0];
        let output = card.eval_grid(input).expect("grid");
        for value in output {
            linear.mix(value);
        }
        let input = [rng.unit(), rng.unit(), rng.unit()];
        let output = card.eval_grid_shaped(input).expect("grid");
        for value in output {
            shaped.mix(value);
        }
    }
    println!("eval-grid {:016x}", linear.finish());
    println!("eval-grid-shaped {:016x}", shaped.finish());

    let mut neutral = Fnv::new();
    for value in card.neutral() {
        for channel in value {
            neutral.mix(f64::from(*channel));
        }
    }
    println!("neutral {:016x}", neutral.finish());

    let mut fit = Fnv::new();
    for record in card.fit() {
        for value in record.input.iter().chain(record.output.iter()) {
            fit.mix(f64::from(*value));
        }
        fit.mix(f64::from(record.sigma));
        fit.mix(f64::from(record.weight));
        fit.mix_u32(u32::from(record.supported));
        fit.mix_u32(record.probe);
    }
    println!("fit {:016x}", fit.finish());
}
