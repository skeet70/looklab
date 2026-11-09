// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use looklab_publish::{BodyPage, ControlPage, EpochPage, ManualPage};
use looklab_schema::{
    CapabilityDomain, ConfirmationBasis, ConfirmationState, Finding, FirmwareConfirmation,
    FirmwareScope, ModeratorFirmwareScope,
};

use super::manual_confirm::{ManualModerationPanel, StandaloneConfirmation};
use crate::data::{self, Load};
use crate::ui;

#[derive(Clone, Debug)]
struct LookGallery {
    index: looklab_publish::SiteIndex,
    scene: Option<std::sync::Arc<looklab_publish::PreparedScene>>,
    preview_error: Option<String>,
}

#[component]
pub fn BodyDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<(Option<BodyPage>, Option<ManualPage>)>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let make = params.read().get("make").unwrap_or_default();
        let model = params.read().get("model").unwrap_or_default();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let body = data::body_optional(&make, &model).await;
            let manual = data::manual(&make, &model).await;
            if generation.try_get_untracked() != Some(mine) {
                return;
            }
            state.set(match (body, manual) {
                (Ok(body), Ok(manual)) if body.is_some() || manual.is_some() => {
                    Load::Ready((body, manual))
                }
                (Ok(_), Ok(_)) => {
                    Load::Failed("This camera has no published body or documentation.".to_owned())
                }
                (Err(error), _) | (_, Err(error)) => Load::Failed(error),
            });
        });
    });

    view! {
        <article class="camera-page">
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready((Some(body), manual)) => view! { <BodyDetailView body manual/> }.into_any(),
            Load::Ready((None, Some(manual))) => view! { <ManualOnlyView manual/> }.into_any(),
            Load::Ready((None, None)) => ui::failure("This camera has no published data.").into_any(),
        }}
        </article>
    }
}

#[component]
fn ManualOnlyView(manual: ManualPage) -> impl IntoView {
    let make = manual.make.clone();
    let model = manual.model.clone();
    let ready = manual.preparation.is_ready();
    view! {
        <h1>{format!("{make} {model}")}</h1>
        <p class="lede">{if ready {
            "This camera has documented guidance from its official manual."
        } else {
            "This camera has documented guidance from its official manual, but no contribution support yet."
        }}</p>
        <ManualSection manual/>
    }
}

#[component]
fn BodyDetailView(body: BodyPage, manual: Option<ManualPage>) -> impl IntoView {
    let epoch = RwSignal::new(body.epochs.first().map(|epoch| epoch.id));
    let selected = {
        let epochs = body.epochs.clone();
        move || {
            let id = epoch.get();
            epochs
                .iter()
                .find(|candidate| Some(candidate.id) == id)
                .or_else(|| epochs.first())
                .cloned()
        }
    };
    let initial_epoch = body.epochs.first().map(|epoch| epoch.id);
    let gallery = RwSignal::new(Load::<LookGallery>::Loading);
    Effect::new(move |_| {
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            gallery.set(
                match async {
                    let index = data::index().await?;
                    let photo = index
                        .scenes
                        .iter()
                        .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
                        .map(|scene| scene.name.clone());
                    let (scene, preview_error) = match photo {
                        Some(name) => match data::scene_thumbnail(name.as_str()).await {
                            Ok(loaded) => (Some(std::sync::Arc::new(loaded)), None),
                            Err(error) => (None, Some(error)),
                        },
                        None => (None, None),
                    };
                    Ok(LookGallery {
                        index,
                        scene,
                        preview_error,
                    })
                }
                .await
                {
                    Ok(value) => Load::Ready(value),
                    Err(error) => Load::Failed(error),
                },
            );
        });
    });
    let camera_id = body.id.clone();
    let match_camera = body.id.clone();
    let navigate = leptos_router::hooks::use_navigate();
    let ready = body.preparation.is_ready();

    view! {
        <h1>{format!("{} {}", body.make, body.model)}</h1>
        <p class="lede">"Explore its built-in color, then find settings for the looks you love."</p>
        <h2>"Built-in looks"</h2>
        {move || match gallery.get() { Load::Ready(LookGallery { index, scene, preview_error }) => {
            let items=super::looks::cards(&index);
            if items.is_empty() {
                crate::prefetch::route_rendered();
                return view! {
                    <p class="muted">"No looks are published yet. Look previews and settings for this camera appear here once looks are published."</p>
                    <p class="field-row">
                        {ready.then(|| view! {
                            <a href=crate::base::site_url("/contribute")>"Share photos from this camera"</a>
                        })}
                        <a href=crate::base::site_url("/about/how-it-works")>"How looks are made →"</a>
                    </p>
                }.into_any();
            }
            let built_in=items.iter().filter(|item|index.bodies.iter().any(|body|body.id==camera_id && body.looks.contains(&item.slug))).cloned().collect::<Vec<_>>();
            let on_select = {
                let camera = match_camera.clone();
                let navigate = navigate.clone();
                Callback::new(move |item: ui::LookCardItem| {
                    navigate(&format!("/match?camera={camera}&look={}", item.slug), Default::default());
                })
            };
            view! {
                {preview_error.map(|error| view! {
                    <p class="muted" role="status">"Look previews could not load. Camera details are still available."</p>
                    <ui::Disclosure summary="Preview details".to_owned()><p>{error}</p></ui::Disclosure>
                })}
                <div class="look-gallery">
                    {built_in.into_iter().map(|item| match &scene {
                        Some(scene) => view! {
                            <super::looks::CharacterCard item scene=scene.clone()/>
                        }.into_any(),
                        None => view! { <ui::LookCard item/> }.into_any(),
                    }).collect_view()}
                </div>
                {ready.then(|| view! {
                    <h2>"Get any look on this camera"</h2>
                    <ui::LookPicker items scene on_select/>
                    <p>"These simulated previews use the shared measurements available so far. More photos help us check agreement across cameras."</p>
                })}
            }.into_any()
        },Load::Failed(error)=>ui::failure(&error).into_any(),_=>ui::loading().into_any() }}
        <h2>"Ways to apply looks"</h2><p>"Start with in-camera settings, or use an editing app."</p>
        {if ready {
            view! { <a href=crate::base::site_url(&format!("/match?camera={}",body.id))>"Find settings and downloads →"</a> }.into_any()
        } else {
            view! { <p class="muted">"Settings matching becomes available when this camera's input transform is published."</p> }.into_any()
        }}
        <section><h2>"Help with this camera"</h2><a href=crate::base::site_url("/contribute/wanted")>"See wanted shots →"</a></section>
        <super::recipes::RecipeSection camera=body.id.clone()/>
        <section aria-labelledby="realization">
            <h2 id="realization">"Rendering notes"</h2>
            {if body.realization_notes.is_empty() {
                view! { <p class="muted">"No rendering notes for this camera."</p> }.into_any()
            } else {
                view! {
                    <ul>
                        {body.realization_notes.iter().map(|note| {
                            view! { <li>{note.text.clone()}</li> }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </section>
        <ui::Disclosure summary="Technical details".to_owned()>
        <dl class="facts">
            <dt>"Kind"</dt><dd>{ui::camera_kind_label(body.kind)}</dd>
            <dt>"Status"</dt><dd>{format!("{:?}", body.status).to_lowercase()}</dd>
            <dt>"Sample-file confirmation"</dt><dd>{ui::mapping_status_label(body.mapping_status)}</dd>
            <dt>"EXIF aliases"</dt><dd>{body.exif_aliases.join(", ")}</dd>
        </dl>
        <section aria-labelledby="capabilities">
            <h2 id="capabilities">"Capabilities"</h2>
            <ul>
                <li>{format!("In-camera RAW conversion: {}", yes_no(body.capabilities.in_camera_raw_conversion))}</li>
                <li>{format!("JPEG color spaces: {}", body.capabilities.jpeg_color_spaces.join(", "))}</li>
                <li>{format!("HEIF: {}", yes_no(body.capabilities.heif))}</li>
            </ul>
        </section>
        <section aria-labelledby="transform">
            <h2 id="transform">"Input transform confidence"</h2>
            {match body.preparation.readiness {
                looklab_publish::PreparationReadiness::Ready => view! {
                    <p>"Ready for measured RAW+JPEG contributions."</p>
                }.into_any(),
                looklab_publish::PreparationReadiness::Unavailable => {
                    let detail = match body.preparation.blocker {
                        Some(looklab_publish::PreparationBlocker::InputTransformMissing) =>
                            "No input transform is published, so RAW+JPEG contributions from this camera cannot enter a colour fit yet.",
                        None =>
                            "A required fit resource is missing, so contributions cannot be measured yet.",
                    };
                    view! { <p class="warn">{detail}</p> }.into_any()
                },
                looklab_publish::PreparationReadiness::Unknown => view! {
                    <p class="muted">"Fit readiness is not published."</p>
                }.into_any(),
            }}
            {match &body.input_transform {
                Some(transform) => view! {
                    <p>
                        {format!(
                            "Version {} · source {:?} · {} anchors · {} tier",
                            transform.version,
                            transform.source,
                            transform.anchor_count,
                            format!("{:?}", transform.tier).to_lowercase(),
                        )}
                    </p>
                }.into_any(),
                None => view! {
                    <p class="muted">
                        {if body.preparation.is_ready() {
                            "Input transform present; source and anchoring details are not published."
                        } else {
                            "No published input transform."
                        }}
                    </p>
                }.into_any(),
            }}
        </section>
        <section aria-labelledby="schema">
            <h2 id="schema">"Control schema"</h2>
            <label for="epoch-select">"Firmware versions"</label>
            <select
                id="epoch-select"
                on:change=move |event| {
                    epoch.set(event_target_value(&event).parse().ok());
                }
            >
                {body.epochs.iter().map(|epoch| {
                    let id = epoch.id;
                    view! {
                        <option value=id.to_string() selected=Some(id) == initial_epoch>
                            {format!("Firmware versions {} to {}", epoch.firmware_from, epoch.firmware_to)}
                        </option>
                    }
                }).collect_view()}
            </select>
            {move || selected().map(|epoch| view! { <ControlTable epoch/> })}
            <h3>"Differences between firmware ranges"</h3>
            <EpochDifferences epochs=body.epochs.clone()/>
        </section>
        </ui::Disclosure>
        {manual.map(|manual| view! { <ManualSection manual/> })}
    }
}

#[component]
fn ManualSection(manual: ManualPage) -> impl IntoView {
    let sources = manual.sources.clone();
    let capabilities = manual.capabilities.clone();
    let upload_support = manual.preparation.is_ready();
    let standalone = !upload_support;
    let form_manual = manual.clone();
    let ready_manual = manual.clone();
    let moderation_manual = manual.clone();
    let summary = RwSignal::new(manual.confirmation.clone());
    let reviewed = move || {
        summary.with(|summary| {
            summary
                .controls
                .iter()
                .filter(|control| has_human_evidence(control))
                .count()
        })
    };
    let contradicted = move || summary.with(|summary| summary.contradicted);
    let total = capabilities.len();
    let table_capabilities = capabilities.clone();
    let table_sources = sources.clone();
    let rows = move || {
        let current = summary.get();
        table_capabilities
            .iter()
            .map(|capability| {
                let confirmation = current
                    .control(capability.documented.key)
                    .cloned()
                    .unwrap_or_else(|| capability.confirmation.clone());
                let documented = &capability.documented;
                let label = documented.label.clone();
                let key = documented.key;
                let category = category_text(documented.category);
                let manual_value = format!(
                    "{} · {}",
                    availability_text(documented.availability),
                    domain_text(&documented.domain)
                );
                let firmware_actuals = confirmation.firmware.iter().map(|result| {
                    let firmware = result.firmware.as_str().to_owned();
                    let actual = firmware_actual_text(result);
                    let detail = firmware_detail(result);
                    let digest = firmware_definition_digest(result);
                    view! {
                        <div>
                            <strong>{format!("Firmware {firmware}")}</strong>{format!(": {actual}")}
                            {detail.map(|detail| view! { <div class="muted">{detail}</div> })}
                            {digest.map(|digest| view! {
                                <ui::Disclosure summary="Attestation details".to_owned()>
                                    <p class="muted">"Control definition digest at confirmation"</p>
                                    <code>{digest}</code>
                                </ui::Disclosure>
                            })}
                        </div>
                    }
                }).collect_view();
                let firmware_bases = confirmation.firmware.iter().map(|result| {
                    let firmware = result.firmware.as_str().to_owned();
                    let basis = firmware_basis_text(result);
                    let contradictions = result.contradictions.iter().map(|contradiction| {
                        view! {
                            <li>{format!("Conflicting report: {}", finding_text(&contradiction.finding))}</li>
                        }
                    }).collect_view();
                    view! {
                        <li>
                            <strong>{format!("Firmware {firmware}")}</strong>{format!(": {basis}")}
                            {(!result.contradictions.is_empty()).then(|| view! {
                                <ul>{contradictions}</ul>
                            })}
                        </li>
                    }
                }).collect_view();
                let control_actual = control_actual_text(&confirmation);
                let control_basis = confirmation_text(&confirmation);
                let moderator = moderator_evidence_text(&confirmation);
                let manual_note = documentation_note(documented.availability, &confirmation);
                let citations = documented.citations.iter().map(|citation| {
                    match table_sources.iter().find(|source| source.id == citation.source) {
                        Some(source) => {
                            let url = source.url.as_str().to_owned();
                            let title = source.title.clone();
                            let locator = citation.locator.clone();
                            view! {
                                <li><a href=url target="_blank" rel="noreferrer">{title}</a>{format!(": {locator}")}</li>
                            }.into_any()
                        }
                        None => view! { <li>{citation.locator.clone()}</li> }.into_any(),
                    }
                }).collect_view();
                view! {
                    <tr>
                        <th scope="row">{label}<div class="muted"><code>{key.as_str()}</code></div></th>
                        <td>{category}</td>
                        <td>
                            {(!confirmation.firmware.is_empty()).then(|| view! {
                                <div>{firmware_actuals}</div>
                            })}
                            {confirmation.firmware.is_empty().then(|| view! {
                                <div>{control_actual}</div>
                            })}
                        </td>
                        <td>
                            {manual_value}
                            {manual_note.map(|text| view! { <div class="muted">{text}</div> })}
                        </td>
                        <td>
                            <div class="muted">{control_basis}</div>
                            {(!confirmation.firmware.is_empty()).then(|| view! {
                                <ul>{firmware_bases}</ul>
                            })}
                            {moderator.map(|text| view! {
                                <div class="muted">{text}</div>
                            })}
                        </td>
                        <td><ul>{citations}</ul></td>
                    </tr>
                }
            })
            .collect_view()
    };
    view! {
        <section class="manual-documentation" aria-labelledby="documentation">
            <h2 id="documentation">"Documented settings and owner confirmations"</h2>
            {move || {
                let current = summary.get();
                let (label, detail) = coverage_text(&current);
                view! {
                    <p><span class="badge">{label}</span></p>
                    <p class="muted">{detail}</p>
                }
            }}
            <p class="muted">"From official manuals and firmware notes. Owner-confirmed corrections are shown separately from the manual claim. Documentation does not imply RAW decoding, contribution support or fit eligibility."</p>
            <p class="muted">"Owner agreement counts profiles linked to this camera by contribution metadata. Unlinked checks are saved but do not count toward confirmation."</p>
            {if upload_support {
                ().into_any()
            } else {
                view! {
                    <p class="notice">"Contribution support is not available for this camera yet. You can still check the documented settings below."</p>
                }.into_any()
            }}
            <h3>"Sources"</h3>
            <ul class="manual-sources">
                {sources.iter().map(|source| {
                    let url = source.url.as_str().to_owned();
                    let title = source.title.clone();
                    let edition = source.edition.clone().unwrap_or_default();
                    let scope = source.firmware_scope.as_str().to_owned();
                    view! {
                        <li>
                            <a href=url target="_blank" rel="noreferrer">{title}</a>
                            {if edition.is_empty() { "".to_owned() } else { format!(" · {edition}") }}
                            <span class="muted">{format!(" · firmware {scope}")}</span>
                        </li>
                    }
                }).collect_view()}
            </ul>
            <p>{move || format!("{} of {total} documented controls have owner or moderator evidence; {} contradicted.", reviewed(), contradicted())}</p>
            <table class="data-table">
                <caption>"Documented controls, confirmed actual values and confirmation basis"</caption>
                <thead>
                    <tr>
                        <th scope="col">"Control"</th>
                        <th scope="col">"Category"</th>
                        <th scope="col">"Confirmed actual"</th>
                        <th scope="col">"Manual says (documentation-derived)"</th>
                        <th scope="col">"Confirmation basis"</th>
                        <th scope="col">"Citations"</th>
                    </tr>
                </thead>
                <tbody>{rows}</tbody>
            </table>
            {crate::data::moderation_session_open().then(|| view! {
                <ManualModerationPanel manual=moderation_manual summary=summary/>
            })}
            {upload_support.then(|| view! {
                <StandaloneConfirmation manual=ready_manual summary=summary measured_contributions=true/>
            })}
            {standalone.then(|| view! {
                <StandaloneConfirmation manual=form_manual summary=summary/>
            })}
        </section>
    }
}

pub(crate) fn documentation_note(
    availability: looklab_schema::Availability,
    confirmation: &looklab_schema::ControlConfirmation,
) -> Option<&'static str> {
    if availability == looklab_schema::Availability::NotDocumented {
        return None;
    }
    Some(
        if confirmation
            .firmware
            .iter()
            .any(|result| matches!(result.basis.as_ref(), Some(ConfirmationBasis::Corrected)))
        {
            "Documentation-derived; some firmware values are corrected by owners"
        } else {
            "Documentation-derived"
        },
    )
}

pub(crate) fn category_text(category: looklab_schema::CapabilityCategory) -> &'static str {
    match category {
        looklab_schema::CapabilityCategory::Rendering => "Rendering",
        looklab_schema::CapabilityCategory::Spatial => "Spatial",
        looklab_schema::CapabilityCategory::Capture => "Capture",
        looklab_schema::CapabilityCategory::Workflow => "Workflow",
        looklab_schema::CapabilityCategory::Mixed => "Mixed impact",
        looklab_schema::CapabilityCategory::Unknown => "Impact not established",
    }
}

pub(crate) fn availability_text(availability: looklab_schema::Availability) -> &'static str {
    match availability {
        looklab_schema::Availability::Available => "Documented available",
        looklab_schema::Availability::Unavailable => "Documented unavailable",
        looklab_schema::Availability::NotDocumented => "Not documented",
    }
}

pub(crate) fn actual_availability_text(availability: looklab_schema::Availability) -> &'static str {
    match availability {
        looklab_schema::Availability::Available => "Available",
        looklab_schema::Availability::Unavailable => "Not available",
        looklab_schema::Availability::NotDocumented => "Not documented",
    }
}

fn moderator_scope_text(scope: &ModeratorFirmwareScope) -> String {
    match scope {
        ModeratorFirmwareScope::Unspecified {} => "firmware scope not recorded".to_owned(),
        ModeratorFirmwareScope::Exact { firmware } => {
            format!("firmware {}", firmware.as_str())
        }
        ModeratorFirmwareScope::Range { from, to } => {
            format!("firmware {} to {}", from.as_str(), to.as_str())
        }
    }
}

fn has_human_evidence(control: &looklab_schema::ControlConfirmation) -> bool {
    control.moderator.is_some() || !control.firmware.is_empty()
}

fn coverage_text(summary: &looklab_schema::ManualConfirmationSummary) -> (&'static str, String) {
    if summary.human_confirmed() {
        let firmware = summary
            .common_confirmed_firmware()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return (
            "Human-confirmed scoped coverage",
            format!("Every documented control is confirmed for firmware {firmware}."),
        );
    }
    let reviewed = summary
        .controls
        .iter()
        .filter(|control| has_human_evidence(control))
        .count();
    if reviewed == 0 {
        return (
            "Documentation-derived only",
            "No owner or moderator evidence is counted for these controls yet.".to_owned(),
        );
    }
    let moderator_only = summary.controls.iter().all(|control| {
        control.state == ConfirmationState::Documented
            || matches!(
                control.state,
                ConfirmationState::ModeratorConfirmed | ConfirmationState::ModeratorRejected
            )
    });
    if moderator_only {
        let unspecified = summary.controls.iter().any(|control| {
            control
                .moderator
                .as_ref()
                .is_some_and(|evidence| evidence.firmware_scope.is_unspecified())
        });
        return if unspecified {
            (
                "Human-reviewed",
                "A moderator reviewed these controls; at least one decision does not record a firmware scope, so no firmware is claimed as camera-confirmed.".to_owned(),
            )
        } else {
            (
                "Human-reviewed; no single firmware confirmed",
                "A moderator reviewed these controls; their scopes do not establish one firmware across every documented control.".to_owned(),
            )
        };
    }
    (
        "Partially human-checked",
        "Some controls have owner or moderator evidence; no firmware is confirmed across every documented control.".to_owned(),
    )
}

pub(crate) fn confirmation_text(confirmation: &looklab_schema::ControlConfirmation) -> String {
    match confirmation.state {
        ConfirmationState::Contradicted => {
            "Owner reports disagree on at least one firmware value; no single actual value confirmed"
                .to_owned()
        }
        ConfirmationState::Confirmed => format!(
            "Confirmed on at least one firmware value; {} group(s) at the strongest firmware",
            confirmation.agreeing_groups
        ),
        ConfirmationState::Unconfirmed => format!(
            "{} independent owner group(s); 3 needed",
            confirmation.agreeing_groups
        ),
        ConfirmationState::Documented => "No owner or moderator evidence".to_owned(),
        ConfirmationState::ModeratorConfirmed => match confirmation.moderator.as_ref() {
            Some(evidence) => match &evidence.firmware_scope {
                ModeratorFirmwareScope::Exact { firmware } => {
                    format!("Moderator-confirmed for firmware {}", firmware.as_str())
                }
                scope => format!(
                    "Human-reviewed by a moderator for {}; no single firmware confirmed",
                    moderator_scope_text(scope)
                ),
            },
            None => "Moderator-confirmed".to_owned(),
        },
        ConfirmationState::ModeratorRejected => {
            "Rejected by a moderator with cited evidence".to_owned()
        }
    }
}

pub(crate) fn control_actual_text(confirmation: &looklab_schema::ControlConfirmation) -> String {
    match confirmation.state {
        ConfirmationState::ModeratorConfirmed => match confirmation.moderator.as_ref() {
            Some(evidence) if evidence.firmware_scope.exact_firmware().is_some() => {
                "Confirmed by moderator; actual value in evidence".to_owned()
            }
            Some(_) => "Human-reviewed by moderator; actual value in evidence".to_owned(),
            None => "Confirmed by moderator; actual value in evidence".to_owned(),
        },
        ConfirmationState::ModeratorRejected => "Rejected by moderator".to_owned(),
        ConfirmationState::Confirmed => "Confirmed; actual value not recorded".to_owned(),
        _ => "Not confirmed".to_owned(),
    }
}

pub(crate) fn firmware_basis_text(result: &FirmwareConfirmation) -> String {
    match result.basis.as_ref() {
        Some(ConfirmationBasis::Documented) => format!(
            "confirmed against the manual by {} group(s)",
            result.agreeing_groups
        ),
        Some(ConfirmationBasis::Corrected) => format!(
            "confirmed from {} owner correction(s); the manual value is not confirmed",
            result.agreeing_groups
        ),
        Some(ConfirmationBasis::Moderator) => "moderator decision".to_owned(),
        None => match result.state {
            ConfirmationState::Contradicted => "owner reports disagree".to_owned(),
            ConfirmationState::Unconfirmed => {
                format!("{} group(s); 3 needed", result.agreeing_groups)
            }
            ConfirmationState::Documented => "no owner or moderator evidence".to_owned(),
            ConfirmationState::Confirmed => {
                format!("confirmed by {} group(s)", result.agreeing_groups)
            }
            ConfirmationState::ModeratorConfirmed => "confirmed by moderator".to_owned(),
            ConfirmationState::ModeratorRejected => "rejected by moderator".to_owned(),
        },
    }
}

pub(crate) fn finding_text(finding: &Finding) -> String {
    match finding {
        Finding::Matches => "matches the manual".to_owned(),
        Finding::Corrected {
            availability,
            domain,
        } => format!(
            "{} · {}",
            actual_availability_text(*availability),
            domain_text(domain)
        ),
    }
}

pub(crate) fn firmware_actual_text(result: &FirmwareConfirmation) -> String {
    match &result.confirmed_actual {
        Some(actual) => format!(
            "{} · {}",
            actual_availability_text(actual.availability),
            domain_text(&actual.domain)
        ),
        None => match result.state {
            ConfirmationState::Contradicted => "No single actual value confirmed".to_owned(),
            ConfirmationState::Confirmed => "Confirmed; actual value not recorded".to_owned(),
            ConfirmationState::ModeratorConfirmed => {
                "Confirmed by moderator; actual value in evidence".to_owned()
            }
            ConfirmationState::ModeratorRejected => "Rejected by moderator".to_owned(),
            ConfirmationState::Unconfirmed | ConfirmationState::Documented => {
                "Not confirmed".to_owned()
            }
        },
    }
}

pub(crate) fn firmware_detail(result: &FirmwareConfirmation) -> Option<String> {
    result.confirmed_actual.as_ref().map(|actual| {
        format!(
            "{} independent group(s) · observed firmware {}",
            actual.agreeing_groups,
            firmware_scopes_text(&actual.firmware_scopes)
        )
    })
}

pub(crate) fn firmware_definition_digest(result: &FirmwareConfirmation) -> Option<String> {
    result
        .confirmed_actual
        .as_ref()
        .map(|actual| actual.definition_digest.to_string())
}

pub(crate) fn moderator_evidence_text(
    confirmation: &looklab_schema::ControlConfirmation,
) -> Option<String> {
    confirmation.moderator.as_ref().map(|evidence| {
        let scope = moderator_scope_text(&evidence.firmware_scope);
        let mut text = format!(
            "{} evidence ({scope}): {}",
            evidence.kind.as_str(),
            evidence.description
        );
        if let Some(url) = &evidence.url {
            text.push_str(&format!(" ({})", url.as_str()));
        }
        text
    })
}

fn firmware_scopes_text(scopes: &[FirmwareScope]) -> String {
    if scopes.is_empty() || scopes.iter().any(FirmwareScope::is_unspecified) {
        "unspecified".to_owned()
    } else {
        scopes
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub(crate) fn domain_text(domain: &CapabilityDomain) -> String {
    match domain {
        CapabilityDomain::Boolean { default } => match default {
            Some(value) => format!("On/off; default {}", if *value { "on" } else { "off" }),
            None => "On/off".to_owned(),
        },
        CapabilityDomain::Enum { values, default } => match default {
            Some(default) => format!("{}; default {default}", values.join(", ")),
            None => values.join(", "),
        },
        CapabilityDomain::Integer {
            min,
            max,
            step,
            steps,
            default,
        } => {
            let increments = step_text(
                step.as_ref().map(|value| value.to_string()),
                steps,
                |value| value.to_string(),
            );
            format!(
                "{min} to {max}{increments}{}",
                default.map_or(String::new(), |value| format!("; default {value}"))
            )
        }
        CapabilityDomain::Rational {
            min,
            max,
            step,
            steps,
            default,
        } => {
            let increments = step_text(step.as_ref().cloned(), steps, Clone::clone);
            format!(
                "{min} to {max}{increments}{}",
                default
                    .as_ref()
                    .map_or(String::new(), |value| format!("; default {value}"))
            )
        }
        CapabilityDomain::Axes { axes } => axes
            .iter()
            .map(|axis| match (axis.min, axis.max, axis.step) {
                (Some(min), Some(max), Some(step)) => {
                    format!("{}: {min} to {max} step {step}", axis.label)
                }
                _ => format!("{}: range not printed", axis.label),
            })
            .collect::<Vec<_>>()
            .join("; "),
        CapabilityDomain::Unspecified => "Not documented numerically".to_owned(),
    }
}

fn step_text<T: Clone>(step: Option<String>, steps: &[T], render: impl Fn(&T) -> String) -> String {
    if let Some(step) = step {
        return format!(", step {step}");
    }
    if steps.is_empty() {
        String::new()
    } else {
        format!(
            ", steps {}",
            steps.iter().map(render).collect::<Vec<_>>().join(", ")
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[component]
fn ControlTable(epoch: EpochPage) -> impl IntoView {
    view! {
        <h3>{format!("Firmware range {} controls", epoch.id)}</h3>
        <table class="data-table">
            <caption>{format!("Controls in firmware range {}", epoch.id)}</caption>
            <thead>
                <tr>
                    <th scope="col">"Control"</th>
                    <th scope="col">"Type"</th>
                    <th scope="col">"Range or values"</th>
                    <th scope="col">"Default"</th>
                    <th scope="col">"Affects"</th>
                    <th scope="col">"Look affecting"</th>
                    <th scope="col">"Spatial"</th>
                </tr>
            </thead>
            <tbody>
                {epoch.controls.iter().map(|control| {
                    view! {
                        <tr>
                            <th scope="row">{control.key.to_string()}</th>
                            <td>{format!("{:?}", control.kind).to_lowercase()}</td>
                            <td>{range_text(control)}</td>
                            <td>{default_text(control)}</td>
                            <td>{control.affects.iter().map(|affect| format!("{affect:?}").to_lowercase()).collect::<Vec<_>>().join(", ")}</td>
                            <td>{yes_no(control.look_affecting)}</td>
                            <td>{yes_no(control.spatial)}</td>
                        </tr>
                    }
                }).collect_view()}
            </tbody>
        </table>
        <p class="muted">
            {format!("Solve plan: {}", epoch.solve_plan.iter().map(|step| format!("{} ({})", step.stage, step.controls.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))).collect::<Vec<_>>().join(" → "))}
        </p>
    }
}

fn range_text(control: &ControlPage) -> String {
    if !control.values.is_empty() {
        return control
            .values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
    }
    match &control.range {
        Some(range) => match &control.step {
            Some(step) => format!("{} to {}, step {step}", range.min, range.max),
            None => format!("{} to {}", range.min, range.max),
        },
        None => "—".to_owned(),
    }
}

fn default_text(control: &ControlPage) -> String {
    match &control.default {
        looklab_publish::ControlDefaultPage::Number(value) => value.clone(),
        looklab_publish::ControlDefaultPage::Enum(value) => value.to_string(),
        looklab_publish::ControlDefaultPage::Bool(value) => value.to_string(),
    }
}

#[component]
fn EpochDifferences(epochs: Vec<EpochPage>) -> impl IntoView {
    let Some(first) = epochs.first() else {
        return view! { <p class="muted">"No firmware ranges."</p> }.into_any();
    };
    let mut rows = Vec::new();
    for epoch in epochs.iter().skip(1) {
        let previous: BTreeMap<_, _> = first
            .controls
            .iter()
            .map(|control| (control.key.to_string(), control))
            .collect();
        let current: BTreeMap<_, _> = epoch
            .controls
            .iter()
            .map(|control| (control.key.to_string(), control))
            .collect();
        for (key, control) in &current {
            match previous.get(key) {
                None => rows.push(format!("Firmware range {} adds {key}", epoch.id)),
                Some(before) if *before != *control => {
                    rows.push(format!("Firmware range {} changes {key}", epoch.id));
                }
                Some(_) => {}
            }
        }
        for key in previous.keys() {
            if !current.contains_key(key) {
                rows.push(format!("Firmware range {} removes {key}", epoch.id));
            }
        }
    }
    if rows.is_empty() {
        view! { <p class="muted">"All firmware ranges share one control set."</p> }.into_any()
    } else {
        view! {
            <ul>{rows.into_iter().map(|row| view! { <li>{row}</li> }).collect_view()}</ul>
        }
        .into_any()
    }
}
