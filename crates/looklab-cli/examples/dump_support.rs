// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let source = PathBuf::from(args.next().expect("look path"));
    let target = args.next().map(PathBuf::from);
    let bytes = std::fs::read(&source).expect("read look");
    let mut document = looklab_model::LookDocument::decode(&bytes).expect("decode look");
    let uncertainty = &document.uncertainty;
    let support = uncertainty.support.as_ref().expect("support array");
    let supported = support.values.iter().filter(|value| **value > 0.5).count();
    let reliable = uncertainty
        .probe_pre_reliable
        .as_ref()
        .map(|values| values.values.iter().filter(|value| **value > 0.5).count());
    let density = uncertainty.density.as_ref().expect("density array");
    let above_floor = density.values.iter().filter(|value| **value >= 0.01).count();
    println!(
        "probes {} supported {} density>=0.01 {} reliable {:?}",
        support.values.len(),
        supported,
        above_floor,
        reliable
    );
    if let Some(target) = target {
        let count = support.values.len();
        document.uncertainty.support = Some(
            looklab_model::ProbeScalars::new(vec![1.0; count]).expect("all-supported mask"),
        );
        let encoded = document.encode().expect("encode look");
        std::fs::write(target, encoded).expect("write look");
    }
}
