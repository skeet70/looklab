// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writes `target/report-example.html` for a synthetic two-look comparison.
#![allow(clippy::disallowed_methods)]

use std::path::Path;

use looklab_color::cam16::{Cam16Model, Chroma, ViewingConditions};
use looklab_color::ucs::Ucs;
use looklab_color::{Illuminant, Xyz};
use looklab_model::basis::TONE_KNOTS;
use looklab_model::file::{
    BodyProvenance, LookIdentity, LookKind, Provenance, RootProvenance, WbPolicy,
};
use looklab_model::probes::{Probe, ProbeGroup, ProbeId, ProbeSet};
use looklab_model::{ColorLayer, Look, LookDocument, LookFile, ProbeSetVersion, Tone};
use looklab_report::compare::{
    Comparison, ComparisonInput, Covariance3, FunctionName, LayerCovariance, LookFunction, Support,
    compare,
};
use looklab_report::{ArtifactHash, ReportSource, comparison_page};

struct SyntheticLook {
    look: Look,
}

impl LookFunction for SyntheticLook {
    fn predict(&self, xyz: Xyz) -> Ucs {
        self.look.eval(xyz)
    }

    fn covariance(&self, probe: &Probe) -> Option<Covariance3> {
        let hue = looklab_model::LookInput::new(probe.xyz).h_in;
        let base = 0.25 + 0.2 * libm::sin(hue * core::f64::consts::PI / 180.0);
        Some(Covariance3::diagonal([base, base, base]))
    }

    fn support(&self, probe: &Probe) -> Support {
        if probe.id.as_str().contains("s-05") || probe.id.as_str().contains("s+03") {
            Support::Unsupported
        } else {
            Support::Supported
        }
    }

    fn layer_values(&self, input: &looklab_model::LookInput) -> Option<looklab_model::LayerValues> {
        Some(looklab_model::Look::layer_values(&self.look, input))
    }

    fn layer_covariance(&self, input: &looklab_model::LookInput) -> Option<LayerCovariance> {
        let hue = input.h_in * core::f64::consts::PI / 180.0;
        Some(LayerCovariance {
            t: 0.04,
            n_a: 0.02,
            n_b: 0.02,
            log_g: 0.004 + 0.002 * libm::sin(hue),
            r: 0.5,
            lambda: 0.02,
        })
    }

    fn look(&self) -> Option<&Look> {
        Some(&self.look)
    }
}

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

fn lifted_look() -> Look {
    let mut look = base_look();
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
    for coefficient in beta.iter_mut().take(18).skip(14) {
        *coefficient += 2.0 / mean;
    }
    look.tone = Tone::spline(c0, beta).expect("non-negative betas");
    for hue in 0..3 {
        for s in 0..10 {
            for c in 0..3 {
                look.color.g.volume[(hue * 10 + s) * 3 + c] = -0.18;
            }
        }
    }
    look.tint.b.fill(1.2);
    look
}

fn synthetic_probes() -> ProbeSet {
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
                let xyz = color.scale(target / color.y.max(1e-9));
                probes.push(Probe {
                    id: ProbeId::new(format!("hue-{hue:03.0}-{:02}-s{s:+03}", (level + 1) * 20)),
                    group: ProbeGroup::HueGrid,
                    weight: 0.5,
                    xyz,
                });
            }
        }
    }
    ProbeSet {
        version: ProbeSetVersion::V1,
        notes: vec!["synthetic report fixture".to_string()],
        probes,
    }
}

fn document(look: Look, slug: &str) -> LookDocument {
    let identity = LookIdentity::new(slug, "0.1.0", LookKind::NativeFamily, WbPolicy::Adaptive)
        .expect("valid identity");
    let mut file = LookFile::from_look(identity, &look).expect("encodable look");
    file.provenance = Provenance {
        bodies: vec![BodyProvenance {
            body: "example/x-body".to_string(),
            epoch_map_version: 1,
            epochs: vec![1, 2],
            input_transform_version: 3,
            realization_params_version: 2,
            control_response_version: 1,
            clip_k: 0.35,
            images: 42,
            credentials: 3,
        }],
        root: RootProvenance {
            admissions: vec!["example-admission".to_string()],
            anchor_version: "2026.1".to_string(),
        },
        base: None,
        needs_rebase: false,
        community_snapshot_version: 7,
        prep_versions: vec![1],
        created: "2026-01-01T00:00:00Z".to_string(),
        software: "looklab-report example".to_string(),
    };
    LookDocument::new(file, look).expect("document validates")
}

fn main() {
    let probes = synthetic_probes();
    let a = SyntheticLook { look: base_look() };
    let b = SyntheticLook {
        look: lifted_look(),
    };
    let a_name = FunctionName::new("A");
    let b_name = FunctionName::new("B");
    let comparison: Comparison = compare(&ComparisonInput {
        a: &a,
        b: &b,
        probes: &probes,
        a_name: &a_name,
        b_name: &b_name,
    });

    let doc_a = document(base_look(), "example-a");
    let doc_b = document(lifted_look(), "example-b");
    let hash = ArtifactHash::from_sha256_hex(&"7a".repeat(32)).expect("valid hash");
    let source_a = ReportSource {
        label: "A (colorimetric spline)".to_string(),
        artifact_hash: Some(&hash),
        manifest_matches: Some(true),
        ..ReportSource::from_document(&doc_a)
    };
    let source_b = ReportSource {
        label: "B (lifted highlights, redder chroma, warmer neutrals)".to_string(),
        artifact_hash: Some(&hash),
        manifest_matches: Some(true),
        ..ReportSource::from_document(&doc_b)
    };

    let html = comparison_page(&comparison, &source_a, &source_b);
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("target/report-example.html");
    std::fs::write(&path, html).expect("write report");
    println!("{}", path.display());
    println!("{}", comparison.summary);
}
