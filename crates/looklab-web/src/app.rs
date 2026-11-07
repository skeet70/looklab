// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application shell and routes (docs/08 §1).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::hooks::use_location;
use leptos_router::path;
use wasm_bindgen::JsCast;

use crate::pages::*;
use crate::{base, ui, webauthn};

#[wasm_bindgen::prelude::wasm_bindgen(
    inline_js = "export async function looklabPlatformPasskey() { return !!globalThis.PublicKeyCredential && await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable(); }"
)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = looklabPlatformPasskey)]
    async fn platform_passkey() -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
}
#[component]
pub fn App() -> impl IntoView {
    crate::session::provide_session();
    view! {
        <Router base=base::router_base()>
            <Shell/>
        </Router>
    }
}

#[component]
fn Shell() -> impl IntoView {
    let location = use_location();
    let chrome = move || !base::page_path(&location.pathname.get()).starts_with("/pattern/");
    let preview = !base::server_available();
    let notice = RwSignal::new(preview);
    let menu = RwSignal::new(false);
    let session = crate::session::use_session();
    let sign_in = session.dialog_open;
    let account_error = RwSignal::new(None::<String>);
    view! {
        <a class="skip-link" href="#main" hidden=move || !chrome() on:click=move |_| {
            if let Some(main) = document().get_element_by_id("main")
                .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = main.focus();
            }
        }>"Skip to content"</a>
        <div class="preview-banner" role="note" hidden=move || !chrome() || !notice.get()>
            <span>"Demo with sample data. Sign-in and sharing are turned off."</span>
            <button aria-label="Dismiss demo notice" on:click=move |_| notice.set(false)>"Dismiss"</button>
        </div>
        <header class="site-header" hidden=move || !chrome()>
            <A href=base::site_url("/") attr:class="brand">"looklab"</A>
            <button class="menu-toggle" aria-controls="main-navigation" aria-expanded=move || menu.get().to_string() on:click=move |_| menu.update(|open| *open = !*open)>"Menu"</button>
            <nav id="main-navigation" aria-label="Main" class:menu-open=move || menu.get() on:click=move |_| menu.set(false)>
                <A href=base::site_url("/looks")>"Looks"</A>
                <A href=base::site_url("/compare")>"Compare"</A>
                <A href=base::site_url("/match")>"Get the look"</A>
                <A href=base::site_url("/contribute")>"Contribute"</A>
            </nav>
            {(!preview).then(|| view! {
                {move || match session.state.get() {
                    crate::session::SessionState::SignedOut => view! {
                        <button class="header-account" on:click=move |_| {
                            account_error.set(None);
                            sign_in.set(true);
                        }>"Sign in"</button>
                    }.into_any(),
                    crate::session::SessionState::SignedIn(profile) => view! {
                        <AccountMenu profile session account_error/>
                    }.into_any(),
                }}
            })}
        </header>
        <SignInDialog open=sign_in/>
        {move || account_error.get().map(|error| view! {
            <div class="notice" role="alert">
                <p>{error}</p>
                <button on:click=move |_| account_error.set(None)>"Dismiss"</button>
            </div>
        })}
        <main id="main" tabindex="-1">
            <Routes fallback=|| view! {
                <NotFound/>
            }>
                <Route path=path!("/") view=Landing/>
                <Route path=path!("/cameras") view=BodiesPage/>
                <Route path=path!("/cameras/:make/:model") view=BodyDetail/>
                <Route path=path!("/looks") view=LooksPage/>
                <Route path=path!("/looks/:look") view=LookDetail/>
                <Route path=path!("/compare") view=ComparePage/>
                <Route path=path!("/match") view=MatchPage/>
                <Route path=path!("/match/r/:permalink") view=MatchResultPage/>
                <Route path=path!("/contribute") view=ContributePage/>
                <Route path=path!("/contribute/wanted") view=BountiesPage/>
                <Route path=path!("/me") view=MePage/>
                <Route path=path!("/status/:token") view=|| view! {
                    <StatusPage/>
                }/>
                <Route path=path!("/help/passkeys") view=PasskeysPage/>
                <Route path=path!("/pattern/:seed") view=PatternPage/>
                <Route path=path!("/community") view=CommunityPage/>
                <Route path=path!("/community/log") view=LogPage/>
                <Route path=path!("/community/propose") view=ProposePage/>
                <Route path=path!("/proposals/:id") view=ProposalDetail/>
                <Route path=path!("/review") view=ModQueuePage/>
                <Route path=path!("/developers") view=DevelopersPage/>
                <Route path=path!("/developers/pipelines") view=PipelinesPage/>
                <Route path=path!("/about/how-it-works") view=HowItWorksPage/>
                <Route path=path!("/about/privacy") view=PrivacyPage/>
            </Routes>
        </main>
        <footer class="site-footer" hidden=move || !chrome()>
            <nav aria-label="Footer">
                <A href=base::site_url("/cameras")>"Cameras"</A>
                <A href=base::site_url("/developers")>"For developers"</A>
                <A href=base::site_url("/community")>"Community"</A>
                <A href=base::site_url("/about/how-it-works")>"How it works"</A>
                <A href=base::site_url("/about/privacy")>"Privacy"</A>
                <a href="https://github.com/skeet70/looklab">"Source (AGPL-3.0-or-later)"</a>
            </nav>
        </footer>
    }
}

#[component]
fn AccountMenu(
    profile: webauthn::Session,
    session: crate::session::SessionContext,
    account_error: RwSignal<Option<String>>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let trigger = NodeRef::<leptos::html::Summary>::new();
    let name = profile
        .display_handle
        .as_ref()
        .map(|name| name.as_str().to_owned())
        .unwrap_or_else(|| "Your profile".to_owned());
    let label = name.clone();
    let title = name.clone();
    let initial = profile
        .display_handle
        .as_ref()
        .and_then(|name| name.as_str().chars().find(|letter| !letter.is_whitespace()));
    view! {
        <details class="account-menu" open=move || open.get() on:keydown=move |event| {
            if event.key() == "Escape" {
                event.prevent_default();
                open.set(false);
                if let Some(trigger) = trigger.get() { let _ = trigger.focus(); }
            }
        }>
            <summary node_ref=trigger role="button" aria-label=label title=title
                aria-controls="account-menu-popup"
                aria-expanded=move || if open.get() { "true" } else { "false" }
                on:click=move |event| {
                    event.prevent_default();
                    open.update(|open| *open = !*open);
                }>
                <span class="account-avatar" aria-hidden="true">
                    {initial.map(|letter| view! {
                        <span>{letter.to_string()}</span>
                    }.into_any()).unwrap_or_else(|| view! {
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" focusable="false">
                            <circle cx="12" cy="8" r="3"/>
                            <path d="M5 20v-2a7 7 0 0 1 14 0v2"/>
                        </svg>
                    }.into_any())}
                </span>
                <span class="account-menu-name">{name}</span>
                <svg class="account-chevron" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true" focusable="false">
                    <path d="m4 6 4 4 4-4"/>
                </svg>
            </summary>
            <div class="account-menu-items" id="account-menu-popup" on:click=move |_| open.set(false)>
                <A href=base::site_url("/me")>"Your contributions"</A>
                {session.can_review().then(|| view! {
                    <A href=base::site_url("/review")>"Review"</A>
                })}
                <button on:click=move |_| {
                    account_error.set(None);
                    spawn_local(async move {
                        if session.sign_out().await.is_err() {
                            account_error.set(Some("Could not forget this browser's sign-in. Try again or clear this site's browser storage.".to_owned()));
                        }
                    });
                }>"Sign out"</button>
                <button on:click=move |_| {
                    account_error.set(None);
                    spawn_local(async move {
                        match webauthn::sign_out_everywhere().await {
                            Ok(()) => {
                                if session.sign_out().await.is_err() {
                                    account_error.set(Some("Other browsers are signed out, but this browser couldn't forget its sign-in. Try Sign out or clear this site's browser storage.".to_owned()));
                                }
                            }
                            Err(error) => account_error.set(Some(error.to_string())),
                        }
                    });
                }>"Sign out everywhere"</button>
            </div>
        </details>
    }
}

#[derive(Clone, Copy)]
enum SignInAction {
    Continue,
    Create,
}

#[component]
fn SignInDialog(open: RwSignal<bool>) -> impl IntoView {
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let no_platform_store = RwSignal::new(false);
    let session_context = crate::session::use_session();
    let display_name = RwSignal::new(String::new());
    let name_error = RwSignal::new(None::<String>);
    spawn_local(async move {
        no_platform_store.set(
            platform_passkey()
                .await
                .ok()
                .and_then(|value| value.as_bool())
                == Some(false),
        );
    });
    let authenticate = move |action: SignInAction| {
        name_error.set(None);
        let name = if matches!(action, SignInAction::Create) {
            let name = display_name.get_untracked();
            match (!name.trim().is_empty())
                .then(|| webauthn::DisplayHandle::try_from(name.trim().to_owned()))
                .transpose()
            {
                Ok(name) => name,
                Err(error) => {
                    name_error.set(Some(error));
                    return;
                }
            }
        } else {
            None
        };
        busy.set(true);
        message.set(None);
        spawn_local(async move {
            let result = async {
                let selected = if matches!(action, SignInAction::Create) {
                    Some(webauthn::create_profile(name).await?)
                } else {
                    None
                };
                let session = webauthn::sign_in(selected.as_ref()).await?;
                session_context
                    .set_signed_in(session)
                    .await
                    .map_err(|_| webauthn::SignInError::Storage)?;
                Ok::<(), webauthn::SignInError>(())
            }
            .await;
            match result {
                Ok(()) => {
                    open.set(false);
                }
                Err(error) => message.set(Some(error.to_string())),
            }
            busy.set(false);
        });
    };
    view! {
        <ui::Dialog title="Sign in to looklab".to_owned() open>
            <p class="muted" hidden=move || !no_platform_store.get()>"This computer can't store passkeys. Your browser will offer to use your phone (scan a QR code) or a security key."</p>
            <section class="profile-returning" aria-label="Sign in">
                <p>"No email or password. Your device keeps a passkey that proves your contributions are yours."</p>
                <button class="button-primary" disabled=move || busy.get() on:click=move |_| authenticate(SignInAction::Continue)>"Continue with passkey"</button>
            </section>
            <section class="profile-create" aria-labelledby="profile-create-heading">
                <h3 id="profile-create-heading">"New here?"</h3>
                <label class="profile-name">"Display name (optional)"
                    <input maxlength="64" autocomplete="nickname" aria-describedby="profile-name-hint profile-name-error" aria-invalid=move || name_error.get().is_some().to_string() prop:value=move || display_name.get() on:input=move |event| {
                        display_name.set(event_target_value(&event));
                        name_error.set(None);
                    }/>
                    <span class="muted" id="profile-name-hint">"Shown next to your public credits. Leave blank to stay anonymous."</span>
                </label>
                <p id="profile-name-error" class="error" role="alert" hidden=move || name_error.get().is_none()>{move || name_error.get().unwrap_or_default()}</p>
                <button disabled=move || busy.get() on:click=move |_| authenticate(SignInAction::Create)>"Create a profile"</button>
            </section>
            <a href=base::site_url("/help/passkeys")>"How this works"</a>
            {move || busy.get().then(|| view! {
                <p role="status">"Follow your browser's passkey prompt."</p>
            })}
            {move || message.get().map(|text| view! {
                <p role="alert">{text}</p>
            })}
        </ui::Dialog>
    }
}

#[component]
fn MePage() -> impl IntoView {
    let session = crate::session::use_session();
    view! {
        {move || match session.state.get() {
            crate::session::SessionState::SignedIn(profile) => view! {
                <StatusPage token_override=profile.status_token.as_str().to_owned()/>
                <crate::pages::match_page::SavedRecipes/>
            }.into_any(),
            crate::session::SessionState::SignedOut => view! {
                <section class="account-intro">
                    <h1>"Your contributions"</h1>
                    <p class="lede">"See which photos counted and how they helped build looks. Sign in with your profile, or open your private status link."</p>
                    <button hidden=!base::server_available() on:click=move |_| session.dialog_open.set(true)>"Sign in"</button>
                    <p>
                        <a href=base::site_url("/help/passkeys")>"How sign-in works"</a>
                    </p>
                </section>
            }.into_any(),
        }}
    }
}
