// SPDX-License-Identifier: AGPL-3.0-or-later
//! Route views that load in their own wasm chunk.

use std::collections::BTreeMap;

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos_router::LazyRoute;

use crate::pages::*;

#[derive(Clone, Copy, Debug)]
pub(crate) struct BodiesRoute;

#[leptos_router::lazy_route]
impl LazyRoute for BodiesRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <BodiesPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BodyRoute;

#[leptos_router::lazy_route]
impl LazyRoute for BodyRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <BodyDetail/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ContributeRoute;

#[leptos_router::lazy_route]
impl LazyRoute for ContributeRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <ContributePage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct BountiesRoute;

#[leptos_router::lazy_route]
impl LazyRoute for BountiesRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <BountiesPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct MeRoute;

#[leptos_router::lazy_route]
impl LazyRoute for MeRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <crate::app::MePage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StatusRoute;

#[leptos_router::lazy_route]
impl LazyRoute for StatusRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <StatusPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PasskeysRoute;

#[leptos_router::lazy_route]
impl LazyRoute for PasskeysRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <PasskeysPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PatternRoute;

#[leptos_router::lazy_route]
impl LazyRoute for PatternRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <PatternPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CommunityRoute;

#[leptos_router::lazy_route]
impl LazyRoute for CommunityRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <CommunityPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LogRoute;

#[leptos_router::lazy_route]
impl LazyRoute for LogRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <LogPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProposeRoute;

#[leptos_router::lazy_route]
impl LazyRoute for ProposeRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <ProposePage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProposalRoute;

#[leptos_router::lazy_route]
impl LazyRoute for ProposalRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <ProposalDetail/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ModQueueRoute;

#[leptos_router::lazy_route]
impl LazyRoute for ModQueueRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <ModQueuePage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DevelopersRoute;

#[leptos_router::lazy_route]
impl LazyRoute for DevelopersRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <DevelopersPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PipelinesRoute;

#[leptos_router::lazy_route]
impl LazyRoute for PipelinesRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <PipelinesPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HowItWorksRoute;

#[leptos_router::lazy_route]
impl LazyRoute for HowItWorksRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <HowItWorksPage/> }.into_any()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PrivacyRoute;

#[leptos_router::lazy_route]
impl LazyRoute for PrivacyRoute {
    fn data() -> Self {
        Self
    }

    fn view(this: Self) -> AnyView {
        let _ = this;
        crate::prefetch::route_rendered();
        view! { <PrivacyPage/> }.into_any()
    }
}

/// `LazyRoute::preload` aborts the app when a chunk request fails.
async fn split_available() -> bool {
    let Ok(response) = Request::get(&crate::base::site_url("pkg/__wasm_split_manifest.json"))
        .send()
        .await
    else {
        return false;
    };
    if !response.ok() {
        return false;
    }
    let Ok(text) = response.text().await else {
        return false;
    };
    let Ok(manifest) = serde_json::from_str::<BTreeMap<String, Vec<String>>>(&text) else {
        return false;
    };
    !manifest.is_empty() && manifest.values().all(|chunks| !chunks.is_empty())
}

pub(crate) async fn preload_all() {
    if !split_available().await {
        return;
    }
    BodiesRoute::preload().await;
    BodyRoute::preload().await;
    ContributeRoute::preload().await;
    BountiesRoute::preload().await;
    MeRoute::preload().await;
    StatusRoute::preload().await;
    PasskeysRoute::preload().await;
    PatternRoute::preload().await;
    CommunityRoute::preload().await;
    LogRoute::preload().await;
    ProposeRoute::preload().await;
    ProposalRoute::preload().await;
    ModQueueRoute::preload().await;
    DevelopersRoute::preload().await;
    PipelinesRoute::preload().await;
    HowItWorksRoute::preload().await;
    PrivacyRoute::preload().await;
}
