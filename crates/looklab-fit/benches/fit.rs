// SPDX-License-Identifier: AGPL-3.0-or-later
//! Timing for the P1.5 budget: 20 images of 10k samples (docs/09).

use criterion::{Criterion, criterion_group, criterion_main};
use looklab_fit::synth::{SynthConfig, dataset};
use looklab_fit::{FitOptions, fit};

fn full(c: &mut Criterion) {
    let config = SynthConfig {
        seed: 71,
        images: 20,
        samples_per_image: 10_000,
        ..SynthConfig::default()
    };
    let data = dataset(&config);
    let mut group = c.benchmark_group("fit");
    group.sample_size(10);
    group.bench_function("20_images_10k_samples", |b| {
        b.iter(|| {
            fit(
                &data.images,
                &FitOptions {
                    seed: 17,
                    ..FitOptions::default()
                },
            )
        })
    });
    group.finish();
}

criterion_group!(benches, full);
criterion_main!(benches);
