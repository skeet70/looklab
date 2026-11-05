// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_trust::RecipeLeaderboard;

#[component]
pub(crate) fn RecipeSection(
    #[prop(optional)] look: Option<looklab_model::file::Slug>,
    #[prop(optional)] camera: Option<looklab_schema::BodyId>,
) -> impl IntoView {
    let state = RwSignal::new(Load::<Vec<RecipeLeaderboard>>::Loading);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::recipes().await {
                Ok(boards) => Load::Ready(boards),
                Err(error) => Load::Failed(error),
            });
        });
    });
    view! {
        <section class="recipes">
            <h2>"Recipes others use"</h2>
            {move || match state.get() {
        Load::Loading => ui::loading().into_any(),
        Load::Failed(_) => view! { <p class="muted">"Shared recipes are not available right now. You can still find settings with Get the look."</p> }.into_any(),
        Load::Ready(boards) => {
            let boards = boards.into_iter().filter(|board| look.as_ref().is_none_or(|look| board.look.as_str() == look.as_str()) && camera.as_ref().is_none_or(|camera| &board.body == camera)).collect::<Vec<_>>();
            if boards.is_empty() { return view! { <p>"No verified recipes yet. Check a match with your shots to help others find settings."</p><a href=crate::base::site_url("/match")>"Find settings →"</a> }.into_any(); }
            view! {
                <div class="recipe-grid">
                    {boards.into_iter().map(|board| view! {
                        <article class="panel">
                            <a href=crate::base::site_url(&format!("/match?camera={}&look={}", board.body, board.look))>
                                "Try these settings on this camera →"
                            </a>
                            {board.entries.into_iter().map(|score| {
                let measured = score.measured_de.and_then(|value| ui::DeltaE::new(value).map(|difference| {
                    let label = ui::closeness_level(difference).label();
                    let spread = score.measured_sigma.map(|sigma| format!("; standard deviation {sigma:.2}")).unwrap_or_else(|| "; uncertainty not reported".to_owned());
                    (format!("{label} (early estimate)"), format!("Measured difference {value:.2}{spread}"))
                }));
                let settings = score.recipe.settings.entries().iter()
                    .map(|entry| format!("{} {}", entry.key, entry.step.get()))
                    .collect::<Vec<_>>().join(", ");
                view! {
                    <div class="recipe">
                        <p>{format!("{} · {settings}", score.recipe.base_sim)}</p>
                        {measured.map(|(label, details)| view! {
                            <span title=details tabindex="0">{label}</span>
                        })}
                        <p>{format!("Checked with {} photos", score.n_verifications)}</p>
                    </div>
                }
                            }).collect_view()}
                        </article>
                    }).collect_view()}
                </div>
            }.into_any()
        }
            }}
        </section>
    }
}
