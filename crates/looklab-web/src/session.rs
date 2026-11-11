// SPDX-License-Identifier: AGPL-3.0-or-later
//! Browser profile and sign-in requests.

use crate::{
    idb,
    webauthn::{Session, SessionRole, SessionTier, StatusToken},
};
use leptos::prelude::*;
use wasm_bindgen::JsValue;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SessionState {
    #[default]
    SignedOut,
    SignedIn(Session),
}

#[derive(Clone, Copy)]
pub struct SessionContext {
    pub state: RwSignal<SessionState>,
    pub dialog_open: RwSignal<bool>,
    /// False until the stored token has been checked, or an explicit sign-in or
    /// sign-out supersedes that check.
    pub ready: RwSignal<bool>,
    generation: RwSignal<u64>,
    waiters: StoredValue<Vec<js_sys::Function>, LocalStorage>,
}

pub fn provide_session() -> SessionContext {
    let context = SessionContext {
        state: RwSignal::new(SessionState::SignedOut),
        dialog_open: RwSignal::new(false),
        ready: RwSignal::new(false),
        generation: RwSignal::new(0),
        waiters: StoredValue::new_local(Vec::new()),
    };
    provide_context(context);
    Effect::new(move |_| {
        if !context.dialog_open.get() {
            let signed_in = matches!(context.state.get_untracked(), SessionState::SignedIn(_));
            context.waiters.update_value(|waiters| {
                for resolve in waiters.drain(..) {
                    let _ = resolve.call1(&JsValue::NULL, &JsValue::from_bool(signed_in));
                }
            });
        }
    });
    let generation = context.generation.get_untracked();
    leptos::task::spawn_local(async move {
        let token = idb::get(idb::StorageKey::STATUS_TOKEN)
            .await
            .ok()
            .flatten()
            .and_then(|token| StatusToken::try_from(token).ok());
        if let Some(token) = token
            && let Ok(session) = crate::webauthn::restore_session(&token).await
            && context.generation.get_untracked() == generation
        {
            context.state.set(SessionState::SignedIn(session));
        }
        context.ready.set(true);
    });
    context
}

pub fn use_session() -> SessionContext {
    expect_context()
}

impl SessionContext {
    pub fn can_review(self) -> bool {
        matches!(self.state.get(), SessionState::SignedIn(session) if session.tier == SessionTier::Seeder || session.roles.iter().any(|role| matches!(role, SessionRole::Moderator | SessionRole::Admin)))
    }
    pub async fn set_signed_in(self, session: Session) -> Result<(), String> {
        self.supersede_restore();
        idb::put(idb::StorageKey::STATUS_TOKEN, session.status_token.as_str()).await?;
        self.apply_signed_in(session);
        Ok(())
    }

    pub async fn set_status_link(self, session: Session) -> Result<(), String> {
        if matches!(self.state.get_untracked(), SessionState::SignedIn(existing) if existing.status_token == session.status_token)
        {
            return self.set_signed_in(session).await;
        }
        self.supersede_restore();
        idb::put_and_remove_atomically(
            idb::StorageKey::STATUS_TOKEN,
            session.status_token.as_str(),
            idb::StorageKey::CREDENTIAL,
        )
        .await?;
        self.apply_signed_in(session);
        Ok(())
    }

    fn supersede_restore(self) {
        self.generation.update(|generation| *generation += 1);
        self.ready.set(true);
    }

    fn apply_signed_in(self, session: Session) {
        crate::data::close_moderation_session();
        self.state.set(SessionState::SignedIn(session));
        self.dialog_open.set(false);
    }

    pub async fn sign_out(self) -> Result<(), String> {
        self.supersede_restore();
        crate::data::close_moderation_session();
        idb::remove(idb::StorageKey::STATUS_TOKEN).await?;
        idb::remove(idb::StorageKey::SESSION).await?;
        idb::remove(idb::StorageKey::CREDENTIAL).await?;
        self.state.set(SessionState::SignedOut);
        Ok(())
    }

    pub async fn require_profile(self) -> Option<Session> {
        if let SessionState::SignedIn(session) = self.state.get_untracked() {
            return Some(session);
        }
        let promise = js_sys::Promise::new(&mut |resolve, _| {
            self.waiters.update_value(|waiters| waiters.push(resolve));
        });
        self.dialog_open.set(true);
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        match self.state.get_untracked() {
            SessionState::SignedIn(session) => Some(session),
            SessionState::SignedOut => None,
        }
    }
}
