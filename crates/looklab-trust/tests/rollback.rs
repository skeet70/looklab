// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rollback planning.

use looklab_trust::{
    ArtifactHash, CredentialId, LookVersionRecord, VersionId, VersionStatus, plan_revert,
};

fn record(id: u32, byte: u8, contributors: &[u128]) -> LookVersionRecord {
    LookVersionRecord {
        id: VersionId::new(id),
        content: ArtifactHash::new([byte; 32]),
        status: VersionStatus::Published,
        contributors: contributors
            .iter()
            .map(|value| CredentialId::from_u128(*value))
            .collect(),
    }
}

#[test]
fn revert_publishes_the_target_content_and_excludes_credentials() {
    let history = vec![
        record(1, 0x11, &[1, 2]),
        record(2, 0x22, &[1, 3, 4]),
        record(3, 0x33, &[5]),
    ];
    let plan = plan_revert(
        &history,
        VersionId::new(2),
        vec![
            CredentialId::from_u128(3),
            CredentialId::from_u128(1),
            CredentialId::from_u128(9),
        ],
    )
    .expect("target exists");

    assert_eq!(plan.target_version, VersionId::new(2));
    assert_eq!(plan.publish_as_new, ArtifactHash::new([0x22; 32]));
    assert_eq!(
        plan.refit_excluding,
        vec![
            CredentialId::from_u128(1),
            CredentialId::from_u128(3),
            CredentialId::from_u128(9),
        ]
    );
    assert_eq!(
        plan.excluded_contributors_of_target,
        vec![CredentialId::from_u128(1), CredentialId::from_u128(3)]
    );
}

#[test]
fn revert_does_not_add_credentials_absent_from_the_target() {
    let history = vec![record(1, 0x11, &[1])];
    let plan = plan_revert(
        &history,
        VersionId::new(1),
        vec![CredentialId::from_u128(2)],
    )
    .expect("target exists");
    assert!(plan.excluded_contributors_of_target.is_empty());
    assert_eq!(plan.refit_excluding, vec![CredentialId::from_u128(2)]);
}

#[test]
fn unknown_target_is_an_error() {
    let history = vec![record(1, 0x11, &[])];
    assert!(plan_revert(&history, VersionId::new(9), Vec::new()).is_err());
}
