// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use leptos::prelude::*;
use looklab_publish::SiteIndex;
use looklab_schema::{EvidenceRequestOrigin, EvidenceRequestWire, ModelSubject};
use looklab_trust::Bounty;

use crate::data::{self, Load};
use crate::ui;

struct ExpectedVarianceReduction(f64);

impl ExpectedVarianceReduction {
    fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then_some(Self(value))
    }
}

#[derive(Clone, Copy)]
pub(super) enum Impact {
    Low,
    Medium,
    High,
}

impl Impact {
    // Expected variance reduction below 0.01 is low, below 0.1 is medium, and 0.1 or above is high.
    fn from_reduction(reduction: ExpectedVarianceReduction) -> Self {
        if reduction.0 < 0.01 {
            Self::Low
        } else if reduction.0 < 0.1 {
            Self::Medium
        } else {
            Self::High
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Low => "Low impact",
            Self::Medium => "Medium impact",
            Self::High => "High impact",
        }
    }
}

#[derive(Clone)]
struct WantedShots {
    bounties: Vec<Bounty>,
    requests: Vec<EvidenceRequestWire>,
    index: Arc<SiteIndex>,
}

#[component]
pub fn BountiesPage() -> impl IntoView {
    view! {
        <header class="wanted-intro">
            <p class="eyebrow">"A few photos, a useful difference"</p>
            <h1>"Wanted shots"</h1>
            <p>"Help fill gaps in a look by sharing a few RAW+JPEG pairs. Find a request for your camera, then look for those colors around you."</p>
            <a href=crate::base::site_url("/contribute")>"Share your shots →"</a>
        </header>
        <WantedCards/>
    }
}

#[component]
pub(super) fn WantedCards(
    #[prop(default = usize::MAX)] limit: usize,
    #[prop(default = true)] show_actions: bool,
) -> impl IntoView {
    let state = RwSignal::new(Load::<WantedShots>::Loading);
    let available = crate::base::server_available();
    Effect::new(move |_| {
        if !available {
            return;
        }
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = async {
                let wanted = data::wanted().await?;
                let mut bounties = wanted.bounties;
                bounties.sort_by(|left, right| {
                    right
                        .expected_variance_reduction
                        .total_cmp(&left.expected_variance_reduction)
                });
                let index = Arc::new(data::index().await?);
                Ok::<_, String>(WantedShots {
                    bounties,
                    requests: wanted.evidence_requests,
                    index,
                })
            }
            .await;
            state.set(match result {
                Ok(wanted) => Load::Ready(wanted),
                Err(error) => Load::Failed(error),
            });
        });
    });
    move || {
        if !available {
            return ui::preview_notice().into_any();
        }
        match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(wanted)
                if wanted.bounties.is_empty() && wanted.requests.is_empty() =>
            {
                view! {
                    <ui::EmptyState
                        title="No wanted shots right now".to_owned()
                        message="You can still help by sharing photos you already have.".to_owned()
                    >
                        {show_actions.then(|| view! {
                            <a href=crate::base::site_url("/contribute")>"Share photos"</a>
                        })}
                    </ui::EmptyState>
                }
                .into_any()
            }
            Load::Ready(wanted) => view! {
                <div class="wanted-grid">
                    {wanted.bounties.into_iter().take(limit).map(|bounty| view! {
                        <WantedCard bounty index=wanted.index.clone() show_action=show_actions/>
                    }).collect_view()}
                    {wanted.requests.into_iter().take(limit).map(|request| view! {
                        <EvidenceRequestCard request index=wanted.index.clone() show_action=show_actions/>
                    }).collect_view()}
                </div>
            }
            .into_any(),
        }
    }
}

#[component]
fn EvidenceRequestCard(
    request: EvidenceRequestWire,
    index: Arc<SiteIndex>,
    show_action: bool,
) -> impl IntoView {
    let camera = request
        .body()
        .map(|id| {
            index
                .bodies
                .iter()
                .find(|camera| &camera.id == id)
                .map(|camera| format!("{} {}", camera.make, camera.model))
                .unwrap_or_else(|| "the requested camera".to_owned())
        })
        .unwrap_or_else(|| "any supported camera".to_owned());
    let look = match &request.subject {
        ModelSubject::SharedFamily { family } => index
            .looks
            .iter()
            .find(|look| look.slug.as_str() == family.as_str())
            .map(|look| look.name.clone())
            .unwrap_or_else(|| "the requested look".to_owned()),
        ModelSubject::BodyRealization { .. } => "its camera model".to_owned(),
    };
    let capability = match request.capability {
        looklab_schema::QualificationCapability::NativePrediction => "camera response",
        looklab_schema::QualificationCapability::ControlsMatching => "settings and controls",
        looklab_schema::QualificationCapability::CrossBodyAdmission => "cross-camera agreement",
    };
    let origin = match &request.origin {
        EvidenceRequestOrigin::Report { .. } => "From a published assessment",
        EvidenceRequestOrigin::PreCandidate => "Before a model is published",
    };
    let context = match (request.epoch(), request.native_sim()) {
        (Some(epoch), Some(sim)) => format!("Firmware epoch {} · {}", epoch.get(), sim.as_str()),
        _ => String::new(),
    };
    view! {
        <article class="wanted-card">
            <p class="help-meta">"5 to 10 minutes · " {origin}</p>
            <h2>{format!("Help measure {capability} for {look}")}</h2>
            <p>{format!("{camera}. {context}")}</p>
            <p>{request.detail.clone()}</p>
            {request.observed_groups.map(|groups| view! {
                <p class="muted">{format!("{groups} independent groups observed so far.")}</p>
            })}
            {show_action.then(|| view! {
                <a href=crate::base::site_url("/contribute")>"Add your photos →"</a>
            })}
            <ui::Disclosure summary="Technical details".to_owned()>
                <p>{format!("Request kind: {}", request.kind.as_str())}</p>
                {request.report().map(|report| view! {
                    <p>{format!("Assessment report {report}.")}</p>
                })}
                <p>{format!("Subject: {}", request.subject_key().as_str())}</p>
            </ui::Disclosure>
        </article>
    }
}

#[component]
fn WantedCard(bounty: Bounty, index: Arc<SiteIndex>, show_action: bool) -> impl IntoView {
    let camera = bounty
        .body
        .as_ref()
        .map(|id| {
            index
                .bodies
                .iter()
                .find(|camera| camera.id.as_str() == id.as_str())
                .map(|camera| format!("{} {}", camera.make, camera.model))
                .unwrap_or_else(|| "the requested camera".to_owned())
        })
        .unwrap_or_else(|| "any supported camera".to_owned());
    let look = index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == bounty.look.as_str())
        .map(|look| look.name.clone())
        .unwrap_or_else(|| "the requested look".to_owned());
    let impact = ExpectedVarianceReduction::new(bounty.expected_variance_reduction)
        .map(Impact::from_reduction)
        .map(Impact::label)
        .unwrap_or("Impact not estimated");
    let colors = [
        "red",
        "orange",
        "yellow",
        "yellow-green",
        "green",
        "turquoise",
        "cyan",
        "sky blue",
        "blue",
        "violet",
        "magenta",
        "pink",
    ][usize::from(bounty.region.hue())];
    let brightness = match bounty.region.exposure() {
        0 | 1 => "deep shadows",
        2 => "shadows",
        3 => "midtones",
        _ => "bright light",
    };
    view! {
        <article class="wanted-card">
            <p class="help-meta">"5 to 10 minutes · " {impact}</p>
            <h2>{format!("Photograph {colors} in {brightness}")}</h2>
            <p>{format!("Helps {look} on {camera}.")}</p>
            <p>"Capture tip: shoot RAW+JPEG at your lowest native ISO. Keep the subject still and turn off lens corrections where possible."</p>
            {show_action.then(|| view! {
                <a href=crate::base::site_url("/contribute")>"Add your photos →"</a>
            })}
            <ui::Disclosure summary="Technical details".to_owned()>
                <p>{format!(
                    "Look version {}. Region {}. Expected variance reduction {:.3}.",
                    bounty.look_version.get(),
                    bounty.region,
                    bounty.expected_variance_reduction,
                )}</p>
            </ui::Disclosure>
        </article>
    }
}
