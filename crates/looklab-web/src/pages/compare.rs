// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_query_map};
use looklab_publish::SiteIndex;
use looklab_report::compare::{Comparison, ComparisonInput, FunctionName, compare};
use looklab_report::svg::{
    delta_e_histogram_svg, grl_svg, region_map_svg, tint_comparison_svg, tone_comparison_svg,
};

use crate::data::{self, Load};
use crate::ui;

#[derive(Clone, Debug)]
struct CompareReady {
    a_slug: String,
    b_slug: String,
    a_name: String,
    b_name: String,
    a_version: String,
    b_version: String,
    comparison: Comparison,
}

#[component]
pub fn ComparePage() -> impl IntoView {
    let query = use_query_map();
    let index = RwSignal::new(Load::<SiteIndex>::Loading);
    let state = RwSignal::new(Load::<CompareReady>::Loading);
    let choice_a = RwSignal::new(String::new());
    let choice_b = RwSignal::new(String::new());
    let generation = RwSignal::new(0_u32);

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            index.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    Effect::new(move |_| {
        let a = query.read().get("a").unwrap_or_default();
        let b = query.read().get("b").unwrap_or_default();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            let loaded = load_pair(a, b).await;
            if generation.get_untracked() != mine {
                return;
            }
            match loaded {
                Ok(ready) => {
                    choice_a.set(ready.a_slug.clone());
                    choice_b.set(ready.b_slug.clone());
                    state.set(Load::Ready(ready));
                }
                Err(error) => state.set(Load::Failed(error)),
            }
        });
    });

    let navigate = use_navigate();
    let go = Callback::new(move |()| {
        let a = choice_a.get();
        let b = choice_b.get();
        navigate(&format!("/compare?a={a}&b={b}"), Default::default());
    });

    view! {
        <h1>"Compare"</h1>
        <p>"Diagnostics, ΔE distributions, a region map and a one-line summary, computed in the browser from two manifest-checked artifacts."</p>
        {move || match index.get() {
            Load::Ready(index) => view! {
                <div class="picker-grid">
                    <section class="panel">
                        <label for="compare-a">"Look A"</label>
                        <select id="compare-a" prop:value=move || choice_a.get() on:change=move |event| choice_a.set(event_target_value(&event))>
                            {index.looks.iter().map(|look| {
                                view! { <option value=look.slug.to_string()>{look.name.clone()}</option> }
                            }).collect_view()}
                        </select>
                    </section>
                    <section class="panel">
                        <label for="compare-b">"Look B"</label>
                        <select id="compare-b" prop:value=move || choice_b.get() on:change=move |event| choice_b.set(event_target_value(&event))>
                            {index.looks.iter().map(|look| {
                                view! { <option value=look.slug.to_string()>{look.name.clone()}</option> }
                            }).collect_view()}
                        </select>
                    </section>
                    <section class="panel">
                        <button class="button" on:click=move |_| go.run(())>"Compare"</button>
                    </section>
                </div>
            }.into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Loading => ui::loading().into_any(),
        }}
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(ready) => view! { <ComparisonView ready/> }.into_any(),
        }}
    }
}

async fn load_pair(a: String, b: String) -> Result<CompareReady, String> {
    let index = data::index().await?;
    let a_slug = if a.is_empty() {
        index
            .looks
            .first()
            .map(|look| look.slug.to_string())
            .unwrap_or_default()
    } else {
        a
    };
    let b_slug = if b.is_empty() {
        index
            .looks
            .get(1)
            .or_else(|| index.looks.first())
            .map(|look| look.slug.to_string())
            .unwrap_or_default()
    } else {
        b
    };
    if a_slug.is_empty() || b_slug.is_empty() {
        return Err("the site index has fewer than two looks".to_owned());
    }
    let a_page = data::look(&a_slug).await?;
    let b_page = data::look(&b_slug).await?;
    let a_version = a_page
        .latest()
        .map(|version| version.version().to_string())
        .ok_or_else(|| format!("{a_slug} has no published version"))?;
    let b_version = b_page
        .latest()
        .map(|version| version.version().to_string())
        .ok_or_else(|| format!("{b_slug} has no published version"))?;
    let a_loaded = data::version(&a_slug, &a_version).await?;
    let b_loaded = data::version(&b_slug, &b_version).await?;
    let probes = looklab_model::probes::v1();
    let a_look = crate::diagnostics::DocumentLook::new(&a_loaded.document, &probes);
    let b_look = crate::diagnostics::DocumentLook::new(&b_loaded.document, &probes);
    let a_name = FunctionName::new(a_page.name.clone());
    let b_name = FunctionName::new(b_page.name.clone());
    let comparison = compare(&ComparisonInput {
        a: &a_look,
        b: &b_look,
        probes: &probes,
        a_name: &a_name,
        b_name: &b_name,
    });
    Ok(CompareReady {
        a_slug,
        b_slug,
        a_name: a_page.name,
        b_name: b_page.name,
        a_version,
        b_version,
        comparison,
    })
}

#[component]
fn ComparisonView(ready: CompareReady) -> impl IntoView {
    let comparison = &ready.comparison;
    let summary = comparison.summary.clone();
    let tone = tone_comparison_svg(&comparison.tone);
    let tint = tint_comparison_svg(&comparison.tint);
    let grl = comparison.grl.as_ref().map(grl_svg);
    let histogram = delta_e_histogram_svg(&comparison.delta_e);
    let ciede = delta_e_histogram_svg(&comparison.ciede2000);
    let region = region_map_svg(&comparison.region_map);
    let attribution = comparison
        .attribution
        .iter()
        .map(|row| {
            view! {
                <tr>
                    <th scope="row">{row.layer.name()}</th>
                    <td>{format!("{:.3}", row.contribution)}</td>
                </tr>
            }
        })
        .collect_view();

    view! {
        <article class="comparison">
            <h2>{format!("{} {} vs {} {}", ready.a_name, ready.a_version, ready.b_name, ready.b_version)}</h2>
            <p class="summary">{summary}</p>
            <table class="data-table">
                <caption>"ΔE summary"</caption>
                <thead>
                    <tr>
                        <th scope="col">"Metric"</th>
                        <th scope="col">"Median"</th>
                        <th scope="col">"p95"</th>
                        <th scope="col">"Max"</th>
                        <th scope="col">"Weighted mean"</th>
                        <th scope="col">"Probes"</th>
                    </tr>
                </thead>
                <tbody>
                    <tr>
                        <th scope="row">"CAM16-UCS ΔE"</th>
                        <td>{format!("{:.3}", comparison.delta_e.median)}</td>
                        <td>{format!("{:.3}", comparison.delta_e.p95)}</td>
                        <td>{format!("{:.3}", comparison.delta_e.max)}</td>
                        <td>{format!("{:.3}", comparison.delta_e.weighted_mean)}</td>
                        <td>{comparison.delta_e.count}</td>
                    </tr>
                    <tr>
                        <th scope="row">"CIEDE2000"</th>
                        <td>{format!("{:.3}", comparison.ciede2000.median)}</td>
                        <td>{format!("{:.3}", comparison.ciede2000.p95)}</td>
                        <td>{format!("{:.3}", comparison.ciede2000.max)}</td>
                        <td>{format!("{:.3}", comparison.ciede2000.weighted_mean)}</td>
                        <td>{comparison.ciede2000.count}</td>
                    </tr>
                </tbody>
            </table>
            <ui::Plot title="Tone comparison".to_owned() svg=tone/>
            <ui::Plot title="Neutral tint comparison".to_owned() svg=tint/>
            {grl.map(|svg| view! { <ui::Plot title="Chroma, hue and lightness layers".to_owned() svg=svg/> })}
            <h3>"ΔE distribution"</h3>
            <ui::Plot title="CAM16-UCS ΔE histogram".to_owned() svg=histogram/>
            <ui::Plot title="CIEDE2000 histogram".to_owned() svg=ciede/>
            <h3>"Region map"</h3>
            <ui::Plot title="ΔE over hue and scene exposure".to_owned() svg=region/>
            <table class="data-table">
                <caption>"Layer attribution"</caption>
                <thead>
                    <tr><th scope="col">"Layer"</th><th scope="col">"Mean ΔE reduction"</th></tr>
                </thead>
                <tbody>{attribution}</tbody>
            </table>
        </article>
    }
}
