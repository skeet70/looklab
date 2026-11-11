// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet, HashMap};

use looklab_color::Mat3;
use looklab_fit::ExposureModeId;
use looklab_model::LookInput;
use looklab_model::realization::{ControlValue, Settings};
use looklab_samples::{OutputSpace, SampleFlags, SampleSet};
use looklab_schema::{
    Affects, BodyRealizationId, CellId, ContributionRef, Control, ControlDefault, ControlEpochId,
    ControlKey, ControlSchema, ControlType, DependencyGeneration, DependencyKind, DependencyRef,
    DependencyTarget, FirmwareId, IdentityResolutionRef, ImageRef, LookId, OutputEncoding,
    ProfileId, PublishedSchemaSnapshot, ReportScope, RootAdmissionRef, ScalarValue,
    SettingCombination, SettingValue, Sha256Hex, SimKey, StandingRef, SupportedMask, Tolerated,
    WbModeKey,
};
use looklab_server::admin::ActiveRootAdmission;
use looklab_server::model_store;
use looklab_trust::{
    CredentialId as TrustCredentialId, EffectiveContributorId, Region, RegionCoverage, SerialLink,
    resolve_effective_contributors,
};
use looklab_upload::ManifestMetadata;
use looklab_upload::{BodyId, CommunitySnapshotVersion, LookSlug};
use looklab_upload::{ControlSettings, SettingSource};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;

use crate::body::{BodyResources, BodyTransforms, DataDir, TransformDir, epoch_for};
use crate::clock::now_unix;
use crate::config::WorkerConfig;
use crate::db::{self, ContributionRow, CredentialInfo, ImageId, ImageOutcome, ImageRow};
use crate::error::{Result, WorkerError};
use crate::meta;

const EXPOSURE_MIN: f64 = looklab_trust::EXPOSURE_MIN;
const EXPOSURE_MAX: f64 = looklab_trust::EXPOSURE_MAX;

/// Which fit stage an image's evidence belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRole {
    /// Native evidence; may enter the native stage and qualified weights.
    Native,
    /// Recipe evidence; enters only the recipe stage, never a Look target.
    Recipe,
}

/// The settings and sample domain a caller wants from a realization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceScope {
    /// All ordinarily eligible native evidence.
    Seed,
    /// The exact report scope, supported cell mask and candidate physical pins.
    Qualified {
        report: ReportScope,
        mask: SupportedMask,
        pinned: crate::assessment::PinnedModes,
    },
    /// Every eligible observation, for diagnostics and experimental realizations.
    Experimental,
}

impl EvidenceScope {
    fn includes(
        &self,
        epoch: &looklab_schema::Epoch,
        family: &LookId,
        body: &BodyId,
        role: EvidenceRole,
        physical: Option<&PhysicalContext>,
        settings: &Settings,
    ) -> bool {
        match self {
            Self::Experimental => true,
            Self::Seed => role == EvidenceRole::Native,
            Self::Qualified { report, pinned, .. } => {
                !report.points.is_empty()
                    && report.assessed_bodies.contains(body)
                    && report.points.iter().any(|point| {
                        point_matches(
                            point,
                            report.defaults_assessed,
                            epoch,
                            family,
                            pinned,
                            physical,
                            settings,
                        )
                    })
            }
        }
    }

    fn admits_sample(&self, input: &LookInput, output: OutputSpace) -> bool {
        match self {
            Self::Experimental | Self::Seed => true,
            Self::Qualified { report, mask, .. } => {
                encoding_matches(report.output_encoding, output)
                    && CellId::new(format!("cell-{}", looklab_fit::model_error::cell(input)))
                        .is_ok_and(|cell| mask.contains(&cell))
            }
        }
    }

    /// Ignores samples the pool's own scope did not admit.
    pub(crate) fn excludes_samples(&self, pool: &LookPool, image: &PoolImage) -> bool {
        let Ok(family) = family_id(&pool.look) else {
            return false;
        };
        let Some(epoch) = pool
            .resources
            .schema
            .epochs
            .iter()
            .find(|epoch| epoch.id == image.epoch)
        else {
            return false;
        };
        if !self.includes(
            epoch,
            &family,
            &image.realization.body,
            image.role,
            image.physical.as_ref(),
            &image.settings,
        ) {
            return image.admitted.iter().any(|admitted| *admitted);
        }
        image
            .samples
            .in_rgb()
            .iter()
            .zip(&image.admitted)
            .any(|(rgb, admitted)| {
                if !admitted {
                    return false;
                }
                let xyz = image.transform.mul_vec(rgb.map(f64::from));
                if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
                    return false;
                }
                let input = LookInput::new(looklab_color::Xyz::from_array(xyz));
                !self.admits_sample(&input, image.samples.header().output_space)
            })
    }
}

fn encoding_matches(encoding: OutputEncoding, output: OutputSpace) -> bool {
    matches!(
        (encoding, output),
        (OutputEncoding::Srgb, OutputSpace::Srgb)
            | (OutputEncoding::AdobeRgb, OutputSpace::AdobeRgb)
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EvidenceBlocker {
    /// A spatial control has no value and no schema default.
    MissingSetting { control: ControlKey },
    /// A spatial control is not at a schema-listed tolerated value.
    SpatialNotTolerated { control: ControlKey },
    /// The contribution names a control the current epoch does not define.
    UnknownControl { control: ControlKey },
    /// The settings are outside the scope the caller selected.
    OutOfScope,
    /// No sample lies in the selected supported cells.
    NoAdmittedSamples,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceEligibility {
    Eligible,
    Ineligible(Vec<EvidenceBlocker>),
}

impl EvidenceEligibility {
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalContext {
    pub firmware: FirmwareId,
    pub wb_mode: WbModeKey,
}

/// The sanitized capture context a contribution records; unknown stays unknown.
pub fn physical_context(image: &PoolImage) -> Option<PhysicalContext> {
    physical_context_from_meta(&image.contribution, image.epoch, &image.image.meta_json)
}

fn physical_context_from_meta(
    contribution: &ContributionRow,
    epoch: u32,
    meta_json: &str,
) -> Option<PhysicalContext> {
    let version = contribution.firmware.parse().ok()?;
    let firmware = FirmwareId {
        body: contribution.body.clone(),
        epoch: ControlEpochId::new(epoch),
        version,
    };
    let meta: ManifestMetadata = serde_json::from_str(meta_json).ok()?;
    let mode = meta.look?.white_balance?;
    let value = serde_json::to_value(mode.value).ok()?;
    let wb_mode = WbModeKey::new(value.as_str()?.to_owned()).ok()?;
    Some(PhysicalContext { firmware, wb_mode })
}

#[derive(Debug, Clone)]
pub struct PoolImage {
    pub image: ImageRow,
    pub contribution: ContributionRow,
    pub credential: CredentialInfo,
    pub contributor: EffectiveContributorId,
    pub samples: SampleSet,
    pub transform: Mat3,
    pub realization: BodyRealizationId,
    pub physical: Option<PhysicalContext>,
    pub epoch: u32,
    pub settings: Settings,
    /// Which controls came from the file and which the contributor entered.
    pub setting_source: BTreeMap<ControlKey, SettingSource>,
    pub exposure_mode: ExposureModeId,
    pub outcome: Option<ImageOutcome>,
    pub capture: looklab_trust::CaptureTuple,
    pub coverage: RegionCoverage,
    /// `(s, hue, √C)` per admitted sample.
    pub axes: Vec<(f64, f64, f64)>,
    /// One entry per sample; only admitted samples enter fits and evidence.
    pub admitted: Vec<bool>,
    pub clip_fraction: f64,
    pub role: EvidenceRole,
    pub eligibility: EvidenceEligibility,
}

impl PoolImage {
    pub fn is_eligible(&self) -> bool {
        self.eligibility.is_eligible()
    }
}

/// Which stage's rows a dependency set covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceStage {
    /// Native rows only.
    Native,
    /// Native and recipe rows.
    Published,
}

fn observed_with_evidence(kind: DependencyKind) -> bool {
    matches!(
        kind,
        DependencyKind::Contribution
            | DependencyKind::Image
            | DependencyKind::RootAdmission
            | DependencyKind::Transform
            | DependencyKind::Profile
            | DependencyKind::Schema
            | DependencyKind::IdentityResolution
            | DependencyKind::Standing
            | DependencyKind::RootWeighting
    )
}

/// The dependency refs of one stage's eligible rows and shared pins.
pub fn stage_dependencies(pools: &[LookPool], stage: EvidenceStage) -> Vec<DependencyRef> {
    let mut dependencies = Vec::new();
    for pool in pools {
        dependencies.push(DependencyRef::required(
            DependencyKind::Schema,
            DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
                pool.body.clone(),
                pool.observation.snapshot_version.get(),
            )),
        ));
        dependencies.push(DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(&pool.body, pool.transforms.version()),
        ));
        dependencies.extend(
            pool.observation
                .dependencies
                .iter()
                .filter(|entry| entry.dependency.kind == DependencyKind::Profile)
                .map(|entry| entry.dependency.clone()),
        );
        dependencies.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_global(),
        ));
        if let Ok(family) = LookId::new(pool.look.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::RootWeighting,
                DependencyTarget::root_weighting_family(&family),
            ));
        }
        dependencies.push(DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_body(&pool.body),
        ));
        for image in &pool.images {
            if !image.is_eligible()
                || (stage == EvidenceStage::Native && image.role != EvidenceRole::Native)
            {
                continue;
            }
            if let Ok(id) = ContributionRef::new(image.contribution.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(&id),
                ));
            }
            if let Ok(id) = ImageRef::new(image.image.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&id),
                ));
            }
            if let Ok(id) = StandingRef::new(image.credential.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::Standing,
                    DependencyTarget::standing(&id),
                ));
            }
            if let Ok(id) = IdentityResolutionRef::new(image.credential.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::IdentityResolution,
                    DependencyTarget::identity_resolution(&id),
                ));
            }
        }
    }
    dependencies.sort();
    dependencies.dedup();
    dependencies
}

/// The metadata rows and dependency generations read with one pool's evidence.
#[derive(Debug, Clone)]
pub struct EvidenceObservation {
    pub snapshot_version: CommunitySnapshotVersion,
    pub dependencies: Vec<DependencyGeneration>,
    pub root_admissions: Vec<ActiveRootAdmission>,
    pub native_contributors: BTreeMap<TrustCredentialId, EffectiveContributorId>,
    pub published_contributors: BTreeMap<TrustCredentialId, EffectiveContributorId>,
}

impl EvidenceObservation {
    /// Rejects a dependency or admission read at two different generations.
    pub fn merge<'a>(observations: impl IntoIterator<Item = &'a Self>) -> Result<Self> {
        let mut merged: Option<Self> = None;
        for observation in observations {
            match &mut merged {
                None => merged = Some(observation.clone()),
                Some(merged) => {
                    if merged.snapshot_version != observation.snapshot_version {
                        return Err(WorkerError::Data(
                            "evidence pools were read from different schema snapshots".to_owned(),
                        ));
                    }
                    for entry in &observation.dependencies {
                        match merged
                            .dependencies
                            .iter()
                            .find(|known| known.dependency == entry.dependency)
                        {
                            Some(known) if known.generation != entry.generation => {
                                return Err(WorkerError::Data(format!(
                                    "evidence pools observed conflicting generations for {} {}",
                                    entry.dependency.kind.as_str(),
                                    entry.dependency.target
                                )));
                            }
                            Some(_) => {}
                            None => merged.dependencies.push(entry.clone()),
                        }
                    }
                    for admission in &observation.root_admissions {
                        match merged
                            .root_admissions
                            .iter()
                            .find(|known| known.id == admission.id)
                        {
                            Some(known) if known != admission => {
                                return Err(WorkerError::Data(format!(
                                    "evidence pools observed conflicting root admission {}",
                                    admission.id.as_str()
                                )));
                            }
                            Some(_) => {}
                            None => merged.root_admissions.push(admission.clone()),
                        }
                    }
                    merge_contributors(
                        &mut merged.native_contributors,
                        &observation.native_contributors,
                    )?;
                    merge_contributors(
                        &mut merged.published_contributors,
                        &observation.published_contributors,
                    )?;
                }
            }
        }
        let mut merged = merged.unwrap_or(Self {
            snapshot_version: CommunitySnapshotVersion::new(0),
            dependencies: Vec::new(),
            root_admissions: Vec::new(),
            native_contributors: BTreeMap::new(),
            published_contributors: BTreeMap::new(),
        });
        merged.dependencies.sort_by(|left, right| {
            (&left.dependency.kind, &left.dependency.target)
                .cmp(&(&right.dependency.kind, &right.dependency.target))
        });
        merged.root_admissions.sort_by(|left, right| {
            (&left.admitted_at, &left.id).cmp(&(&right.admitted_at, &right.id))
        });
        Ok(merged)
    }

    /// The observed generation of every requested dependency.
    pub fn snapshot_for(
        &self,
        dependencies: &[DependencyRef],
    ) -> Result<Vec<DependencyGeneration>> {
        let mut snapshot = Vec::new();
        for dependency in dependencies {
            let Some(entry) = self
                .dependencies
                .iter()
                .find(|entry| entry.dependency == *dependency)
            else {
                return Err(WorkerError::Data(format!(
                    "dependency {} {} was not observed with the evidence",
                    dependency.kind.as_str(),
                    dependency.target
                )));
            };
            if !snapshot
                .iter()
                .any(|known: &DependencyGeneration| known.dependency == *dependency)
            {
                snapshot.push(entry.clone());
            }
        }
        Ok(snapshot)
    }

    pub fn dependency_refs(&self) -> Vec<DependencyRef> {
        self.dependencies
            .iter()
            .map(|entry| entry.dependency.clone())
            .collect()
    }

    pub fn contributor(
        &self,
        stage: EvidenceStage,
        credential: TrustCredentialId,
    ) -> EffectiveContributorId {
        let map = match stage {
            EvidenceStage::Native => &self.native_contributors,
            EvidenceStage::Published => &self.published_contributors,
        };
        map.get(&credential)
            .copied()
            .unwrap_or_else(|| EffectiveContributorId::from(credential))
    }
}

fn merge_contributors(
    merged: &mut BTreeMap<TrustCredentialId, EffectiveContributorId>,
    observed: &BTreeMap<TrustCredentialId, EffectiveContributorId>,
) -> Result<()> {
    for (credential, contributor) in observed {
        match merged.get(credential) {
            Some(known) if known != contributor => {
                return Err(WorkerError::Data(
                    "evidence pools observed conflicting contributor identities".to_owned(),
                ));
            }
            Some(_) => {}
            None => {
                merged.insert(*credential, *contributor);
            }
        }
    }
    Ok(())
}

/// Registers and snapshots refs the pool does not read, in one short transaction.
pub async fn complete_observation(
    db_pool: &SqlitePool,
    observation: &mut EvidenceObservation,
    dependencies: &[DependencyRef],
) -> Result<()> {
    let mut missing = Vec::new();
    for dependency in dependencies {
        if observation
            .dependencies
            .iter()
            .any(|entry| entry.dependency == *dependency)
        {
            continue;
        }
        if observed_with_evidence(dependency.kind) {
            return Err(WorkerError::Data(format!(
                "dependency {} {} was not observed with the evidence",
                dependency.kind.as_str(),
                dependency.target
            )));
        }
        missing.push(dependency.clone());
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let captured = model_store::observe_dependency_snapshot(&mut tx, &missing, now_unix()).await?;
    tx.commit().await?;
    for entry in captured {
        let known = observation
            .dependencies
            .iter()
            .find(|known| known.dependency == entry.dependency)
            .map(|known| known.generation);
        match known {
            Some(generation) if generation != entry.generation => {
                return Err(WorkerError::Data(format!(
                    "observation conflicts with the current generation for {} {}",
                    entry.dependency.kind.as_str(),
                    entry.dependency.target
                )));
            }
            Some(_) => {}
            None => observation.dependencies.push(entry),
        }
    }
    observation.dependencies.sort_by(|left, right| {
        (&left.dependency.kind, &left.dependency.target)
            .cmp(&(&right.dependency.kind, &right.dependency.target))
    });
    Ok(())
}

/// One body's evidence for one native family and one realization.
#[derive(Debug, Clone)]
pub struct LookPool {
    pub body: BodyId,
    pub look: LookSlug,
    pub realization: BodyRealizationId,
    pub resources: BodyResources,
    pub transforms: BodyTransforms,
    pub images: Vec<PoolImage>,
    /// The community snapshot the schemas were read from.
    pub snapshot_version: CommunitySnapshotVersion,
    /// The data root those schemas came from.
    pub data: DataDir,
    /// The metadata rows and dependency generations read with the evidence.
    pub observation: EvidenceObservation,
}

impl LookPool {
    pub fn image_index(&self, id: &ImageId) -> Option<usize> {
        self.images.iter().position(|image| &image.image.id == id)
    }

    pub fn contribution_ids(&self) -> Vec<looklab_server::ContributionId> {
        let mut ids: Vec<_> = self
            .images
            .iter()
            .map(|image| image.contribution.id.clone())
            .collect();
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        ids.dedup();
        ids
    }

    /// The realization's epoch and native sim, which key its control-response model.
    pub fn epoch_and_sim(&self) -> (u32, SimKey) {
        (
            self.realization.epoch.get(),
            self.realization.native_sim.clone(),
        )
    }
}

fn family_id(look: &LookSlug) -> Result<LookId> {
    LookId::new(look.as_str())
        .map_err(|_| WorkerError::Data(format!("look `{look}` is not a schema family id")))
}

/// The native family a contribution's source sim maps to; a recipe's target Look is metadata.
pub fn realization_scope(
    resources: &BodyResources,
    contribution: &ContributionRow,
) -> Result<Option<(BodyRealizationId, LookSlug)>> {
    let epoch = epoch_for(&resources.schema, &contribution.firmware)?;
    let Ok(sim) = SimKey::new(&contribution.sim_key) else {
        return Ok(None);
    };
    let Some(native) = epoch.native_sims.iter().find(|native| native.key == sim) else {
        return Ok(None);
    };
    let look = LookSlug::new(native.look.as_str()).map_err(|_| {
        WorkerError::Data(format!(
            "native sim `{}` has an invalid Look `{}`",
            sim.as_str(),
            native.look.as_str()
        ))
    })?;
    Ok(Some((
        BodyRealizationId {
            body: contribution.body.clone(),
            epoch: ControlEpochId::new(epoch.id),
            native_sim: sim,
        },
        look,
    )))
}

fn is_colour_contribution(contribution: &ContributionRow) -> bool {
    matches!(
        contribution.status,
        db::ContributionStatus::Pending | db::ContributionStatus::Accepted
    ) && contribution.intent != db::ContributionIntent::Card
        && contribution.render_class == looklab_upload::RenderClass::Colour
}

pub async fn realizations_for_body(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
) -> Result<Vec<BodyRealizationId>> {
    let resources = crate::body::published_data_dir(db_pool, config)
        .await?
        .load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let mut realizations = BTreeSet::new();
    for (contribution, _) in rows {
        if contribution.body != *body || !is_colour_contribution(&contribution) {
            continue;
        }
        let Some((realization, _)) = realization_scope(&resources, &contribution)? else {
            continue;
        };
        realizations.insert(realization);
    }
    Ok(realizations.into_iter().collect())
}

pub async fn looks_for_realization(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    realization: &BodyRealizationId,
) -> Result<Vec<LookSlug>> {
    let resources = crate::body::published_data_dir(db_pool, config)
        .await?
        .load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let mut looks = Vec::new();
    for (contribution, images) in rows {
        if images.is_empty() || contribution.body != *body || !is_colour_contribution(&contribution)
        {
            continue;
        }
        let Some((resolved, look)) = realization_scope(&resources, &contribution)? else {
            continue;
        };
        if resolved != *realization {
            continue;
        }
        looks.push(look);
    }
    looks.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    looks.dedup();
    Ok(looks)
}

struct ObservedPool {
    contributions: Vec<(ContributionRow, Vec<ImageRow>)>,
    credentials: HashMap<looklab_server::CredentialId, CredentialInfo>,
    observation: EvidenceObservation,
}

#[allow(clippy::too_many_arguments)]
async fn observe(
    db_pool: &SqlitePool,
    body: &BodyId,
    look: &LookSlug,
    realization: &BodyRealizationId,
    resources: &BodyResources,
    transforms_version: u32,
    profile: Option<&ProfileId>,
    snapshot_version: u64,
    snapshot_hash: Option<&str>,
) -> Result<ObservedPool> {
    let mut tx = model_store::begin_immediate(db_pool).await?;
    let identity = db::published_snapshot_identity(&mut tx).await?;
    let observed_identity = identity
        .as_ref()
        .map(|(version, hash)| (*version, hash.as_str()));
    if observed_identity != snapshot_hash.map(|hash| (snapshot_version, hash)) {
        return Err(WorkerError::Data(
            "the published schema snapshot changed while observing evidence".to_owned(),
        ));
    }
    let rows = db::body_pool_conn(&mut tx, body).await?;
    let mut contributions = Vec::new();
    for (contribution, images) in rows {
        if contribution.body != *body || !is_colour_contribution(&contribution) || images.is_empty()
        {
            continue;
        }
        let Some((resolved, family)) = realization_scope(resources, &contribution)? else {
            continue;
        };
        if resolved != *realization || &family != look {
            continue;
        }
        contributions.push((contribution, images));
    }
    let mut credentials: HashMap<looklab_server::CredentialId, CredentialInfo> = HashMap::new();
    for (contribution, _) in &contributions {
        if !credentials.contains_key(&contribution.credential) {
            let info = db::credential_conn(&mut tx, &contribution.credential).await?;
            credentials.insert(contribution.credential.clone(), info);
        }
    }
    let contribution_ids: BTreeSet<_> = contributions
        .iter()
        .map(|(contribution, _)| contribution.id.clone())
        .collect();
    let root_admissions: Vec<ActiveRootAdmission> = db::active_root_admissions(&mut tx)
        .await?
        .into_iter()
        .filter(|admission| {
            contribution_ids.contains(&admission.contribution) && admission.scope.covers(look, body)
        })
        .collect();
    let family = family_id(look)?;
    let mut dependencies = vec![
        DependencyRef::required(
            DependencyKind::Schema,
            DependencyTarget::published_schema(&PublishedSchemaSnapshot::new(
                body.clone(),
                snapshot_version,
            )),
        ),
        DependencyRef::required(
            DependencyKind::Transform,
            DependencyTarget::transform(body, transforms_version),
        ),
        DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_global(),
        ),
        DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_family(&family),
        ),
        DependencyRef::required(
            DependencyKind::RootWeighting,
            DependencyTarget::root_weighting_body(body),
        ),
    ];
    if let Some(profile) = profile {
        dependencies.push(DependencyRef::required(
            DependencyKind::Profile,
            DependencyTarget::profile(profile, transforms_version),
        ));
    }
    for (contribution, images) in &contributions {
        if let Ok(id) = ContributionRef::new(contribution.id.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::Contribution,
                DependencyTarget::contribution(&id),
            ));
        }
        if let Ok(id) = StandingRef::new(contribution.credential.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::Standing,
                DependencyTarget::standing(&id),
            ));
        }
        if let Ok(id) = IdentityResolutionRef::new(contribution.credential.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::IdentityResolution,
                DependencyTarget::identity_resolution(&id),
            ));
        }
        for image in images {
            if let Ok(id) = ImageRef::new(image.id.as_str()) {
                dependencies.push(DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&id),
                ));
            }
        }
    }
    for admission in &root_admissions {
        if let Ok(id) = RootAdmissionRef::new(admission.id.as_str()) {
            dependencies.push(DependencyRef::required(
                DependencyKind::RootAdmission,
                DependencyTarget::root_admission(&id),
            ));
        }
    }
    dependencies.sort();
    dependencies.dedup();
    let generations =
        model_store::observe_dependency_snapshot(&mut tx, &dependencies, now_unix()).await?;
    tx.commit().await?;
    Ok(ObservedPool {
        contributions,
        credentials,
        observation: EvidenceObservation {
            snapshot_version: CommunitySnapshotVersion::new(snapshot_version),
            dependencies: generations,
            root_admissions,
            native_contributors: BTreeMap::new(),
            published_contributors: BTreeMap::new(),
        },
    })
}

/// Build one realization's pool for one native family.
pub async fn build(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
    realization: &BodyRealizationId,
    scope: &EvidenceScope,
) -> Result<LookPool> {
    if &realization.body != body {
        return Err(WorkerError::Data(format!(
            "realization body {} does not match {body}",
            realization.body
        )));
    }
    let identity = {
        let mut conn = db_pool.acquire().await?;
        db::published_snapshot_identity(&mut conn).await?
    };
    let (snapshot_version, snapshot_hash, snapshot_root) = match identity {
        Some((version, hash)) => {
            let root = crate::body::verified_snapshot_root(config, version, &hash)?;
            (version, Some(hash), root)
        }
        None => (0, None, config.data.clone()),
    };
    let data = DataDir::new(snapshot_root);
    let resources = data.load(body)?;
    let epoch = resources
        .schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == realization.epoch.get())
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "epoch {} is not in the schema of {body}",
                realization.epoch.get()
            ))
        })?;
    if !epoch
        .native_sims
        .iter()
        .any(|sim| sim.key == realization.native_sim)
    {
        return Err(WorkerError::Data(format!(
            "sim `{}` is not in epoch {} of {body}",
            realization.native_sim.as_str(),
            realization.epoch.get()
        )));
    }
    let family = family_id(look)?;
    let (transforms, profile_bytes) =
        TransformDir::new(&config.transforms).load_with_bytes(body)?;
    let profile = {
        let hash = Sha256Hex::from_bytes(Sha256::digest(&profile_bytes).into());
        ProfileId::new(hash.to_hex()).ok()
    };
    let ObservedPool {
        contributions,
        credentials,
        observation,
    } = observe(
        db_pool,
        body,
        look,
        realization,
        &resources,
        transforms.version(),
        profile.as_ref(),
        snapshot_version,
        snapshot_hash.as_deref(),
    )
    .await?;

    let mut images = Vec::new();
    for (contribution, rows) in contributions {
        let credential = credentials
            .get(&contribution.credential)
            .cloned()
            .ok_or_else(|| WorkerError::Data("missing credential row".to_owned()))?;
        let contributor = EffectiveContributorId::from(credential.trust);
        let epoch_schema = epoch_for(&resources.schema, &contribution.firmware)?;
        let (settings, unknown) = model_settings(&contribution.settings, epoch_schema)?;
        let epoch = epoch_schema.id;
        let role = match contribution.intent {
            db::ContributionIntent::Native => EvidenceRole::Native,
            db::ContributionIntent::Recipe => EvidenceRole::Recipe,
            db::ContributionIntent::Card => continue,
        };
        let setting_source: BTreeMap<ControlKey, SettingSource> = contribution
            .settings
            .entries()
            .iter()
            .map(|entry| (entry.key.clone(), entry.source))
            .collect();
        let exposure_mode = exposure_mode(&resources.schema, epoch, &settings)?;
        for row in rows {
            let path = db::blob_path(&config.blobs, &row.samples_hash);
            let bytes = std::fs::read(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let samples = looklab_samples::decode(&bytes)?;
            let transform = transforms.matrix_for(None, samples.header().wb_multipliers)?;
            let (axes, admitted) = axes_and_admission(&samples, &transform, scope);
            let coverage = coverage_of(&axes)?;
            let clip_fraction = clip_fraction(&samples, &admitted);
            let capture = meta::capture_tuple(&row.meta_json)?;
            let outcome = row.status;
            let physical = physical_context_from_meta(&contribution, epoch, &row.meta_json);
            let eligibility = eligibility(
                epoch_schema,
                &family,
                body,
                role,
                physical.as_ref(),
                &settings,
                &unknown,
                scope,
            );
            let image_eligibility = if !eligibility.is_eligible() {
                eligibility
            } else if admitted.iter().any(|admitted| *admitted) {
                EvidenceEligibility::Eligible
            } else {
                EvidenceEligibility::Ineligible(vec![EvidenceBlocker::NoAdmittedSamples])
            };
            images.push(PoolImage {
                image: row,
                contribution: contribution.clone(),
                credential: credential.clone(),
                contributor,
                samples,
                transform,
                realization: realization.clone(),
                physical,
                epoch,
                settings: settings.clone(),
                setting_source: setting_source.clone(),
                exposure_mode,
                outcome,
                capture,
                coverage,
                axes,
                admitted,
                clip_fraction,
                role,
                eligibility: image_eligibility,
            });
        }
    }
    images.sort_by_key(|left| left.image.id.as_str());

    Ok(LookPool {
        body: body.clone(),
        look: look.clone(),
        realization: realization.clone(),
        resources,
        transforms,
        images,
        snapshot_version: CommunitySnapshotVersion::new(snapshot_version),
        data,
        observation,
    })
}

/// Resolves stage contributor graphs once over the union of admitted links.
pub fn resolve_contributors(pools: &mut [LookPool]) -> Result<()> {
    let (native_trust, native_links) = admitted_links(pools, false);
    let (published_trust, published_links) = admitted_links(pools, true);
    let native_map = resolve_effective_contributors(native_trust, &native_links);
    let published_map = resolve_effective_contributors(published_trust, &published_links);
    for pool in pools {
        pool.observation.native_contributors = native_map.clone();
        pool.observation.published_contributors = published_map.clone();
        for image in &mut pool.images {
            image.contributor = pool
                .observation
                .contributor(EvidenceStage::Native, image.credential.trust);
        }
    }
    Ok(())
}

fn admitted_links(
    pools: &[LookPool],
    include_recipe: bool,
) -> (BTreeSet<TrustCredentialId>, Vec<SerialLink>) {
    let mut credentials = BTreeSet::new();
    let mut links = BTreeSet::new();
    for pool in pools {
        for image in &pool.images {
            if !image.is_eligible()
                || !image.admitted.iter().any(|admitted| *admitted)
                || (!include_recipe && image.role != EvidenceRole::Native)
            {
                continue;
            }
            credentials.insert(image.credential.trust);
            if let Some(hmac) = image.contribution.serial_hmac {
                links.insert((image.credential.trust, hmac));
            }
        }
    }
    (
        credentials,
        links
            .into_iter()
            .map(|(credential, hmac)| SerialLink {
                credential,
                hmac: looklab_meta::SerialHmac::from_bytes(hmac),
            })
            .collect(),
    )
}

/// Every eligible realization pool for one body and native family.
pub async fn build_all(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
    look: &LookSlug,
    scope: &EvidenceScope,
) -> Result<Vec<LookPool>> {
    let resources = crate::body::published_data_dir(db_pool, config)
        .await?
        .load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let mut realizations = BTreeSet::new();
    for (contribution, images) in rows {
        if contribution.body != *body || !is_colour_contribution(&contribution) || images.is_empty()
        {
            continue;
        }
        let Some((realization, family)) = realization_scope(&resources, &contribution)? else {
            continue;
        };
        if &family == look {
            realizations.insert(realization);
        }
    }
    let mut pools = Vec::with_capacity(realizations.len());
    for realization in realizations {
        pools.push(build(db_pool, config, body, look, &realization, scope).await?);
    }
    resolve_contributors(&mut pools)?;
    Ok(pools)
}

pub async fn family_realizations(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
) -> Result<Vec<(BodyId, BodyRealizationId)>> {
    let data = crate::body::published_data_dir(db_pool, config).await?;
    let contributions = db::all_contributions(db_pool).await?;
    let mut by_body: BTreeMap<BodyId, Vec<ContributionRow>> = BTreeMap::new();
    for contribution in contributions {
        if !is_colour_contribution(&contribution) {
            continue;
        }
        by_body
            .entry(contribution.body.clone())
            .or_default()
            .push(contribution);
    }
    let mut scopes = BTreeSet::new();
    for (body, contributions) in by_body {
        let resources = match data.load(&body) {
            Ok(resources) => resources,
            Err(error) => {
                tracing::warn!(%body, %error, "skipping body without loadable resources");
                continue;
            }
        };
        for contribution in contributions {
            let Some((realization, family)) = realization_scope(&resources, &contribution)? else {
                continue;
            };
            if &family != look {
                continue;
            }
            scopes.insert((body.clone(), realization));
        }
    }
    Ok(scopes.into_iter().collect())
}

pub async fn family_pools(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    look: &LookSlug,
    scope: &EvidenceScope,
) -> Result<Vec<LookPool>> {
    let mut pools = Vec::new();
    for (body, realization) in family_realizations(db_pool, config, look).await? {
        pools.push(build(db_pool, config, &body, look, &realization, scope).await?);
    }
    resolve_contributors(&mut pools)?;
    Ok(pools)
}

fn model_settings(
    wire: &ControlSettings,
    epoch: &looklab_schema::Epoch,
) -> Result<(Settings, Vec<ControlKey>)> {
    let mut settings = Settings::new();
    let mut unknown = Vec::new();
    for entry in wire.entries() {
        let Some(control) = epoch
            .controls
            .iter()
            .find(|control| control.key == entry.key)
        else {
            unknown.push(entry.key.clone());
            continue;
        };
        let step = entry.step.get();
        let value = match control.kind {
            ControlType::Int | ControlType::Decimal => ControlValue::Step(step),
            ControlType::Enum => {
                let value = usize::try_from(step)
                    .ok()
                    .and_then(|index| control.values.as_ref()?.get(index))
                    .ok_or_else(|| {
                        WorkerError::Data(format!(
                            "control {} has no value at step {step}",
                            entry.key
                        ))
                    })?;
                ControlValue::Enum(value.clone())
            }
            ControlType::Bool => match step {
                0 => ControlValue::Bool(false),
                1 => ControlValue::Bool(true),
                _ => {
                    return Err(WorkerError::Data(format!(
                        "control {} has no boolean value at step {step}",
                        entry.key
                    )));
                }
            },
        };
        settings.set(entry.key.clone(), value);
    }
    Ok((settings, unknown))
}

fn default_value(control: &Control) -> Option<ControlValue> {
    match &control.default {
        ControlDefault::Number(number) => {
            let step = control.step.as_ref()?;
            number.div_exact(step).map(ControlValue::Step)
        }
        ControlDefault::Enum(value) => Some(ControlValue::Enum(value.clone())),
        ControlDefault::Bool(value) => Some(ControlValue::Bool(*value)),
    }
}

fn setting_value(value: &SettingValue) -> ControlValue {
    match value {
        SettingValue::Step(step) => ControlValue::Step(*step),
        SettingValue::Enum(value) => ControlValue::Enum(value.clone()),
        SettingValue::Bool(value) => ControlValue::Bool(*value),
    }
}

fn combination_matches(
    epoch: &looklab_schema::Epoch,
    family: &LookId,
    settings: &Settings,
    combination: &SettingCombination,
) -> bool {
    if combination.is_empty() {
        return false;
    }
    for control in epoch
        .controls
        .iter()
        .filter(|control| control.scope.includes(family) && control.look_affecting)
    {
        let Some(value) = combination.get(&control.key) else {
            return false;
        };
        if settings.get(&control.key) != Some(&setting_value(value)) {
            return false;
        }
    }
    combination
        .iter()
        .all(|(key, value)| settings.get(key) == Some(&setting_value(value)))
}

pub(crate) fn defaults_match(
    epoch: &looklab_schema::Epoch,
    family: &LookId,
    settings: &Settings,
) -> bool {
    epoch
        .controls
        .iter()
        .filter(|control| control.scope.includes(family) && control.look_affecting)
        .all(|control| {
            let Some(default) = default_value(control) else {
                return false;
            };
            settings.get(&control.key) == Some(&default)
        })
}

fn point_matches(
    point: &looklab_schema::AssessedPoint,
    defaults_assessed: bool,
    epoch: &looklab_schema::Epoch,
    family: &LookId,
    pinned: &crate::assessment::PinnedModes,
    physical: Option<&PhysicalContext>,
    settings: &Settings,
) -> bool {
    let Some((point_firmware, point_wb_mode)) = crate::assessment::resolve_point(point, pinned)
    else {
        return false;
    };
    let Some(physical) = physical else {
        return false;
    };
    if physical.firmware != point_firmware || physical.wb_mode != point_wb_mode {
        return false;
    }
    if point.settings.is_empty() {
        return defaults_assessed && defaults_match(epoch, family, settings);
    }
    combination_matches(epoch, family, settings, &point.settings)
}

fn scalar_of(control: &Control, value: &ControlValue) -> Option<ScalarValue> {
    match (control.kind, value) {
        (ControlType::Int | ControlType::Decimal, ControlValue::Step(index)) => {
            control.value_at(*index).map(ScalarValue::Number)
        }
        (ControlType::Enum, ControlValue::Enum(name)) => {
            Some(ScalarValue::Text(name.as_str().to_owned()))
        }
        (ControlType::Bool, ControlValue::Bool(value)) => Some(ScalarValue::Bool(*value)),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn eligibility(
    epoch: &looklab_schema::Epoch,
    family: &LookId,
    body: &BodyId,
    role: EvidenceRole,
    physical: Option<&PhysicalContext>,
    settings: &Settings,
    unknown: &[ControlKey],
    scope: &EvidenceScope,
) -> EvidenceEligibility {
    let mut blockers = BTreeSet::new();
    for key in unknown {
        blockers.insert(EvidenceBlocker::UnknownControl {
            control: key.clone(),
        });
    }
    for control in &epoch.controls {
        if !control.scope.includes(family) {
            continue;
        }
        if control.spatial
            && let Some(Tolerated::Values(allowed)) = control.tolerated.as_ref()
        {
            let value = settings
                .get(&control.key)
                .and_then(|value| scalar_of(control, value))
                .or_else(|| {
                    default_value(control)
                        .as_ref()
                        .and_then(|value| scalar_of(control, value))
                });
            match value {
                Some(value) if allowed.contains(&value) => {}
                Some(_) => {
                    blockers.insert(EvidenceBlocker::SpatialNotTolerated {
                        control: control.key.clone(),
                    });
                }
                None => {
                    blockers.insert(EvidenceBlocker::MissingSetting {
                        control: control.key.clone(),
                    });
                }
            }
        }
    }
    if !blockers.is_empty() {
        return EvidenceEligibility::Ineligible(blockers.into_iter().collect());
    }
    if scope.includes(epoch, family, body, role, physical, settings) {
        EvidenceEligibility::Eligible
    } else {
        EvidenceEligibility::Ineligible(vec![EvidenceBlocker::OutOfScope])
    }
}

/// The exposure mode a settings map selects: the first enum control whose value
/// names an exposure mode key; mode 0 when none does, and no schema default counts.
fn exposure_mode(
    schema: &ControlSchema,
    epoch: u32,
    settings: &Settings,
) -> Result<ExposureModeId> {
    let epoch = schema
        .epochs
        .iter()
        .find(|candidate| candidate.id == epoch)
        .ok_or_else(|| WorkerError::Data(format!("body schema has no epoch {epoch}")))?;
    for control in &epoch.controls {
        if control.kind != ControlType::Enum || !control.affects.contains(&Affects::Exposure) {
            continue;
        }
        let Some(ControlValue::Enum(selected)) = settings.get(&control.key) else {
            continue;
        };
        if let Some(index) = epoch
            .exposure_modes
            .iter()
            .position(|mode| mode.key.as_str() == selected.as_str())
        {
            return u32::try_from(index)
                .map(ExposureModeId)
                .map_err(|_| WorkerError::Data("too many exposure modes".to_owned()));
        }
    }
    Ok(ExposureModeId(0))
}

/// The schema key of the exposure mode `settings` selects.
pub(crate) fn exposure_mode_key(
    schema: &ControlSchema,
    epoch: u32,
    settings: &Settings,
) -> Result<looklab_schema::ExposureModeKey> {
    let declared = schema
        .epochs
        .iter()
        .find(|candidate| candidate.id == epoch)
        .ok_or_else(|| WorkerError::Data(format!("body schema has no epoch {epoch}")))?;
    let mode = exposure_mode(schema, epoch, settings)?;
    declared
        .exposure_modes
        .get(mode.0 as usize)
        .map(|mode| mode.key.clone())
        .ok_or_else(|| WorkerError::Data(format!("exposure mode {} is not declared", mode.0)))
}

fn axes_and_admission(
    samples: &SampleSet,
    transform: &Mat3,
    scope: &EvidenceScope,
) -> (Vec<(f64, f64, f64)>, Vec<bool>) {
    let mut axes = Vec::new();
    let mut admitted = Vec::with_capacity(samples.n());
    for rgb in samples.in_rgb() {
        let xyz = transform.mul_vec(rgb.map(f64::from));
        if !(xyz[0].is_finite() && xyz[1].is_finite() && xyz[2].is_finite()) {
            admitted.push(false);
            continue;
        }
        let input = LookInput::new(looklab_color::Xyz::from_array(xyz));
        let accepted = scope.admits_sample(&input, samples.header().output_space);
        admitted.push(accepted);
        if accepted {
            axes.push((
                input.s.clamp(EXPOSURE_MIN, EXPOSURE_MAX),
                input.h_in,
                input.c_in,
            ));
        }
    }
    (axes, admitted)
}

fn coverage_of(axes: &[(f64, f64, f64)]) -> Result<RegionCoverage> {
    let mut counts: BTreeMap<Region, f64> = BTreeMap::new();
    for (s, hue, _chroma) in axes {
        let region = Region::from_probe(*s, *hue)?;
        *counts.entry(region).or_insert(0.0) += 1.0;
    }
    Ok(RegionCoverage::from_shares(counts)?)
}

fn clip_fraction(samples: &SampleSet, admitted: &[bool]) -> f64 {
    let count = admitted.iter().filter(|admitted| **admitted).count();
    if count == 0 {
        return 0.0;
    }
    let clipped = samples
        .flags()
        .iter()
        .zip(admitted)
        .filter(|(flags, admitted)| **admitted && flags.contains(SampleFlags::NEAR_CLIP_OUTPUT))
        .count();
    clipped as f64 / count as f64
}

pub async fn looks_for_body(
    db_pool: &SqlitePool,
    config: &WorkerConfig,
    body: &BodyId,
) -> Result<Vec<LookSlug>> {
    let resources = crate::body::published_data_dir(db_pool, config)
        .await?
        .load(body)?;
    let rows = db::body_pool(db_pool, body).await?;
    let mut looks = Vec::new();
    for (contribution, images) in rows {
        if images.is_empty() || contribution.body != *body || !is_colour_contribution(&contribution)
        {
            continue;
        }
        if let Some((_, look)) = realization_scope(&resources, &contribution)? {
            looks.push(look);
        }
    }
    looks.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    looks.dedup();
    Ok(looks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numerical_identity_uses_native_mapping_for_every_role() {
        let credential_x = TrustCredentialId::from_u128(7);
        let credential_y = TrustCredentialId::from_u128(9);
        let published = EffectiveContributorId::from(TrustCredentialId::from_u128(1));
        let native = EffectiveContributorId::from(TrustCredentialId::from_u128(2));
        let observation = EvidenceObservation {
            snapshot_version: CommunitySnapshotVersion::SHIPPED,
            dependencies: Vec::new(),
            root_admissions: Vec::new(),
            native_contributors: BTreeMap::from([(credential_x, native), (credential_y, native)]),
            published_contributors: BTreeMap::from([
                (credential_x, published),
                (credential_y, native),
            ]),
        };
        assert_eq!(
            observation.contributor(EvidenceStage::Native, credential_x),
            native
        );
        assert_eq!(
            observation.contributor(EvidenceStage::Native, credential_y),
            native
        );
        assert_eq!(
            observation.contributor(EvidenceStage::Published, credential_x),
            published
        );
    }
}
