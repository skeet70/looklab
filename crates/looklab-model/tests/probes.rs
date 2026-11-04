// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::HashSet;

use looklab_model::ProbeGroup;
use looklab_model::probes::{PROBES_V1_YAML, v1, validate_yaml};

#[test]
fn committed_yaml_matches_generation() {
    assert_eq!(PROBES_V1_YAML, v1().to_yaml().expect("yaml"));
}

#[test]
fn committed_yaml_parses_back() {
    let file = validate_yaml(PROBES_V1_YAML).expect("committed yaml");
    let generated = v1();
    assert_eq!(file.probes, generated.probes);
    assert_eq!(file.notes, generated.notes);
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
