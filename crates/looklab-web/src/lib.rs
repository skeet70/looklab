// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser frontend: a static-data reader for bodies, looks, comparisons and the audit log.

pub mod app;
pub mod data;
pub mod pages;
pub mod preview;
pub mod ui;

pub use app::App;

pub fn mount() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
