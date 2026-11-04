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
    Effect::new(move |_| {
        let make = params.read().get("make").unwrap_or_default();
        let model = params.read().get("model").unwrap_or_default();
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::body(&make, &model).await {
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

    view! {
        <h1>{format!("{} {}", body.make, body.model)}</h1>
        <p class="muted">{body.id.to_string()}</p>
        <dl class="facts">
            <dt>"Kind"</dt><dd>{format!("{:?}", body.kind).to_lowercase()}</dd>
            <dt>"Status"</dt><dd>{format!("{:?}", body.status).to_lowercase()}</dd>
            <dt>"Mapping status"</dt><dd>{format!("{:?}", body.mapping_status).to_lowercase().replace('_', " ")}</dd>
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
        <section aria-labelledby="realization">
            <h2 id="realization">"Realization notes"</h2>
            {if body.realization_notes.is_empty() {
                view! { <p class="muted">"No realization notes for this body."</p> }.into_any()
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
        <section aria-labelledby="schema">
            <h2 id="schema">"Control schema"</h2>
            <label for="epoch-select">"Epoch"</label>
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
                            {format!("Epoch {id} · firmware {}–{}", epoch.firmware_from, epoch.firmware_to)}
                        </option>
                    }
                }).collect_view()}
            </select>
            {move || selected().map(|epoch| view! { <ControlTable epoch/> })}
            <h3>"Differences between epochs"</h3>
            <EpochDifferences epochs=body.epochs.clone()/>
        </section>
        <section aria-labelledby="native-looks">
            <h2 id="native-looks">"Native looks"</h2>
            <ul class="card-list">
                {body.native_looks.iter().map(|card| {
                    let slug = card.look.to_string();
                    view! {
                        <li class="card">
                            <h3><a href=format!("/looks/{slug}")>{slug.clone()}</a></h3>
                            <p class="muted">{format!("Epoch {} · sim {}", card.epoch, card.sim_key)}</p>
                            <p>
                                {match &card.version {
                                    Some(version) => format!("Version {version}"),
                                    None => "No published version".to_owned(),
                                }}
                            </p>
                            {card.status.map(|status| view! { <ui::StatusBadge status/> })}
                        </li>
                    }
                }).collect_view()}
            </ul>
        </section>
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

#[component]
fn ControlTable(epoch: EpochPage) -> impl IntoView {
    view! {
        <h3>{format!("Epoch {} controls", epoch.id)}</h3>
        <table class="data-table">
            <caption>{format!("Controls in epoch {}", epoch.id)}</caption>
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
        return view! { <p class="muted">"No epochs."</p> }.into_any();
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
                None => rows.push(format!("Epoch {} adds {key}", epoch.id)),
                Some(before) if *before != *control => {
                    rows.push(format!("Epoch {} changes {key}", epoch.id));
                }
                Some(_) => {}
            }
        }
        for key in previous.keys() {
            if !current.contains_key(key) {
                rows.push(format!("Epoch {} removes {key}", epoch.id));
            }
        }
    }
    if rows.is_empty() {
        view! { <p class="muted">"All epochs share one control set."</p> }.into_any()
    } else {
        view! {
            <ul>{rows.into_iter().map(|row| view! { <li>{row}</li> }).collect_view()}</ul>
        }
        .into_any()
    }
}
