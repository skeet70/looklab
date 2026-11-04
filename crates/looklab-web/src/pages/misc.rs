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

#[component]
pub fn MatchPlaceholder() -> impl IntoView {
    view! {
        <h1>"Match"</h1>
        <p>
            "The browser match flow (body → settings card → artifact) is a later phase. "
            "It will run the same optimizer as the CLI and pin every model version in the permalink."
        </p>
        <p>
            "For now, browse "
            <a href="/bodies">"bodies"</a>
            " and "
            <a href="/looks">"looks"</a>
            "."
        </p>
    }
}
