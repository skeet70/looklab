// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::ProbeId;
use looklab_trust::harness::{Batch, HarnessParams, Sim};
use looklab_trust::{GateInput, GateReference, GatedUnion, VersionProbes, Violation, gate_release};
use looklab_upload::{BodyId, ContributionKind, LookSlug};

fn rooted(seed: u64) -> Sim {
    let truth = looklab_fit::Look::colorimetric_spline().expect("colorimetric look");
    let params = HarnessParams {
        fit: looklab_fit::FitOptions {
            seed,
            ..looklab_fit::FitOptions::default()
        },
        ..HarnessParams::default()
    };
    let mut sim = Sim::new(
        params,
        LookSlug::new("classic-chrome").expect("look slug"),
        BodyId::new("fujifilm/x-t5").expect("body id"),
        truth,
    )
    .expect("sim");
    let look = sim.truth().clone();
    let seeder = sim.seeder();
    for (index, kind) in [
        ContributionKind::Chart,
        ContributionKind::Gradient,
        ContributionKind::Pattern,
    ]
    .into_iter()
    .enumerate()
    {
        sim.add(
            seeder,
            Batch {
                images: 1,
                seed: seed ^ (0x51 + index as u64),
                samples: 500,
                noise_ucs: 0.0,
                ..Batch::new(&look, kind)
            },
        )
        .expect("root images");
    }
    sim.admit_root();
    sim
}

#[test]
fn fitted_probe_estimates_publish_and_enforce_the_anchor_ceiling() {
    let mut sim = rooted(0x0c01);
    let outcome = sim.release().expect("fitted release");
    assert!(outcome.published, "{:?}", outcome.gate.violations);
    let published = sim.published().expect("published fitted version");
    assert_eq!(published.id, outcome.version);

    let stable_probe = ProbeId::new("gray-s+0.00");
    let fitted = published
        .probes
        .iter()
        .find(|probe| probe.id == stable_probe)
        .expect("fitted gray reference");
    assert!(fitted.supported(), "unsupported reference: {fitted:?}");
    assert!(fitted.sigma_post_clip.is_finite() && fitted.sigma_post_clip > 0.0);
    assert!(fitted.bias_de.is_finite());

    let candidate = VersionProbes {
        probes: published.probes.clone(),
    };
    let mut anchor = candidate.clone();
    let ceiling = sim.params().trust.gate_anchor_ceiling;
    let displacement = ceiling + 0.5;
    let reference = anchor
        .probes
        .iter_mut()
        .find(|probe| probe.id == stable_probe)
        .expect("persisted gray reference");
    reference.pre_clip[0] -= displacement;
    reference.post_clip[0] -= displacement;

    let report = gate_release(
        &sim.params().trust,
        &GateInput {
            previous: candidate.clone(),
            candidate,
            anchor,
            root_observations: Vec::new(),
            gated_since_anchor: GatedUnion::default(),
        },
    );
    assert!(report.drift_previous.pass);
    assert!(!report.passed);
    assert!(
        report.violations.iter().any(|violation| matches!(
            violation,
            Violation::OverCeiling {
                reference: GateReference::Anchor,
                probe,
                shift,
                ceiling: reported_ceiling,
            } if probe == &stable_probe
                && (*shift - displacement).abs() < 1e-9
                && *reported_ceiling == ceiling
        )),
        "{:?}",
        report.violations
    );
}
