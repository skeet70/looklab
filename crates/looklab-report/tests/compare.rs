// SPDX-License-Identifier: AGPL-3.0-or-later
//! P1.6 acceptance tests: injected layer deltas, identical looks, SVG structure.

use looklab_color::cam16::{Cam16Model, Chroma, ViewingConditions};
use looklab_color::ucs::Ucs;
use looklab_color::{Illuminant, Xyz};
use looklab_model::basis::TONE_KNOTS;
use looklab_model::probes::{Probe, ProbeGroup, ProbeId, ProbeSet};
use looklab_model::{ColorLayer, Look, LookInput, ProbeSetVersion, Tone};
use looklab_report::compare::{
    Comparison, ComparisonInput, Covariance3, FunctionName, LayerId, LookFunction, Support,
    compare, hue_bins, look_diagnostics,
};
use looklab_report::{
    ReportSource, delta_e_histogram_svg, grl_svg, look_page, region_map_svg, tint_comparison_svg,
    tone_comparison_svg,
};
use roxmltree::Document;

fn base_look() -> Look {
    Look::new(
        Tone::colorimetric(&TONE_KNOTS).expect("fixed knots"),
        looklab_model::Tint::zeros(),
        ColorLayer::zeros(),
        looklab_model::Residual::zeros(),
        looklab_model::ClipSettings::default_v1(),
    )
    .expect("zero layers")
}

fn with_tone_lift(mut look: Look, lift: f64) -> Look {
    let Tone::Spline { c0, mut beta } = look.tone else {
        unreachable!("constructed as a spline")
    };
    let base = look.tone;
    let mut shape = beta;
    for coefficient in shape.iter_mut().take(18).skip(14) {
        *coefficient += 1.0;
    }
    let trial = Tone::spline(c0, shape).expect("non-negative betas");
    let mean = (trial.eval(1.0) + trial.eval(2.0) + trial.eval(3.0)) / 3.0
        - (base.eval(1.0) + base.eval(2.0) + base.eval(3.0)) / 3.0;
    let scale = lift / mean;
    for coefficient in beta.iter_mut().take(18).skip(14) {
        *coefficient += scale;
    }
    look.tone = Tone::spline(c0, beta).expect("non-negative betas");
    look
}

fn with_tint(mut look: Look, b_shift: f64) -> Look {
    look.tint.b.fill(b_shift);
    look
}

fn with_chroma(mut look: Look, log_gain: f64, hue_indices: &[usize]) -> Look {
    for hue in hue_indices {
        for s in 0..10 {
            for c in 0..3 {
                look.color.g.volume[(hue * 10 + s) * 3 + c] = log_gain;
            }
        }
    }
    look
}

fn with_hue_rotation(mut look: Look, degrees: f64, hue_indices: &[usize]) -> Look {
    for hue in hue_indices {
        for s in 0..10 {
            for c in 0..3 {
                look.color.r.volume[(hue * 10 + s) * 3 + c] = degrees;
            }
        }
    }
    look
}

fn with_lightness(mut look: Look, shift: f64, hue_indices: &[usize]) -> Look {
    for hue in hue_indices {
        for s in 0..10 {
            for c in 0..3 {
                look.color.lambda.volume[(hue * 10 + s) * 3 + c] = shift;
            }
        }
    }
    look
}

fn probes() -> ProbeSet {
    let model = Cam16Model::new(&ViewingConditions::reference());
    let mut probes = Vec::new();
    for step in 0..=60 {
        let s = -10.0 + 0.25 * step as f64;
        probes.push(Probe {
            id: ProbeId::new(format!("gray-s{s:+.2}")),
            group: ProbeGroup::GrayRamp,
            weight: 1.0,
            xyz: Illuminant::D65.xyz().scale(0.18 * libm::pow(2.0, s)),
        });
    }
    for bin in 0..24 {
        let hue = 15.0 * bin as f64 + 7.5;
        for s in -5..=3 {
            for (level, chroma) in [20.0, 40.0, 60.0, 80.0].into_iter().enumerate() {
                let color = model.cam16_to_xyz(50.0, Chroma::M(chroma), hue);
                let target = 0.18 * libm::pow(2.0, s as f64);
                probes.push(Probe {
                    id: ProbeId::new(format!("hue-{hue:03.0}-{:02}-s{s:+03}", (level + 1) * 20)),
                    group: ProbeGroup::HueGrid,
                    weight: 0.5,
                    xyz: color.scale(target / color.y.max(1e-9)),
                });
            }
        }
    }
    ProbeSet {
        version: ProbeSetVersion::V1,
        notes: vec!["synthetic comparison fixture".to_string()],
        probes,
    }
}

struct SupportZone {
    look: Look,
}

impl LookFunction for SupportZone {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.look.eval(xyz)
    }

    fn support(&self, probe: &Probe) -> Support {
        if probe.id.as_str().contains("s-05") {
            Support::Unsupported
        } else {
            Support::Supported
        }
    }

    fn layer_values(&self, input: &LookInput) -> Option<looklab_model::LayerValues> {
        Some(Look::layer_values(&self.look, input))
    }

    fn look(&self) -> Option<&Look> {
        Some(&self.look)
    }
}

struct Uncertain {
    look: Look,
}

impl LookFunction for Uncertain {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.look.eval(xyz)
    }

    fn covariance(&self, probe: &Probe) -> Option<Covariance3> {
        let hue = LookInput::new(probe.xyz).h_in;
        let sigma = 0.2 + 0.1 * libm::sin(hue * core::f64::consts::PI / 180.0);
        Some(Covariance3::diagonal([sigma, sigma, sigma]))
    }

    fn layer_values(&self, input: &LookInput) -> Option<looklab_model::LayerValues> {
        Some(Look::layer_values(&self.look, input))
    }

    fn look(&self) -> Option<&Look> {
        Some(&self.look)
    }
}

fn run(a: &dyn LookFunction, b: &dyn LookFunction, set: &ProbeSet) -> Comparison {
    let a_name = FunctionName::new("A");
    let b_name = FunctionName::new("B");
    compare(&ComparisonInput {
        a,
        b,
        probes: set,
        a_name: &a_name,
        b_name: &b_name,
    })
}

fn top_layer(comparison: &Comparison) -> LayerId {
    comparison.attribution[0].layer
}

#[test]
fn tone_shoulder_lift_is_named_and_ranked_first() {
    let a = base_look();
    let b = with_tone_lift(base_look(), 2.5);
    let comparison = run(&a, &b, &probes());

    assert_eq!(top_layer(&comparison), LayerId::Tone);
    assert!(comparison.attribution[0].contribution > 0.1);
    assert!(comparison.summary.contains("brighter in the highlights"));
    assert!(comparison.summary.contains("J′ above s = +1"));
}

#[test]
fn tint_shift_is_named_and_ranked_first() {
    let a = base_look();
    let b = with_tint(base_look(), 1.2);
    let comparison = run(&a, &b, &probes());

    assert_eq!(top_layer(&comparison), LayerId::Tint);
    assert!(comparison.summary.contains("warmer neutrals"));
    assert!(comparison.summary.contains("b′ +1.2"));
}

#[test]
fn chroma_gain_in_reds_is_named_and_ranked_first() {
    let a = base_look();
    let b = with_chroma(base_look(), -0.18, &[1]);
    let comparison = run(&a, &b, &probes());

    assert_eq!(top_layer(&comparison), LayerId::Chroma);
    assert!(comparison.summary.contains("less saturated in reds"));
    assert!(comparison.summary.contains("0–40°"));
}

#[test]
fn hue_rotation_in_blues_is_named_and_ranked_first() {
    let a = base_look();
    let b = with_hue_rotation(base_look(), 6.0, &[16, 17]);
    let comparison = run(&a, &b, &probes());

    assert_eq!(top_layer(&comparison), LayerId::Hue);
    assert!(comparison.summary.contains("hue rotated"));
    assert!(comparison.summary.contains("blues (205–265°)"));
}

#[test]
fn lightness_shift_in_yellows_is_named_and_ranked_first() {
    let a = base_look();
    let b = with_lightness(base_look(), 2.0, &[6, 7]);
    let comparison = run(&a, &b, &probes());

    assert_eq!(top_layer(&comparison), LayerId::Lightness);
    assert!(comparison.summary.contains("lighter in yellows"));
    assert!(comparison.summary.contains("75–115°"));
}

#[test]
fn combined_deltas_appear_in_order_of_magnitude() {
    let a = base_look();
    let mut b = with_chroma(base_look(), -0.10, &[1]);
    b = with_tone_lift(b, 3.0);
    let comparison = run(&a, &b, &probes());

    let order: Vec<LayerId> = comparison.attribution.iter().map(|c| c.layer).collect();
    let tone_rank = order.iter().position(|layer| *layer == LayerId::Tone);
    let chroma_rank = order.iter().position(|layer| *layer == LayerId::Chroma);
    assert!(tone_rank.is_some() && chroma_rank.is_some());
    assert!(tone_rank.unwrap() < chroma_rank.unwrap());

    let tone_at = comparison.summary.find("highlights");
    let chroma_at = comparison.summary.find("saturated");
    assert!(tone_at.is_some() && chroma_at.is_some());
    assert!(tone_at.unwrap() < chroma_at.unwrap());
}

#[test]
fn identical_looks_have_zero_delta_and_no_summary_difference() {
    let a = base_look();
    let b = base_look();
    let comparison = run(&a, &b, &probes());

    assert_eq!(comparison.delta_e.max, 0.0);
    assert_eq!(comparison.ciede2000.max, 0.0);
    assert!(comparison.summary.contains("no meaningful difference"));
    assert!(
        comparison
            .attribution
            .iter()
            .all(|contribution| contribution.contribution == 0.0)
    );
}

#[test]
fn every_svg_parses_and_unsupported_regions_are_hatched() {
    let a = SupportZone { look: base_look() };
    let b = SupportZone {
        look: with_tone_lift(base_look(), 1.5),
    };
    let set = probes();
    let comparison = run(&a, &b, &set);

    let documents = [
        tone_comparison_svg(&comparison.tone),
        tint_comparison_svg(&comparison.tint),
        grl_svg(
            comparison
                .grl
                .as_ref()
                .expect("both functions expose layers"),
        ),
        delta_e_histogram_svg(&comparison.delta_e),
        region_map_svg(&comparison.region_map),
    ];
    for document in &documents {
        Document::parse(document).expect("well-formed SVG");
    }
    assert!(documents[4].contains("url(#hatch-unsupported)"));
    assert!(comparison.region_map.cells[0][0].delta_e.is_none());
    assert!(comparison.delta_e.excluded > 0);
}

#[test]
fn look_page_renders_well_formed_html_with_inline_svgs() {
    let view = Uncertain {
        look: with_tone_lift(base_look(), 1.0),
    };
    let diagnostics = look_diagnostics(&view, &probes());
    let comparison = run(&view, &view, &probes());
    let page = look_page(
        &ReportSource {
            label: "synthetic".to_string(),
            ..ReportSource::default()
        },
        &diagnostics,
    );
    assert!(page.starts_with("<!DOCTYPE html>"));
    assert!(page.contains("<svg"));
    let grl = comparison.grl.as_ref().expect("layer curves");
    let svg = grl_svg(grl);
    Document::parse(&svg).expect("well-formed GRL SVG");
    assert!(svg.contains("fill-opacity=\"0.16\""));
    Document::parse(&looklab_report::tone_curve_svg(&diagnostics.tone)).expect("tone SVG");
    Document::parse(&looklab_report::tint_curve_svg(&diagnostics.tint)).expect("tint SVG");
}

#[test]
fn probe_set_v1_yields_region_map_and_layer_curves() {
    let set = looklab_model::probes::v1();
    let a = base_look();
    let b = with_chroma(base_look(), -0.18, &[1]);
    let comparison = run(&a, &b, &set);

    let populated = comparison
        .region_map
        .cells
        .iter()
        .flatten()
        .filter(|cell| cell.delta_e.is_some())
        .count();
    assert!(populated >= 100, "populated cells: {populated}");
    assert!(comparison.grl.is_some());
    assert!(comparison.delta_e.count > 0);
    Document::parse(&region_map_svg(&comparison.region_map)).expect("well-formed SVG");
}

#[test]
fn worst_probes_are_ordered_and_hue_bins_cover_the_circle() {
    let a = base_look();
    let b = with_chroma(base_look(), -0.18, &[1]);
    let comparison = run(&a, &b, &probes());
    let worst = &comparison.delta_e.worst;
    assert!(worst.len() <= 10);
    assert!(
        worst
            .windows(2)
            .all(|pair| pair[0].delta_e >= pair[1].delta_e)
    );
    assert_eq!(hue_bins().len(), 24);
}
