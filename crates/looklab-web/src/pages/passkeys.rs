// SPDX-License-Identifier: AGPL-3.0-or-later
//! Sign-in guidance.
use leptos::prelude::*;

#[component]
pub fn PasskeysPage() -> impl IntoView {
    let session = crate::session::use_session();
    view! {
        <article class="prose account-help">
            <p class="eyebrow">"Your profile"</p>
            <h1>"Sign in without a password"</h1>
            <p class="lede">"A profile keeps your contributions together. There's no email address or password to remember. Browsing, comparing and matching looks never needs a profile."</p>
            <h2>"How it works"</h2>
            <p>"Choose Create a profile in the sign-in window. Your browser helps you save a passkey. Next time, choose Continue with passkey and unlock it with your device's screen lock, fingerprint or face recognition. Your optional display name appears beside public credits."</p>
            <button hidden=!crate::base::server_available() on:click=move |_| session.dialog_open.set(true)>"Sign in or create a profile"</button>
            <h2>"Use your phone"</h2>
            <p>"At your browser's prompt, choose to use another device. Scan the QR code with your phone and approve the request there. Keep Bluetooth enabled and your phone nearby. The passkey stays on your phone."</p>
            <h2>"Security keys and password managers"</h2>
            <p>"A USB or NFC security key can hold your passkey. Choose the security-key option in the browser prompt, then touch the key when asked. Hardware-backed passkeys or security keys are recommended for community moderators."</p>
            <p>"A password manager that supports passkeys can save and sync yours across devices. Follow your manager's browser instructions when creating your profile."</p>
            <h2>"On Linux"</h2>
            <p>"Many Linux desktops have no built-in passkey store. This doesn't stop you signing in. Use the phone QR option, a security key, or a passkey-capable password manager. Your browser controls which options are offered."</p>
            <h2>"If a prompt doesn't finish"</h2>
            <p>"Try Continue with passkey again and choose the device where you saved it. A cancelled prompt doesn't create a profile. Create a profile only if you want a new, separate contribution history."</p>
            <h2>"What's stored"</h2>
            <p>"Your device or password manager keeps the private part of your passkey. looklab stores the public part to check that a request came from you. Your fingerprint, face scan and device unlock code are never sent to looklab."</p>
            <p>"This browser remembers your sign-in in its local storage. Sign out removes it here without affecting your other browsers. Sign out everywhere asks for your passkey and ends all of your profile's sessions."</p>
            <p>"Your private status link opens your contributions without a passkey. Bookmark it somewhere private. Anyone with that link can read your contributions. Signing in on another device creates another link, without breaking the old one."</p>
            <p>
                <a href=crate::base::site_url("/about/privacy")>"Read about photo privacy"</a>
            </p>
        </article>
    }
}
