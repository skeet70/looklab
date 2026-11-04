// SPDX-License-Identifier: AGPL-3.0-or-later

#[allow(
    clippy::disallowed_methods,
    reason = "dev-only generator; not part of the wasm core path"
)]
fn main() {
    let yaml = looklab_model::probes::v1_yaml().expect("probe generation");
    std::fs::write("data/probes/v1.yaml", yaml).expect("write data/probes/v1.yaml");
}
