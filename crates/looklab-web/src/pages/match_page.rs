// SPDX-License-Identifier: AGPL-3.0-or-later
//! Match flow and pinned permalinks ([docs/04 §3-§4](../../../docs/04-matching-and-export.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use looklab_match::Permalink;
use looklab_wasm::{MatchInputs, MatchReply, decode_match_reply, encode_match_request};

use crate::data::{self, Load};
use crate::match_data;
use crate::ui;
use crate::webauthn::encode_base64url;
use crate::worker::MatchWorker;

#[component]
pub fn MatchPage() -> impl IntoView {
    let state = RwSignal::new(Load::<looklab_publish::SiteIndex>::Loading);
    let selected = RwSignal::new(String::new());
    let epoch = RwSignal::new(1u32);
    let emphasis = RwSignal::new("balanced".to_owned());
    let running = RwSignal::new(false);
    let reply = RwSignal::new(None::<MatchReply>);
    let error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        spawn_local(async move {
            match data::index().await {
                Ok(index) => {
                    let first = index
                        .looks
                        .iter()
                        .find(|look| look.slug.as_str() == "synthetic-match")
                        .or_else(|| index.looks.first())
                        .map(|look| look.slug.to_string())
                        .unwrap_or_default();
                    selected.set(first);
                    state.set(Load::Ready(index));
                }
                Err(error) => state.set(Load::Failed(error)),
            }
        });
    });

    let run = move |_| {
        let slug = selected.get();
        if slug.is_empty() || running.get() {
            return;
        }
        running.set(true);
        reply.set(None);
        error.set(None);
        let emphasis_value = emphasis.get();
        let epoch_value = epoch.get();
        spawn_local(async move {
            let result = async {
                let index = data::index().await?;
                let version = index
                    .looks
                    .iter()
                    .find(|look| look.slug.as_str() == slug)
                    .and_then(|look| look.latest())
                    .map(|entry| entry.version.to_string())
                    .ok_or_else(|| format!("{slug} has no published version"))?;
                let loaded = data::version(&slug, &version).await?;
                let inputs = match_data::inputs_from_version(
                    &loaded,
                    &emphasis_value,
                    epoch_value,
                    0x5eed_1234,
                )?;
                match_worker(&inputs).await
            }
            .await;
            match result {
                Ok(value) => reply.set(Some(value)),
                Err(message) => error.set(Some(message)),
            }
            running.set(false);
        });
    };

    view! {
        <h1>"Match"</h1>
        <p class="lede">
            "The optimizer runs in a worker on published, hash-checked artifacts. Nothing is uploaded."
        </p>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(index) => view! {
                <div class="panel">
                    <label for="look">"Target look"</label>
                    <select id="look" on:change=move |event| selected.set(event_target_value(&event))>
                        {index.looks.into_iter().map(|look| {
                            let slug = look.slug.to_string();
                            let value = slug.clone();
                            let chosen = slug == selected.get_untracked();
                            view! { <option value=value selected=chosen>{look.name}</option> }
                        }).collect_view()}
                    </select>
                    <label for="emphasis">"Emphasis"</label>
                    <select id="emphasis" on:change=move |event| emphasis.set(event_target_value(&event))>
                        <option value="balanced" selected=true>"Balanced"</option>
                        <option value="skin">"Skin"</option>
                        <option value="landscape">"Landscape"</option>
                        <option value="product">"Product"</option>
                    </select>
                    <label for="epoch">"Epoch"</label>
                    <input id="epoch" type="number" min="1" prop:value=move || epoch.get().to_string()
                        on:input=move |event| epoch.set(event_target_value(&event).parse().unwrap_or(1))/>
                    <button on:click=run disabled=move || running.get()>"Compute settings"</button>
                </div>
            }.into_any(),
        }}
        {move || running.get().then(|| view! { <p role="status">"Optimizing in a worker…"</p> })}
        {move || error.get().map(|message| view! { <div class="error" role="alert">{message}</div> })}
        {move || reply.get().map(|reply| view! { <MatchResult reply/> })}
    }
}

#[component]
fn MatchResult(reply: MatchReply) -> impl IntoView {
    let permalink = reply.permalink.clone();
    let encoded = encode_base64url(permalink.as_bytes());
    view! {
        <h2>"Settings card"</h2>
        <Card card=reply.card.clone()/>
        <p>
            <a href=format!("/match/r/{encoded}")>"Permalink"</a>
            " (pins every model version)"
        </p>
        <details>
            <summary>"Canonical permalink"</summary>
            <pre class="json">{permalink}</pre>
        </details>
        <h3>"Suggestions"</h3>
        <table class="data-table">
            <thead>
                <tr><th scope="col">#</th><th scope="col">Sim</th><th scope="col">Median ΔE</th><th scope="col">p95 ΔE</th><th scope="col">Band</th></tr>
            </thead>
            <tbody>
                {reply.results.into_iter().enumerate().map(|(index, suggestion)| view! {
                    <tr>
                        <th scope="row">{index + 1}</th>
                        <td>{suggestion.sim.to_string()}</td>
                        <td>{format!("{:.2}", suggestion.median_delta_e)}</td>
                        <td>{format!("{:.2}", suggestion.p95_delta_e)}</td>
                        <td>{format!("±{:.2}", suggestion.sigma_band)}</td>
                    </tr>
                }).collect_view()}
            </tbody>
        </table>
        <p class="muted">
            {format!("{} sim(s) considered, {} probes supported, {} evaluations.", reply.stats.sims_considered, reply.stats.supported_probes, reply.stats.exact_evaluations)}
        </p>
        <p><a href=format!("/verify?permalink={encoded}")>"Run one verification iteration"</a></p>
    }
}

#[component]
pub fn Card(card: looklab_wasm::dto::CardDto) -> impl IntoView {
    view! {
        <div class="panel">
            <h3>{format!("Base sim: {}", card.sim)}</h3>
            <dl class="facts">
                {card.entries.into_iter().map(|entry| view! {
                    <dt>{entry.key.to_string()}</dt>
                    <dd>
                        {entry.value}
                        {entry.changed.then(|| view! { <span class="muted">{format!(" (default {})", entry.default_value)}</span> })}
                    </dd>
                }).collect_view()}
            </dl>
            {card.exposure_note.map(|note| view! { <p>{note}</p> })}
            <p>
                {format!("Predicted ΔE: median {:.2}, p95 {:.2}", card.median_delta_e, card.p95_delta_e)}
                {card.sigma_band.map(|band| view! { <span>{format!(", band ±{band:.2}")}</span> })}
            </p>
            <p>{card.residual_summary}</p>
            {(!card.unreachable.is_empty()).then(|| view! {
                <div>
                    <h4>"Unreachable regions"</h4>
                    <ul>
                        {card.unreachable.into_iter().map(|region| view! {
                            <li>{format!("{} ({}): {:.1} ΔE over {} probes", region.region, region.layer, region.remaining_delta_e, region.probes)}</li>
                        }).collect_view()}
                    </ul>
                </div>
            })}
            <p class="muted">
                {format!("Confidence: {} — {}", card.confidence.as_str(), card.confidence_reason)}
            </p>
        </div>
    }
}

#[component]
pub fn MatchResultPage() -> impl IntoView {
    let params = use_params_map();
    let running = RwSignal::new(true);
    let reply = RwSignal::new(None::<MatchReply>);
    let error = RwSignal::new(None::<String>);
    let reproduced = RwSignal::new(false);

    Effect::new(move |_| {
        let raw = params.get().get("permalink").unwrap_or_default();
        spawn_local(async move {
            let Some(bytes) = crate::webauthn::decode_base64url(&raw) else {
                error.set(Some("the permalink is not base64url".to_owned()));
                running.set(false);
                return;
            };
            let Ok(text) = String::from_utf8(bytes) else {
                error.set(Some("the permalink is not UTF-8".to_owned()));
                running.set(false);
                return;
            };
            let result = async {
                let pinned = Permalink::parse(&text).map_err(|error| error.to_string())?;
                let inputs = match_data::inputs_from_permalink(&pinned).await?;
                let reply = match_worker(&inputs).await?;
                let same = reply.permalink == pinned.canonical();
                Ok::<_, String>((reply, same))
            }
            .await;
            match result {
                Ok((value, same)) => {
                    reproduced.set(same);
                    reply.set(Some(value));
                }
                Err(message) => error.set(Some(message)),
            }
            running.set(false);
        });
    });

    view! {
        <h1>"Match result"</h1>
        {move || running.get().then(|| ui::loading())}
        {move || error.get().map(|message| ui::failure(&message))}
        {move || reply.get().map(|reply| view! {
            <div>
                {move || reproduced.get().then(|| view! { <p class="pass">"Reproduced from the pinned versions."</p> })}
                <MatchResult reply/>
            </div>
        })}
    }
}

async fn match_worker(inputs: &MatchInputs) -> Result<MatchReply, String> {
    let request = encode_match_request(&looklab_wasm::MatchRequest {
        inputs: inputs.clone(),
    });
    let worker = MatchWorker::spawn()?;
    let reply = worker
        .run_match(&request)
        .await
        .map_err(|failure| failure.0)?;
    decode_match_reply(&reply)
}
