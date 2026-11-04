// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reputation updates and decay.

mod common;

use looklab_trust::{REPUTATION_MAX, REPUTATION_MIN, Reputation, ReputationUpdate, TrustParams};

fn update() -> ReputationUpdate {
    ReputationUpdate {
        accepted_fraction: 0.5,
        deliberate_quarantines: 0,
        automatic_quarantines: 0,
        passing_chart_sessions: 0,
        days_elapsed: 0.0,
    }
}

#[test]
fn neutral_update_keeps_the_value() {
    let reputation = Reputation::new(0.9).expect("valid reputation");
    let updated = reputation.updated(&TrustParams::default(), update());
    assert!((updated.value() - 0.9).abs() < 1e-15);
}

#[test]
fn one_year_halves_the_distance_to_neutral() {
    let reputation = Reputation::new(1.2).expect("valid reputation");
    let updated = reputation.updated(
        &TrustParams::default(),
        ReputationUpdate {
            days_elapsed: 365.0,
            ..update()
        },
    );
    assert!((updated.value() - 0.85).abs() < 1e-12);
}

#[test]
fn decays_toward_neutral_from_below() {
    let reputation = Reputation::new(0.3).expect("valid reputation");
    let updated = reputation.updated(
        &TrustParams::default(),
        ReputationUpdate {
            days_elapsed: 365.0,
            ..update()
        },
    );
    assert!((updated.value() - 0.4).abs() < 1e-12);
}

#[test]
fn accepted_fraction_moves_by_eta() {
    let reputation = Reputation::new(0.5).expect("valid reputation");
    let accepted = reputation.updated(
        &TrustParams::default(),
        ReputationUpdate {
            accepted_fraction: 1.0,
            ..update()
        },
    );
    assert!((accepted.value() - 0.55).abs() < 1e-12);

    let rejected = reputation.updated(
        &TrustParams::default(),
        ReputationUpdate {
            accepted_fraction: 0.0,
            ..update()
        },
    );
    assert!((rejected.value() - 0.45).abs() < 1e-12);
}

#[test]
fn penalties_and_chart_bonus_apply() {
    let params = TrustParams::default();
    let reputation = Reputation::new(0.5).expect("valid reputation");
    let deliberate = reputation.updated(
        &params,
        ReputationUpdate {
            deliberate_quarantines: 1,
            ..update()
        },
    );
    assert!((deliberate.value() - 0.3).abs() < 1e-12);

    let automatic = reputation.updated(
        &params,
        ReputationUpdate {
            automatic_quarantines: 1,
            ..update()
        },
    );
    assert!((automatic.value() - 0.45).abs() < 1e-12);

    let chart = reputation.updated(
        &params,
        ReputationUpdate {
            passing_chart_sessions: 1,
            ..update()
        },
    );
    assert!((chart.value() - 0.55).abs() < 1e-12);
}

#[test]
fn value_clamps_at_both_ends() {
    let params = TrustParams::default();
    let high = Reputation::new(REPUTATION_MAX).expect("valid reputation");
    let updated = high.updated(
        &params,
        ReputationUpdate {
            accepted_fraction: 1.0,
            passing_chart_sessions: 100,
            ..update()
        },
    );
    assert!((updated.value() - REPUTATION_MAX).abs() < 1e-15);

    let low = Reputation::new(REPUTATION_MIN).expect("valid reputation");
    let updated = low.updated(
        &params,
        ReputationUpdate {
            accepted_fraction: 0.0,
            deliberate_quarantines: 10,
            ..update()
        },
    );
    assert!((updated.value() - REPUTATION_MIN).abs() < 1e-15);
}

#[test]
fn non_finite_values_are_rejected() {
    assert!(Reputation::new(f64::NAN).is_err());
    assert!(Reputation::new(f64::INFINITY).is_err());
    assert!((Reputation::saturating(f64::NEG_INFINITY).value() - 0.5).abs() < 1e-15);
}
