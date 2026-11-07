// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_fit::{ComponentGauge, ExposureModeId, WbGaugeChange, WbModeClass, merge_gauges};
use looklab_model::{LookInput, ProbeId, probes};
use looklab_trust::{
    GateInput, GatedUnion, ProbeEstimate, ProbePrediction, SupportThresholds, TrustParams,
    VersionProbes, convert_exposure_gauge, exposure_gauge_excluded, gate_release,
};

fn probes_at(look: &looklab_model::Look, delta_xi: f64) -> VersionProbes {
    let probes = probes::v1();
    let thresholds = SupportThresholds::default();
    let convert = convert_exposure_gauge(
        |xyz: [f64; 3]| look.eval(looklab_color::Xyz::from_array(xyz)).to_array(),
        delta_xi,
    );
    VersionProbes {
        probes: probes
            .probes
            .iter()
            .map(|probe| {
                let jab = convert(probe.xyz.to_array());
                ProbeEstimate::new(
                    ProbePrediction {
                        id: probe.id.clone(),
                        pre_clip: jab,
                        sigma_pre_clip: 0.2,
                        post_clip: jab,
                        sigma_post_clip: 0.2,
                        bias_de: 0.0,
                        density: 10.0,
                        clusters: 5,
                        pre_clip_identified: true,
                    },
                    None,
                    &thresholds,
                )
            })
            .collect(),
    }
}

#[test]
fn body_joining_a_component_keeps_the_gauge() {
    let designated = body_of("fujifilm/x-t5");
    let gauge = ComponentGauge {
        designated: designated.clone(),
        exposure_mode: ExposureModeId(0),
        wb_mode: WbModeClass(0),
        anchored_delta: true,
    };
    let before = merge_gauges(1.25, 1.25, &gauge, &gauge);
    assert_eq!(before.wb, WbGaugeChange::Unchanged);
    assert!(before.is_pure_exposure());
    assert!(before.exposure.delta_xi.abs() < 1e-12);
    assert!((before.exposure.s_ceiling() - 6.0).abs() < 1e-12);

    let truth = looklab_fit::synth::truth_look(4);
    let version = probes_at(&truth, before.exposure.delta_xi);
    let report = gate_release(
        &TrustParams::default(),
        &GateInput {
            candidate: version.clone(),
            previous: version.clone(),
            anchor: version,
            root_observations: Vec::new(),
            gated_since_anchor: GatedUnion::default(),
        },
    );
    assert!(report.passed, "{:?}", report.violations);
    assert!(report.drift_anchor.over_bound.is_empty());
    assert!(report.drift_previous.over_bound.is_empty());
    assert!(report.drift_anchor.p95_ratio < 1e-9);
}

#[test]
fn merging_components_converts_the_exposure_gauge() {
    let left = ComponentGauge {
        designated: body_of("fujifilm/x-t5"),
        exposure_mode: ExposureModeId(0),
        wb_mode: WbModeClass(0),
        anchored_delta: true,
    };
    let right = ComponentGauge {
        designated: body_of("nikon/z6"),
        exposure_mode: ExposureModeId(1),
        wb_mode: WbModeClass(0),
        anchored_delta: false,
    };
    let change = merge_gauges(-0.4, 0.35, &left, &left);
    assert_eq!(change.wb, WbGaugeChange::Unchanged);
    assert!(change.is_pure_exposure());
    assert!((change.exposure.delta_xi - 0.75).abs() < 1e-12);

    let truth = looklab_fit::synth::truth_look(4);
    for probe in probes::v1().probes.iter().take(200) {
        let evaluated = change.exposure.evaluate(&truth, probe.xyz);
        let rescaled = truth.eval(change.exposure.apply(probe.xyz));
        assert!((evaluated.j - rescaled.j).abs() < 1e-9);
        assert!((evaluated.a - rescaled.a).abs() < 1e-9);
        assert!((evaluated.b - rescaled.b).abs() < 1e-9);
    }

    let reference = probes_at(&truth, 0.0);
    let converted = probes_at(&truth, change.exposure.delta_xi);
    let report = gate_release(
        &TrustParams::default(),
        &GateInput {
            candidate: converted.clone(),
            previous: converted.clone(),
            anchor: VersionProbes {
                probes: reference
                    .probes
                    .iter()
                    .map(|probe| {
                        let jab = change
                            .exposure
                            .evaluate(&truth, looklab_color::Xyz::from_array(probe_xyz(&probe.id)))
                            .to_array();
                        ProbeEstimate {
                            pre_clip: jab,
                            post_clip: jab,
                            ..probe.clone()
                        }
                    })
                    .collect(),
            },
            root_observations: Vec::new(),
            gated_since_anchor: GatedUnion::default(),
        },
    );
    assert!(report.passed, "{:?}", report.violations);
    assert!(report.drift_anchor.p95_ratio < 1e-9);

    let excluded = probes::v1()
        .probes
        .iter()
        .filter(|probe| {
            exposure_gauge_excluded(LookInput::new(probe.xyz).s, change.exposure.delta_xi)
        })
        .count();
    assert_eq!(excluded, 0, "a 0.75-stop conversion leaves the s-clamp");

    let wb_change = merge_gauges(
        -0.4,
        0.35,
        &left,
        &ComponentGauge {
            wb_mode: WbModeClass(1),
            ..right
        },
    );
    assert_eq!(wb_change.wb, WbGaugeChange::RefitAndReanchor);
    assert!(!wb_change.is_pure_exposure());
}

fn body_of(id: &str) -> looklab_body::BodyId {
    looklab_body::BodyId::new(id).expect("body id")
}

fn probe_xyz(id: &ProbeId) -> [f64; 3] {
    probes::v1()
        .probes
        .iter()
        .find(|probe| &probe.id == id)
        .expect("probe id is in v1")
        .xyz
        .to_array()
}
