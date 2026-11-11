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
    bodies: Arc<Vec<looklab_publish::BodyPage>>,
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
            Some(
                "More independent photos (different scenes) of this film simulation on that camera help.",
            ),
        ),
        None => (
            "Experimental: this target is not a qualified result for these settings.".to_owned(),
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
    let origin = origin(&index, &bodies, &request).label();
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
}
