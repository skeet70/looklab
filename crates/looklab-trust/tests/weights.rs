// SPDX-License-Identifier: AGPL-3.0-or-later
//! Weighting, cap and duplicate rules.

mod common;

use common::*;
use looklab_trust::{CapReason, Region, RegionCoverage, Standing, weight_body, weight_look};
use looklab_upload::{ContributionKind, Intent};

fn has_reason(report: &looklab_trust::CapReport, reason: CapReason) -> bool {
    report
        .reductions
        .iter()
        .any(|reduction| reduction.reason == reason)
}

#[test]
fn new_tier_group_allows_exactly_thirty_percent() {
    let mut images = Vec::new();
    for index in 0..7u128 {
        images.push(
            ImageBuilder::new(&format!("trusted-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    for index in 0..6u128 {
        images.push(
            ImageBuilder::new(&format!("new-{index}"), index + 20)
                .standing(Standing::New)
                .build(),
        );
    }
    let report = weight_look(&params(), &look_input_with(images, &[]));

    let region_weights = &report.regions[0];
    assert!((region_weights.trusted - 14.0).abs() < 1e-12);
    assert!((region_weights.new - 6.0).abs() < 1e-12);
    assert!((region_weights.total - 20.0).abs() < 1e-12);
    assert!(!has_reason(&report.caps, CapReason::NewTierGroupCap));
}

#[test]
fn new_tier_group_caps_above_thirty_percent() {
    let mut images = Vec::new();
    for index in 0..6u128 {
        images.push(
            ImageBuilder::new(&format!("trusted-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    for index in 0..8u128 {
        images.push(
            ImageBuilder::new(&format!("new-{index}"), index + 20)
                .standing(Standing::New)
                .build(),
        );
    }
    let report = weight_look(&params(), &look_input_with(images, &[]));

    let region_weights = &report.regions[0];
    assert!((region_weights.trusted - 12.0).abs() < 1e-12);
    assert!((region_weights.new - 12.0 * 0.3 / 0.7).abs() < 1e-12);
    assert!((region_weights.total - region_weights.trusted - region_weights.new).abs() < 1e-12);
    assert!(has_reason(&report.caps, CapReason::NewTierGroupCap));
}

#[test]
fn new_tier_data_defines_a_region_without_trusted_data() {
    let images = vec![
        ImageBuilder::new("a", 1).standing(Standing::New).build(),
        ImageBuilder::new("b", 2).standing(Standing::New).build(),
    ];
    let input = looklab_trust::LookWeightInput {
        images,
        anchored_weight: Default::default(),
        rooted: false,
    };
    let report = weight_look(&params(), &input);

    assert!(report.regions[0].provisional);
    assert!((report.regions[0].new - 2.0).abs() < 1e-12);
    assert!((report.images[0].total - 1.0).abs() < 1e-12);
    assert!(report.provisional);
}

#[test]
fn diminishing_returns_follow_the_documented_curve() {
    let mut images = Vec::new();
    for index in 0..5u8 {
        images.push(
            ImageBuilder::new(&format!("image-{index}"), 1)
                .capture(vec![index])
                .build(),
        );
    }
    let report = weight_look(&params(), &look_input(images));
    let scale = params().diminishing_scale;

    for index in 0..5usize {
        let expected = 1.0 / (1.0 + index as f64 / scale).sqrt();
        let id = format!("image-{index}");
        assert!(
            (weight(&report, id.as_str()) - expected).abs() < 1e-12,
            "{id}: expected {expected}, got {}",
            weight(&report, id.as_str())
        );
    }
    assert!(has_reason(
        &report.caps,
        CapReason::DiminishingReturns { position: 1 }
    ));
}

#[test]
fn per_contributor_cap_does_not_apply_below_the_minimum_pool() {
    let mut images = Vec::new();
    for index in 0..19u128 {
        images.push(
            ImageBuilder::new(&format!("image-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    let report = weight_look(&params(), &look_input_with(images, &[]));
    assert_eq!(report.contributors, 19);
    assert!(!report.provisional, "a rooted Look is not provisional");
    assert!(!has_reason(&report.caps, CapReason::ContributorCap));
    assert_eq!(report.regions.len(), 1);
    assert!((report.regions[0].trusted - 38.0).abs() < 1e-12);
    assert!(!report.regions[0].provisional);
}

#[test]
fn per_contributor_cap_applies_from_twenty_contributors() {
    let mut images = Vec::new();
    for index in 0..19u128 {
        images.push(
            ImageBuilder::new(&format!("image-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    images.push(
        ImageBuilder::new("image-19a", 100)
            .standing(Standing::Endorsed)
            .capture(b"a".to_vec())
            .build(),
    );
    images.push(
        ImageBuilder::new("image-19b", 100)
            .standing(Standing::Endorsed)
            .capture(b"b".to_vec())
            .build(),
    );
    let report = weight_look(&params(), &look_input_with(images, &[]));

    assert_eq!(report.contributors, 20);
    assert!(has_reason(&report.caps, CapReason::ContributorCap));
    let contributor_total: f64 = report
        .images
        .iter()
        .filter(|image| image.credential == credential(100))
        .map(|image| image.total)
        .sum();
    let second = 1.0 / (1.0 + 1.0 / params().diminishing_scale).sqrt();
    let cap = params().look_contributor_cap * (19.0 * 2.0 + 2.0 + 2.0 * second);
    assert!((contributor_total - cap).abs() < 1e-9);
}

#[test]
fn lower_standing_credential_loses_weight_first() {
    let mut images = Vec::new();
    for index in 0..19u128 {
        images.push(
            ImageBuilder::new(&format!("other-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    images.push(
        ImageBuilder::new("new-a", 100)
            .contributor(100)
            .standing(Standing::New)
            .capture(vec![0])
            .build(),
    );
    images.push(
        ImageBuilder::new("new-b", 101)
            .contributor(100)
            .standing(Standing::New)
            .capture(vec![1])
            .build(),
    );
    images.push(
        ImageBuilder::new("trusted-a", 100)
            .standing(Standing::Endorsed)
            .capture(vec![2])
            .build(),
    );
    images.push(
        ImageBuilder::new("trusted-b", 102)
            .contributor(100)
            .standing(Standing::Endorsed)
            .capture(vec![3])
            .build(),
    );
    let report = weight_look(&params(), &look_input_with(images, &[]));

    assert!((weight(&report, "new-a")).abs() < 1e-12);
    assert!((weight(&report, "new-b")).abs() < 1e-12);
    assert!(weight(&report, "trusted-b") > 0.0);
    let merged_total: f64 = ["new-a", "new-b", "trusted-a", "trusted-b"]
        .iter()
        .map(|id| weight(&report, *id))
        .sum();
    let scale = params().diminishing_scale;
    let expected = 0.05
        * (19.0 * 2.0
            + 1.0
            + 1.0 / (1.0 + 1.0 / scale).sqrt()
            + 2.0 / (1.0 + 2.0 / scale).sqrt()
            + 2.0 / (1.0 + 3.0 / scale).sqrt());
    assert!((merged_total - expected).abs() < 1e-9);
}

#[test]
fn honest_duplicates_merge_into_one_weight() {
    let images = vec![
        ImageBuilder::new("first", 1)
            .capture(b"same".to_vec())
            .build(),
        ImageBuilder::new("second", 1)
            .capture(b"same".to_vec())
            .build(),
        ImageBuilder::new("third", 1)
            .capture(b"z-other".to_vec())
            .build(),
    ];
    let report = weight_look(&params(), &look_input(images));

    assert!(
        report
            .images
            .iter()
            .all(|image| image.id.as_str() != "second"),
        "the merged duplicate leaves no entry of its own"
    );
    assert!(has_reason(&report.caps, CapReason::DuplicateMerge));
    let merged = report
        .images
        .iter()
        .find(|image| image.duplicates == 2)
        .expect("one entry absorbed the duplicate");
    assert!((merged.total - 1.0).abs() < 1e-12);
}

#[test]
fn same_capture_different_contributors_are_not_duplicates() {
    let images = vec![
        ImageBuilder::new("first", 1)
            .capture(b"same".to_vec())
            .build(),
        ImageBuilder::new("second", 2)
            .capture(b"same".to_vec())
            .build(),
    ];
    let report = weight_look(&params(), &look_input(images));
    assert!(weight(&report, "first") > 0.0);
    assert!(weight(&report, "second") > 0.0);
    assert!(!has_reason(&report.caps, CapReason::DuplicateMerge));
}

#[test]
fn recipe_images_get_zero_look_weight() {
    let target = looklab_upload::LookSlug::new("classic-chrome").expect("valid slug");
    let images = vec![
        ImageBuilder::new("native", 1).build(),
        ImageBuilder::new("recipe", 2)
            .kind(ContributionKind::Chart)
            .intent(Intent::Recipe {
                target_look: target.clone(),
            })
            .build(),
    ];
    let report = weight_look(&params(), &look_input(images));

    assert!((weight(&report, "recipe")).abs() < 1e-12);
    assert!(weight(&report, "native") > 0.0);
    let reduction = report
        .caps
        .reductions
        .iter()
        .find(|reduction| reduction.reason == CapReason::RecipeChannel)
        .expect("recipe exclusion is reported");
    assert_eq!(
        reduction.image.as_ref().map(|id| id.as_str()),
        Some("recipe")
    );

    let partition = looklab_trust::partition_stages([(
        looklab_trust::ImageRef::new("recipe"),
        Intent::Recipe {
            target_look: target,
        },
    )]);
    assert_eq!(partition.recipe.len(), 1);
    assert!(partition.native.is_empty());
}

#[test]
fn weight_components_multiply_in_the_documented_order() {
    let image = ImageBuilder::new("chart", 1)
        .kind(ContributionKind::Chart)
        .evidence(0.25)
        .standing(Standing::Endorsed)
        .reputation(1.5)
        .build();
    let report = weight_look(&params(), &look_input(vec![image]));
    let expected = 2.0 * 0.25 * 2.0 * 1.5;
    assert!((weight(&report, "chart") - expected).abs() < 1e-12);
}

#[test]
fn body_cap_uses_its_own_minimum_pool() {
    let mut images = Vec::new();
    for index in 0..4u128 {
        images.push(
            ImageBuilder::new(&format!("image-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    let below = weight_body(&params(), &body_input(images));

    let mut images = Vec::new();
    for index in 0..5u128 {
        images.push(
            ImageBuilder::new(&format!("image-{index}"), index + 1)
                .standing(Standing::Endorsed)
                .build(),
        );
    }
    let at = weight_body(&params(), &body_input(images));

    assert_eq!(below.contributors, 4);
    assert_eq!(at.contributors, 5);
    assert!(
        !below
            .caps
            .reductions
            .iter()
            .any(|reduction| { reduction.reason == CapReason::ContributorCap })
    );
    // Five equal contributors are each exactly at the 20% cap.
    assert!(
        !at.caps
            .reductions
            .iter()
            .any(|reduction| { reduction.reason == CapReason::ContributorCap })
    );
}

#[test]
fn region_grid_boundaries() {
    let low = Region::from_probe(-12.0, 0.0).expect("probe in range");
    assert_eq!((low.hue(), low.exposure()), (0, 0));
    let high = Region::from_probe(6.0, 359.9).expect("probe in range");
    assert_eq!((high.hue(), high.exposure()), (11, 5));
    let boundary = Region::from_probe(-9.0, 30.0).expect("probe in range");
    assert_eq!((boundary.hue(), boundary.exposure()), (1, 1));
    assert!(Region::from_probe(6.5, 0.0).is_err());
    assert!(Region::from_probe(0.0, f64::NAN).is_err());
}

#[test]
fn coverage_shares_normalize() {
    let first = Region::new(0, 0).expect("cell");
    let second = Region::new(1, 0).expect("cell");
    let coverage =
        RegionCoverage::from_shares([(first, 3.0), (second, 1.0)]).expect("positive shares");
    assert!((coverage.share(first) - 0.75).abs() < 1e-12);
    assert!((coverage.share(second) - 0.25).abs() < 1e-12);
    assert!(RegionCoverage::from_shares([(first, 0.0)]).is_err());
    assert!(RegionCoverage::from_shares([(first, -1.0)]).is_err());
}

#[test]
fn weight_image_raw_weight_matches_components() {
    let image = ImageBuilder::new("i", 1)
        .kind(ContributionKind::Library)
        .evidence(0.5)
        .standing(Standing::Endorsed)
        .reputation(0.8)
        .build();
    let expected = 0.25 * 0.5 * 2.0 * 0.8;
    assert!((image.raw_weight(&params()) - expected).abs() < 1e-12);
}
