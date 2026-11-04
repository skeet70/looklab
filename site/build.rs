// SPDX-License-Identifier: AGPL-3.0-or-later

fn main() {
    println!("cargo:rerun-if-changed=site-root.txt");
    let root = std::fs::read_to_string("site-root.txt").unwrap_or_default();
    println!("cargo:rustc-env=LOOKLAB_SITE_ROOT={}", root.trim());
}
