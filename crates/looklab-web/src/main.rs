// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser entry point.

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(looklab_web::App);
}
