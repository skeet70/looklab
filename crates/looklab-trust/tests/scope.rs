// SPDX-License-Identifier: AGPL-3.0-or-later
//! Scope rules: matching, migration, containment, rendering and weight effect.

use std::collections::BTreeSet;

use looklab_trust::{
    Actor, CredentialId, Endorsement, EvidenceRef, Grant, Scope, ScopeRule, Standing, Tier,
    TrustParams, TrustState,
};
use looklab_upload::{BodyId, LookSlug};

fn body(value: &str) -> BodyId {
    BodyId::new(value).expect("valid body id")
}

fn look(value: &str) -> LookSlug {
    LookSlug::new(value).expect("valid look slug")
}

fn pair_scope() -> Scope {
    Scope::new([ScopeRule::new(
        Some(body("fujifilm/x-t5")),
        Some(look("classic-chrome")),
    )])
}

#[test]
fn a_pair_rule_matches_only_the_pair() {
    let scope = pair_scope();
    assert!(scope.covers(&look("classic-chrome"), &body("fujifilm/x-t5")));
    assert!(!scope.covers(&look("provia"), &body("fujifilm/x-t5")));
    assert!(!scope.covers(&look("classic-chrome"), &body("fujifilm/x-h2")));
}

#[test]
fn a_body_rule_covers_all_looks_on_that_body() {
    let scope = Scope::new([ScopeRule::body_all_looks(body("fujifilm/x-t5"))]);
    assert!(scope.covers(&look("classic-chrome"), &body("fujifilm/x-t5")));
    assert!(scope.covers(&look("provia"), &body("fujifilm/x-t5")));
    assert!(!scope.covers(&look("provia"), &body("fujifilm/x-h2")));
}

#[test]
fn a_look_rule_covers_that_look_on_any_body() {
    let scope = Scope::new([ScopeRule::any_body(look("classic-chrome"))]);
    assert!(scope.covers(&look("classic-chrome"), &body("fujifilm/x-t5")));
    assert!(scope.covers(&look("classic-chrome"), &body("fujifilm/x-h2")));
    assert!(!scope.covers(&look("provia"), &body("fujifilm/x-t5")));
}

#[test]
fn legacy_scopes_migrate_to_wildcard_rules() {
    let scope = Scope::from_legacy([body("fujifilm/x-t5")], [look("classic-chrome")]);
    assert_eq!(
        scope.rules,
        vec![
            ScopeRule::any_body(look("classic-chrome")),
            ScopeRule::body_all_looks(body("fujifilm/x-t5")),
        ]
    );
    assert!(scope.covers(&look("provia"), &body("fujifilm/x-t5")));
    assert!(scope.covers(&look("classic-chrome"), &body("fujifilm/x-h2")));
}

#[test]
fn containment_respects_wildcards() {
    let issuer = Scope::new([ScopeRule::body_all_looks(body("fujifilm/x-t5"))]);
    assert!(issuer.contains(&pair_scope()));
    assert!(
        !issuer.contains(&Scope::new([ScopeRule::body_all_looks(body(
            "fujifilm/x-h2"
        ))]))
    );

    let look_issuer = Scope::new([ScopeRule::any_body(look("classic-chrome"))]);
    assert!(look_issuer.contains(&pair_scope()));
    assert!(!look_issuer.contains(&Scope::new([ScopeRule::new(
        Some(body("fujifilm/x-t5")),
        Some(look("provia")),
    )])));

    assert!(!issuer.contains(&Scope::default()));
}

#[test]
fn scopes_render_in_plain_language() {
    assert_eq!(
        ScopeRule::body_all_looks(body("fujifilm/x-t5")).render(),
        "Fujifilm X-T5 · all looks"
    );
    assert_eq!(
        ScopeRule::any_body(look("classic-chrome")).render(),
        "Classic Chrome · any body"
    );
    assert_eq!(
        ScopeRule::new(Some(body("fujifilm/x-t5")), Some(look("classic-chrome"))).render(),
        "Fujifilm X-T5 · Classic Chrome"
    );
    assert_eq!(
        Scope::new([
            ScopeRule::body_all_looks(body("fujifilm/x-t5")),
            ScopeRule::any_body(look("classic-chrome")),
        ])
        .render(),
        "Classic Chrome · any body; Fujifilm X-T5 · all looks"
    );
}

#[test]
fn an_endorsement_pair_scope_does_not_leak_to_other_pairs() {
    let params = TrustParams::default();
    let mut state = TrustState::new();
    let seeder = CredentialId::from_u128(1);
    let subject = CredentialId::from_u128(2);
    let _ = state.register(seeder, &params);
    let _ = state.register(subject, &params);
    let (next, _) = state
        .apply_grant(Actor::Cli, seeder, Grant::Seeder, &params)
        .expect("cli grants");
    let (next, _) = next
        .apply_endorsement(
            Actor::Credential(seeder),
            subject,
            Endorsement {
                scope: pair_scope(),
                reason: "reviewed chart session".to_owned(),
                evidence: vec![EvidenceRef::new("session-1")],
            },
        )
        .expect("seeder endorses");
    let state = next;

    let standing = |look_slug: &str, body_id: &str| {
        state
            .standing(subject, &look(look_slug), &body(body_id))
            .expect("registered")
    };
    assert_eq!(
        standing("classic-chrome", "fujifilm/x-t5"),
        Standing::Endorsed
    );
    assert_eq!(standing("provia", "fujifilm/x-t5"), Standing::New);
    assert_eq!(standing("classic-chrome", "fujifilm/x-h2"), Standing::New);

    assert!((standing("classic-chrome", "fujifilm/x-t5").factor(&params) - 2.0).abs() < 1e-12);
    assert!((standing("provia", "fujifilm/x-t5").factor(&params) - 1.0).abs() < 1e-12);
    assert!((standing("classic-chrome", "fujifilm/x-h2").factor(&params) - 1.0).abs() < 1e-12);
}

#[test]
fn body_params_need_a_rule_naming_the_body() {
    let look_only = Scope::new([ScopeRule::any_body(look("classic-chrome"))]);
    assert!(!look_only.covers_body(&body("fujifilm/x-t5")));
    let body_rule = Scope::new([ScopeRule::body_all_looks(body("fujifilm/x-t5"))]);
    assert!(body_rule.covers_body(&body("fujifilm/x-t5")));
}

#[test]
fn a_rule_with_both_wildcards_matches_every_pair() {
    let scope = Scope::new([ScopeRule::new(None, None)]);
    let bodies: BTreeSet<&str> = ["fujifilm/x-t5", "fujifilm/x-h2"].into_iter().collect();
    for body_id in bodies {
        assert!(scope.covers(&look("provia"), &body(body_id)));
        assert!(scope.covers(&look("classic-chrome"), &body(body_id)));
    }
    assert_eq!(scope.render(), "any body · any look");
}

#[test]
fn seeder_grants_use_the_same_rule_form() {
    let params = TrustParams::default();
    let mut state = TrustState::new();
    let seeder = CredentialId::from_u128(1);
    let subject = CredentialId::from_u128(2);
    state.insert(
        seeder,
        looklab_trust::CredentialState {
            tier: Tier::Seeder,
            roles: BTreeSet::new(),
            reputation: looklab_trust::Reputation::new(1.2).expect("valid reputation"),
            scope: Some(pair_scope()),
        },
    );
    let _ = state.register(subject, &params);

    let result = state.apply_endorsement(
        Actor::Credential(seeder),
        subject,
        Endorsement {
            scope: Scope::new([ScopeRule::new(
                Some(body("fujifilm/x-t5")),
                Some(look("provia")),
            )]),
            reason: "outside".to_owned(),
            evidence: Vec::new(),
        },
    );
    assert!(matches!(
        result,
        Err(looklab_trust::TrustError::ScopeNotWithinIssuer)
    ));
}
