// SPDX-License-Identifier: AGPL-3.0-or-later
//! Property tests for the tier and serial-merge invariants.

mod common;

use common::*;
use looklab_meta::{RawSerial, SerialHmacKey};
use looklab_trust::{
    CredentialId, ImageRef, Region, ReputationUpdate, SerialLink, Standing, Tier, TrustState,
    WeightImage, resolve_effective_contributors, weight_look,
};
use looklab_upload::ContributionKind;
use proptest::prelude::*;

fn kind(index: u8) -> ContributionKind {
    match index % 5 {
        0 => ContributionKind::Library,
        1 => ContributionKind::Rerender,
        2 => ContributionKind::Pattern,
        3 => ContributionKind::Gradient,
        _ => ContributionKind::Chart,
    }
}

fn standing(index: u8) -> Standing {
    match index % 3 {
        0 => Standing::New,
        1 => Standing::Endorsed,
        _ => Standing::Seeder,
    }
}

#[derive(Debug, Clone)]
struct Spec {
    credential: u128,
    kind: u8,
    standing: u8,
    evidence: f64,
    reputation: f64,
    capture: u8,
    two_regions: bool,
}

fn spec_strategy() -> impl Strategy<Value = Spec> {
    (
        0u128..24,
        0u8..5,
        0u8..3,
        0.2f64..1.0,
        0.2f64..1.5,
        0u8..4,
        any::<bool>(),
    )
        .prop_map(
            |(credential, kind, standing, evidence, reputation, capture, two_regions)| Spec {
                credential,
                kind,
                standing,
                evidence,
                reputation,
                capture,
                two_regions,
            },
        )
}

fn build_image(index: usize, spec: &Spec, contributor: u128) -> WeightImage {
    let first = Region::new(0, 0).expect("cell");
    let second = Region::new(1, 0).expect("cell");
    let coverage = if spec.two_regions {
        coverage(&[first, second])
    } else {
        coverage(&[first])
    };
    ImageBuilder::new(&format!("image-{index}"), spec.credential)
        .contributor(contributor)
        .kind(kind(spec.kind))
        .standing(standing(spec.standing))
        .evidence(spec.evidence)
        .reputation(spec.reputation)
        .capture(vec![spec.capture])
        .coverage(coverage)
        .build()
}

fn anchored_regions() -> Vec<(Region, f64)> {
    vec![
        (Region::new(0, 0).expect("cell"), 100.0),
        (Region::new(1, 0).expect("cell"), 100.0),
    ]
}

fn total_for(report: &looklab_trust::LookWeightReport, credential_id: u128) -> f64 {
    report
        .images
        .iter()
        .filter(|image| image.credential == CredentialId::from_u128(credential_id))
        .map(|image| image.total)
        .sum()
}

proptest! {
    #[test]
    fn serial_merges_never_increase_weight(
        specs in prop::collection::vec(spec_strategy(), 1..24),
        groups in prop::collection::vec(any::<bool>(), 24),
    ) {
        let merge_group = |credential: u128| {
            groups[(credential as usize) % groups.len()]
        };
        let merged_representative = (0u128..24)
            .filter(|credential| merge_group(*credential))
            .min();

        let separate: Vec<WeightImage> = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| build_image(index, spec, spec.credential))
            .collect();
        let merged: Vec<WeightImage> = specs
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let contributor = if merge_group(spec.credential) {
                    merged_representative.unwrap_or(spec.credential)
                } else {
                    spec.credential
                };
                build_image(index, spec, contributor)
            })
            .collect();

        let params = params();
        let before = weight_look(&params, &look_input_with(separate, &anchored_regions()));
        let after = weight_look(&params, &look_input_with(merged, &anchored_regions()));

        let credentials: std::collections::BTreeSet<u128> =
            specs.iter().map(|spec| spec.credential).collect();
        for credential in credentials {
            let separate_weight = total_for(&before, credential);
            let merged_weight = total_for(&after, credential);
            prop_assert!(
                merged_weight <= separate_weight + 1e-9,
                "credential {credential}: merged {merged_weight} > separate {separate_weight}"
            );
        }
    }

    #[test]
    fn reputation_and_serial_merges_never_change_tier(
        operations in prop::collection::vec(
            (0u8..3, 0u8..8, 0.0f64..1.0, 0u32..2, 0u32..2, 0u32..2, 0.0f64..500.0),
            1..25,
        ),
        credential_count in 1usize..8,
    ) {
        let params = params();
        let mut state = TrustState::new();
        let ids: Vec<CredentialId> = (0..credential_count as u128)
            .map(CredentialId::from_u128)
            .collect();
        for id in &ids {
            state.register(*id, &params);
        }
        let before: Vec<(CredentialId, Tier)> = state
            .credentials()
            .map(|(id, record)| (id, record.tier))
            .collect();

        let key = SerialHmacKey::new([9u8; 32]);
        for (operation, which, accepted, deliberate, automatic, charts, days) in operations {
            let id = ids[(which as usize) % ids.len()];
            match operation {
                0 => {
                    let (next, _) = state
                        .record_reputation(
                            id,
                            ReputationUpdate {
                                accepted_fraction: accepted,
                                deliberate_quarantines: deliberate,
                                automatic_quarantines: automatic,
                                passing_chart_sessions: charts,
                                days_elapsed: days,
                            },
                            &params,
                        )
                        .expect("registered credential");
                    state = next;
                }
                1 => {
                    let image = ImageBuilder::new("upload", id.to_u128())
                        .reputation(accepted.max(0.2))
                        .build();
                    let _ = weight_look(&params, &look_input(vec![image]));
                }
                _ => {
                    let serial = RawSerial::new(b"SERIAL123").expect("ASCII serial");
                    let tag = serial.hmac(&key, "MAKE", "MODEL");
                    let links = vec![
                        SerialLink {
                            credential: ids[0],
                            hmac: tag,
                        },
                        SerialLink { credential: id, hmac: tag },
                    ];
                    let resolved = resolve_effective_contributors(ids.iter().copied(), &links);
                    prop_assert_eq!(resolved.len(), ids.len());
                }
            }
        }

        for (id, tier) in before {
            prop_assert_eq!(state.credential(id).expect("registered").tier, tier);
        }
    }

    #[test]
    fn weight_is_monotone_in_reputation_within_a_tier(
        reputation in 0.2f64..1.5,
        other in 0.2f64..1.5,
    ) {
        let params = params();
        let low = weight_look(
            &params,
            &look_input(vec![ImageBuilder::new("image", 1)
                .reputation(reputation)
                .build()]),
        );
        let high = weight_look(
            &params,
            &look_input(vec![ImageBuilder::new("image", 1)
                .reputation(reputation.max(other))
                .build()]),
        );
        prop_assert!(weight(&high, ImageRef::new("image")) >= weight(&low, ImageRef::new("image")) - 1e-12);
    }
}
