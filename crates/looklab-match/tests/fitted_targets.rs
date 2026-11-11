// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::RgbSpace;
use looklab_fit::synth::{
    ControlSynthConfig, SynthConfig, controlled_dataset, random_control_model, random_settings,
    truth_look,
};
use looklab_fit::{FitOptions, fit_controlled};
use looklab_match::synth;
use looklab_match::{
    Emphasis, MatchModel, MatchOptions, OutputPin, Plan, PreparedTarget, Settings,
    TargetAppearance, TargetSpec, optimize,
};
use looklab_model::realization::ControlValue;
use looklab_schema::{OutputEncoding, SimKey};
use rand_chacha::ChaCha12Rng;
use rand_chacha::rand_core::SeedableRng;

const SCHEMA: &str = r#"
body: synthetic/fitted-match
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes:
      - key: dr100
    controls:
      - key: highlight_tone
        type: decimal
        range: [-1.0, 1.0]
        step: 0.5
        default: 0.0
        look_affecting: true
        affects: [tone]
      - key: color
        type: int
        range: [-2, 2]
        step: 1
        default: 0
        look_affecting: true
        affects: [chroma]
      - key: exposure_compensation
        type: decimal
        range: [-1.0, 1.0]
        step: 1/3
        default: 0.0
        look_affecting: false
        affects: [exposure]
    solve_plan:
      - stage: tone
        controls: [highlight_tone, exposure_compensation]
      - stage: color
        controls: [color]
"#;

fn within_one_step(plan: &Plan, a: &Settings, b: &Settings) -> bool {
    plan.plan_controls.iter().all(|&control| {
        match (plan.value(control, a), plan.value(control, b)) {
            (ControlValue::Step(x), ControlValue::Step(y)) => (x - y).abs() <= 1,
            (left, right) => left == right,
        }
    })
}

fn appearance(model: &MatchModel) -> TargetAppearance {
    TargetAppearance::Rendered(
        OutputPin::new(OutputEncoding::Srgb, model.base_look.clip.k).expect("clip"),
    )
}

fn median_delta(model: &MatchModel, target: &PreparedTarget<'_>, settings: &Settings) -> f64 {
    let realization = model.realize(settings).expect("realize");
    let mut values: Vec<f64> = target
        .supported
        .iter()
        .map(|index| {
            let probe = &target.probes.probes[*index];
            let prediction = model.scored_ucs(target.metric(), &realization, probe.xyz);
            let expected = target.target[*index];
            let delta = [
                prediction.j - expected.j,
                prediction.a - expected.a,
                prediction.b - expected.b,
            ];
            (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt()
        })
        .collect();
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn labels(plan: &Plan, settings: &Settings) -> String {
    plan.plan_controls
        .iter()
        .map(|&control| {
            format!(
                "{}={}",
                plan.key(control),
                plan.current_label(control, settings)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn fitted_realization_recovers_held_out_nondefault_renders() {
    let seed = 21;
    let schema = looklab_schema::validate_control_schema(SCHEMA).expect("schema");
    let epoch = schema.epochs[0].id;
    let sim = SimKey::new("classic_chrome").expect("sim");
    let truth_model = random_control_model(&schema, epoch, &sim, seed, 0.4);
    let mut rng = ChaCha12Rng::seed_from_u64(seed);
    let mut training_settings = Vec::new();
    let mut groups = Vec::new();
    for index in 0..8 {
        training_settings.push(random_settings(&truth_model, &mut rng, 0.8));
        let group = if index % 2 == 0 {
            [index as u8; 16]
        } else {
            [(index - 1) as u8; 16]
        };
        groups.push(Some(group));
    }
    let config = ControlSynthConfig {
        base: SynthConfig {
            seed,
            images: training_settings.len(),
            samples_per_image: 300,
            noise_ucs: 0.25,
            exposure_sd: 0.0,
            wb_sd: 0.0,
            hint_sd: 0.0,
            space: RgbSpace::Srgb,
            ..SynthConfig::default()
        },
        schema: schema.clone(),
        epoch,
        base_sim: sim.clone(),
        settings: training_settings.clone(),
        groups,
        response_scale: 0.4,
        truth_seed: seed,
    };
    let dataset = controlled_dataset(&config);
    let fit_options = FitOptions {
        seed,
        max_sweeps: 6,
        sweep_budget: 60_000.0,
        max_samples_per_image: 300,
        hessian_samples: 600,
        run_gcv: false,
        ..FitOptions::default()
    };
    let result = fit_controlled(&dataset.images, &schema, epoch, &sim, &fit_options).expect("fit");
    let response = result.control_response.as_ref().expect("control response");
    let transform = dataset.images[0].transform;
    let fitted = MatchModel::new(
        result.look.clone(),
        response.model.clone(),
        transform,
        None,
        1,
        1,
        OutputEncoding::Srgb,
    )
    .expect("fitted model");
    let truth = MatchModel::new(
        truth_look(seed),
        dataset.truth_model.clone(),
        transform,
        None,
        1,
        1,
        OutputEncoding::Srgb,
    )
    .expect("truth model");
    let plan = Plan::new(&schema, epoch).expect("plan");
    let probes = synth::probes();
    let options = MatchOptions {
        seed,
        warm_restarts: 8,
        perturbation_rounds: 3,
        ..MatchOptions::default()
    };
    let mut passed = 0;
    let targets = 10;
    for index in 0..targets {
        let mut settings = plan.default.clone();
        let source = &training_settings[index % training_settings.len()];
        for (key, value) in source.iter() {
            settings.set(key.clone(), value.clone());
        }
        let mut changed = false;
        for (offset, &control) in plan.plan_controls.iter().enumerate() {
            if (index + offset) % 3 != 0 {
                continue;
            }
            let size = plan.domains[control].values.len();
            let current = plan.value_index(control, &settings);
            let value = if current + 1 < size {
                current + 1
            } else {
                current.saturating_sub(1)
            };
            if value != current {
                plan.set(control, value, &mut settings);
                changed = true;
            }
        }
        if !changed {
            let control = plan.plan_controls[0];
            let size = plan.domains[control].values.len();
            let current = plan.value_index(control, &settings);
            plan.set(control, (current + 1).min(size - 1), &mut settings);
        }
        let target_function = synth::RealizationTarget {
            model: truth.clone(),
            settings: settings.clone(),
        };
        let target = PreparedTarget::new(TargetSpec {
            probes: &probes,
            function: &target_function,
            uncertainty: None,
            emphasis: Emphasis::Balanced,
            tau: 1.0,
            support_sigma: 1.5,
            density_floor: None,
            appearance: appearance(&truth),
        })
        .expect("target");
        let outcome =
            optimize(std::slice::from_ref(&fitted), &target, &plan, &options).expect("optimize");
        let best = outcome.results.first().expect("suggestion");
        let true_delta = median_delta(&fitted, &target, &settings);
        let close = within_one_step(&plan, &best.settings, &settings);
        assert!(
            close || best.median_delta_e <= true_delta + 0.5,
            "target {index}: {} best {:.3} (true {:.3}), true settings {}",
            labels(&plan, &best.settings),
            best.median_delta_e,
            true_delta,
            labels(&plan, &settings)
        );
        passed += 1;
    }
    assert_eq!(passed, targets);
}
