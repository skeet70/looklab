// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application shell and routes (docs/08 §1).

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::path;

use crate::pages::{
    BodiesPage, BodyDetail, ComparePage, Landing, LogPage, LookDetail, LooksPage, MatchPlaceholder,
    ModQueuePage, NotFound, PrivacyPage, ProposalDetail, ProposePage,
};

#[component]
pub fn App() -> impl IntoView {
    view! {
        <Router>
            <a class="skip-link" href="#main">"Skip to content"</a>
            <header class="site-header">
                <A href="/" attr:class="brand">"looklab"</A>
                <nav aria-label="Main">
                    <A href="/bodies">"Bodies"</A>
                    <A href="/looks">"Looks"</A>
                    <A href="/compare">"Compare"</A>
                    <A href="/log">"Audit log"</A>
                    <A href="/propose">"Propose"</A>
                    <A href="/mod">"Moderation"</A>
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
                    <Route path=path!("/match") view=MatchPlaceholder/>
                    <Route path=path!("/log") view=LogPage/>
                    <Route path=path!("/propose") view=ProposePage/>
                    <Route path=path!("/proposals/:id") view=ProposalDetail/>
                    <Route path=path!("/mod") view=ModQueuePage/>
                    <Route path=path!("/about/privacy") view=PrivacyPage/>
                </Routes>
            </main>
            <footer class="site-footer">
                <p>"Static site: every artifact is checked against its version manifest before use."</p>
                <p>
                    <a href="https://github.com/skeet70/looklab">"Source"</a>
                    " · AGPL-3.0-or-later"
                </p>
            </footer>
        </Router>
    }
}
