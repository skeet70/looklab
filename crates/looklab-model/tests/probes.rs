// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;

use looklab_model::ProbeGroup;
use looklab_model::probes::{PROBES_V1_YAML, v1, validate_yaml};

#[test]
fn committed_probe_set_matches_generation() {
    let file = validate_yaml(PROBES_V1_YAML).expect("committed yaml");
    let generated = v1();
    assert_eq!(file.version, generated.version);
    assert_eq!(file.probes, generated.probes);
}

#[test]
fn groups_have_the_documented_counts() {
    let probes = v1().probes;
    let count = |group: ProbeGroup| probes.iter().filter(|probe| probe.group == group).count();
    assert_eq!(count(ProbeGroup::GrayRamp), 61);
    assert_eq!(count(ProbeGroup::ChartPatches), 120);
    assert_eq!(count(ProbeGroup::HueGrid), 864);
    assert_eq!(count(ProbeGroup::Skin), 6);
    assert_eq!(count(ProbeGroup::MemoryColors), 6);
    assert_eq!(probes.len(), 1057);
}

#[test]
fn ids_are_unique_and_values_finite() {
    let probes = v1().probes;
    let mut ids = HashSet::new();
    for probe in &probes {
        assert!(
            ids.insert(probe.id.clone()),
            "duplicate id {}",
            probe.id.as_str()
        );
        assert!(probe.weight > 0.0);
        assert!(probe.xyz.x.is_finite() && probe.xyz.y.is_finite() && probe.xyz.z.is_finite());
        assert!(probe.xyz.y > 0.0);
    }
}

#[test]
fn hue_grid_uses_cam16_chroma_fractions() {
    use looklab_color::{Xyy, xyy::xyz_to_xy};
    use looklab_model::probes::pointer_boundary_xy;
    use looklab_model::solve::ucs_of_xyz;

    let probes = v1().probes;
    let mut checked = 0;
    for probe in &probes {
        if probe.group != ProbeGroup::HueGrid || !probe.id.as_str().ends_with("s+00") {
            continue;
        }
        let parts: Vec<&str> = probe.id.as_str().split('-').collect();
        let parameter_hue: f64 = parts[1].parse().expect("hue");
        let percent: f64 = parts[2].parse::<f64>().expect("percent") / 100.0;
        let boundary_xy = pointer_boundary_xy(parameter_hue);
        let boundary = ucs_of_xyz(looklab_color::xyy::xyy_to_xyz(Xyy::new(
            boundary_xy.0,
            boundary_xy.1,
            0.18,
        )));
        let boundary_m = (boundary.a * boundary.a + boundary.b * boundary.b).sqrt();
        let boundary_h = libm::atan2(boundary.b, boundary.a);

        let actual = ucs_of_xyz(probe.xyz);
        let actual_m = (actual.a * actual.a + actual.b * actual.b).sqrt();
        let actual_h = libm::atan2(actual.b, actual.a);
        assert!(
            (actual_m - percent * boundary_m).abs() < 1e-6,
            "probe {} chroma {} target {}",
            probe.id.as_str(),
            actual_m,
            percent * boundary_m
        );
        let mut hue_error = actual_h - boundary_h;
        let pi = core::f64::consts::PI;
        while hue_error > pi {
            hue_error -= 2.0 * pi;
        }
        while hue_error < -pi {
            hue_error += 2.0 * pi;
        }
        assert!(
            hue_error.abs() < 1e-6,
            "probe {} hue {} target {boundary_h}",
            probe.id.as_str(),
            actual_h
        );
        checked += 1;
    }
    assert_eq!(checked, 96);

    let mut groups: std::collections::BTreeMap<String, Vec<[f64; 2]>> = Default::default();
    for probe in probes
        .iter()
        .filter(|probe| probe.group == ProbeGroup::HueGrid)
    {
        let prefix = probe.id.as_str().rsplit_once("-s").expect("id").0;
        let (x, y) = xyz_to_xy(probe.xyz);
        groups.entry(prefix.to_string()).or_default().push([x, y]);
    }
    for (prefix, chromaticities) in groups {
        assert_eq!(chromaticities.len(), 9, "{prefix}");
        for chromaticity in &chromaticities[1..] {
            assert!((chromaticity[0] - chromaticities[0][0]).abs() < 1e-12);
            assert!((chromaticity[1] - chromaticities[0][1]).abs() < 1e-12);
        }
    }
}

#[test]
fn weights_match_the_groups() {
    for probe in v1().probes {
        let expected = match probe.group {
            ProbeGroup::GrayRamp | ProbeGroup::ChartPatches => 1.0,
            ProbeGroup::HueGrid => 0.5,
            ProbeGroup::Skin => 2.0,
            ProbeGroup::MemoryColors => 1.5,
        };
        assert_eq!(probe.weight, expected);
    }
}
