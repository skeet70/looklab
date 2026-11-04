// SPDX-License-Identifier: AGPL-3.0-or-later
//! Root-set eligibility, coverage and sunset.

use std::collections::BTreeSet;

use looklab_trust::{
    CredentialId, EndorsedContributor, Region, RootAdmission, RootDataKind, RootSession,
    RootTarget, Scope, Tier, TrustParams, admission_eligible, rooted, sunset_ready,
    sunset_sessions,
};
use looklab_upload::{BodyId, ContributionKind, LookSlug};

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

fn observation_region(value: u8) -> Region {
    Region::new(value, 0).expect("valid region")
}

fn look_session(kind: ContributionKind) -> RootSession {
    RootSession {
        credential: CredentialId::from_u128(2),
        tier: Tier::Endorsed,
        scope: Some(scope(&["fujifilm/x-t5"], &["classic-chrome"])),
        anchored: true,
        kind: RootDataKind::Session(kind),
        target: RootTarget::Look {
            look: look("classic-chrome"),
            body: body("fujifilm/x-t5"),
        },
        covered_regions: [observation_region(0)].into_iter().collect(),
    }
}

fn seeder_admission() -> RootAdmission {
    RootAdmission {
        seeder: CredentialId::from_u128(1),
        seeder_tier: Tier::Seeder,
        seeder_scope: Some(scope(&["fujifilm/x-t5"], &[])),
    }
}

#[test]
fn a_scoped_chart_session_is_eligible() {
    assert!(admission_eligible(
        &look_session(ContributionKind::Chart),
        &seeder_admission()
    ));
}

#[test]
fn new_tier_credentials_never_root_however_good_their_reputation() {
    let mut session = look_session(ContributionKind::Chart);
    session.tier = Tier::New;
    assert!(!admission_eligible(&session, &seeder_admission()));
}

#[test]
fn admission_requires_a_seeder() {
    let mut admission = seeder_admission();
    admission.seeder_tier = Tier::Endorsed;
    assert!(!admission_eligible(
        &look_session(ContributionKind::Chart),
        &admission
    ));
}

#[test]
fn a_session_outside_the_issuer_scope_is_ineligible() {
    let mut admission = seeder_admission();
    admission.seeder_scope = Some(scope(&["fujifilm/x-h2"], &[]));
    assert!(!admission_eligible(
        &look_session(ContributionKind::Chart),
        &admission
    ));
}

#[test]
fn library_and_rerender_sessions_do_not_root_a_look() {
    assert!(!admission_eligible(
        &look_session(ContributionKind::Library),
        &seeder_admission()
    ));
    assert!(!admission_eligible(
        &look_session(ContributionKind::Rerender),
        &seeder_admission()
    ));
}

#[test]
fn spectral_data_roots_a_body_but_not_a_look() {
    let body_session = RootSession {
        kind: RootDataKind::SpectralSensitivity,
        target: RootTarget::Body(body("fujifilm/x-t5")),
        scope: Some(scope(&["fujifilm/x-t5"], &[])),
        covered_regions: BTreeSet::new(),
        ..look_session(ContributionKind::Chart)
    };
    assert!(admission_eligible(&body_session, &seeder_admission()));

    let look_session = RootSession {
        kind: RootDataKind::SpectralSensitivity,
        ..look_session(ContributionKind::Chart)
    };
    assert!(!admission_eligible(&look_session, &seeder_admission()));
}

#[test]
fn gradient_and_pattern_do_not_root_a_body() {
    let body_session = |kind| RootSession {
        kind: RootDataKind::Session(kind),
        target: RootTarget::Body(body("fujifilm/x-t5")),
        scope: Some(scope(&["fujifilm/x-t5"], &[])),
        covered_regions: BTreeSet::new(),
        ..look_session(ContributionKind::Chart)
    };
    assert!(admission_eligible(
        &body_session(ContributionKind::Chart),
        &seeder_admission()
    ));
    assert!(!admission_eligible(
        &body_session(ContributionKind::Gradient),
        &seeder_admission()
    ));
    assert!(!admission_eligible(
        &body_session(ContributionKind::Pattern),
        &seeder_admission()
    ));
}

#[test]
fn rooted_requires_admitted_sessions_over_every_supported_region() {
    let supported: BTreeSet<Region> = [observation_region(0), observation_region(1)]
        .into_iter()
        .collect();
    let partial = vec![look_session(ContributionKind::Chart)];
    assert!(!rooted(&partial, &supported));

    let mut second = look_session(ContributionKind::Gradient);
    second.covered_regions = [observation_region(1)].into_iter().collect();
    assert!(rooted(
        &[look_session(ContributionKind::Chart), second],
        &supported
    ));
    assert!(!rooted(&[], &supported));
}

#[test]
fn sunset_needs_twenty_contributors_across_three_bodies() {
    let params = TrustParams::default();
    let contributor = |index: u128, bodies: &[&str]| EndorsedContributor {
        credential: CredentialId::from_u128(index),
        bodies: bodies.iter().map(|value| body(value)).collect(),
    };

    let mut contributors = Vec::new();
    for index in 0..20u128 {
        let bodies: &[&str] = match index % 3 {
            0 => &["fujifilm/x-t5"],
            1 => &["fujifilm/x-h2"],
            _ => &["fujifilm/x100v"],
        };
        contributors.push(contributor(index + 100, bodies));
    }
    assert!(sunset_ready(&contributors, &params));

    contributors.truncate(19);
    assert!(!sunset_ready(&contributors, &params));

    let two_bodies: Vec<EndorsedContributor> = (0..20u128)
        .map(|index| {
            let bodies: &[&str] = if index % 2 == 0 {
                &["fujifilm/x-t5"]
            } else {
                &["fujifilm/x-h2"]
            };
            contributor(index + 100, bodies)
        })
        .collect();
    assert!(!sunset_ready(&two_bodies, &params));
}

#[test]
fn sunset_sessions_keep_only_endorsed_contributors() {
    let contributors = vec![EndorsedContributor {
        credential: CredentialId::from_u128(2),
        bodies: [body("fujifilm/x-t5")].into_iter().collect(),
    }];
    let mut seeder_session = look_session(ContributionKind::Chart);
    seeder_session.credential = CredentialId::from_u128(3);
    seeder_session.tier = Tier::Seeder;

    let sessions = [
        look_session(ContributionKind::Chart),
        seeder_session,
        look_session(ContributionKind::Gradient),
    ];
    let eligible = sunset_sessions(&sessions, &contributors);
    assert_eq!(eligible.len(), 2);
    assert!(
        eligible
            .iter()
            .all(|session| session.tier == Tier::Endorsed)
    );
}
