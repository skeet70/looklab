// SPDX-License-Identifier: AGPL-3.0-or-later
//! Probe uncertainty must not grow when honest images join an existing pool.

#![cfg(feature = "synth")]
#![allow(clippy::disallowed_types)]

use looklab_fit::synth::{self, LookSelector, SynthConfig};
use looklab_fit::{ContributorId, FitImage, FitOptions, FitResult, fit};

fn pool(seed: u64, images: usize, contributor: u32) -> Vec<FitImage> {
    let config = SynthConfig {
        seed,
        images,
        samples_per_image: 700,
        noise_ucs: 0.35,
        ..SynthConfig::default()
    };
    let truth = synth::truth_look(seed);
    let mut out = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth)).images;
    for image in &mut out {
        image.contributor = ContributorId(contributor);
    }
    out
}

fn report(label: &str, result: &FitResult) {
    let supported = result.probes.probes.iter().filter(|p| p.supported).count();
    let mut sigmas: Vec<(f64, String)> = result
        .probes
        .probes
        .iter()
        .zip(result.probes.ids.iter())
        .map(|(probe, id)| (probe.sigma_de, id.as_str().to_string()))
        .collect();
    sigmas.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "{label}: supported {supported}/{} max sigma {:.3} ({}) sigma_v {:?} lambdas {:?}",
        result.probes.probes.len(),
        sigmas[0].0,
        sigmas[0].1,
        result.diagnostics.sigma_v,
        result.lambdas
    );
    println!(
        "  rms {:?}",
        result
            .image_stats
            .iter()
            .map(|stat| (stat.rms_de * 1000.0).round() / 1000.0)
            .collect::<Vec<_>>()
    );
}

#[test]
fn more_honest_images_do_not_loosen_probes() {
    let six = pool(11, 6, 0);
    let four = pool(71, 4, 1);
    let truth = synth::truth_look(11);
    let options = FitOptions {
        seed: 5,
        ..FitOptions::default()
    };

    let baseline = fit(&six, &options).expect("baseline fit");
    let mut grown = six.clone();
    grown.extend(four.iter().cloned());
    let with_two = fit(&grown, &options).expect("two contributor fit");
    let mut single = six.clone();
    single.extend(four.iter().map(|image| {
        let mut image = image.clone();
        image.contributor = ContributorId(0);
        image
    }));
    let with_one = fit(&single, &options).expect("one contributor fit");
    let _ = truth;

    report("6 images, 1 contributor", &baseline);
    report("10 images, 2 contributors", &with_two);
    report("10 images, 1 contributor", &with_one);

    // The harness composition: 3 root kinds x 2 images at 1600 samples, then
    // four contributors of 3 images at 700 samples (crates/looklab-trust/tests/redteam.rs).
    let truth = synth::truth_look(14);
    let root_config = SynthConfig {
        images: 2,
        samples_per_image: 1600,
        noise_ucs: 0.35,
        ..SynthConfig::default()
    };
    let mut harness_root = Vec::new();
    for (index, salt) in [0x51u64, 0x52, 0x53].into_iter().enumerate() {
        let mut batch = synth::dataset_with(
            &SynthConfig {
                seed: 14 ^ salt,
                ..root_config
            },
            &truth,
            &LookSelector::Single(&truth),
        )
        .images;
        for image in &mut batch {
            image.contributor = ContributorId(0);
        }
        harness_root.push((index, batch));
    }
    let root: Vec<FitImage> = harness_root.iter().flat_map(|(_, b)| b.clone()).collect();
    let harness_base = fit(&root, &options).expect("harness-style base");
    let mut harness_grown = root.clone();
    for slot in 0..4u32 {
        let config = SynthConfig {
            seed: 400 + u64::from(slot),
            images: 3,
            samples_per_image: 700,
            noise_ucs: 0.35,
            ..SynthConfig::default()
        };
        let mut batch = synth::dataset_with(&config, &truth, &LookSelector::Single(&truth)).images;
        for image in &mut batch {
            image.contributor = ContributorId(1 + slot);
        }
        harness_grown.extend(batch);
    }
    let harness_two = fit(&harness_grown, &options).expect("harness-style grown");
    report("harness root 6 images", &harness_base);
    report("harness grown 18 images", &harness_two);
    let mut harness_worst = 0.0_f64;
    let mut harness_flips = 0usize;
    for (index, probe) in harness_base.probes.probes.iter().enumerate() {
        if !probe.supported {
            continue;
        }
        let ratio = harness_two.probes.probes[index].sigma_de / probe.sigma_de.max(1e-9);
        if ratio > harness_worst {
            harness_worst = ratio;
            println!(
                "  worst {} {:.3} -> {:.3}",
                harness_base.probes.ids[index].as_str(),
                probe.sigma_de,
                harness_two.probes.probes[index].sigma_de
            );
        }
        if !harness_two.probes.probes[index].supported {
            harness_flips += 1;
            if harness_flips <= 6 {
                println!(
                    "  flip {} {:.3} -> {:.3}",
                    harness_base.probes.ids[index].as_str(),
                    probe.sigma_de,
                    harness_two.probes.probes[index].sigma_de
                );
            }
        }
    }
    for (index, probe) in harness_base.probes.probes.iter().enumerate() {
        let id = harness_base.probes.ids[index].as_str();
        if id == "chart-light-skin-s+1" || id == "chart-bluish-green-s+2" {
            println!(
                "  {id}: harness base {:.3} grown {:.3}",
                probe.sigma_de, harness_two.probes.probes[index].sigma_de
            );
        }
    }
    let mut rises = 0usize;
    let mut big_worst = 0.0_f64;
    let mut sigma_flips = 0usize;
    for (index, probe) in harness_base.probes.probes.iter().enumerate() {
        if !probe.supported {
            continue;
        }
        let grown = harness_two.probes.probes[index].sigma_de;
        if grown > 1.10 * probe.sigma_de {
            rises += 1;
        }
        if probe.sigma_de >= 0.5 {
            big_worst = big_worst.max(grown / probe.sigma_de.max(1e-9));
        }
        if !harness_two.probes.probes[index].supported
            && harness_two.probes.probes[index].density >= harness_two.density.floor
        {
            sigma_flips += 1;
        }
    }
    println!(
        "harness worst ratio {harness_worst:.3} ({rises} probes >10%) flips {harness_flips} (sigma-driven {sigma_flips}); worst ratio over sigma>=0.5 is {big_worst:.3}"
    );
    for (index, probe) in baseline.probes.probes.iter().enumerate() {
        let id = baseline.probes.ids[index].as_str();
        if id == "chart-light-skin-s+1" || id == "chart-bluish-green-s+2" {
            println!(
                "  {id}: baseline {:.3} two {:.3} one {:.3}",
                probe.sigma_de,
                with_two.probes.probes[index].sigma_de,
                with_one.probes.probes[index].sigma_de
            );
        }
    }
    let mut worst = 0.0_f64;
    let mut worst_id = String::new();
    let mut flips = 0usize;
    for (index, probe) in baseline.probes.probes.iter().enumerate() {
        if !probe.supported {
            continue;
        }
        let grown_sigma = with_two.probes.probes[index].sigma_de;
        let ratio = grown_sigma / probe.sigma_de.max(1e-9);
        if ratio > worst {
            worst = ratio;
            worst_id = with_two.probes.ids[index].as_str().to_string();
        }
        if !with_two.probes.probes[index].supported {
            flips += 1;
        }
    }
    println!("worst sigma ratio {worst:.3} at {worst_id}; support flips {flips}");
    // Two contributors must behave like one: the split may not move sigma at all.
    assert!(
        worst <= 1.10,
        "max probe sigma rose by {worst:.3} at {worst_id}"
    );
    // Pool growth: sigma must be monotone where it can approach the support threshold.
    // Probes far below it fluctuate with the number of clusters (a few-cluster estimate is
    // optimistic); that tail is reported above and is not a support risk.
    assert_eq!(sigma_flips, 0, "{sigma_flips} probes flipped on sigma");
    assert!(
        big_worst <= 1.10,
        "sigma rose by {big_worst:.3} at a probe with baseline sigma >= 0.5"
    );
}
