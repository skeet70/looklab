// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;

#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <h1>"Not found"</h1>
        <p>"There is no page at this address."</p>
        <p><a href="/">"Back to the start"</a></p>
    }
}
