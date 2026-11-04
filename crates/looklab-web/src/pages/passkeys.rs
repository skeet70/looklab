// SPDX-License-Identifier: AGPL-3.0-or-later
//! Passkey guidance and status-link recovery ([docs/07](../../../docs/07-contribution.md), [docs/08 §1](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::idb;
use crate::webauthn;

#[component]
pub fn PasskeysPage() -> impl IntoView {
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let link = RwSignal::new(None::<String>);

    let recover = move |_| {
        busy.set(true);
        message.set(None);
        link.set(None);
        spawn_local(async move {
            match webauthn::reissue_status_token(None).await {
                Ok(token) => {
                    let _ = idb::put(idb::StorageKey::STATUS_TOKEN, &token).await;
                    link.set(Some(format!("/status/{token}")));
                    message.set(Some("A new status link was issued; the old one is revoked.".to_owned()));
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    view! {
        <h1>"Passkeys"</h1>
        <p class="lede">
            "Contributing uses a passkey instead of an account. Browsing, matching and verification need none."
        </p>
        <section class="panel">
            <h2>"Options"</h2>
            <dl class="facts">
                <dt>"This device"</dt>
                <dd>"Face, fingerprint or screen lock, stored by your platform. The simplest option."</dd>
                <dt>"Phone via QR"</dt>
                <dd>
                    "On a desktop without a platform passkey store, choose the cross-device option and scan the QR code with your phone. "
                    "The passkey stays on the phone."
                </dd>
                <dt>"Security key"</dt>
                <dd>"A USB or NFC key works everywhere. Recommended for seeders, moderators and admins."</dd>
                <dt>"Password manager"</dt>
                <dd>"Managers that store passkeys sync them across devices. Check that yours supports WebAuthn."</dd>
            </dl>
            <p>
                "Linux desktops often have no built-in passkey store. Use a phone through the QR flow or a security key; "
                "the browser offers both at the passkey prompt."
            </p>
        </section>
        <section class="panel">
            <h2>"Lost your status link?"</h2>
            <p>
                "A passkey assertion from the same credential re-issues the link and revokes the old token. "
                "Your browser may offer any passkey for this site, including a discoverable one."
            </p>
            <button on:click=recover disabled=move || busy.get()>"Re-issue my status link"</button>
            {move || busy.get().then(|| view! { <p role="status">"Waiting for the passkey…"</p> })}
            {move || message.get().map(|text| view! { <p class="notice">{text}</p> })}
            {move || link.get().map(|href| view! {
                <p>"New link: " <a href=href.clone()>{href.clone()}</a></p>
            })}
        </section>
        <section class="panel">
            <h2>"What a passkey does here"</h2>
            <ul>
                <li>"It signs one batch manifest per contribution, in one gesture."</li>
                <li>"The public key verifies uploads; it is never published."</li>
                <li>"The status token is separate from the passkey and can be revoked without losing the credential."</li>
            </ul>
        </section>
    }
}
