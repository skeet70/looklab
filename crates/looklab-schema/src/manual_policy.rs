// SPDX-License-Identifier: AGPL-3.0-or-later
//! Agreement and contradiction policy for documented camera controls.

use core::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ids::BodyId;
use crate::manual::{
    Availability, CapabilityDomain, CapabilityKey, DocumentedCapability, FirmwareScope,
    FirmwareScopeError, ManualCamera, ManualUrl,
};
use crate::manual_confirmation::{Finding, ReportedFirmware};
use crate::proposal::Sha256Hex;

pub const AGREEMENT_GROUPS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdentityGroupId(String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("expected a non-empty identity group")]
pub struct IdentityGroupError;

impl IdentityGroupId {
    /// # Errors
    /// Empty text.
    pub fn new(value: &str) -> Result<Self, IdentityGroupError> {
        if value.trim().is_empty() {
            return Err(IdentityGroupError);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for IdentityGroupId {
    type Error = IdentityGroupError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<IdentityGroupId> for String {
    fn from(value: IdentityGroupId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualEvidence {
    pub identity_group: IdentityGroupId,
    pub firmware: ReportedFirmware,
    pub control: CapabilityKey,
    pub definition_digest: Sha256Hex,
    pub finding: Finding,
    pub submitted_at: i64,
    /// Whether the reporting profile has a non-withdrawn serial-bearing photo of this camera.
    #[serde(default = "default_linked")]
    pub linked: bool,
}

const fn default_linked() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeratorEvidenceKind {
    Video,
    Physical,
}

impl ModeratorEvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Physical => "physical",
        }
    }
}

impl core::str::FromStr for ModeratorEvidenceKind {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "video" => Ok(Self::Video),
            "physical" => Ok(Self::Physical),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModeratorFirmwareScope {
    Unspecified {},
    Exact {
        firmware: FirmwareScope,
    },
    Range {
        from: FirmwareScope,
        to: FirmwareScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModeratorScopeError {
    #[error("invalid firmware scope: {0}")]
    InvalidScope(#[from] FirmwareScopeError),
    #[error("expected a firmware version")]
    NotAVersion,
    #[error("range start is after its end")]
    InvertedRange,
}

impl ModeratorFirmwareScope {
    pub fn unspecified() -> Self {
        Self::Unspecified {}
    }

    /// # Errors
    /// A non-version string or the literal `unspecified`.
    pub fn exact(firmware: &str) -> Result<Self, ModeratorScopeError> {
        let firmware = FirmwareScope::new(firmware)?;
        if firmware.is_unspecified() || parse_version(firmware.as_str()).is_none() {
            return Err(ModeratorScopeError::NotAVersion);
        }
        Ok(Self::Exact { firmware })
    }

    /// # Errors
    /// A non-version endpoint, the literal `unspecified`, or a start after its end.
    pub fn range(from: &str, to: &str) -> Result<Self, ModeratorScopeError> {
        let from = FirmwareScope::new(from)?;
        let to = FirmwareScope::new(to)?;
        let (Some(start), Some(end)) = (parse_version(from.as_str()), parse_version(to.as_str()))
        else {
            return Err(ModeratorScopeError::NotAVersion);
        };
        if from.is_unspecified() || to.is_unspecified() {
            return Err(ModeratorScopeError::NotAVersion);
        }
        if compare_versions(&start, &end) == Ordering::Greater {
            return Err(ModeratorScopeError::InvertedRange);
        }
        Ok(Self::Range { from, to })
    }

    pub fn is_unspecified(&self) -> bool {
        matches!(self, Self::Unspecified {})
    }

    pub fn exact_firmware(&self) -> Option<ReportedFirmware> {
        match self {
            Self::Exact { firmware } => ReportedFirmware::new(firmware.as_str()).ok(),
            Self::Range { .. } | Self::Unspecified {} => None,
        }
    }
}

impl<'de> Deserialize<'de> for ModeratorFirmwareScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Raw {
            Unspecified {},
            Exact {
                firmware: FirmwareScope,
            },
            Range {
                from: FirmwareScope,
                to: FirmwareScope,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Unspecified {} => Self::Unspecified {},
            Raw::Exact { firmware } => {
                Self::exact(firmware.as_str()).map_err(<D::Error as serde::de::Error>::custom)?
            }
            Raw::Range { from, to } => Self::range(from.as_str(), to.as_str())
                .map_err(<D::Error as serde::de::Error>::custom)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeratorEvidence {
    pub kind: ModeratorEvidenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<ManualUrl>,
    pub description: String,
    pub firmware_scope: ModeratorFirmwareScope,
}

impl ModeratorEvidence {
    pub fn exact_firmware(&self) -> Option<ReportedFirmware> {
        self.firmware_scope.exact_firmware()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeratorRecord {
    pub confirmed: bool,
    pub evidence: ModeratorEvidence,
}

impl ModeratorRecord {
    /// An unspecified scope covers no firmware.
    pub fn applies_to(&self, firmware: &ReportedFirmware) -> bool {
        moderator_applies_to(self, firmware.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationState {
    Documented,
    Unconfirmed,
    Confirmed,
    /// Unlinked profiles agree on one actual value; it counts immediately but is not confirmed.
    OwnerReported,
    Contradicted,
    ModeratorConfirmed,
    ModeratorRejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contradiction {
    pub firmware: ReportedFirmware,
    pub finding: Finding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationBasis {
    Documented,
    Corrected,
    Moderator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmedActual {
    pub availability: Availability,
    pub domain: CapabilityDomain,
    pub agreeing_groups: u32,
    pub definition_digest: Sha256Hex,
    pub firmware_scopes: Vec<FirmwareScope>,
}

/// One distinct reported actual value with the number of linked groups and unlinked profiles behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedOption {
    pub finding: Finding,
    #[serde(default)]
    pub linked_groups: u32,
    #[serde(default)]
    pub owner_profiles: u32,
}

impl ReportedOption {
    pub fn reports(&self) -> u32 {
        self.linked_groups.saturating_add(self.owner_profiles)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FirmwareConfirmation {
    pub firmware: ReportedFirmware,
    pub state: ConfirmationState,
    pub agreeing_groups: u32,
    pub contradictions: Vec<Contradiction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<ConfirmationBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_actual: Option<ConfirmedActual>,
    /// Every distinct actual value reported for this firmware, most reported first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported_options: Vec<ReportedOption>,
    /// The number of unlinked profiles whose latest answer is counted for this firmware.
    #[serde(default)]
    pub owner_profiles: u32,
    /// Every profile whose latest answer is counted, including one per linked group.
    #[serde(default)]
    pub reported_profiles: u32,
}

impl FirmwareConfirmation {
    /// Whether this firmware's counted reports agree with the claim in `finding`.
    pub fn reports_agree(&self, capability: &DocumentedCapability, finding: &Finding) -> bool {
        let claim = ActualClaim::from_finding(capability, finding);
        match self.state {
            ConfirmationState::Confirmed => self
                .confirmed_actual
                .as_ref()
                .is_some_and(|actual| actual.agrees_with(capability, finding)),
            ConfirmationState::OwnerReported => {
                self.reported_options.first().is_some_and(|option| {
                    ActualClaim::from_finding(capability, &option.finding) == claim
                })
            }
            _ => false,
        }
    }

    /// Whether more than one actual value was reported for this firmware.
    pub fn reports_conflict(&self) -> bool {
        self.reported_options.len() > 1
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ManualControlIntake {
    /// Latest non-withdrawn checks recorded against the current control definition.
    pub recorded: u32,
    /// The subset of `recorded` whose profile has no serial-linked photo for this camera.
    pub unlinked: u32,
    /// Latest non-withdrawn checks recorded against an older control definition.
    pub stale: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlConfirmation {
    pub key: CapabilityKey,
    pub state: ConfirmationState,
    pub agreeing_groups: u32,
    pub contradictions: Vec<Contradiction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moderator: Option<ModeratorEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub firmware: Vec<FirmwareConfirmation>,
    #[serde(default = "default_documented_availability")]
    pub documented_availability: Availability,
    #[serde(default)]
    pub intake: ManualControlIntake,
}

fn default_documented_availability() -> Availability {
    Availability::NotDocumented
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualConfirmationSummary {
    pub controls: Vec<ControlConfirmation>,
    pub confirmed: u32,
    pub contradicted: u32,
    pub unconfirmed: u32,
    #[serde(default)]
    pub owner_reported: u32,
    pub documented: u32,
}

impl ManualConfirmationSummary {
    pub fn control(&self, key: CapabilityKey) -> Option<&ControlConfirmation> {
        self.controls.iter().find(|control| control.key == key)
    }

    pub fn confirmed_keys(&self) -> impl Iterator<Item = CapabilityKey> + '_ {
        self.controls
            .iter()
            .filter(|control| {
                matches!(
                    control.state,
                    ConfirmationState::Confirmed | ConfirmationState::ModeratorConfirmed
                )
            })
            .map(|control| control.key)
    }

    /// A control is required when it is documented available or carries evidence.
    pub fn common_confirmed_firmware(&self) -> Vec<ReportedFirmware> {
        let mut common: Option<BTreeSet<ReportedFirmware>> = None;
        let mut considered = false;
        for control in &self.controls {
            if matches!(
                control.state,
                ConfirmationState::Contradicted | ConfirmationState::ModeratorRejected
            ) {
                return Vec::new();
            }
            let mut confirmed = BTreeSet::new();
            for result in &control.firmware {
                if matches!(
                    result.state,
                    ConfirmationState::Confirmed | ConfirmationState::ModeratorConfirmed
                ) {
                    confirmed.insert(result.firmware.clone());
                }
            }
            if confirmed.is_empty() {
                if control.documented_availability == Availability::Available
                    || !control.firmware.is_empty()
                    || control.moderator.is_some()
                {
                    return Vec::new();
                }
                continue;
            }
            considered = true;
            common = Some(match common {
                Some(previous) => previous.intersection(&confirmed).cloned().collect(),
                None => confirmed,
            });
            if common.as_ref().is_some_and(BTreeSet::is_empty) {
                return Vec::new();
            }
        }
        if considered {
            common.into_iter().flatten().collect()
        } else {
            Vec::new()
        }
    }

    /// Whether every required control shares at least one confirmed firmware.
    pub fn human_confirmed(&self) -> bool {
        !self.common_confirmed_firmware().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualOwnerLinkage {
    Linked,
    Unlinked,
}

/// What a signed confirmation submission recorded for its sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualSubmissionReceipt {
    pub camera: BodyId,
    pub owner_linkage: ManualOwnerLinkage,
    pub recorded_checks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualConfirmationSubmission {
    #[serde(flatten)]
    pub summary: ManualConfirmationSummary,
    pub receipt: ManualSubmissionReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualCheckReviewStatus {
    /// The newest answer for its profile and agrees with the firmware's reported or confirmed value.
    Counted,
    /// Part of a disagreement at its control and firmware; the conflict is the review item.
    Conflicted,
    /// Recorded against an older control definition.
    StaleDefinition,
    /// Received before a moderator decision that is unscoped or whose firmware scope covers it.
    Reviewed,
}

/// One control and firmware where owner reports disagree, with every option and its count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCheckConflict {
    pub camera: BodyId,
    pub control: CapabilityKey,
    pub label: String,
    pub firmware: ReportedFirmware,
    pub definition_digest: Sha256Hex,
    pub options: Vec<ReportedOption>,
    pub newest_submitted_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualCheckReview {
    pub camera: BodyId,
    pub control: CapabilityKey,
    pub label: String,
    pub firmware: ReportedFirmware,
    pub finding: Finding,
    pub definition_digest: Sha256Hex,
    pub submitted_at: i64,
    pub owner_linkage: ManualOwnerLinkage,
    pub status: ManualCheckReviewStatus,
}

pub fn evaluate_camera(
    camera: &ManualCamera,
    evidence: &[ManualEvidence],
    moderators: &BTreeMap<CapabilityKey, Vec<ModeratorRecord>>,
) -> ManualConfirmationSummary {
    let mut controls = Vec::with_capacity(camera.capabilities.len());
    for capability in &camera.capabilities {
        let observations = evidence
            .iter()
            .filter(|entry| entry.control == capability.key)
            .cloned()
            .collect::<Vec<_>>();
        controls.push(evaluate_control(
            camera,
            capability,
            &observations,
            moderators
                .get(&capability.key)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        ));
    }
    let mut summary = ManualConfirmationSummary {
        controls,
        confirmed: 0,
        contradicted: 0,
        unconfirmed: 0,
        owner_reported: 0,
        documented: 0,
    };
    for control in &summary.controls {
        match control.state {
            ConfirmationState::Confirmed | ConfirmationState::ModeratorConfirmed => {
                summary.confirmed += 1;
            }
            ConfirmationState::Contradicted | ConfirmationState::ModeratorRejected => {
                summary.contradicted += 1;
            }
            ConfirmationState::OwnerReported => summary.owner_reported += 1,
            ConfirmationState::Unconfirmed => summary.unconfirmed += 1,
            ConfirmationState::Documented => summary.documented += 1,
        }
    }
    summary
}

pub fn evaluate_control(
    camera: &ManualCamera,
    capability: &DocumentedCapability,
    evidence: &[ManualEvidence],
    moderators: &[ModeratorRecord],
) -> ControlConfirmation {
    let definition = capability.definition_digest(camera);
    let mut latest: BTreeMap<(&IdentityGroupId, &ReportedFirmware), &ManualEvidence> =
        BTreeMap::new();
    for entry in evidence {
        if entry.definition_digest != definition
            || !firmware_scope_accepts(camera, capability, &entry.firmware)
        {
            continue;
        }
        let key = (&entry.identity_group, &entry.firmware);
        match latest.get(&key) {
            Some(existing) if existing.submitted_at >= entry.submitted_at => {}
            _ => {
                latest.insert(key, entry);
            }
        }
    }
    let mut by_firmware: BTreeMap<&ReportedFirmware, Vec<&ManualEvidence>> = BTreeMap::new();
    for &entry in latest.values() {
        by_firmware.entry(&entry.firmware).or_default().push(entry);
    }
    let mut firmware = by_firmware
        .into_iter()
        .map(|(reported, entries)| {
            evaluate_firmware(capability, &definition, reported.clone(), &entries)
        })
        .collect::<Vec<_>>();
    let moderator = moderators.first().map(|record| record.evidence.clone());
    let mut known = firmware
        .iter()
        .map(|result| result.firmware.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    for record in moderators {
        if let ModeratorFirmwareScope::Exact { firmware } = &record.evidence.firmware_scope {
            known.insert(firmware.as_str().to_owned());
        }
    }
    let decisions = known
        .into_iter()
        .filter_map(|firmware| {
            let record = moderators
                .iter()
                .find(|record| moderator_applies_to(record, &firmware))?;
            Some((firmware, record))
        })
        .collect::<BTreeMap<_, _>>();
    for (reported_firmware, record) in decisions {
        let state = if record.confirmed {
            ConfirmationState::ModeratorConfirmed
        } else {
            ConfirmationState::ModeratorRejected
        };
        match firmware.iter_mut().find(|result| {
            result
                .firmware
                .as_str()
                .eq_ignore_ascii_case(&reported_firmware)
        }) {
            Some(result) => {
                if !(record.confirmed && result.state == ConfirmationState::Confirmed) {
                    result.state = state;
                    result.basis = Some(ConfirmationBasis::Moderator);
                    result.confirmed_actual = None;
                }
                result.contradictions.clear();
            }
            None => {
                if let Ok(reported) = ReportedFirmware::new(&reported_firmware) {
                    firmware.push(FirmwareConfirmation {
                        firmware: reported,
                        state,
                        agreeing_groups: 0,
                        contradictions: Vec::new(),
                        basis: Some(ConfirmationBasis::Moderator),
                        confirmed_actual: None,
                        reported_options: Vec::new(),
                        owner_profiles: 0,
                        reported_profiles: 0,
                    });
                }
            }
        }
    }
    let mut contradictions = firmware
        .iter()
        .flat_map(|result| result.contradictions.iter().cloned())
        .collect::<Vec<_>>();
    contradictions.sort_by(|left, right| {
        left.firmware
            .as_str()
            .cmp(right.firmware.as_str())
            .then_with(|| finding_order(&left.finding).cmp(&finding_order(&right.finding)))
    });
    let agreeing_groups = firmware
        .iter()
        .map(|result| result.agreeing_groups)
        .max()
        .unwrap_or(0);
    let state = if firmware
        .iter()
        .any(|result| result.state == ConfirmationState::Contradicted)
    {
        ConfirmationState::Contradicted
    } else if firmware
        .iter()
        .any(|result| result.state == ConfirmationState::Confirmed)
    {
        ConfirmationState::Confirmed
    } else if firmware
        .iter()
        .any(|result| result.state == ConfirmationState::ModeratorConfirmed)
    {
        ConfirmationState::ModeratorConfirmed
    } else if firmware
        .iter()
        .any(|result| result.state == ConfirmationState::ModeratorRejected)
    {
        ConfirmationState::ModeratorRejected
    } else if firmware
        .iter()
        .any(|result| result.state == ConfirmationState::OwnerReported)
    {
        ConfirmationState::OwnerReported
    } else if firmware.is_empty() {
        match moderators.first() {
            Some(record) if record.confirmed => ConfirmationState::ModeratorConfirmed,
            Some(_) => ConfirmationState::ModeratorRejected,
            None if capability.availability != Availability::Available => {
                ConfirmationState::Documented
            }
            None => ConfirmationState::Unconfirmed,
        }
    } else {
        ConfirmationState::Unconfirmed
    };
    ControlConfirmation {
        key: capability.key,
        state,
        agreeing_groups,
        contradictions,
        moderator,
        firmware,
        documented_availability: capability.availability,
        intake: ManualControlIntake::default(),
    }
}

fn moderator_applies_to(record: &ModeratorRecord, firmware: &str) -> bool {
    match &record.evidence.firmware_scope {
        ModeratorFirmwareScope::Exact { firmware: scoped } => {
            scoped.as_str().eq_ignore_ascii_case(firmware)
        }
        ModeratorFirmwareScope::Range { from, to } => firmware_in_range(firmware, from, to),
        ModeratorFirmwareScope::Unspecified {} => false,
    }
}

fn firmware_in_range(firmware: &str, from: &FirmwareScope, to: &FirmwareScope) -> bool {
    let (Some(version), Some(start), Some(end)) = (
        parse_version(firmware),
        parse_version(from.as_str()),
        parse_version(to.as_str()),
    ) else {
        return false;
    };
    compare_versions(&version, &start) != Ordering::Less
        && compare_versions(&version, &end) != Ordering::Greater
}

fn evaluate_firmware(
    capability: &DocumentedCapability,
    definition: &Sha256Hex,
    firmware: ReportedFirmware,
    evidence: &[&ManualEvidence],
) -> FirmwareConfirmation {
    let mut linked_groups: Vec<ClaimGroup<'_>> = Vec::new();
    let mut owner_groups: Vec<ClaimGroup<'_>> = Vec::new();
    for &entry in evidence {
        let claim = ActualClaim::from_finding(capability, &entry.finding);
        let groups = if entry.linked {
            &mut linked_groups
        } else {
            &mut owner_groups
        };
        match groups.iter_mut().find(|group| group.claim == claim) {
            Some(group) => group.evidence.push(entry),
            None => groups.push(ClaimGroup {
                claim,
                evidence: vec![entry],
            }),
        }
    }
    linked_groups.sort_by_key(|group| std::cmp::Reverse(group.evidence.len()));
    owner_groups.sort_by_key(|group| std::cmp::Reverse(group.evidence.len()));
    let owner_profiles = owner_groups
        .iter()
        .map(|group| group.evidence.len() as u32)
        .sum::<u32>();
    let reported_profiles = owner_profiles.saturating_add(
        linked_groups
            .iter()
            .map(|group| group.evidence.len() as u32)
            .sum::<u32>(),
    );
    let mut reported_options = reported_options(capability, &linked_groups, &owner_groups);
    reported_options.sort_by(|left, right| {
        right
            .reports()
            .cmp(&left.reports())
            .then_with(|| finding_key(&left.finding).cmp(&finding_key(&right.finding)))
    });
    let Some(leading) = linked_groups.first() else {
        if !owner_groups.is_empty() {
            let state = if owner_groups.len() > 1 {
                ConfirmationState::Contradicted
            } else {
                ConfirmationState::OwnerReported
            };
            return FirmwareConfirmation {
                firmware,
                state,
                agreeing_groups: 0,
                contradictions: owner_contradictions(&owner_groups),
                basis: None,
                confirmed_actual: None,
                reported_options,
                owner_profiles,
                reported_profiles,
            };
        }
        return FirmwareConfirmation {
            firmware,
            state: if capability.availability == Availability::Available {
                ConfirmationState::Unconfirmed
            } else {
                ConfirmationState::Documented
            },
            agreeing_groups: 0,
            contradictions: Vec::new(),
            basis: None,
            confirmed_actual: None,
            reported_options,
            owner_profiles,
            reported_profiles,
        };
    };
    let agreeing_groups = leading.evidence.len() as u32;
    if linked_groups.len() > 1
        || (agreeing_groups < AGREEMENT_GROUPS as u32 && reported_options.len() > 1)
    {
        let leading_claim = ActualClaim::from_finding(capability, &reported_options[0].finding);
        let mut contradictions = linked_groups
            .iter()
            .chain(owner_groups.iter())
            .filter(|group| group.claim != leading_claim)
            .flat_map(|group| group.evidence.iter().copied())
            .map(|entry| Contradiction {
                firmware: entry.firmware.clone(),
                finding: entry.finding.clone(),
            })
            .collect::<Vec<_>>();
        contradictions.sort_by(|left, right| {
            left.firmware
                .as_str()
                .cmp(right.firmware.as_str())
                .then_with(|| finding_order(&left.finding).cmp(&finding_order(&right.finding)))
        });
        return FirmwareConfirmation {
            firmware,
            state: ConfirmationState::Contradicted,
            agreeing_groups,
            contradictions,
            basis: None,
            confirmed_actual: None,
            reported_options,
            owner_profiles,
            reported_profiles,
        };
    }
    if agreeing_groups as usize >= AGREEMENT_GROUPS {
        let basis = if leading.claim.is_documented(capability) {
            ConfirmationBasis::Documented
        } else {
            ConfirmationBasis::Corrected
        };
        let confirmed_actual = ConfirmedActual {
            availability: leading.claim.availability,
            domain: leading.claim.domain.clone(),
            agreeing_groups,
            definition_digest: *definition,
            firmware_scopes: FirmwareScope::new(firmware.as_str())
                .ok()
                .into_iter()
                .collect(),
        };
        return FirmwareConfirmation {
            firmware,
            state: ConfirmationState::Confirmed,
            agreeing_groups,
            contradictions: Vec::new(),
            basis: Some(basis),
            confirmed_actual: Some(confirmed_actual),
            reported_options,
            owner_profiles,
            reported_profiles,
        };
    }
    FirmwareConfirmation {
        firmware,
        state: ConfirmationState::OwnerReported,
        agreeing_groups,
        contradictions: Vec::new(),
        basis: None,
        confirmed_actual: None,
        reported_options,
        owner_profiles,
        reported_profiles,
    }
}

/// Options from every linked claim group except the leading one.
fn owner_contradictions(groups: &[ClaimGroup<'_>]) -> Vec<Contradiction> {
    groups
        .iter()
        .skip(1)
        .flat_map(|group| group.evidence.iter().copied())
        .map(|entry| Contradiction {
            firmware: entry.firmware.clone(),
            finding: entry.finding.clone(),
        })
        .collect()
}

fn reported_options(
    capability: &DocumentedCapability,
    linked_groups: &[ClaimGroup<'_>],
    owner_groups: &[ClaimGroup<'_>],
) -> Vec<ReportedOption> {
    let mut options: Vec<ReportedOption> = Vec::new();
    for (groups, linked) in [(linked_groups, true), (owner_groups, false)] {
        for group in groups {
            let count = group.evidence.len() as u32;
            match options.iter_mut().find(|option| {
                ActualClaim::from_finding(capability, &option.finding) == group.claim
            }) {
                Some(option) => {
                    if linked {
                        option.linked_groups = option.linked_groups.saturating_add(count);
                    } else {
                        option.owner_profiles = option.owner_profiles.saturating_add(count);
                    }
                }
                None => options.push(ReportedOption {
                    finding: group.evidence[0].finding.clone(),
                    linked_groups: if linked { count } else { 0 },
                    owner_profiles: if linked { 0 } else { count },
                }),
            }
        }
    }
    options
}

fn finding_key(finding: &Finding) -> String {
    serde_json::to_string(finding).unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActualClaim {
    availability: Availability,
    domain: CapabilityDomain,
}

impl ConfirmedActual {
    /// Whether `finding` claims this availability and domain.
    pub fn agrees_with(&self, capability: &DocumentedCapability, finding: &Finding) -> bool {
        let claim = ActualClaim::from_finding(capability, finding);
        claim.availability == self.availability && claim.domain == self.domain
    }
}

impl ActualClaim {
    fn from_finding(capability: &DocumentedCapability, finding: &Finding) -> Self {
        match finding {
            Finding::Matches => Self {
                availability: capability.availability,
                domain: capability.domain.clone(),
            },
            Finding::Corrected {
                availability,
                domain,
            } => Self {
                availability: *availability,
                domain: domain.clone(),
            },
        }
    }

    fn is_documented(&self, capability: &DocumentedCapability) -> bool {
        self.availability == capability.availability && self.domain == capability.domain
    }
}

struct ClaimGroup<'a> {
    claim: ActualClaim,
    evidence: Vec<&'a ManualEvidence>,
}

fn finding_order(finding: &Finding) -> u8 {
    match finding {
        Finding::Matches => 0,
        Finding::Corrected { .. } => 1,
    }
}

pub fn firmware_scope_accepts(
    camera: &ManualCamera,
    capability: &DocumentedCapability,
    firmware: &ReportedFirmware,
) -> bool {
    let scopes = capability.source_scopes(camera);
    if scopes.is_empty() || scopes.iter().any(|scope| scope.is_unspecified()) {
        return true;
    }
    let reported = firmware.as_str().trim();
    scopes.iter().any(|scope| scope_accepts(scope, reported))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ScopeRelation {
    Unspecified,
    LowerBound(Vec<u32>),
    Range { from: Vec<u32>, to: Vec<u32> },
    Exact(Vec<u32>),
    Text(String),
}

fn scope_relation(scope: &str) -> ScopeRelation {
    let scope = scope.trim();
    if scope.eq_ignore_ascii_case("unspecified") {
        return ScopeRelation::Unspecified;
    }
    let lower = scope
        .strip_suffix(" or later")
        .or_else(|| scope.strip_suffix(" or newer"))
        .or_else(|| scope.strip_prefix(">="))
        .map(str::trim);
    if let Some(lower) = lower
        && let Some(lower) = parse_version(lower)
    {
        return ScopeRelation::LowerBound(lower);
    }
    if let Some((from, to)) = scope.split_once('-')
        && let (Some(from), Some(to)) = (parse_version(from), parse_version(to))
    {
        return ScopeRelation::Range { from, to };
    }
    if let Some(exact) = parse_version(scope) {
        return ScopeRelation::Exact(exact);
    }
    ScopeRelation::Text(scope.to_owned())
}

fn scope_accepts(scope: &FirmwareScope, reported: &str) -> bool {
    let relation = scope_relation(scope.as_str());
    let reported_text = reported.trim();
    let Some(reported_version) = parse_version(reported_text) else {
        return matches!(&relation, ScopeRelation::Unspecified)
            || matches!(&relation, ScopeRelation::Text(text) if text.eq_ignore_ascii_case(reported_text));
    };
    match relation {
        ScopeRelation::Unspecified => true,
        ScopeRelation::LowerBound(lower) => {
            compare_versions(&reported_version, &lower) != Ordering::Less
        }
        ScopeRelation::Range { from, to } => {
            compare_versions(&reported_version, &from) != Ordering::Less
                && compare_versions(&reported_version, &to) != Ordering::Greater
        }
        ScopeRelation::Exact(exact) => {
            compare_versions(&reported_version, &exact) == Ordering::Equal
        }
        ScopeRelation::Text(text) => text.eq_ignore_ascii_case(reported_text),
    }
}

fn parse_version(value: &str) -> Option<Vec<u32>> {
    let value = value.trim();
    let value = value
        .strip_prefix("Ver.")
        .or_else(|| value.strip_prefix("ver."))
        .unwrap_or(value);
    let value = value
        .strip_prefix('v')
        .or_else(|| value.strip_prefix('V'))
        .unwrap_or(value);
    if value.is_empty() {
        return None;
    }
    value
        .split('.')
        .map(|part| {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                None
            } else {
                part.parse().ok()
            }
        })
        .collect()
}

fn compare_versions(left: &[u32], right: &[u32]) -> Ordering {
    let length = left.len().max(right.len());
    for index in 0..length {
        let left = left.get(index).copied().unwrap_or(0);
        let right = right.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    Ordering::Equal
}
