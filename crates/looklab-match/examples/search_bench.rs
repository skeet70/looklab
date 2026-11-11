// SPDX-License-Identifier: AGPL-3.0-or-later
//! Times the full settings search; run natively and under wasmtime:
//! `cargo run --release --example search_bench` and
//! `cargo build --release --example search_bench --target wasm32-wasip1`
//! then `wasmtime target/wasm32-wasip1/release/examples/search_bench.wasm`.
#![allow(clippy::disallowed_types)]

use std::time::Instant;

use looklab_match::synth;
use looklab_match::{Emphasis, MatchOptions, PreparedTarget, TargetSpec, optimize};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let plan = synth::plan();
    let probes = synth::probes();
    let truth = synth::body(0)?;
    let target_function = truth.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: truth.appearance(),
    })?;
    let mut models = Vec::new();
    for seed in 0..3 {
        let mut model = synth::body(seed)?.model;
        model.crm.base_sim = looklab_schema::SimKey::new(&format!("sim{seed}"))?;
        models.push(model);
    }
    let runs = 20;
    let mut times = Vec::with_capacity(runs);
    for run in 0..runs {
        let options = MatchOptions {
            seed: run as u64,
            ..MatchOptions::default()
        };
        let start = Instant::now();
        let outcome = optimize(&models, &target, &plan, &options)?;
        times.push(start.elapsed().as_secs_f64());
        if run == 0 {
            println!(
                "sims kept {}/{}, probes {} of {}, first median ΔE {:.3}",
                outcome.stats.sims_kept,
                outcome.stats.sims_considered,
                outcome.stats.probe_subset,
                outcome.stats.supported_probes,
                outcome.results[0].median_delta_e
            );
        }
    }
    times.sort_by(f64::total_cmp);
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95).div_ceil(100) - 1];
    println!(
        "search: median {:.3} s, p95 {:.3} s, max {:.3} s over {runs} runs",
        median,
        p95,
        times[times.len() - 1]
    );
    Ok(())
}
