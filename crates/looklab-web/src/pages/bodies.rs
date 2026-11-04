// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_publish::SiteIndex;

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn BodiesPage() -> impl IntoView {
    let state = RwSignal::new(Load::<SiteIndex>::Loading);
    let filter = RwSignal::new(String::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <h1>"Bodies"</h1>
        <p>"Snapshots exported from the server authority. Mapping status and capabilities come from the data files."</p>
        <label for="body-filter">"Filter by make or model"</label>
        <input
            id="body-filter"
            type="search"
            placeholder="Fujifilm"
            on:input=move |event| filter.set(event_target_value(&event))
        />
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(index) => {
                let needle = filter.get().to_lowercase();
                let rows = index
                    .bodies
                    .into_iter()
                    .filter(|body| {
                        needle.is_empty()
                            || body.make.to_lowercase().contains(&needle)
                            || body.model.to_lowercase().contains(&needle)
                    })
                    .map(|body| {
                        view! {
                            <tr>
                                <th scope="row">
                                    <a href=format!("/bodies/{}", body.id)>
                                        {format!("{} {}", body.make, body.model)}
                                    </a>
                                </th>
                                <td>{format!("{:?}", body.kind).to_lowercase()}</td>
                                <td>{format!("{:?}", body.mapping_status).to_lowercase().replace('_', " ")}</td>
                                <td>{body.epochs}</td>
                                <td>{body.looks.len()}</td>
                            </tr>
                        }
                    })
                    .collect_view();
                view! {
                    <table class="data-table">
                        <caption>"Bodies in this snapshot"</caption>
                        <thead>
                            <tr>
                                <th scope="col">"Body"</th>
                                <th scope="col">"Kind"</th>
                                <th scope="col">"Mapping"</th>
                                <th scope="col">"Epochs"</th>
                                <th scope="col">"Looks"</th>
                            </tr>
                        </thead>
                        <tbody>{rows}</tbody>
                    </table>
                }
                .into_any()
            }
        }}
    }
}
