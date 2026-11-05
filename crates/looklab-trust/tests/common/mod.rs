// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared fixtures for the looklab-trust integration tests.

#![allow(dead_code)]

use std::collections::BTreeMap;

use looklab_meta::SanitizedMetadata;
use looklab_trust::{
    BodyWeightInput, CaptureTuple, CredentialId, EffectiveContributorId, ImageRef, LookWeightInput,
    Region, RegionCoverage, Reputation, Standing, TrustParams, WeightImage,
};
use looklab_upload::{ContributionKind, Intent};

pub fn region() -> Region {
    Region::new(0, 0).expect("cell 0/0 is valid")
}

pub fn credential(value: u128) -> CredentialId {
    CredentialId::from_u128(value)
}

pub fn contributor(value: u128) -> EffectiveContributorId {
    EffectiveContributorId::from(credential(value))
}

pub fn coverage(regions: &[Region]) -> RegionCoverage {
    RegionCoverage::uniform(regions.iter().copied()).expect("non-empty coverage")
}

#[derive(Clone)]
pub struct ImageBuilder {
    image: WeightImage,
}

fn capture_from_bytes(bytes: Vec<u8>) -> CaptureTuple {
    let metadata = SanitizedMetadata {
        make: None,
        model: None,
        firmware: None,
        lens_model: None,
        focal_length: None,
        iso: None,
        shutter: None,
        aperture: None,
        exposure_compensation: None,
        as_shot_wb: None,
        capture_month: None,
        software: Some(String::from_utf8_lossy(&bytes).into_owned()),
        render_tag: None,
        render_tag_malformed: false,
        look: None,
    };
    CaptureTuple::from_metadata(&metadata).expect("capture tuple encodes")
}

impl ImageBuilder {
    pub fn new(id: &str, credential: u128) -> Self {
        Self {
            image: WeightImage {
                id: ImageRef::new(id),
                credential: self::credential(credential),
                contributor: contributor(credential),
                kind: ContributionKind::Rerender,
                intent: Intent::Native,
                evidence: 1.0,
                reputation: Reputation::new(1.0).expect("valid reputation"),
                standing: Standing::New,
                capture: capture_from_bytes(id.as_bytes().to_vec()),
                coverage: coverage(&[region()]),
            },
        }
    }

    pub fn contributor(mut self, credential: u128) -> Self {
        self.image.contributor = contributor(credential);
        self
    }

    pub fn standing(mut self, standing: Standing) -> Self {
        self.image.standing = standing;
        self
    }

    pub fn reputation(mut self, value: f64) -> Self {
        self.image.reputation = Reputation::new(value).expect("valid reputation");
        self
    }

    pub fn evidence(mut self, value: f64) -> Self {
        self.image.evidence = value;
        self
    }

    pub fn kind(mut self, kind: ContributionKind) -> Self {
        self.image.kind = kind;
        self
    }

    pub fn intent(mut self, intent: Intent) -> Self {
        self.image.intent = intent;
        self
    }

    pub fn capture(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.image.capture = capture_from_bytes(bytes.into());
        self
    }

    pub fn coverage(mut self, coverage: RegionCoverage) -> Self {
        self.image.coverage = coverage;
        self
    }

    pub fn build(self) -> WeightImage {
        self.image
    }
}

pub fn look_input(images: Vec<WeightImage>) -> LookWeightInput {
    look_input_with(images, &[(region(), 100.0)])
}

pub fn look_input_with(images: Vec<WeightImage>, anchored: &[(Region, f64)]) -> LookWeightInput {
    LookWeightInput {
        images,
        anchored_weight: anchored.iter().copied().collect::<BTreeMap<Region, f64>>(),
        rooted: true,
    }
}

pub fn body_input(images: Vec<WeightImage>) -> BodyWeightInput {
    BodyWeightInput {
        images,
        anchored_weight: 100.0,
        rooted: true,
    }
}

pub fn weight(report: &looklab_trust::LookWeightReport, id: impl Into<ImageRef>) -> f64 {
    let id = id.into();
    report
        .images
        .iter()
        .find(|image| image.id == id)
        .map(|image| image.total)
        .unwrap_or_else(|| panic!("{id} is in the report"))
}

pub fn params() -> TrustParams {
    TrustParams::default()
}
