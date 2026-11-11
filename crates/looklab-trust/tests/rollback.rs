// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rollback planning.

use looklab_trust::{ArtifactHash, ContributionId, LookVersionRecord, VersionId, plan_revert};

fn record(id: u32, byte: u8) -> LookVersionRecord {
    LookVersionRecord {
        id: VersionId::new(id),
        content: ArtifactHash::new([byte; 32]),
    }
}

#[test]
fn revert_publishes_the_target_content_and_rejects_named_contributions() {
    let history = vec![record(1, 0x11), record(2, 0x22), record(3, 0x33)];
    let plan = plan_revert(
        &history,
        VersionId::new(2),
        vec![
            ContributionId::from_u128(13),
            ContributionId::from_u128(11),
            ContributionId::from_u128(99),
        ],
    )
    .expect("target exists");

    assert_eq!(plan.target_version, VersionId::new(2));
    assert_eq!(plan.publish_as_new, ArtifactHash::new([0x22; 32]));
    assert_eq!(
        plan.rejected_contributions,
        vec![
            ContributionId::from_u128(11),
            ContributionId::from_u128(13),
            ContributionId::from_u128(99),
        ]
    );
}

#[test]
fn rejecting_a_contribution_absent_from_the_target_keeps_the_target_content() {
    let history = vec![record(1, 0x11)];
    let plan = plan_revert(
        &history,
        VersionId::new(1),
        vec![ContributionId::from_u128(12)],
    )
    .expect("target exists");
    assert_eq!(plan.publish_as_new, ArtifactHash::new([0x11; 32]));
    assert_eq!(
        plan.rejected_contributions,
        vec![ContributionId::from_u128(12)]
    );
}

#[test]
fn unknown_target_is_an_error() {
    let history = vec![record(1, 0x11)];
    assert!(plan_revert(&history, VersionId::new(9), Vec::new()).is_err());
}
