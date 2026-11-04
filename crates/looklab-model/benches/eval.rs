// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-color evaluation cost (docs/02 §8). Target: under 3 µs including the
//! `Y_ref` solve.

use criterion::{Criterion, criterion_group, criterion_main};
use looklab_color::Xyz;
use looklab_model::look::{Look, LookInput};
use std::hint::black_box;

const N: usize = 4096;

fn inputs() -> Vec<LookInput> {
    let mut state = 0x5eed_0000_0000_0001_u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..N)
        .map(|_| {
            let y = 0.02 + 2.0 * next();
            let x = y * (0.6 + 0.8 * next());
            let z = y * (0.5 + 0.9 * next());
            LookInput::new(Xyz::new(x, y, z))
        })
        .collect()
}

fn bench_eval(c: &mut Criterion) {
    let look = Look::colorimetric_spline().expect("v1 tone knots");
    let samples = inputs();
    let mut index = 0;
    c.bench_function("look_eval", |b| {
        b.iter(|| {
            let input = &samples[index % N];
            index += 1;
            black_box(look.eval_input(black_box(input)))
        });
    });
}

criterion_group!(benches, bench_eval);
criterion_main!(benches);
