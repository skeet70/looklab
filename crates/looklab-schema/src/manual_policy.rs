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

    /// An unspecified scope covers no firmware.
    pub fn covers(&self, firmware: &ReportedFirmware) -> bool {
        match self {
            Self::Unspecified {} => false,
            Self::Exact { firmware: scope } => {
                !scope.is_unspecified() && scope_accepts(scope, firmware.as_str())
            }
            Self::Range { from, to } => {
                if from.is_unspecified() || to.is_unspecified() {
                    return false;
                }
                let (Some(reported), Some(start), Some(end)) = (
                    parse_version(firmware.as_str()),
                    parse_version(from.as_str()),
                    parse_version(to.as_str()),
                ) else {
                    return false;
                };
                compare_versions(&reported, &start) != Ordering::Less
                    && compare_versions(&reported, &end) != Ordering::Greater
            }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationState {
    Documented,
    Unconfirmed,
    Confirmed,
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
                if result.state == ConfirmationState::Confirmed {
                    confirmed.insert(result.firmware.clone());
                }
            }
            if control.state == ConfirmationState::ModeratorConfirmed
                && let Some(evidence) = &control.moderator
                && let Some(firmware) = evidence.exact_firmware()
            {
                confirmed.insert(firmware);
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
    /// Linked owner agreement counted for this firmware.
    Counted,
    /// Current, but neither counted nor covered by moderator evidence.
    Pending,
    /// Recorded against an older control definition.
    StaleDefinition,
    /// Covered by the latest moderator decision watermark and firmware scope.
    Reviewed,
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
    moderators: &BTreeMap<CapabilityKey, ModeratorRecord>,
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
            moderators.get(&capability.key),
        ));
    }
    let mut summary = ManualConfirmationSummary {
        controls,
        confirmed: 0,
        contradicted: 0,
        unconfirmed: 0,
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
    moderator: Option<&ModeratorRecord>,
) -> ControlConfirmation {
    if let Some(record) = moderator {
        return ControlConfirmation {
            key: capability.key,
            state: if record.confirmed {
                ConfirmationState::ModeratorConfirmed
            } else {
                ConfirmationState::ModeratorRejected
            },
            agreeing_groups: 0,
            contradictions: Vec::new(),
            moderator: Some(record.evidence.clone()),
            firmware: Vec::new(),
            documented_availability: capability.availability,
            intake: ManualControlIntake::default(),
        };
    }
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
    let firmware = by_firmware
        .into_iter()
        .map(|(reported, entries)| {
            evaluate_firmware(capability, &definition, reported.clone(), &entries)
        })
        .collect::<Vec<_>>();
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
    } else if !firmware.is_empty()
        && firmware
            .iter()
            .all(|result| result.state == ConfirmationState::Confirmed)
    {
        ConfirmationState::Confirmed
    } else if firmware.is_empty() && capability.availability != Availability::Available {
        ConfirmationState::Documented
    } else {
        ConfirmationState::Unconfirmed
    };
    ControlConfirmation {
        key: capability.key,
        state,
        agreeing_groups,
        contradictions,
        moderator: None,
        firmware,
        documented_availability: capability.availability,
        intake: ManualControlIntake::default(),
    }
}

fn evaluate_firmware(
    capability: &DocumentedCapability,
    definition: &Sha256Hex,
    firmware: ReportedFirmware,
    evidence: &[&ManualEvidence],
) -> FirmwareConfirmation {
    let mut groups: Vec<ClaimGroup<'_>> = Vec::new();
    for &entry in evidence {
        let claim = ActualClaim::from_finding(capability, &entry.finding);
        match groups.iter_mut().find(|group| group.claim == claim) {
            Some(group) => group.evidence.push(entry),
            None => groups.push(ClaimGroup {
                claim,
                evidence: vec![entry],
            }),
        }
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.evidence.len()));
    let Some(leading) = groups.first() else {
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
        };
    };
    let agreeing_groups = leading.evidence.len() as u32;
    if groups.len() > 1 {
        let mut contradictions = groups[1..]
            .iter()
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
        };
    }
    FirmwareConfirmation {
        firmware,
        state: ConfirmationState::Unconfirmed,
        agreeing_groups,
        contradictions: Vec::new(),
        basis: None,
        confirmed_actual: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActualClaim {
    availability: Availability,
    domain: CapabilityDomain,
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

fn scope_accepts(scope: &FirmwareScope, reported: &str) -> bool {
    let scope = scope.as_str().trim();
    if scope.eq_ignore_ascii_case("unspecified") {
        return true;
    }
    if let Some((min, max)) = scope.split_once('-')
        && let (Some(min), Some(max), Some(reported)) = (
            parse_version(min),
            parse_version(max),
            parse_version(reported),
        )
    {
        return compare_versions(&reported, &min) != Ordering::Less
            && compare_versions(&reported, &max) != Ordering::Greater;
    }
    if let (Some(scope), Some(reported)) = (parse_version(scope), parse_version(reported)) {
        return compare_versions(&scope, &reported) == Ordering::Equal;
    }
    scope.eq_ignore_ascii_case(reported)
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
