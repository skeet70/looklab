// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use leptos::prelude::*;
use looklab_model::file::Slug;
use looklab_publish::{BodyKind, BodyPage, LookVisibility, SiteIndex};
use looklab_schema::{BodyId, EvidenceRequestOrigin, EvidenceRequestWire, ModelSubject};
use looklab_trust::{Bounty, BountyStatus};
use wasm_bindgen::JsCast;

use crate::data::{self, Load, WantedFile};
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
    bodies: Arc<Vec<looklab_publish::BodyPage>>,
}

/// One camera's published looks and the evidence still wanted for it.
#[derive(Clone, Debug, PartialEq)]
struct CameraStanding {
    id: BodyId,
    make: String,
    model: String,
    published: Vec<(Slug, String)>,
    requests: Vec<EvidenceRequestWire>,
    bounties: Vec<Bounty>,
}

fn published_looks(index: &SiteIndex, looks: &[Slug]) -> Vec<(Slug, String)> {
    index
        .looks
        .iter()
        .filter(|look| look.visibility == LookVisibility::Public)
        .filter(|look| look.latest().is_some())
        .filter(|look| looks.contains(&look.slug))
        .map(|look| (look.slug.clone(), look.name.clone()))
        .collect()
}

/// Each camera in the site index with its published looks and open requests.
///
/// A shared-family request or a bounty without a body applies to every camera
/// whose built-in looks include that Look, because any of them can supply the
/// missing photos.
fn standings(index: &SiteIndex, wanted: Option<&WantedFile>) -> Vec<CameraStanding> {
    let mut cameras = index
        .bodies
        .iter()
        .filter(|body| body.kind == BodyKind::Camera)
        .map(|body| CameraStanding {
            id: body.id.clone(),
            make: body.make.clone(),
            model: body.model.clone(),
            published: published_looks(index, &body.looks),
            requests: Vec::new(),
            bounties: Vec::new(),
        })
        .collect::<Vec<_>>();
    cameras.sort_by(|left, right| {
        left.make
            .cmp(&right.make)
            .then_with(|| left.model.cmp(&right.model))
    });
    let Some(wanted) = wanted else {
        return cameras;
    };
    for camera in &mut cameras {
        let Some(body) = index.bodies.iter().find(|body| body.id == camera.id) else {
            continue;
        };
        for request in &wanted.evidence_requests {
            let applies = match &request.subject {
                ModelSubject::BodyRealization { realization } => realization.body == camera.id,
                ModelSubject::SharedFamily { family } => body
                    .looks
                    .iter()
                    .any(|look| look.as_str() == family.as_str()),
            };
            if applies {
                camera.requests.push(request.clone());
            }
        }
        for bounty in &wanted.bounties {
            if bounty.status != BountyStatus::Open {
                continue;
            }
            let applies = match &bounty.body {
                Some(body) => body == &camera.id,
                None => body
                    .looks
                    .iter()
                    .any(|look| look.as_str() == bounty.look.as_str()),
            };
            if applies {
                camera.bounties.push(bounty.clone());
            }
        }
    }
    cameras
}

/// The Look a request asks about, by its display name.
///
/// A camera-specific request resolves through its body page; the raw setting
/// key is only a fallback when no published look names it.
fn request_label(index: &SiteIndex, bodies: &[BodyPage], request: &EvidenceRequestWire) -> String {
    if let Some(slug) = data::evidence_look(index, bodies, request) {
        return index
            .looks
            .iter()
            .find(|candidate| &candidate.slug == slug)
            .map(|look| look.name.clone())
            .unwrap_or_else(|| data::plain_id(slug.as_str()));
    }
    match &request.subject {
        ModelSubject::BodyRealization { realization } => data::sim_name(&realization.native_sim),
        ModelSubject::SharedFamily { family } => look_name(index, family.as_str()),
    }
}

/// The groups seen so far, when the request publishes that count.
fn observed_groups_line(groups: u32) -> String {
    if groups == 1 {
        "1 independent group observed so far.".to_owned()
    } else {
        format!("{groups} independent groups observed so far.")
    }
}

fn capability_words(capability: looklab_schema::QualificationCapability) -> &'static str {
    match capability {
        looklab_schema::QualificationCapability::NativePrediction => "camera response",
        looklab_schema::QualificationCapability::ControlsMatching => "settings and controls",
        looklab_schema::QualificationCapability::CrossBodyAdmission => "cross-camera agreement",
    }
}

fn look_name(index: &SiteIndex, look: &str) -> String {
    index
        .looks
        .iter()
        .find(|candidate| candidate.slug.as_str() == look)
        .map(|candidate| candidate.name.clone())
        .unwrap_or_else(|| data::plain_id(look))
}

/// The color and brightness a bounty asks for, in plain words.
fn bounty_region(bounty: &Bounty) -> String {
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
    format!("Photograph {colors} in {brightness}")
}

#[derive(Clone)]
enum WantedLoad {
    Loading,
    /// The preview build has no API to ask.
    Preview,
    /// The API is configured but the request failed.
    Failed,
    Loaded(WantedFile),
}

#[derive(Clone)]
struct Standings {
    index: SiteIndex,
    wanted: WantedLoad,
    bodies: Vec<looklab_publish::BodyPage>,
}

/// Each camera's published looks and open requests, above the wanted-shot cards.
#[component]
fn WhereThingsStand(wanted: RwSignal<WantedLoad>) -> impl IntoView {
    let state = RwSignal::new(Load::<Standings>::Loading);
    Effect::new(move |_| {
        let wanted = wanted.get();
        let request_bodies = match &wanted {
            WantedLoad::Loaded(wanted) => wanted
                .evidence_requests
                .iter()
                .filter_map(|request| request.body().cloned())
                .collect::<Vec<_>>(),
            WantedLoad::Loading | WantedLoad::Preview | WantedLoad::Failed => Vec::new(),
        };
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let index = data::index().await?;
                let mut bodies = Vec::new();
                for id in request_bodies {
                    if bodies
                        .iter()
                        .any(|body: &looklab_publish::BodyPage| body.id == id)
                    {
                        continue;
                    }
                    let Some((make, model)) = id.as_str().split_once('/') else {
                        continue;
                    };
                    if let Ok(Some(page)) = data::body_optional(make, model).await {
                        bodies.push(page);
                    }
                }
                Ok::<_, String>(Standings {
                    index,
                    wanted,
                    bodies,
                })
            }
            .await;
            state.set(match result {
                Ok(standings) => Load::Ready(standings),
                Err(error) => Load::Failed(error),
            });
            ui::paint_browser().await;
            // The section mounts after its data arrives, so the router cannot
            // scroll to the fragment on its own.
            if web_sys::window()
                .and_then(|window| window.location().hash().ok())
                .as_deref()
                == Some("#where-things-stand")
                && let Some(section) = document()
                    .get_element_by_id("where-things-stand")
                    .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
            {
                section.scroll_into_view();
            }
        });
    });
    view! {
        <section
            class="where-things-stand"
            id="where-things-stand"
            aria-labelledby="where-things-stand-title"
        >
            <h2 id="where-things-stand-title">"Where things stand"</h2>
            <p>
                "Each camera on the site, the looks published for it, and the photos still being asked for. Counts show what people have shared, not how close a look is."
            </p>
            {move || match state.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(Standings { index, wanted, bodies }) => {
                    let loaded = match &wanted {
                        WantedLoad::Loaded(wanted) => Some(wanted),
                        WantedLoad::Loading | WantedLoad::Preview | WantedLoad::Failed => None,
                    };
                    let cameras = standings(&index, loaded);
                    let any_open = cameras
                        .iter()
                        .any(|camera| !camera.requests.is_empty() || !camera.bounties.is_empty());
                    let cards = cameras.into_iter().map(|camera| {
                        let name = format!("{} {}", camera.make, camera.model);
                        let published = if camera.published.is_empty() {
                            view! { <p class="muted">"No built-in looks published yet."</p> }.into_any()
                        } else {
                            let count = camera.published.len();
                            let links = camera.published.iter().enumerate().map(|(position, (slug, look))| view! {
                                {(position > 0).then_some(view! { ", " })}
                                <a href=crate::base::site_url(&format!("/looks/{slug}"))>{look.clone()}</a>
                            }).collect_view();
                            view! {
                                <p class="muted">
                                    {format!("{count} built-in {} published: ", if count == 1 { "look" } else { "looks" })}
                                    {links}
                                </p>
                            }.into_any()
                        };
                        let requests = camera.requests.iter().map(|request| {
                            let label = request_label(&index, &bodies, request);
                            let observed = request.observed_groups.map(|groups| view! {
                                <span class="muted">" " {observed_groups_line(groups)}</span>
                            });
                            view! {
                                <li>
                                    <strong>{label}</strong>
                                    ": "
                                    {request.detail.clone()}
                                    {observed}
                                </li>
                            }
                        }).collect_view();
                        let bounties = camera.bounties.iter().map(|bounty| view! {
                            <li>
                                {format!("{} for {}.", bounty_region(bounty), look_name(&index, bounty.look.as_str()))}
                            </li>
                        }).collect_view();
                        view! {
                            <article class="stand-card">
                                <h3>
                                    <a href=crate::base::site_url(&format!("/cameras/{}", camera.id))>{name}</a>
                                </h3>
                                {published}
                                {(!camera.requests.is_empty() || !camera.bounties.is_empty()).then(|| view! {
                                    <ul class="stand-requests">{requests}{bounties}</ul>
                                })}
                            </article>
                        }
                    }).collect_view();
                    let empty = match &wanted {
                        WantedLoad::Loading => None,
                        WantedLoad::Preview => {
                            Some("Open requests are not available in this demo.")
                        }
                        WantedLoad::Failed => {
                            Some("Open requests could not be loaded right now.")
                        }
                        WantedLoad::Loaded(_) if !any_open => Some(
                            "No open requests right now. You can still share photos you already have.",
                        ),
                        WantedLoad::Loaded(_) => None,
                    };
                    view! {
                        {empty.map(|line| view! { <p class="muted" role="status">{line}</p> })}
                        <div class="stand-grid">{cards}</div>
                    }.into_any()
                }
            }}
        </section>
    }
}

#[component]
pub fn BountiesPage() -> impl IntoView {
    let wanted = RwSignal::new(WantedLoad::Loading);
    Effect::new(move |_| {
        if !crate::base::server_available() {
            wanted.set(WantedLoad::Preview);
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            wanted.set(match data::wanted().await {
                Ok(wanted) => WantedLoad::Loaded(wanted),
                Err(_) => WantedLoad::Failed,
            });
        });
    });
    view! {
        <header class="wanted-intro">
            <p class="eyebrow">"A few photos, a useful difference"</p>
            <h1>"Wanted shots"</h1>
            <p>"Help fill gaps in a look by sharing a few RAW+JPEG pairs. Find a request for your camera, then look for those colors around you."</p>
            <a href=crate::base::site_url("/contribute")>"Share your shots →"</a>
        </header>
        <WhereThingsStand wanted/>
        {move || match wanted.get() {
            WantedLoad::Loading => ui::loading().into_any(),
            WantedLoad::Preview => ui::preview_notice().into_any(),
            WantedLoad::Failed => ui::failure("Wanted shots could not be loaded right now.").into_any(),
            WantedLoad::Loaded(loaded) => view! { <WantedCards preloaded=loaded/> }.into_any(),
        }}
    }
}

#[component]
pub(super) fn WantedCards(
    #[prop(default = usize::MAX)] limit: usize,
    #[prop(default = true)] show_actions: bool,
    #[prop(optional)] preloaded: Option<WantedFile>,
) -> impl IntoView {
    let state = RwSignal::new(Load::<WantedShots>::Loading);
    let available = crate::base::server_available();
    Effect::new(move |_| {
        if !available {
            return;
        }
        let preloaded = preloaded.clone();
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = async {
                let wanted = match preloaded {
                    Some(wanted) => wanted,
                    None => data::wanted().await?,
                };
                let mut bounties = wanted.bounties;
                bounties.sort_by(|left, right| {
                    right
                        .expected_variance_reduction
                        .total_cmp(&left.expected_variance_reduction)
                });
                let index = Arc::new(data::index().await?);
                let mut bodies: Vec<looklab_publish::BodyPage> = Vec::new();
                let mut missing: Vec<looklab_schema::BodyId> = Vec::new();
                for request in wanted.evidence_requests.iter().take(limit) {
                    let ModelSubject::BodyRealization { realization } = &request.subject else {
                        continue;
                    };
                    if bodies.iter().any(|body| body.id == realization.body)
                        || missing.contains(&realization.body)
                    {
                        continue;
                    }
                    let Some((make, model)) = realization.body.as_str().split_once('/') else {
                        missing.push(realization.body.clone());
                        continue;
                    };
                    match data::body_optional(make, model).await {
                        Ok(Some(page)) => bodies.push(page),
                        _ => missing.push(realization.body.clone()),
                    }
                }
                Ok::<_, String>(WantedShots {
                    bounties,
                    requests: wanted.evidence_requests,
                    index,
                    bodies: Arc::new(bodies),
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
                        <EvidenceRequestCard request index=wanted.index.clone() bodies=wanted.bodies.clone() show_action=show_actions/>
                    }).collect_view()}
                </div>
            }
            .into_any(),
        }
    }
}

/// Where a wanted-shot card's evidence request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WantedOrigin {
    PublishedEstimate,
    BeforePublication,
    Assessment,
}

impl WantedOrigin {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PublishedEstimate => "A published early estimate still needs this",
            Self::BeforePublication => "Before a model is published",
            Self::Assessment => "From a published assessment",
        }
    }
}

/// The origin a wanted-shot card shows for one evidence request.
pub(crate) fn origin(
    index: &SiteIndex,
    bodies: &[looklab_publish::BodyPage],
    request: &EvidenceRequestWire,
) -> WantedOrigin {
    match &request.origin {
        EvidenceRequestOrigin::Report { .. } => WantedOrigin::Assessment,
        EvidenceRequestOrigin::PreCandidate => {
            if crate::data::evidence_look_published(index, bodies, request) {
                WantedOrigin::PublishedEstimate
            } else {
                WantedOrigin::BeforePublication
            }
        }
    }
}

/// The experimental label, the missing evidence a published request names, and
/// the wanted-shots call to action.
#[component]
pub(crate) fn ExperimentalEvidence(
    #[prop(optional_no_strip)] detail: Option<String>,
) -> impl IntoView {
    let (line, action) = match detail {
        Some(detail) => (
            format!("Experimental: {detail}"),
            Some("More independent photos, from different scenes, help check this look."),
        ),
        None => (
            "Experimental: no checked result covers these settings yet.".to_owned(),
            None,
        ),
    };
    view! {
        <p class="notice" role="note">
            {line}
            " "
            <a href=crate::base::site_url("/contribute/wanted")>"Help with a few more shots →"</a>
        </p>
        {action.map(|action| view! { <p class="muted">{action}</p> })}
    }
}

#[component]
fn EvidenceRequestCard(
    request: EvidenceRequestWire,
    index: Arc<SiteIndex>,
    bodies: Arc<Vec<looklab_publish::BodyPage>>,
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
    let look = request_label(&index, &bodies, &request);
    let capability = capability_words(request.capability);
    let origin = origin(&index, &bodies, &request).label();
    let firmware = request
        .body()
        .zip(request.epoch())
        .and_then(|(body, epoch)| {
            bodies
                .iter()
                .find(|page| &page.id == body)
                .and_then(|page| page.epochs.iter().find(|range| range.id == epoch.get()))
        })
        .map(|range| {
            if range.firmware_from == range.firmware_to {
                format!("Firmware {}", range.firmware_from)
            } else {
                format!("Firmware {} to {}", range.firmware_from, range.firmware_to)
            }
        });
    let context = firmware
        .map(|firmware| format!("{camera}. {firmware}"))
        .unwrap_or(camera);
    view! {
        <article class="wanted-card">
            <p class="help-meta">"5 to 10 minutes · " {origin}</p>
            <h2>{format!("Help measure {capability} for {look}")}</h2>
            <p>{context}</p>
            <p>{request.detail.clone()}</p>
            {request.observed_groups.map(|groups| view! {
                <p class="muted">{observed_groups_line(groups)}</p>
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
    let region = bounty_region(&bounty);
    view! {
        <article class="wanted-card">
            <p class="help-meta">"5 to 10 minutes · " {impact}</p>
            <h2>{region}</h2>
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

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
    use looklab_publish::{LookStatus, LookSummary, LookVisibility, VersionSummary};
    use looklab_schema::{EvidenceRequestKind, LookId, ModelSubject, QualificationCapability};

    fn version_summary() -> VersionSummary {
        VersionSummary {
            support: looklab_publish::PublishedSupport::Unstated,
            version: LookVersion::new("1.0.0").expect("version"),
            status: LookStatus::Published,
            matchable: true,
            card_eligible: false,
            approximated: false,
            applicability: looklab_schema::Applicability::Current,
            eligibility_status: None,
            card: None,
            manifest: looklab_publish::SitePath::new("data/looks/provia/1.0.0/manifest.json")
                .expect("manifest path"),
            manifest_sha256: looklab_publish::ArtifactHash::of(b""),
            manifest_bytes: 0,
            artifact: looklab_publish::ArtifactName::new("look.looklab").expect("artifact"),
            sha256: looklab_publish::ArtifactHash::of(b""),
            bytes: 0,
        }
    }

    fn index_for(slug: &str, published: bool) -> SiteIndex {
        SiteIndex {
            format: looklab_publish::SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies: Vec::new(),
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![LookSummary {
                slug: Slug::new(slug).expect("slug"),
                name: "Look".to_owned(),
                kind: LookKind::NativeFamily,
                wb_policy: WbPolicy::Adaptive,
                visibility: LookVisibility::Public,
                head: None,
                selected: None,
                versions: if published {
                    vec![version_summary()]
                } else {
                    Vec::new()
                },
            }],
            scenes: Vec::new(),
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        }
    }

    fn index(published: bool) -> SiteIndex {
        index_for("provia", published)
    }

    fn request() -> EvidenceRequestWire {
        EvidenceRequestWire {
            origin: EvidenceRequestOrigin::PreCandidate,
            subject: ModelSubject::SharedFamily {
                family: LookId::new("provia").expect("family"),
            },
            capability: QualificationCapability::NativePrediction,
            kind: EvidenceRequestKind::MissingIndependentCaptures,
            detail: "one independent RAW capture".to_owned(),
            observed_groups: Some(1),
        }
    }

    #[test]
    fn an_unpublished_look_reads_before_publication() {
        assert_eq!(
            origin(&index(false), &[], &request()),
            WantedOrigin::BeforePublication
        );
    }

    #[test]
    fn a_published_experimental_look_reads_as_a_published_estimate() {
        assert_eq!(
            origin(&index(true), &[], &request()),
            WantedOrigin::PublishedEstimate
        );
    }

    #[test]
    fn a_report_origin_keeps_its_assessment_label() {
        let mut request = request();
        request.origin = EvidenceRequestOrigin::Report {
            report: looklab_schema::ModelReportId::from_digest([7; 32]),
        };
        assert_eq!(
            origin(&index(false), &[], &request),
            WantedOrigin::Assessment
        );
    }

    #[test]
    fn missing_captures_names_the_request_for_the_look() {
        let wanted = crate::data::WantedFile {
            bounties: Vec::new(),
            evidence_requests: vec![request()],
        };
        let index = index(true);
        assert_eq!(
            crate::data::missing_captures(&wanted, &index, "provia"),
            Some("one independent RAW capture".to_owned())
        );
        assert_eq!(
            crate::data::missing_captures(&wanted, &index, "classic-chrome"),
            None
        );
    }

    fn body_page() -> looklab_publish::BodyPage {
        let body = looklab_schema::validate_body(
            "id: fujifilm/x-t5\nmake: Fujifilm\nmodel: X-T5\nexif_aliases: [X-T5]\nkind: camera\ncapabilities:\n  in_camera_raw_conversion: true\n  jpeg_color_spaces: [srgb]\n  heif: false\nstatus: active\nmapping_status: provisional\n",
        )
        .expect("body");
        let schema = looklab_schema::validate_control_schema(include_str!(
            "../../../../data/bodies/fujifilm-x-t5.yaml"
        ))
        .expect("schema");
        looklab_publish::BodyPage::from_schema(
            &body,
            &schema,
            &looklab_publish::BodyExtras::default(),
        )
        .expect("body page")
    }

    fn realization_request(page: &looklab_publish::BodyPage) -> EvidenceRequestWire {
        let card = page.native_looks.first().expect("native sim card");
        EvidenceRequestWire {
            origin: EvidenceRequestOrigin::PreCandidate,
            subject: ModelSubject::BodyRealization {
                realization: looklab_schema::BodyRealizationId {
                    body: page.id.clone(),
                    epoch: looklab_schema::ControlEpochId::new(card.epoch),
                    native_sim: card.sim_key.clone(),
                },
            },
            capability: QualificationCapability::NativePrediction,
            kind: EvidenceRequestKind::MissingIndependentCaptures,
            detail: "one independent RAW capture".to_owned(),
            observed_groups: Some(1),
        }
    }

    #[test]
    fn a_realization_request_resolves_its_look_through_the_body_page() {
        let page = body_page();
        let request = realization_request(&page);
        let look = page.native_looks[0].look.as_str().to_owned();
        let bodies = std::slice::from_ref(&page);
        assert_eq!(
            origin(&index_for(&look, true), bodies, &request),
            WantedOrigin::PublishedEstimate
        );
        assert_eq!(
            origin(&index_for(&look, false), bodies, &request),
            WantedOrigin::BeforePublication
        );
    }

    fn look_summary(
        slug: &str,
        name: &str,
        published: bool,
        visibility: LookVisibility,
    ) -> LookSummary {
        LookSummary {
            slug: Slug::new(slug).expect("slug"),
            name: name.to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            visibility,
            head: None,
            selected: None,
            versions: if published {
                vec![version_summary()]
            } else {
                Vec::new()
            },
        }
    }

    fn body_summary(id: &str, kind: BodyKind, looks: &[&str]) -> looklab_publish::BodySummary {
        let id = BodyId::new(id).expect("body id");
        looklab_publish::BodySummary {
            make: id.make().to_owned(),
            model: id.model().to_owned(),
            id,
            kind,
            status: looklab_publish::BodyStatus::Active,
            mapping_status: looklab_publish::MappingStatus::Provisional,
            preparation: looklab_publish::PreparationPage::default(),
            epochs: 1,
            looks: looks
                .iter()
                .map(|slug| Slug::new(*slug).expect("slug"))
                .collect(),
        }
    }

    fn index_with(bodies: Vec<looklab_publish::BodySummary>, looks: Vec<LookSummary>) -> SiteIndex {
        SiteIndex {
            format: looklab_publish::SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies,
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks,
            scenes: Vec::new(),
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        }
    }

    fn body_request(body: &str, sim: &str, observed: Option<u32>) -> EvidenceRequestWire {
        EvidenceRequestWire {
            origin: EvidenceRequestOrigin::PreCandidate,
            subject: ModelSubject::BodyRealization {
                realization: looklab_schema::BodyRealizationId {
                    body: BodyId::new(body).expect("body"),
                    epoch: looklab_schema::ControlEpochId::new(1),
                    native_sim: looklab_schema::SimKey::new(sim).expect("sim"),
                },
            },
            capability: QualificationCapability::NativePrediction,
            kind: EvidenceRequestKind::MissingIndependentCaptures,
            detail: "Add independent RAW captures with their in-camera JPEGs.".to_owned(),
            observed_groups: observed,
        }
    }

    fn bounty(body: Option<&str>, look: &str, status: BountyStatus) -> Bounty {
        Bounty {
            look: looklab_upload::LookSlug::new(look).expect("look"),
            look_version: looklab_trust::VersionId::new(1),
            body: body.map(|body| BodyId::new(body).expect("body")),
            region: looklab_trust::Region::new(2, 1).expect("region"),
            output: None,
            expected_variance_reduction: 0.2,
            reason: looklab_trust::BountyReason::UnverifiedRegion { sigma: 0.2 },
            status,
        }
    }

    #[test]
    fn each_camera_lists_published_looks_and_open_requests() {
        let index = index_with(
            vec![
                body_summary("fujifilm/x-t5", BodyKind::Camera, &["provia", "velvia"]),
                body_summary("sony/a7", BodyKind::Camera, &["provia"]),
                body_summary("acme/editor", BodyKind::Software, &["provia"]),
            ],
            vec![
                look_summary("provia", "PROVIA / Standard", true, LookVisibility::Public),
                look_summary("velvia", "Velvia / Vivid", false, LookVisibility::Public),
                look_summary("fixture", "Fixture", true, LookVisibility::TestFixture),
            ],
        );
        let wanted = WantedFile {
            bounties: vec![
                bounty(Some("sony/a7"), "provia", BountyStatus::Open),
                bounty(None, "provia", BountyStatus::Resolved),
            ],
            evidence_requests: vec![body_request("fujifilm/x-t5", "provia", Some(2)), request()],
        };
        let stands = standings(&index, Some(&wanted));
        assert_eq!(stands.len(), 2);
        let fuji = &stands[0];
        assert_eq!(fuji.make, "fujifilm");
        assert_eq!(fuji.published.len(), 1);
        assert_eq!(fuji.published[0].0.as_str(), "provia");
        assert_eq!(fuji.published[0].1, "PROVIA / Standard");
        assert_eq!(fuji.requests.len(), 2);
        assert_eq!(fuji.requests[0].observed_groups, Some(2));
        assert!(fuji.bounties.is_empty());
        let sony = &stands[1];
        assert_eq!(sony.published.len(), 1);
        assert_eq!(sony.requests.len(), 1);
        assert_eq!(sony.requests[0].observed_groups, Some(1));
        assert_eq!(sony.bounties.len(), 1);
        assert_eq!(sony.bounties[0].status, BountyStatus::Open);
    }

    #[test]
    fn a_camera_without_open_requests_still_lists_its_looks() {
        let index = index_with(
            vec![body_summary("fujifilm/x-t5", BodyKind::Camera, &["provia"])],
            vec![look_summary(
                "provia",
                "PROVIA / Standard",
                true,
                LookVisibility::Public,
            )],
        );
        let wanted = WantedFile {
            bounties: Vec::new(),
            evidence_requests: Vec::new(),
        };
        let stands = standings(&index, Some(&wanted));
        assert_eq!(stands.len(), 1);
        assert_eq!(stands[0].published.len(), 1);
        assert!(stands[0].requests.is_empty());
        assert!(stands[0].bounties.is_empty());
    }

    #[test]
    fn a_request_without_an_observed_count_keeps_the_request_text() {
        let index = index_with(
            vec![body_summary("fujifilm/x-t5", BodyKind::Camera, &["provia"])],
            Vec::new(),
        );
        let wanted = WantedFile {
            bounties: Vec::new(),
            evidence_requests: vec![body_request("fujifilm/x-t5", "provia", None)],
        };
        let stands = standings(&index, Some(&wanted));
        assert_eq!(stands[0].requests.len(), 1);
        assert_eq!(stands[0].requests[0].observed_groups, None);
        assert!(
            stands[0].requests[0]
                .observed_groups
                .map(observed_groups_line)
                .is_none()
        );
        assert_eq!(
            stands[0].requests[0].detail,
            "Add independent RAW captures with their in-camera JPEGs."
        );
    }

    #[test]
    fn a_bounty_without_a_body_reaches_cameras_with_that_look() {
        let index = index_with(
            vec![
                body_summary("fujifilm/x-t5", BodyKind::Camera, &["provia"]),
                body_summary("fujifilm/x-s10", BodyKind::Camera, &["provia"]),
                body_summary("fujifilm/x-e3", BodyKind::Camera, &["velvia"]),
            ],
            Vec::new(),
        );
        let wanted = WantedFile {
            bounties: vec![bounty(None, "provia", BountyStatus::Open)],
            evidence_requests: Vec::new(),
        };
        let stands = standings(&index, Some(&wanted));
        let with_bounty = stands
            .iter()
            .filter(|camera| camera.bounties.len() == 1)
            .map(|camera| camera.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(with_bounty, ["fujifilm/x-s10", "fujifilm/x-t5"]);
    }

    #[test]
    fn a_realization_request_labels_its_look_by_display_name() {
        let page = body_page();
        let request = realization_request(&page);
        let look = page.native_looks[0].look.as_str().to_owned();
        assert_eq!(
            request_label(
                &index_for(&look, true),
                std::slice::from_ref(&page),
                &request
            ),
            "Look"
        );
    }
}
