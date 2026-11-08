// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use looklab_publish::{BodyPage, ControlPage, EpochPage};

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn BodyDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<BodyPage>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let make = params.read().get("make").unwrap_or_default();
        let model = params.read().get("model").unwrap_or_default();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = data::body(&make, &model).await;
            if generation.try_get_untracked() != Some(mine) {
                return;
            }
            state.set(match result {
                Ok(body) => Load::Ready(body),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(body) => view! { <BodyDetailView body/> }.into_any(),
        }}
    }
}

#[component]
fn BodyDetailView(body: BodyPage) -> impl IntoView {
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
    let gallery = RwSignal::new(
        Load::<(
            looklab_publish::SiteIndex,
            std::sync::Arc<looklab_publish::PreparedScene>,
        )>::Loading,
    );
    Effect::new(move |_| {
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            gallery.set(
                match async { Ok((data::index().await?, super::looks::photo().await?)) }.await {
                    Ok(value) => Load::Ready(value),
                    Err(error) => Load::Failed(error),
                },
            );
        });
    });
    let camera_id = body.id.clone();
    let match_camera = body.id.clone();
    let navigate = leptos_router::hooks::use_navigate();

    view! {
        <h1>{format!("{} {}", body.make, body.model)}</h1>
        <p class="lede">"Explore its built-in color, then find settings for the looks you love."</p>
        <h2>"Built-in looks"</h2>
        {move || match gallery.get() { Load::Ready((index,scene)) => {
            let items=super::looks::cards(&index);
            if items.is_empty() {
                crate::prefetch::route_rendered();
                return view! {
                    <p class="muted">"No looks are published yet. Look previews and settings for this camera appear here once looks are published."</p>
                    <p class="field-row">
                        <a href=crate::base::site_url("/contribute")>"Share photos from this camera"</a>
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
                <div class="look-gallery">
                    {built_in.into_iter().map(|item| view! {
                        <super::looks::CharacterCard item scene=scene.clone()/>
                    }).collect_view()}
                </div>
                <h2>"Get any look on this camera"</h2>
                <ui::LookPicker items scene on_select/>
                <p>"These simulated previews use the shared measurements available so far. More photos help us check agreement across cameras."</p>
            }.into_any()
        },Load::Failed(error)=>ui::failure(&error).into_any(),_=>ui::loading().into_any() }}
        <h2>"Ways to apply looks"</h2><p>"Start with in-camera settings, or use an editing app."</p><a href=crate::base::site_url(&format!("/match?camera={}",body.id))>"Find settings and downloads →"</a>
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
                None => view! { <p class="muted">"No published input transform."</p> }.into_any(),
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
