// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application shell and routes (docs/08 §1).

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::hooks::use_location;
use leptos_router::path;

use crate::pages::{
    BodiesPage, BodyDetail, ComparePage, ContributePage, Landing, LogPage, LookDetail, LooksPage,
    MatchPage, MatchResultPage, ModQueuePage, NotFound, PasskeysPage, PatternPage, PrivacyPage,
    ProposalDetail, ProposePage, StatusPage, VerifyPage,
};

#[component]
pub fn App() -> impl IntoView {
    view! {
        <Router>
            <Shell/>
        </Router>
    }
}

#[component]
fn Shell() -> impl IntoView {
    let location = use_location();
    let chrome = move || !location.pathname.get().starts_with("/pattern/");
    view! {
        <a class="skip-link" href="#main" hidden=move || !chrome()>"Skip to content"</a>
        <header class="site-header" hidden=move || !chrome()>
            <A href="/" attr:class="brand">"looklab"</A>
            <nav aria-label="Main">
                <A href="/bodies">"Bodies"</A>
                <A href="/looks">"Looks"</A>
                <A href="/compare">"Compare"</A>
                <A href="/match">"Match"</A>
                <A href="/contribute">"Contribute"</A>
                <A href="/passkeys">"Passkeys"</A>
                <A href="/propose">"Propose"</A>
                <A href="/mod">"Moderation"</A>
                <A href="/log">"Audit log"</A>
                <A href="/about/privacy">"Privacy"</A>
            </nav>
        </header>
        <main id="main">
            <Routes fallback=|| view! { <NotFound/> }>
                <Route path=path!("/") view=Landing/>
                <Route path=path!("/bodies") view=BodiesPage/>
                <Route path=path!("/bodies/:make/:model") view=BodyDetail/>
                <Route path=path!("/looks") view=LooksPage/>
                <Route path=path!("/looks/:look") view=LookDetail/>
                <Route path=path!("/compare") view=ComparePage/>
                <Route path=path!("/match") view=MatchPage/>
                <Route path=path!("/match/r/:permalink") view=MatchResultPage/>
                <Route path=path!("/verify") view=VerifyPage/>
                <Route path=path!("/contribute") view=ContributePage/>
                <Route path=path!("/status/:token") view=StatusPage/>
                <Route path=path!("/passkeys") view=PasskeysPage/>
                <Route path=path!("/pattern/:seed") view=PatternPage/>
                <Route path=path!("/log") view=LogPage/>
                <Route path=path!("/propose") view=ProposePage/>
                <Route path=path!("/proposals/:id") view=ProposalDetail/>
                <Route path=path!("/mod") view=ModQueuePage/>
                <Route path=path!("/about/privacy") view=PrivacyPage/>
            </Routes>
        </main>
        <footer class="site-footer" hidden=move || !chrome()>
            <p>"Static site: every artifact is checked against its version manifest before use."</p>
            <p>
                <a href="https://github.com/skeet70/looklab">"Source"</a>
                " · AGPL-3.0-or-later"
            </p>
        </footer>
    }
}
