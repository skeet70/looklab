// SPDX-License-Identifier: AGPL-3.0-or-later
//! The release loop: weights, refit, gate, publish or hold, reputation (docs/05 §6, §8).
//!
//! Every version is refitted from the whole pool with the trust weights applied as
//! sample weights, converted into one exposure gauge, and gated against the previous
//! published version and the anchor.

use std::collections::{BTreeMap, BTreeSet};

use looklab_color::RgbSpace;
use looklab_fit::{
    ContributorId, FitImage, FitOptions, ImageFitStats, PreviousSupport, Verdict, fit_with_history,
    post_clip,
};
use looklab_meta::{RawSerial, SerialHmac, SerialHmacKey};
use looklab_model::{Look, LookInput, ProbeId, ProbeSet, probes};
use looklab_upload::{BodyId, ContributionKind, Intent, LookSlug};

use super::population::{self, SampleAxes};
use super::{HarnessError, HarnessResult};
use crate::error::TrustError;
use crate::evidence::{EvidenceGrid, coverage_gain, evidence};
use crate::gates::{
    GateInput, GateReport, GatedUnion, ProbeEstimate, ProbeSupport, RootObservation, VersionProbes,
    exposure_gauge_excluded, gate_release,
};
use crate::ids::{
    ArtifactHash, CredentialId, EffectiveContributorId, ImageRef, SerialLink, SerialReuse,
    VersionId, resolve_effective_contributors, serial_reuse,
};
use crate::params::TrustParams;
use crate::region::RegionCoverage;
use crate::reputation::Reputation;
use crate::rollback::{LookVersionRecord, RevertPlan, VersionStatus, plan_revert};
use crate::standing::{Scope, Standing};
use crate::state::{Actor, AuditEvent, Endorsement, Grant, TrustState};
use crate::weight::{
    BodyWeightInput, BodyWeightReport, CaptureTuple, LookWeightInput, LookWeightReport,
    WeightImage, weight_body, weight_look,
};

/// Simulator tuning.
#[derive(Clone, Debug)]
pub struct HarnessParams {
    pub trust: TrustParams,
    pub fit: FitOptions,
    /// Days between releases, for reputation decay.
    pub days_between_releases: f64,
}

impl Default for HarnessParams {
    fn default() -> Self {
        Self {
            trust: TrustParams::default(),
            fit: FitOptions {
                seed: 0x1eaf_5eed,
                ..FitOptions::default()
            },
            days_between_releases: 30.0,
        }
    }
}

/// One upload in the simulated pool.
pub struct SimImage {
    pub id: ImageRef,
    pub credential: CredentialId,
    pub kind: ContributionKind,
    pub intent: Intent,
    pub capture: CaptureTuple,
    pub coverage: RegionCoverage,
    pub grid: EvidenceGrid,
    pub axes: Vec<SampleAxes>,
    pub fit: FitImage,
    pub true_exposure: f64,
}

/// The capture-metadata tuple a batch reports.
#[derive(Clone, Copy)]
pub enum CaptureSpec<'a> {
    /// Every image carries its own tuple.
    Distinct,
    /// Every image in the batch shares one tuple.
    Shared(&'a str),
}

/// One batch of images rendered through `look` and uploaded by one credential.
#[derive(Clone, Copy)]
pub struct Batch<'a> {
    pub look: &'a Look,
    pub images: usize,
    pub seed: u64,
    pub samples: usize,
    /// Per-channel noise in `J'a'b'` units; larger values make the batch disagree
    /// with the rest of the pool.
    pub noise_ucs: f64,
    pub kind: ContributionKind,
    pub capture: CaptureSpec<'a>,
}

impl<'a> Batch<'a> {
    pub fn new(look: &'a Look, kind: ContributionKind) -> Self {
        Self {
            look,
            images: 1,
            seed: 0,
            samples: 500,
            noise_ucs: 1.0,
            kind,
            capture: CaptureSpec::Distinct,
        }
    }
}

/// A fitted version and the facts the gate needs about it.
#[derive(Clone, Debug)]
pub struct Version {
    pub id: VersionId,
    pub look: Look,
    /// `median(fitted image exposure − generator exposure)`; the version's exposure gauge.
    pub gauge: f64,
    /// Fitted per-image exposures, for the gauge conversion between versions.
    pub exposures: BTreeMap<ImageRef, f64>,
    /// Per probe-set index, in this version's own gauge.
    pub probes: Vec<ProbeEstimate>,
    /// The reference output encoding the post-clip prediction goes through.
    pub space: RgbSpace,
    pub clip_k: f64,
    pub status: VersionStatus,
    pub content: ArtifactHash,
    pub contributors: Vec<CredentialId>,
    pub release: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModerationCause {
    GateFailed,
    SupportedRegionLost,
    SerialReuse,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModerationItem {
    pub version: VersionId,
    pub cause: ModerationCause,
    pub violations: Vec<crate::gates::Violation>,
}

#[derive(Clone, Debug)]
pub struct ReleaseOutcome {
    pub version: VersionId,
    pub previous: VersionId,
    pub published: bool,
    pub gate: GateReport,
    pub weights: LookWeightReport,
    pub body_weights: BodyWeightReport,
    pub serial_reuse: Vec<SerialReuse>,
    pub moderation: Vec<ModerationItem>,
    pub quarantined: Vec<ImageRef>,
    pub audit: Option<AuditEvent>,
}

#[derive(Clone, Debug)]
pub struct RevertOutcome {
    pub plan: RevertPlan,
    /// The target's content republished as a new version.
    pub restored: VersionId,
    /// The refit without the excluded credentials, gated against `restored`.
    pub refit: ReleaseOutcome,
}

/// Post-clip ΔE at one probe between two versions in the anchor's gauge.
#[derive(Clone, Debug)]
pub struct ProbeShift {
    pub probe: ProbeId,
    pub delta: f64,
    pub sigma_new: f64,
    pub sigma_reference: f64,
}

impl ProbeShift {
    /// The docs/05 §8 drift bound `b_p` for this comparison.
    pub fn bound(&self, params: &TrustParams, floor: f64, ceiling: f64) -> f64 {
        let variance =
            self.sigma_new * self.sigma_new + self.sigma_reference * self.sigma_reference;
        (params.gate_bound_scale * variance.sqrt()).clamp(floor, ceiling)
    }
}

pub struct Sim {
    params: HarnessParams,
    look: LookSlug,
    body: BodyId,
    truth: Look,
    probes: ProbeSet,
    probe_s: Vec<f64>,
    pool: Vec<SimImage>,
    trust: TrustState,
    seeder: CredentialId,
    versions: Vec<Version>,
    anchor: Option<VersionId>,
    published: Option<VersionId>,
    root: Vec<RootObservation>,
    gated: GatedUnion,
    moderation: Vec<ModerationItem>,
    serials: BTreeMap<CredentialId, SerialHmac>,
    slots: BTreeMap<EffectiveContributorId, u32>,
    release_index: u32,
    next_image: u64,
    root_admitted: bool,
}

impl Sim {
    pub fn new(
        params: HarnessParams,
        look: LookSlug,
        body: BodyId,
        truth: Look,
    ) -> HarnessResult<Self> {
        let seeder = CredentialId::from_u128(0x5eed_0001);
        let mut trust = TrustState::new();
        trust.register(seeder, &params.trust);
        let (trust, _event) =
            trust.apply_grant(Actor::Cli, seeder, Grant::Seeder, &params.trust)?;
        let probes = probes::v1();
        let probe_s = probes
            .probes
            .iter()
            .map(|probe| LookInput::new(probe.xyz).s)
            .collect();
        Ok(Self {
            params,
            look,
            body,
            truth,
            probes,
            probe_s,
            pool: Vec::new(),
            trust,
            seeder,
            versions: Vec::new(),
            anchor: None,
            published: None,
            root: Vec::new(),
            gated: GatedUnion::default(),
            moderation: Vec::new(),
            serials: BTreeMap::new(),
            slots: BTreeMap::new(),
            release_index: 0,
            next_image: 0,
            root_admitted: false,
        })
    }

    pub fn seeder(&self) -> CredentialId {
        self.seeder
    }

    pub fn look(&self) -> &LookSlug {
        &self.look
    }

    pub fn body(&self) -> &BodyId {
        &self.body
    }

    pub fn truth(&self) -> &Look {
        &self.truth
    }

    pub fn params(&self) -> &HarnessParams {
        &self.params
    }

    pub fn pool(&self) -> &[SimImage] {
        &self.pool
    }

    pub fn versions(&self) -> &[Version] {
        &self.versions
    }

    pub fn version(&self, id: VersionId) -> Option<&Version> {
        self.versions.iter().find(|version| version.id == id)
    }

    pub fn published(&self) -> Option<&Version> {
        self.published.and_then(|id| self.version(id))
    }

    pub fn anchor(&self) -> Option<&Version> {
        self.anchor.and_then(|id| self.version(id))
    }

    pub fn moderation(&self) -> &[ModerationItem] {
        &self.moderation
    }

    pub fn root_observations(&self) -> &[RootObservation] {
        &self.root
    }

    pub fn reputation(&self, credential: CredentialId) -> Option<Reputation> {
        self.trust
            .credential(credential)
            .map(|state| state.reputation)
    }

    pub fn standing(&self, credential: CredentialId) -> Option<Standing> {
        self.trust.standing(credential, &self.look, &self.body)
    }

    /// Applies a seeder endorsement of `subject` over `scope`.
    pub fn endorse(&mut self, subject: CredentialId, scope: Scope) -> HarnessResult<()> {
        if self.trust.credential(subject).is_none() {
            self.trust.register(subject, &self.params.trust);
        }
        let endorsement = Endorsement {
            scope,
            reason: "reviewed chart session".to_owned(),
            evidence: Vec::new(),
        };
        let (state, _event) =
            self.trust
                .apply_endorsement(Actor::Credential(self.seeder), subject, endorsement)?;
        self.trust = state;
        Ok(())
    }

    /// Records the serial HMAC the server derives from a batch's serial field.
    pub fn report_serial(&mut self, credential: CredentialId, serial: &str) -> HarnessResult<()> {
        let key = SerialHmacKey::new([0x7b; 32]);
        let raw = RawSerial::new(serial.as_bytes()).ok_or(HarnessError::InvalidSerial)?;
        let hmac = raw.hmac(&key, "looklab", self.body.as_str());
        self.serials.insert(credential, hmac);
        Ok(())
    }

    pub fn add(
        &mut self,
        credential: CredentialId,
        batch: Batch<'_>,
    ) -> HarnessResult<Vec<ImageRef>> {
        if self.trust.credential(credential).is_none() {
            self.trust.register(credential, &self.params.trust);
        }
        let truth = self.truth.clone();
        let generated = population::render(
            batch.seed,
            batch.images,
            batch.samples,
            batch.noise_ucs,
            &truth,
            batch.look,
        );
        let mut ids = Vec::with_capacity(generated.len());
        for image in generated {
            let id = ImageRef::new(format!("r{:02}-{:04}", self.release_index, self.next_image));
            self.next_image += 1;
            let tag = match batch.capture {
                CaptureSpec::Distinct => id.as_str().to_owned(),
                CaptureSpec::Shared(tag) => tag.to_owned(),
            };
            let axes = population::axes(&image.points);
            let coverage = population::region_coverage(&axes)?;
            let grid = population::evidence_grid(&axes)?;
            self.pool.push(SimImage {
                id: id.clone(),
                credential,
                kind: batch.kind,
                intent: Intent::Native,
                capture: population::capture_tuple(&tag)?,
                coverage,
                grid,
                axes,
                fit: image.fit,
                true_exposure: image.true_exposure,
            });
            ids.push(id);
        }
        Ok(ids)
    }

    pub fn admit_root(&mut self) {
        self.root_admitted = true;
    }

    pub fn release(&mut self) -> HarnessResult<ReleaseOutcome> {
        self.release_excluding(&BTreeSet::new())
    }

    /// The Look weight report the next release would use.
    pub fn look_weights(&mut self) -> HarnessResult<LookWeightReport> {
        self.weights(&BTreeSet::new())
    }

    /// The body weight report the next release would use.
    pub fn body_weight_report(&mut self) -> HarnessResult<BodyWeightReport> {
        self.body_weights(&BTreeSet::new())
    }

    /// Serial HMACs reported by enough credentials to raise a moderation flag.
    pub fn serial_flags(&self) -> Vec<SerialReuse> {
        serial_reuse(
            &self.params.trust,
            &self
                .serials
                .iter()
                .map(|(&credential, &hmac)| SerialLink { credential, hmac })
                .collect::<Vec<_>>(),
        )
    }

    /// "Revert to version N, exclude credential(s) K, refit" (docs/05 §8).
    pub fn revert(
        &mut self,
        target: VersionId,
        exclude: impl IntoIterator<Item = CredentialId>,
    ) -> HarnessResult<RevertOutcome> {
        let history: Vec<LookVersionRecord> = self
            .versions
            .iter()
            .map(|version| LookVersionRecord {
                id: version.id,
                content: version.content,
                status: version.status,
                contributors: version.contributors.clone(),
            })
            .collect();
        let plan = plan_revert(&history, target, exclude)?;
        let source = self
            .version(target)
            .cloned()
            .ok_or(TrustError::UnknownVersion(target))?;

        let id = VersionId::new(self.next_version());
        let restored = Version {
            id,
            look: source.look,
            gauge: source.gauge,
            exposures: source.exposures,
            probes: source.probes,
            space: source.space,
            clip_k: source.clip_k,
            status: VersionStatus::Published,
            content: plan.publish_as_new,
            contributors: source.contributors,
            release: self.release_index,
        };
        self.versions.push(restored);
        self.published = Some(id);

        let excluded: BTreeSet<CredentialId> = plan.refit_excluding.iter().copied().collect();
        let refit = self.release_excluding(&excluded)?;
        Ok(RevertOutcome {
            plan,
            restored: id,
            refit,
        })
    }

    /// Per-probe ΔE between two versions, in the anchor's gauge.
    pub fn shifts(&self, newer: VersionId, reference: VersionId) -> Vec<ProbeShift> {
        let (Some(newer), Some(reference)) = (self.version(newer), self.version(reference)) else {
            return Vec::new();
        };
        let (newer_probes, _) = self.canonical(newer);
        let (reference_probes, _) = self.canonical(reference);
        newer_probes
            .iter()
            .filter_map(|(id, a)| {
                reference_probes.get(id).map(|b| ProbeShift {
                    probe: id.clone(),
                    delta: distance(&a.post_clip, &b.post_clip),
                    sigma_new: a.sigma_post_clip,
                    sigma_reference: b.sigma_post_clip,
                })
            })
            .collect()
    }

    fn next_version(&self) -> u32 {
        self.versions
            .iter()
            .map(|version| version.id.get() + 1)
            .max()
            .unwrap_or(1)
    }

    fn release_excluding(
        &mut self,
        exclude: &BTreeSet<CredentialId>,
    ) -> HarnessResult<ReleaseOutcome> {
        let weights = self.weights(exclude)?;
        let body_weights = self.body_weights(exclude)?;
        let (result, fit_indices) = self.refit(&weights, exclude)?;

        let (gauge, exposures) = self.gauge(&result, &fit_indices);
        let estimates: BTreeMap<usize, &looklab_fit::ProbeEstimate> = result
            .probes
            .probes
            .iter()
            .map(|estimate| (estimate.id_hint, estimate))
            .collect();
        let mut probes = Vec::with_capacity(self.probes.probes.len());
        for (index, probe) in self.probes.probes.iter().enumerate() {
            let estimate = estimates
                .get(&index)
                .ok_or(HarnessError::MissingProbe(index))?;
            probes.push(ProbeEstimate {
                id: probe.id.clone(),
                pre_clip: estimate.ucs.to_array(),
                sigma_pre_clip: estimate.sigma_de,
                post_clip: estimate.post_ucs.to_array(),
                sigma_post_clip: estimate.post_sigma_de,
                bias_de: estimate.bias_de,
                support: ProbeSupport {
                    sigma: estimate.supported_sigma,
                    density: estimate.supported_density,
                    clusters: estimate.supported_clusters,
                    bias: estimate.supported_bias,
                },
                pre_clip_identified: estimate.pre_clip_reliable,
            });
        }
        let contributors: Vec<CredentialId> = self
            .pool
            .iter()
            .filter(|image| !exclude.contains(&image.credential))
            .map(|image| image.credential)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let id = VersionId::new(self.next_version());
        let candidate = Version {
            id,
            look: result.look.clone(),
            gauge,
            exposures,
            probes,
            space: result.output_space,
            clip_k: result.realization.clip_k,
            status: VersionStatus::Provisional,
            content: population::artifact_hash(&result.look),
            contributors,
            release: self.release_index,
        };
        self.versions.push(candidate.clone());

        if self.anchor.is_none() {
            self.anchor = Some(id);
            self.root = self.observations(&candidate);
            self.gated = GatedUnion::at_anchor(&version_probes(&candidate));
        }
        let previous = self.published.unwrap_or(id);
        let anchor = self.anchor.unwrap_or(id);
        let gate = self.gate(&candidate, previous, anchor);

        let mut moderation = Vec::new();
        if !gate.supported_regressions.is_empty() {
            moderation.push(ModerationItem {
                version: id,
                cause: ModerationCause::SupportedRegionLost,
                violations: gate
                    .violations
                    .iter()
                    .filter(|violation| {
                        matches!(
                            violation,
                            crate::gates::Violation::SupportedToUnsupported { .. }
                        )
                    })
                    .cloned()
                    .collect(),
            });
        }
        if !gate.passed {
            moderation.push(ModerationItem {
                version: id,
                cause: ModerationCause::GateFailed,
                violations: gate.violations.clone(),
            });
        }
        let flags = serial_reuse(
            &self.params.trust,
            &self
                .serials
                .iter()
                .map(|(&credential, &hmac)| SerialLink { credential, hmac })
                .collect::<Vec<_>>(),
        );
        for _ in &flags {
            moderation.push(ModerationItem {
                version: id,
                cause: ModerationCause::SerialReuse,
                violations: Vec::new(),
            });
        }
        self.moderation.extend(moderation.iter().cloned());

        let published = gate.passed;
        let audit = if published {
            self.published = Some(id);
            self.gated.include(&version_probes(&candidate));
            let slot = self
                .versions
                .iter_mut()
                .find(|version| version.id == id)
                .ok_or(TrustError::UnknownVersion(id))?;
            slot.status = VersionStatus::Published;
            Some(AuditEvent::ReleaseManifest {
                look: self.look.clone(),
                version: id,
                artifact: candidate.content,
            })
        } else {
            None
        };

        let quarantined = result
            .image_stats
            .iter()
            .zip(&fit_indices)
            .filter(|(stat, _)| stat.verdict == Verdict::Quarantine)
            .map(|(_, &index)| self.pool[index].id.clone())
            .collect();
        self.update_reputations(&weights, &result.image_stats, &fit_indices)?;
        self.release_index += 1;

        Ok(ReleaseOutcome {
            version: id,
            previous,
            published,
            gate,
            weights,
            body_weights,
            serial_reuse: flags,
            moderation,
            quarantined,
            audit,
        })
    }

    fn gauge(
        &self,
        result: &looklab_fit::FitResult,
        fit_indices: &[usize],
    ) -> (f64, BTreeMap<ImageRef, f64>) {
        let exposures: BTreeMap<ImageRef, f64> = fit_indices
            .iter()
            .enumerate()
            .map(|(fit_index, &pool_index)| {
                (
                    self.pool[pool_index].id.clone(),
                    result.images[fit_index].exposure,
                )
            })
            .collect();
        let mut offsets: Vec<f64> = fit_indices
            .iter()
            .enumerate()
            .map(|(fit_index, &pool_index)| {
                result.images[fit_index].exposure - self.pool[pool_index].true_exposure
            })
            .collect();
        if offsets.is_empty() {
            return (0.0, exposures);
        }
        offsets.sort_by(f64::total_cmp);
        (offsets[offsets.len() / 2], exposures)
    }

    /// The exposure scale that maps `version` into the anchor's gauge: the median exposure
    /// difference over the images both fits saw.
    fn delta_xi(&self, version: &Version) -> f64 {
        let Some(anchor) = self.anchor() else {
            return 0.0;
        };
        let mut offsets: Vec<f64> = version
            .exposures
            .iter()
            .filter_map(|(id, exposure)| {
                anchor
                    .exposures
                    .get(id)
                    .map(|anchor_exposure| exposure - anchor_exposure)
            })
            .collect();
        if offsets.is_empty() {
            return version.gauge - anchor.gauge;
        }
        offsets.sort_by(f64::total_cmp);
        offsets[offsets.len() / 2]
    }

    fn observations(&self, anchor: &Version) -> Vec<RootObservation> {
        let scale = libm::pow(2.0, -anchor.gauge);
        self.probes
            .probes
            .iter()
            .map(|probe| RootObservation {
                id: probe.id.clone(),
                jab: post_clip(
                    &self.truth,
                    probe.xyz.scale(scale),
                    anchor.space,
                    self.truth.clip.k,
                )
                .to_array(),
            })
            .collect()
    }

    fn canonical(
        &self,
        version: &Version,
    ) -> (BTreeMap<ProbeId, ProbeEstimate>, BTreeSet<ProbeId>) {
        let delta = self.delta_xi(version);
        let scale = libm::pow(2.0, delta);
        let mut estimates = BTreeMap::new();
        let mut excluded = BTreeSet::new();
        for (index, probe) in self.probes.probes.iter().enumerate() {
            if exposure_gauge_excluded(self.probe_s[index], delta) {
                excluded.insert(probe.id.clone());
                continue;
            }
            let stored = &version.probes[index];
            let xyz = probe.xyz.scale(scale);
            estimates.insert(
                probe.id.clone(),
                ProbeEstimate {
                    id: probe.id.clone(),
                    pre_clip: version.look.eval(xyz).to_array(),
                    sigma_pre_clip: stored.sigma_pre_clip,
                    post_clip: post_clip(&version.look, xyz, version.space, version.clip_k)
                        .to_array(),
                    sigma_post_clip: stored.sigma_post_clip,
                    bias_de: stored.bias_de,
                    support: stored.support,
                    pre_clip_identified: stored.pre_clip_identified,
                },
            );
        }
        (estimates, excluded)
    }

    fn gate(&self, candidate: &Version, previous: VersionId, anchor: VersionId) -> GateReport {
        let (Some(previous), Some(anchor)) = (self.version(previous), self.version(anchor)) else {
            return gate_release(&self.params.trust, &GateInput::default());
        };
        let (candidate_probes, _) = self.canonical(candidate);
        let (previous_probes, _) = self.canonical(previous);
        let (anchor_probes, _) = self.canonical(anchor);
        let kept: Vec<ProbeId> = candidate_probes
            .keys()
            .filter(|id| previous_probes.contains_key(*id) && anchor_probes.contains_key(*id))
            .cloned()
            .collect();
        let select = |source: &BTreeMap<ProbeId, ProbeEstimate>| VersionProbes {
            probes: kept
                .iter()
                .filter_map(|id| source.get(id).cloned())
                .collect(),
        };
        gate_release(
            &self.params.trust,
            &GateInput {
                candidate: select(&candidate_probes),
                previous: select(&previous_probes),
                anchor: select(&anchor_probes),
                root_observations: self.root.clone(),
                gated_since_anchor: self.gated.clone(),
            },
        )
    }

    fn effective(&mut self) -> BTreeMap<CredentialId, EffectiveContributorId> {
        let credentials: BTreeSet<CredentialId> =
            self.pool.iter().map(|image| image.credential).collect();
        let links: Vec<SerialLink> = self
            .serials
            .iter()
            .map(|(&credential, &hmac)| SerialLink { credential, hmac })
            .collect();
        let mapping = resolve_effective_contributors(credentials, &links);
        for contributor in mapping.values() {
            let next = self.slots.len() as u32;
            self.slots.entry(*contributor).or_insert(next);
        }
        mapping
    }

    fn weights(&mut self, exclude: &BTreeSet<CredentialId>) -> HarnessResult<LookWeightReport> {
        let mapping = self.effective();
        let images = self.weight_images(&mapping, |image| !exclude.contains(&image.credential))?;
        Ok(weight_look(
            &self.params.trust,
            &LookWeightInput {
                images,
                anchored_weight: BTreeMap::new(),
                rooted: self.root_admitted,
            },
        ))
    }

    fn body_weights(
        &mut self,
        exclude: &BTreeSet<CredentialId>,
    ) -> HarnessResult<BodyWeightReport> {
        let mapping = self.effective();
        let images = self.weight_images(&mapping, |image| {
            !exclude.contains(&image.credential)
                && (image.kind == ContributionKind::Chart
                    || matches!(image.intent, Intent::Recipe { .. }))
        })?;
        Ok(weight_body(
            &self.params.trust,
            &BodyWeightInput {
                images,
                anchored_weight: 0.0,
                rooted: self.root_admitted,
            },
        ))
    }

    fn weight_images(
        &self,
        mapping: &BTreeMap<CredentialId, EffectiveContributorId>,
        keep: impl Fn(&SimImage) -> bool,
    ) -> HarnessResult<Vec<WeightImage>> {
        let selected: Vec<&SimImage> = self.pool.iter().filter(|image| keep(image)).collect();
        let mut images = Vec::with_capacity(selected.len());
        for image in &selected {
            let mut rest = EvidenceGrid::default();
            for other in &selected {
                if other.id == image.id {
                    continue;
                }
                rest.add_samples(
                    other
                        .axes
                        .iter()
                        .map(|axes| (axes.s, axes.hue, axes.chroma)),
                )?;
            }
            let gain = coverage_gain(&rest, &image.grid)?;
            let standing = self
                .trust
                .standing(image.credential, &self.look, &self.body)
                .unwrap_or(Standing::New);
            let reputation = self
                .trust
                .credential(image.credential)
                .map(|state| state.reputation)
                .unwrap_or_else(|| Standing::New.tier().starting_reputation(&self.params.trust));
            images.push(WeightImage {
                id: image.id.clone(),
                credential: image.credential,
                contributor: mapping[&image.credential],
                kind: image.kind,
                intent: image.intent.clone(),
                evidence: evidence(gain, 0.0, &self.params.trust),
                reputation,
                standing,
                capture: image.capture.clone(),
                coverage: image.coverage.clone(),
            });
        }
        Ok(images)
    }

    fn refit(
        &mut self,
        weights: &LookWeightReport,
        exclude: &BTreeSet<CredentialId>,
    ) -> HarnessResult<(looklab_fit::FitResult, Vec<usize>)> {
        let mapping = self.effective();
        let by_id: BTreeMap<&ImageRef, f64> = weights
            .images
            .iter()
            .map(|image| (&image.id, image.total))
            .collect();
        let mut fit_indices = Vec::new();
        let mut raw_weights = Vec::new();
        for (index, image) in self.pool.iter().enumerate() {
            if exclude.contains(&image.credential) {
                continue;
            }
            if !matches!(image.intent, Intent::Native) {
                continue;
            }
            let weight = by_id.get(&image.id).copied().unwrap_or(0.0);
            if weight <= 0.0 {
                continue;
            }
            fit_indices.push(index);
            raw_weights.push(weight);
        }
        if fit_indices.is_empty() {
            return Err(HarnessError::EmptyPool);
        }
        let mean = raw_weights.iter().sum::<f64>() / raw_weights.len() as f64;
        let images: Vec<FitImage> = fit_indices
            .iter()
            .zip(&raw_weights)
            .map(|(&index, &weight)| {
                let mut fit = self.pool[index].fit.clone();
                fit.trust_weight = weight / mean;
                fit.contributor = ContributorId(self.slots[&mapping[&self.pool[index].credential]]);
                fit
            })
            .collect();
        let previous = self.published().map(|version| PreviousSupport {
            sigma: version
                .probes
                .iter()
                .map(|probe| probe.support.sigma)
                .collect(),
            bias: version
                .probes
                .iter()
                .map(|probe| probe.support.bias)
                .collect(),
        });
        let result = fit_with_history(&images, &self.params.fit, previous.as_ref())?;
        Ok((result, fit_indices))
    }

    fn update_reputations(
        &mut self,
        weights: &LookWeightReport,
        stats: &[ImageFitStats],
        fit_indices: &[usize],
    ) -> HarnessResult<()> {
        let verdicts: BTreeMap<usize, Verdict> = stats
            .iter()
            .zip(fit_indices)
            .map(|(stat, &index)| (index, stat.verdict))
            .collect();
        let mut totals: BTreeMap<CredentialId, (usize, usize, u32)> = BTreeMap::new();
        for image in &weights.images {
            let Some(index) = self
                .pool
                .iter()
                .position(|candidate| candidate.id == image.id)
            else {
                continue;
            };
            if !matches!(self.pool[index].intent, Intent::Native) {
                continue;
            }
            let entry = totals.entry(image.credential).or_insert((0, 0, 0));
            entry.0 += 1;
            // Caps are not down-weighting (docs/05 §4); only a consistency verdict is.
            match verdicts.get(&index).copied().unwrap_or(Verdict::Accept) {
                Verdict::Accept => entry.1 += 1,
                Verdict::DownWeight => {}
                Verdict::Quarantine => entry.2 += 1,
            }
        }
        for (credential, (total, accepted, quarantines)) in totals {
            let update = crate::reputation::ReputationUpdate {
                accepted_fraction: accepted as f64 / total.max(1) as f64,
                deliberate_quarantines: 0,
                automatic_quarantines: quarantines,
                passing_chart_sessions: 0,
                days_elapsed: self.params.days_between_releases,
            };
            let (state, _reputation) =
                self.trust
                    .record_reputation(credential, update, &self.params.trust)?;
            self.trust = state;
        }
        Ok(())
    }
}

fn version_probes(version: &Version) -> VersionProbes {
    VersionProbes {
        probes: version.probes.clone(),
    }
}

fn distance(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    let d0 = a[0] - b[0];
    let d1 = a[1] - b[1];
    let d2 = a[2] - b[2];
    (d0 * d0 + d1 * d1 + d2 * d2).sqrt()
}
