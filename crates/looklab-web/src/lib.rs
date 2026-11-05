// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser frontend: a static-data reader for bodies, looks, comparisons and the audit log.

pub mod app;
pub mod base;
pub mod data;
pub mod diagnostics;
pub mod files;
pub mod idb;
pub mod match_data;
pub mod pages;
pub mod proposal_form;
pub mod request_digest;
pub mod session;
pub mod ui;
pub mod webauthn;
pub mod worker;

use std::sync::OnceLock;

pub use app::App;

static SITE_ROOT: OnceLock<&'static str> = OnceLock::new();

pub fn mount_with_root(root: &'static str) {
    let _ = SITE_ROOT.set(root);
    mount();
}

pub(crate) fn site_root() -> &'static str {
    SITE_ROOT.get().copied().unwrap_or("")
}

pub fn mount() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
