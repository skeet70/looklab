// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tier transitions, scopes, endorsements, nominations and audit events.

use std::collections::BTreeSet;

use looklab_trust::{
    Actor, AuditEvent, CredentialId, CredentialState, Endorsement, EvidenceRef, Grant,
    ReputationUpdate, Role, Scope, Standing, Tier, TrustError, TrustParams, TrustState,
};
use looklab_upload::{BodyId, LookSlug};

fn body(value: &str) -> BodyId {
    BodyId::new(value).expect("valid body id")
}

fn look(value: &str) -> LookSlug {
    LookSlug::new(value).expect("valid look slug")
}

fn scope(bodies: &[&str], looks: &[&str]) -> Scope {
    Scope::from_legacy(
        bodies.iter().map(|value| body(value)),
        looks.iter().map(|value| look(value)),
    )
}

fn endorsement(scope: Scope) -> Endorsement {
    Endorsement {
        scope,
        reason: "reviewed chart session".to_owned(),
        evidence: vec![EvidenceRef::new("chart-session-1")],
    }
}

fn state_with_new(ids: &[u128]) -> TrustState {
    let mut state = TrustState::new();
    for id in ids {
        let _ = state.register(CredentialId::from_u128(*id), &TrustParams::default());
    }
    state
}

fn make_seeder(state: &mut TrustState, id: u128) {
    let (next, event) = state
        .apply_grant(
            Actor::Cli,
            CredentialId::from_u128(id),
            Grant::Seeder,
            &TrustParams::default(),
        )
        .expect("cli may grant");
    assert!(matches!(event, AuditEvent::GrantCreated { .. }));
    *state = next;
}

#[test]
fn registration_starts_new_and_neutral() {
    let state = state_with_new(&[1]);
    let record = state
        .credential(CredentialId::from_u128(1))
        .expect("registered");
    assert_eq!(record.tier, Tier::New);
    assert!((record.reputation.value() - 0.5).abs() < 1e-15);
    assert!(record.roles.is_empty());
}

#[test]
fn cli_grants_seeder_with_the_starting_reputation() {
    let state = state_with_new(&[1]);
    let (next, event) = state
        .apply_grant(
            Actor::Cli,
            CredentialId::from_u128(1),
            Grant::Seeder,
            &TrustParams::default(),
        )
        .expect("cli may grant");
    assert_eq!(
        event,
        AuditEvent::GrantCreated {
            actor: Actor::Cli,
            subject: CredentialId::from_u128(1),
            grant: Grant::Seeder,
        }
    );
    let record = next
        .credential(CredentialId::from_u128(1))
        .expect("registered");
    assert_eq!(record.tier, Tier::Seeder);
    assert!((record.reputation.value() - 1.2).abs() < 1e-15);
}

#[test]
fn only_an_admin_grants_trust() {
    let params = TrustParams::default();
    let mut state = state_with_new(&[1, 2]);
    state.register(CredentialId::from_u128(3), &params);
    let (next, _) = state
        .apply_grant(
            Actor::Cli,
            CredentialId::from_u128(1),
            Grant::Admin,
            &params,
        )
        .expect("cli may grant");
    state = next;

    let result = state.apply_grant(
        Actor::Credential(CredentialId::from_u128(3)),
        CredentialId::from_u128(2),
        Grant::Seeder,
        &params,
    );
    assert!(matches!(result, Err(TrustError::NotAuthorized { .. })));

    let (state, _) = state
        .apply_grant(
            Actor::Credential(CredentialId::from_u128(1)),
            CredentialId::from_u128(2),
            Grant::Moderator,
            &params,
        )
        .expect("admin may grant");
    assert!(
        state
            .credential(CredentialId::from_u128(2))
            .expect("registered")
            .roles
            .contains(&Role::Moderator)
    );
}

#[test]
fn endorsement_keeps_reputation_and_can_be_revoked() {
    let mut state = state_with_new(&[1, 2]);
    make_seeder(&mut state, 1);
    state.insert(
        CredentialId::from_u128(2),
        CredentialState {
            tier: Tier::New,
            roles: BTreeSet::new(),
            reputation: looklab_trust::Reputation::new(0.7).expect("valid reputation"),
            scope: None,
        },
    );

    let (next, event) = state
        .apply_endorsement(
            Actor::Credential(CredentialId::from_u128(1)),
            CredentialId::from_u128(2),
            endorsement(scope(&["fujifilm/x-t5"], &[])),
        )
        .expect("seeder endorses within scope");
    assert!(matches!(event, AuditEvent::EndorsementIssued { .. }));
    let record = next
        .credential(CredentialId::from_u128(2))
        .expect("registered");
    assert_eq!(record.tier, Tier::Endorsed);
    assert!((record.reputation.value() - 0.7).abs() < 1e-15);

    let (next, event) = next
        .revoke_endorsement(
            Actor::Credential(CredentialId::from_u128(1)),
            CredentialId::from_u128(2),
        )
        .expect("seeder may revoke");
    assert!(matches!(event, AuditEvent::EndorsementRevoked { .. }));
    assert_eq!(
        next.credential(CredentialId::from_u128(2))
            .expect("registered")
            .tier,
        Tier::New
    );
}

#[test]
fn endorsement_outside_the_seeder_scope_is_rejected() {
    let mut state = state_with_new(&[1, 2]);
    state.insert(
        CredentialId::from_u128(1),
        CredentialState {
            tier: Tier::Seeder,
            roles: BTreeSet::new(),
            reputation: looklab_trust::Reputation::new(1.2).expect("valid reputation"),
            scope: Some(scope(&["fujifilm/x-t5"], &[])),
        },
    );

    let result = state.apply_endorsement(
        Actor::Credential(CredentialId::from_u128(1)),
        CredentialId::from_u128(2),
        endorsement(scope(&["fujifilm/x-h2"], &[])),
    );
    assert!(matches!(result, Err(TrustError::ScopeNotWithinIssuer)));

    let result = state.apply_endorsement(
        Actor::Credential(CredentialId::from_u128(1)),
        CredentialId::from_u128(2),
        endorsement(scope(&[], &[])),
    );
    assert!(matches!(result, Err(TrustError::ScopeNotWithinIssuer)));
}

#[test]
fn endorsement_rejects_a_seeder_subject() {
    let mut state = state_with_new(&[1, 2]);
    make_seeder(&mut state, 1);
    make_seeder(&mut state, 2);
    let subject = CredentialId::from_u128(2);
    let before = state.credential(subject).expect("registered").clone();

    let result = state.apply_endorsement(
        Actor::Credential(CredentialId::from_u128(1)),
        subject,
        endorsement(scope(&["fujifilm/x-t5"], &[])),
    );
    assert!(matches!(result, Err(TrustError::CannotEndorseSeeder(_))));
    let after = state.credential(subject).expect("registered");
    assert_eq!(after.tier, Tier::Seeder);
    assert_eq!(after.scope, before.scope);
}

#[test]
fn a_non_seeder_cannot_endorse() {
    let params = TrustParams::default();
    let mut state = state_with_new(&[1, 2]);
    let (next, _) = state
        .apply_grant(
            Actor::Cli,
            CredentialId::from_u128(1),
            Grant::Moderator,
            &params,
        )
        .expect("cli may grant");
    state = next;

    let result = state.apply_endorsement(
        Actor::Credential(CredentialId::from_u128(1)),
        CredentialId::from_u128(2),
        endorsement(scope(&["fujifilm/x-t5"], &[])),
    );
    assert!(matches!(result, Err(TrustError::NotAuthorized { .. })));
}

#[test]
fn revoked_seeder_grant_returns_the_credential_to_new() {
    let mut state = state_with_new(&[1]);
    make_seeder(&mut state, 1);
    let (next, event) = state
        .revoke_grant(Actor::Cli, CredentialId::from_u128(1), Grant::Seeder)
        .expect("cli may revoke");
    assert!(matches!(event, AuditEvent::GrantRevoked { .. }));
    assert_eq!(
        next.credential(CredentialId::from_u128(1))
            .expect("registered")
            .tier,
        Tier::New
    );
}

#[test]
fn standing_falls_back_to_new_outside_scope() {
    let mut state = state_with_new(&[1]);
    state.insert(
        CredentialId::from_u128(1),
        CredentialState {
            tier: Tier::Endorsed,
            roles: BTreeSet::new(),
            reputation: looklab_trust::Reputation::new(1.0).expect("valid reputation"),
            scope: Some(scope(&["fujifilm/x-t5"], &["classic-chrome"])),
        },
    );
    let id = CredentialId::from_u128(1);

    assert_eq!(
        state
            .standing(id, &look("classic-chrome"), &body("fujifilm/x-h2"))
            .expect("registered"),
        Standing::Endorsed
    );
    assert_eq!(
        state
            .standing(id, &look("provia"), &body("fujifilm/x-t5"))
            .expect("registered"),
        Standing::Endorsed
    );
    assert_eq!(
        state
            .standing(id, &look("provia"), &body("fujifilm/x-h2"))
            .expect("registered"),
        Standing::New
    );
}

#[test]
fn a_new_credential_stays_new_whatever_its_scope() {
    let standing = Standing::resolve(
        Tier::New,
        Some(&scope(&["fujifilm/x-t5"], &["classic-chrome"])),
        &look("classic-chrome"),
        &body("fujifilm/x-t5"),
    );
    assert_eq!(standing, Standing::New);
    assert!((standing.factor(&TrustParams::default()) - 1.0).abs() < 1e-15);
}

#[test]
fn nominations_list_high_reputation_new_tier_only() {
    let params = TrustParams::default();
    let mut state = state_with_new(&[1, 2, 3]);
    let mut high = state
        .credential(CredentialId::from_u128(1))
        .expect("registered")
        .clone();
    high.reputation = looklab_trust::Reputation::new(0.8).expect("valid reputation");
    state.insert(CredentialId::from_u128(1), high);

    let mut endorsed = state
        .credential(CredentialId::from_u128(3))
        .expect("registered")
        .clone();
    endorsed.tier = Tier::Endorsed;
    endorsed.reputation = looklab_trust::Reputation::new(1.5).expect("valid reputation");
    state.insert(CredentialId::from_u128(3), endorsed);

    assert_eq!(state.nominations(&params), vec![CredentialId::from_u128(1)]);
}

#[test]
fn reputation_updates_do_not_touch_tier() {
    let params = TrustParams::default();
    let mut state = state_with_new(&[1]);
    for _ in 0..20 {
        let (next, reputation) = state
            .record_reputation(
                CredentialId::from_u128(1),
                ReputationUpdate {
                    accepted_fraction: 1.0,
                    deliberate_quarantines: 0,
                    automatic_quarantines: 0,
                    passing_chart_sessions: 1,
                    days_elapsed: 0.0,
                },
                &params,
            )
            .expect("registered");
        state = next;
        assert!(reputation.value() <= params.reputation_max);
    }
    assert_eq!(
        state
            .credential(CredentialId::from_u128(1))
            .expect("registered")
            .tier,
        Tier::New
    );
}
