// SPDX-License-Identifier: AGPL-3.0-or-later
//! Bit-identity workload for the match path.

use looklab_match::synth;
use looklab_match::{Emphasis, MatchModel, MatchOptions, PreparedTarget, TargetSpec, optimize};
use looklab_model::realization::{decode_crm, encode_crm};

fn main() {
    let body = synth::body(9).expect("synthetic body");
    let encoded = encode_crm(&body.model.crm).expect("encode crm");
    let crm = decode_crm(&encoded).expect("decode crm");
    println!("crm_bytes {}", encoded.len());
    println!("crm_equal {}", crm == body.model.crm);

    let model = MatchModel::new(
        body.model.base_look.clone(),
        crm,
        body.model.transform,
        None,
        1,
        1,
        body.model.output_encoding,
    )
    .expect("model");
    let probes = synth::probes();
    let target_function = body.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("target");
    let plan = synth::plan();
    let options = MatchOptions {
        seed: 7,
        ..MatchOptions::default()
    };
    let outcome =
        optimize(std::slice::from_ref(&model), &target, &plan, &options).expect("optimize");
    let best = &outcome.results[0];
    println!("linear_evaluations {}", outcome.stats.linear_evaluations);
    println!("exact_evaluations {}", outcome.stats.exact_evaluations);
    println!("relinearizations {}", outcome.stats.relinearizations);
    println!("supported {}", outcome.stats.supported_probes);
    println!("objective {:016x}", best.objective.to_bits());
    println!("median {:016x}", best.median_delta_e.to_bits());
    println!("p95 {:016x}", best.p95_delta_e.to_bits());
    println!("max {:016x}", best.max_delta_e.to_bits());
    println!("band {:016x}", best.sigma_band.map_or(0, f64::to_bits));
}
