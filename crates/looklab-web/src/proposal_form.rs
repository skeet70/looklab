// SPDX-License-Identifier: AGPL-3.0-or-later
//! Parsing helpers shared by the proposal form and the confirmation form.

use std::collections::BTreeMap;
use std::str::FromStr;

use looklab_model::file::{BaseField, BaseReference, BaseStatus, NEUTRAL_BASELINE_SLUG};
use looklab_publish::{LookStatus, LookVisibility, SOURCE_LUT_ARTIFACT, SiteIndex};
use looklab_schema::{
    Affects, Body, BodyId, BodyKind, BodyStatus, Capabilities, ColorSpace, ControlChange,
    ControlDefinition, ControlKey, ControlSchema, ControlType, Date, EnumValue, FirmwareVersion,
    FixtureDonation, FixtureLicense, LabeledFile, LabeledTag, LicenseDeclaration, LutEmulationDecl,
    LutEncoding, LutFile, LutLookProposal, LutLookVersion, Makernote, MakernoteTag, MakernoteValue,
    MappingInference, MappingProposal, MappingStatus, NativeSim, Placement, Range, Rational,
    ScalarValue, Sha256Hex, SimKey, Tolerated, encode_proposal, infer_mapping,
    validate_confirmation,
};

pub fn kind_label(kind: looklab_schema::ProposalKind) -> &'static str {
    use looklab_schema::ProposalKind;
    match kind {
        ProposalKind::Body => "Add a camera",
        ProposalKind::Mapping => "Add or fix a film simulation on a camera",
        ProposalKind::SchemaChange => "Change a camera's controls",
        ProposalKind::LutLook => "Add a custom look",
        ProposalKind::InterfaceProfile => "Add an editing-app pipeline",
        ProposalKind::EpochSplit => "Separate firmware versions",
        ProposalKind::LookSplit => "Separate looks",
        ProposalKind::GateOverride => "Review a release exception",
    }
}

pub fn menu_summary(
    kind: looklab_schema::ProposalKind,
    summary: &looklab_schema::ProposalSummary,
) -> looklab_schema::ProposalSummary {
    use looklab_schema::ProposalKind;
    let headline = match kind {
        ProposalKind::Body => summary
            .headline
            .replace("camera body", "camera")
            .replace("software body", "editing app"),
        ProposalKind::SchemaChange => summary.headline.replace("control schema", "controls"),
        ProposalKind::LutLook => summary.headline.replace("LUT-defined look", "custom look"),
        ProposalKind::InterfaceProfile => summary
            .headline
            .replace("software interface profile", "editing-app pipeline"),
        ProposalKind::EpochSplit => summary.headline.replace("new epoch", "new firmware range"),
        _ => summary.headline.clone(),
    };
    let details = if kind == ProposalKind::SchemaChange {
        summary.details.clone()
    } else {
        Vec::new()
    };
    looklab_schema::ProposalSummary::with_details(headline, details)
}

use sha2::{Digest, Sha256};

pub fn file_menu_value(bytes: &[u8]) -> Result<String, String> {
    let metadata = looklab_meta::parse(bytes).map_err(|error| error.to_string())?;
    let look = metadata.fuji.map(|fuji| fuji.look).ok_or_else(|| {
        "This file has no supported camera menu information. Try an original camera JPEG."
            .to_owned()
    })?;
    let values = [
        ("FilmMode", look.film_mode.map(|value| value.raw)),
        ("DynamicRange", look.dynamic_range.map(|value| value.raw)),
        (
            "DynamicRangeSetting",
            look.dynamic_range_setting.map(|value| value.raw),
        ),
        (
            "DevelopmentDynamicRange",
            look.development_dynamic_range.map(|value| value.raw),
        ),
        ("GrainSize", look.grain_size.map(|value| value.raw)),
        (
            "GrainRoughness",
            look.grain_roughness.map(|value| value.raw),
        ),
        (
            "ColorChromeEffect",
            look.color_chrome_effect.map(|value| value.raw),
        ),
        (
            "ColorChromeFxBlue",
            look.color_chrome_fx_blue.map(|value| value.raw),
        ),
        ("WhiteBalance", look.white_balance.map(|value| value.raw)),
        ("HighlightTone", look.highlight_tone.map(|value| value.raw)),
        ("ShadowTone", look.shadow_tone.map(|value| value.raw)),
        (
            "NoiseReduction",
            look.noise_reduction.map(|value| value.raw),
        ),
    ];
    let tags = values
        .into_iter()
        .filter_map(|(tag, value)| {
            value
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| format!("{tag}={value}"))
        })
        .collect::<Vec<_>>();
    if tags.is_empty() {
        return Err(
            "This file has no supported camera menu information. Try an original camera JPEG."
                .to_owned(),
        );
    }
    Ok(tags.join("\n"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct FixtureFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub sha256: Sha256Hex,
}

impl FixtureFile {
    /// # Errors
    /// Reports a file with no name.
    pub fn new(name: String, bytes: Vec<u8>) -> Result<Self, String> {
        if name.is_empty() {
            return Err("the browser reported a file with no name".to_owned());
        }
        let sha256 = digest(&bytes)?;
        Ok(Self {
            name,
            bytes,
            sha256,
        })
    }
}

fn digest(bytes: &[u8]) -> Result<Sha256Hex, String> {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Sha256Hex::parse(&hex).map_err(|error| error.to_string())
}

/// # Errors
/// Reports a malformed line, an unknown menu answer or an unknown tag.
pub fn parse_labeled_files(text: &str) -> Result<MappingInference, String> {
    let mut files = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (label, rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("line {}: expected `answer Tag=value`", index + 1))?;
        let label =
            SimKey::try_from(label).map_err(|error| format!("line {}: {error}", index + 1))?;
        let mut tags = BTreeMap::new();
        for token in rest.split_whitespace() {
            let (tag, value) = token
                .split_once('=')
                .ok_or_else(|| format!("line {}: `{token}` is not `Tag=value`", index + 1))?;
            let tag = MakernoteTag::try_from(tag)
                .map_err(|error| format!("line {}: {error}", index + 1))?;
            let value: u64 = match value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse(),
            }
            .map_err(|_| format!("line {}: `{value}` is not a makernote value", index + 1))?;
            tags.insert(tag, MakernoteValue(value));
        }
        files.push(LabeledFile { label, tags });
    }
    infer_mapping(&files).map_err(|error| error.to_string())
}

/// # Errors
/// Reports a malformed line, an unknown tag, or a tag outside the confirmation whitelist.
pub fn parse_tag_values(text: &str, label: &str) -> Result<Vec<LabeledTag>, String> {
    let label = SimKey::try_from(label).map_err(|error| error.to_string())?;
    let mut tags = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (tag, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {}: `{line}` is not `Tag=value`", index + 1))?;
        let tag = MakernoteTag::try_from(tag.trim())
            .map_err(|error| format!("line {}: {error}", index + 1))?;
        let value: u64 = match value.trim().strip_prefix("0x") {
            Some(hex) => u64::from_str_radix(hex, 16),
            None => value.trim().parse(),
        }
        .map_err(|_| format!("line {}: not a makernote value", index + 1))?;
        tags.push(LabeledTag {
            tag,
            value: MakernoteValue(value),
            label: label.clone(),
        });
    }
    validate_confirmation(&tags).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(tags)
}

/// # Errors
/// Reports a donation the fixture manifest would reject.
pub fn donation(
    file: &FixtureFile,
    make: &str,
    model: &str,
    source: String,
) -> Result<FixtureDonation, String> {
    let donated_on = Date::parse(&today()).map_err(|error| error.to_string())?;
    let size = u64::try_from(file.bytes.len()).map_err(|_| "file is too large".to_owned())?;
    let donation = FixtureDonation {
        file: file.name.clone(),
        description: format!("Donated with a proposal: {}", file.name),
        make: make.trim().to_owned(),
        model: model.trim().to_owned(),
        sha256: file.sha256,
        size,
        source_url: None,
        source: Some(source),
        donator: None,
        donated_on,
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    };
    donation.validate().map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(donation)
}

#[derive(Clone, Debug, PartialEq)]
pub struct BodyDraft {
    pub make: String,
    pub model: String,
    pub kind: BodyKind,
    pub mapping_status: MappingStatus,
    pub in_camera_raw_conversion: bool,
    pub jpeg_color_spaces: Vec<String>,
    pub heif: bool,
    pub exif_aliases: Vec<String>,
}

impl BodyDraft {
    /// # Errors
    /// A name that reduces to an empty slug, or a color space with whitespace.
    pub fn payload(&self) -> Result<String, String> {
        let id = looklab_schema::body_id_from_name(&self.make, &self.model)
            .map_err(|error| error.to_string())?;
        let spaces = self
            .jpeg_color_spaces
            .iter()
            .map(|space| ColorSpace::try_from(space.clone()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let body = Body {
            id,
            make: self.make.trim().to_owned(),
            model: self.model.trim().to_owned(),
            exif_aliases: self.exif_aliases.clone(),
            kind: self.kind,
            capabilities: Capabilities {
                in_camera_raw_conversion: self.in_camera_raw_conversion,
                jpeg_color_spaces: spaces,
                heif: self.heif,
            },
            status: BodyStatus::Proposed,
            mapping_status: self.mapping_status,
        };
        encode_proposal(&body).map_err(join_validation)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MappingDraft {
    pub body: BodyId,
    pub epoch: u32,
    pub sim_key: SimKey,
    pub look: looklab_schema::LookId,
    pub firmware_from: FirmwareVersion,
    pub tag: MakernoteTag,
    pub value: u64,
    pub monochrome: bool,
}

impl MappingDraft {
    /// # Errors
    /// A payload the YAML serializer rejects.
    pub fn payload(&self) -> Result<String, String> {
        let proposal = MappingProposal {
            body: self.body.clone(),
            epoch: self.epoch,
            sim_key: self.sim_key.clone(),
            look: self.look.clone(),
            firmware_from: self.firmware_from.clone(),
            firmware_to: None,
            makernote: Makernote {
                tag: self.tag.clone(),
                value: MakernoteValue(self.value),
            },
            monochrome: self.monochrome,
        };
        encode_proposal(&proposal).map_err(join_validation)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LutLookDraft {
    pub look: looklab_schema::LookId,
    pub name: String,
    pub license: LicenseDeclaration,
    pub attribution: String,
    pub lut: LutFile,
    pub encoding: LutEncoding,
    pub emulates: Option<LutEmulationDecl>,
}

impl LutLookDraft {
    /// # Errors
    /// A payload the YAML serializer rejects.
    pub fn payload(&self) -> Result<String, String> {
        let proposal = LutLookProposal {
            look: self.look.clone(),
            name: self.name.clone(),
            license: self.license.clone(),
            attribution: self.attribution.clone(),
            lut: self.lut.clone(),
            encoding: self.encoding.clone(),
            emulates: self.emulates.clone(),
        };
        encode_proposal(&proposal).map_err(join_validation)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CubeSummary {
    pub title: Option<String>,
    pub size: u32,
    pub domain: [f64; 2],
}

#[derive(Clone, Debug, PartialEq)]
pub struct LutFileDraft {
    pub name: String,
    pub bytes: Vec<u8>,
    pub sha256: Sha256Hex,
    pub summary: CubeSummary,
}

impl LutFileDraft {
    pub fn file(&self) -> LutFile {
        LutFile {
            file: self.name.clone(),
            sha256: self.sha256,
            size: self.bytes.len() as u64,
        }
    }
}

/// # Errors
/// A file that is not a supported 3D `.cube`, or one over the 16 MiB cap.
pub fn parse_cube(name: String, bytes: Vec<u8>) -> Result<LutFileDraft, String> {
    if bytes.len() as u64 > looklab_schema::MAX_LUT_BYTES {
        return Err("Choose a .cube file under 16 MiB.".to_owned());
    }
    looklab_schema::FixtureFileName::parse(&name)
        .map_err(|_| "Choose a .cube file with a plain file name.".to_owned())?;
    if !name.ends_with(".cube") {
        return Err("Choose a .cube file.".to_owned());
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| "This .cube file is not text.".to_owned())?;
    let cube = looklab_export::read_cube(text).map_err(|error| error.to_string())?;
    looklab_export::lut_data_from_cube(&cube).map_err(|error| error.to_string())?;
    let title = text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("TITLE")
            .map(|value| value.trim().trim_matches('"').to_owned())
            .filter(|value| !value.is_empty())
    });
    let sha256 = digest(&bytes)?;
    Ok(LutFileDraft {
        name,
        bytes,
        sha256,
        summary: CubeSummary {
            title,
            size: cube.lut.size,
            domain: cube.domain,
        },
    })
}

/// # Errors
/// The base schema or an edit fails validation.
pub fn schema_payload(
    base: &str,
    edits: &[(u32, ControlChange)],
) -> Result<String, Vec<looklab_schema::ValidationError>> {
    looklab_schema::apply_control_changes(base, edits)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimIdentity {
    pub epoch: u32,
    pub key: SimKey,
}

pub fn inferred_makernote(text: &str, name: &str) -> Option<(MakernoteTag, u64)> {
    let key = looklab_schema::sim_key_from_name(name).ok()?;
    let inference = parse_labeled_files(text).ok()?;
    let best = inference.best()?;
    let value = best.value_for(&key)?;
    Some((best.tag.clone(), value.0))
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimSuggestion {
    pub identity: SimIdentity,
    pub firmware_from: String,
    pub firmware_to: String,
    pub look: looklab_schema::LookId,
    pub monochrome: bool,
}

/// Every existing simulation in this camera whose key or Look link the name reaches.
pub fn suggest_sims(schema: &ControlSchema, name: &str) -> Vec<SimSuggestion> {
    let Ok(key) = looklab_schema::sim_key_from_name(name) else {
        return Vec::new();
    };
    let Ok(look) = looklab_schema::look_id_from_name(name) else {
        return Vec::new();
    };
    schema
        .epochs
        .iter()
        .flat_map(|epoch| {
            epoch
                .native_sims
                .iter()
                .filter(|sim| sim.key == key || sim.look == look)
                .map(|sim| SimSuggestion {
                    identity: SimIdentity {
                        epoch: epoch.id,
                        key: sim.key.clone(),
                    },
                    firmware_from: epoch.firmware.from.as_str().to_owned(),
                    firmware_to: epoch.firmware.to.as_str().to_owned(),
                    look: sim.look.clone(),
                    monochrome: sim.monochrome,
                })
        })
        .collect()
}

pub fn sim_correction(
    schema: &ControlSchema,
    identity: &SimIdentity,
    target_epoch: u32,
) -> Result<NativeSim, String> {
    let source = schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == identity.epoch)
        .ok_or_else(|| "Choose the simulation to correct.".to_owned())?;
    let sim = source
        .native_sims
        .iter()
        .find(|sim| sim.key == identity.key)
        .ok_or_else(|| "Choose the simulation to correct.".to_owned())?;
    let target = schema
        .epochs
        .iter()
        .find(|epoch| epoch.id == target_epoch)
        .ok_or_else(|| "Choose a firmware range.".to_owned())?;
    if target
        .native_sims
        .iter()
        .any(|existing| existing.key == sim.key)
    {
        return Err(
            "That simulation is already available in this firmware range. Choose the range where it actually appeared."
                .to_owned(),
        );
    }
    Ok(sim.clone())
}

pub fn parse_rational(value: &str) -> Result<Rational, String> {
    Rational::parse(value).map_err(|error| error.to_string())
}

pub fn parse_step(value: &str) -> Result<Rational, String> {
    Rational::parse_step(value).map_err(|error| error.to_string())
}

pub fn parse_enum_values(value: &str) -> Result<Vec<EnumValue>, String> {
    value
        .split([',', ' ', '\n'])
        .filter(|value| !value.is_empty())
        .map(|value| EnumValue::new(value).map_err(|error| error.to_string()))
        .collect()
}

pub fn parse_affects(value: &str) -> Result<Vec<Affects>, String> {
    value
        .split([',', ' ', '\n'])
        .filter(|value| !value.is_empty())
        .map(|value| Affects::from_str(value).map_err(|()| format!("Unknown effect `{value}`.")))
        .collect()
}

pub fn parse_scalar(value: &str, kind: ControlType) -> Result<ScalarValue, String> {
    match kind {
        ControlType::Int | ControlType::Decimal => Rational::parse(value)
            .map(ScalarValue::Number)
            .map_err(|error| error.to_string()),
        ControlType::Enum => EnumValue::new(value.trim())
            .map(|value| ScalarValue::Text(value.to_string()))
            .map_err(|error| error.to_string()),
        ControlType::Bool => match value.trim() {
            "true" => Ok(ScalarValue::Bool(true)),
            "false" => Ok(ScalarValue::Bool(false)),
            _ => Err("Enter true or false.".to_owned()),
        },
    }
}

/// An absent field stays absent; only written text produces an edit.
///
/// # Errors
/// Non-empty text with no value tokens.
pub fn parse_optional_tolerated(value: &str) -> Result<Option<Tolerated>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed == "any" {
        return Ok(Some(Tolerated::Any));
    }
    let values = trimmed
        .split([',', ' '])
        .filter(|value| !value.is_empty())
        .map(ScalarValue::from_raw)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Err("Enter tolerated values, `any`, or leave the field blank.".to_owned());
    }
    Ok(Some(Tolerated::Values(values)))
}

pub fn scalar_text(value: &ScalarValue) -> String {
    match value {
        ScalarValue::Bool(value) => value.to_string(),
        ScalarValue::Number(value) => value.to_string(),
        ScalarValue::Text(value) => value.clone(),
    }
}

pub fn tolerated_text(value: &Tolerated) -> String {
    match value {
        Tolerated::Any => "any".to_owned(),
        Tolerated::Values(values) => values
            .iter()
            .map(scalar_text)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

pub fn control_kind(kind: ControlType) -> &'static str {
    match kind {
        ControlType::Int => "Whole numbers",
        ControlType::Decimal => "Decimals",
        ControlType::Enum => "Menu choices",
        ControlType::Bool => "On or off",
    }
}

/// One published Look version the importer can resolve as a display base.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseLookVersion {
    pub look: looklab_schema::LookId,
    pub version: LutLookVersion,
    pub name: String,
    pub matchable: bool,
}

pub fn base_look_versions(index: &SiteIndex) -> Vec<BaseLookVersion> {
    index
        .looks
        .iter()
        .filter(|look| look.visibility == LookVisibility::Public)
        .flat_map(|look| {
            look.versions
                .iter()
                .filter(|version| admissible_base_version(version))
                .filter_map(|version| {
                    Some(BaseLookVersion {
                        look: looklab_schema::LookId::new(look.slug.as_str()).ok()?,
                        version: LutLookVersion::new(version.version.as_str()).ok()?,
                        name: look.name.clone(),
                        matchable: version.matchable,
                    })
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn admissible_base_version(version: &looklab_publish::VersionSummary) -> bool {
    matches!(version.status, LookStatus::Published | LookStatus::Rooted)
        && !matches!(
            &version.applicability,
            looklab_schema::Applicability::Invalidated { .. }
        )
        && version.support != looklab_publish::PublishedSupport::EarlyEstimate
        && (version.matchable || version.artifact.as_str() == SOURCE_LUT_ARTIFACT)
}

/// A neutral baseline row, unless the index already publishes that Look version.
pub fn neutral_base(versions: &[BaseLookVersion]) -> Option<BaseLookVersion> {
    let look = looklab_schema::LookId::new("looklab-neutral-srgb-v1").ok()?;
    if versions.iter().any(|version| version.look == look) {
        return None;
    }
    Some(BaseLookVersion {
        look,
        version: LutLookVersion::new("1.0.0").ok()?,
        name: "Neutral baseline".to_owned(),
        matchable: true,
    })
}

/// A path a custom look can offer after its display base is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BasePath {
    /// Available now.
    Now,
    /// Available once the destination camera's model is ready for the settings.
    WhenCameraReady,
    /// Available once the named base has a measured rendering.
    WhenBaseMeasured,
    /// Not available for this base.
    NotOffered,
}

/// Which data the browser has for the base camera's published schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BaseData {
    Loading,
    Ready,
    Unavailable,
}

/// The paths a display base offers before a destination camera is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BasePaths {
    pub preview: BasePath,
    pub settings: BasePath,
    pub card: BasePath,
}

/// The resolved base status and the paths it offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BaseOutcome {
    pub status: BaseStatus,
    pub neutral_baseline: bool,
    pub data: BaseData,
    pub paths: BasePaths,
}

impl BaseOutcome {
    pub(crate) fn of(
        base: &BaseReference,
        schema: Option<&ControlSchema>,
        data: BaseData,
        matchable: bool,
    ) -> Self {
        let status = if matches!(base, BaseReference::Look { .. }) && !matchable {
            BaseStatus::BaseIdentifiedUnmeasured
        } else {
            base.resolve_status(schema)
        };
        let neutral_baseline = matches!(
            base,
            BaseReference::Look { slug, .. } if slug.as_str() == NEUTRAL_BASELINE_SLUG
        );
        Self {
            status,
            neutral_baseline,
            data,
            paths: base_paths(status),
        }
    }
}

/// Every base status maps to one typed set of available paths.
pub(crate) fn base_paths(status: BaseStatus) -> BasePaths {
    match status {
        BaseStatus::Pinned => BasePaths {
            preview: BasePath::Now,
            settings: BasePath::WhenCameraReady,
            card: BasePath::Now,
        },
        BaseStatus::BaseIdentifiedUnmeasured => BasePaths {
            preview: BasePath::Now,
            settings: BasePath::WhenBaseMeasured,
            card: BasePath::WhenBaseMeasured,
        },
        BaseStatus::BaseIncomplete { .. } | BaseStatus::BaseInconsistent { .. } => BasePaths {
            preview: BasePath::Now,
            settings: BasePath::NotOffered,
            card: BasePath::NotOffered,
        },
    }
}

/// A base field in photographer words.
pub(crate) fn base_field_label(field: BaseField) -> &'static str {
    match field {
        BaseField::Body => "camera",
        BaseField::Renderer => "rendering app",
        BaseField::FilmSimulation => "film simulation",
        BaseField::DynamicRange => "dynamic range",
    }
}

/// A control to add to an epoch that does not declare it.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlDefinitionDraft {
    pub key: ControlKey,
    pub kind: ControlType,
    pub range: Option<Range>,
    pub step: Option<Rational>,
    pub default: Option<ScalarValue>,
    pub values: Option<Vec<EnumValue>>,
    pub look_affecting: bool,
    pub affects: Vec<Affects>,
    pub spatial: bool,
    pub tolerated: Option<Tolerated>,
}

pub fn control_definition(draft: ControlDefinitionDraft) -> ControlDefinition {
    ControlDefinition {
        key: draft.key,
        kind: draft.kind,
        range: draft.range,
        step: draft.step,
        default: draft.default,
        values: draft.values,
        look_affecting: draft.look_affecting,
        affects: draft.affects,
        spatial: draft.spatial,
        tolerated: draft.tolerated,
    }
}

fn join_validation(errors: Vec<looklab_schema::ValidationError>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn today() -> String {
    let iso = js_sys::Date::new_0().to_iso_string();
    iso.as_string()
        .and_then(|text| text.get(0..10).map(str::to_owned))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_epoch_schema() -> ControlSchema {
        let yaml = r#"
body: fujifilm/x-s10
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls: []
    solve_plan: []
  - id: 2
    firmware: { from: "2.00", to: "2.99" }
    native_sims:
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls: []
    solve_plan: []
"#;
        looklab_schema::validate_control_schema(yaml).expect("schema")
    }

    fn two_epoch_schema_with_another_second_sim() -> ControlSchema {
        let yaml = r#"
body: fujifilm/x-s10
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls: []
    solve_plan: []
  - id: 2
    firmware: { from: "2.00", to: "2.99" }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0600 }
    exposure_modes: []
    controls: []
    solve_plan: []
"#;
        looklab_schema::validate_control_schema(yaml).expect("schema")
    }

    #[test]
    fn labeling_files_infers_the_current_sim_value() {
        let text = "provia FilmMode=0x0000\nacros FilmMode=0x0500";
        let (tag, value) = inferred_makernote(text, "acros").expect("inferred makernote");
        assert_eq!(tag.as_str(), "FilmMode");
        assert_eq!(value, 0x0500);
    }

    #[test]
    fn a_target_epoch_that_already_lists_the_sim_is_not_a_correction() {
        let schema = two_epoch_schema();
        let identity = SimIdentity {
            epoch: 1,
            key: SimKey::new("provia").expect("key"),
        };
        assert!(sim_correction(&schema, &identity, 2).is_err());
    }

    #[test]
    fn a_target_epoch_without_the_sim_accepts_the_correction() {
        let schema = two_epoch_schema_with_another_second_sim();
        let identity = SimIdentity {
            epoch: 1,
            key: SimKey::new("provia").expect("key"),
        };
        assert!(sim_correction(&schema, &identity, 2).is_ok());
    }

    #[test]
    fn a_sim_key_in_two_epochs_keeps_both_identities() {
        let suggestions = suggest_sims(&two_epoch_schema(), "provia");
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].identity.epoch, 1);
        assert_eq!(suggestions[1].identity.epoch, 2);
        assert_eq!(suggestions[0].identity.key, suggestions[1].identity.key);
    }

    #[test]
    fn the_neutral_baseline_is_not_offered_twice() {
        let neutral = neutral_base(&[]).expect("neutral baseline");
        assert!(neutral_base(std::slice::from_ref(&neutral)).is_none());
    }

    fn base_schema() -> ControlSchema {
        let yaml = r#"
body: fujifilm/x-s10
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }
    exposure_modes: []
    controls:
      - key: dynamic_range
        type: enum
        values: [dr100, dr200, dr400]
        default: dr100
        look_affecting: true
        affects: [tone]
    solve_plan: []
"#;
        looklab_schema::validate_control_schema(yaml).expect("schema")
    }

    #[test]
    fn base_outcomes_follow_the_resolved_status() {
        use looklab_model::file::{DynamicRange, LookVersion, Renderer, Slug};

        let schema = base_schema();
        let neutral = BaseReference::neutral_baseline();
        let outcome = BaseOutcome::of(&neutral, None, BaseData::Unavailable, true);
        assert_eq!(outcome.status, BaseStatus::Pinned);
        assert!(outcome.neutral_baseline);
        assert_eq!(outcome.paths, base_paths(BaseStatus::Pinned));

        let published = BaseReference::Look {
            slug: Slug::new("provia").expect("slug"),
            version: LookVersion::new("1.0.0").expect("version"),
        };
        let outcome = BaseOutcome::of(&published, None, BaseData::Unavailable, true);
        assert_eq!(outcome.status, BaseStatus::Pinned);
        assert!(!outcome.neutral_baseline);

        let preview = BaseReference::Look {
            slug: Slug::new("source-preview").expect("slug"),
            version: LookVersion::new("1.0.0").expect("version"),
        };
        let outcome = BaseOutcome::of(&preview, None, BaseData::Unavailable, false);
        assert_eq!(outcome.status, BaseStatus::BaseIdentifiedUnmeasured);
        assert_eq!(
            outcome.paths,
            base_paths(BaseStatus::BaseIdentifiedUnmeasured)
        );

        let family = BaseReference::FamilyRendering {
            maker: "Fujifilm".to_owned(),
            rendering: "Classic Chrome".to_owned(),
            dynamic_range: None,
            notes: String::new(),
        };
        assert_eq!(
            BaseOutcome::of(&family, None, BaseData::Unavailable, true).status,
            BaseStatus::BaseIdentifiedUnmeasured
        );

        let body = |sim: Option<&str>, dynamic_range: Option<DynamicRange>| {
            BaseReference::BodyRealization {
                body: BodyId::new("fujifilm/x-s10").expect("body"),
                renderer: Renderer::XRawStudio,
                film_simulation: sim.map(|key| SimKey::new(key).expect("sim")),
                dynamic_range,
                notes: String::new(),
            }
        };
        assert_eq!(
            BaseOutcome::of(&body(None, None), None, BaseData::Unavailable, true).status,
            BaseStatus::BaseIncomplete {
                missing: BaseField::Body
            }
        );
        assert_eq!(
            BaseOutcome::of(&body(None, None), Some(&schema), BaseData::Ready, true).status,
            BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation
            }
        );
        assert_eq!(
            BaseOutcome::of(
                &body(Some("provia"), None),
                Some(&schema),
                BaseData::Ready,
                true
            )
            .status,
            BaseStatus::BaseIncomplete {
                missing: BaseField::DynamicRange
            }
        );
        assert_eq!(
            BaseOutcome::of(
                &body(Some("provia"), Some(DynamicRange::Dr200)),
                Some(&schema),
                BaseData::Ready,
                true,
            )
            .status,
            BaseStatus::BaseIdentifiedUnmeasured
        );
        assert_eq!(
            BaseOutcome::of(
                &body(Some("eterna"), Some(DynamicRange::Dr200)),
                Some(&schema),
                BaseData::Ready,
                true,
            )
            .status,
            BaseStatus::BaseInconsistent {
                field: BaseField::FilmSimulation
            }
        );
    }

    #[test]
    fn every_base_status_maps_to_its_typed_paths() {
        let measured = BasePaths {
            preview: BasePath::Now,
            settings: BasePath::WhenCameraReady,
            card: BasePath::Now,
        };
        let unmeasured = BasePaths {
            preview: BasePath::Now,
            settings: BasePath::WhenBaseMeasured,
            card: BasePath::WhenBaseMeasured,
        };
        let incomplete = BasePaths {
            preview: BasePath::Now,
            settings: BasePath::NotOffered,
            card: BasePath::NotOffered,
        };
        for (status, expected) in [
            (BaseStatus::Pinned, measured),
            (BaseStatus::BaseIdentifiedUnmeasured, unmeasured),
            (
                BaseStatus::BaseIncomplete {
                    missing: BaseField::Body,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseIncomplete {
                    missing: BaseField::Renderer,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseIncomplete {
                    missing: BaseField::FilmSimulation,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseIncomplete {
                    missing: BaseField::DynamicRange,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseInconsistent {
                    field: BaseField::Body,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseInconsistent {
                    field: BaseField::Renderer,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseInconsistent {
                    field: BaseField::FilmSimulation,
                },
                incomplete,
            ),
            (
                BaseStatus::BaseInconsistent {
                    field: BaseField::DynamicRange,
                },
                incomplete,
            ),
        ] {
            assert_eq!(base_paths(status), expected, "{status:?}");
        }
    }

    #[test]
    fn base_options_include_source_only_previews_without_test_fixtures() {
        use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
        use looklab_publish::{
            ArtifactHash, ArtifactName, DOCUMENT_ARTIFACT, LookStatus, LookSummary, LookVisibility,
            SOURCE_LUT_ARTIFACT, SiteIndex, SitePath, VersionSummary,
        };

        fn version(
            matchable: bool,
            artifact: &str,
            status: LookStatus,
            applicability: looklab_schema::Applicability,
        ) -> VersionSummary {
            VersionSummary {
                support: looklab_publish::PublishedSupport::Unstated,
                version: LookVersion::new("1.0.0").expect("version"),
                status,
                matchable,
                card_eligible: false,
                approximated: false,
                applicability,
                eligibility_status: None,
                card: None,
                manifest: SitePath::new("looks/test/1.0.0/manifest.json").expect("manifest"),
                manifest_sha256: ArtifactHash::of(b"manifest"),
                manifest_bytes: 8,
                artifact: ArtifactName::new(artifact).expect("artifact"),
                sha256: ArtifactHash::of(b"artifact"),
                bytes: 8,
            }
        }

        fn look(
            slug: &str,
            visibility: LookVisibility,
            versions: Vec<VersionSummary>,
        ) -> LookSummary {
            LookSummary {
                slug: Slug::new(slug).expect("slug"),
                name: slug.to_owned(),
                kind: LookKind::LutDefined,
                wb_policy: WbPolicy::Adaptive,
                visibility,
                head: None,
                selected: None,
                versions,
            }
        }

        let mut ineligible = version(
            true,
            DOCUMENT_ARTIFACT,
            LookStatus::Published,
            looklab_schema::Applicability::Current,
        );
        ineligible.eligibility_status = Some(looklab_schema::TargetEligibilityStatus::Ineligible);
        let index = SiteIndex {
            format: looklab_publish::SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies: Vec::new(),
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![
                look(
                    "layered",
                    LookVisibility::Public,
                    vec![version(
                        true,
                        DOCUMENT_ARTIFACT,
                        LookStatus::Published,
                        looklab_schema::Applicability::Current,
                    )],
                ),
                look(
                    "source-preview",
                    LookVisibility::Public,
                    vec![version(
                        false,
                        SOURCE_LUT_ARTIFACT,
                        LookStatus::Published,
                        looklab_schema::Applicability::Current,
                    )],
                ),
                look(
                    "fixture-preview",
                    LookVisibility::TestFixture,
                    vec![version(
                        false,
                        SOURCE_LUT_ARTIFACT,
                        LookStatus::Published,
                        looklab_schema::Applicability::Current,
                    )],
                ),
                look(
                    "invalidated-preview",
                    LookVisibility::Public,
                    vec![version(
                        false,
                        SOURCE_LUT_ARTIFACT,
                        LookStatus::Published,
                        looklab_schema::Applicability::Invalidated {
                            reason: looklab_schema::InvalidationReason::Withdrawal,
                        },
                    )],
                ),
                look(
                    "provisional",
                    LookVisibility::Public,
                    vec![version(
                        true,
                        DOCUMENT_ARTIFACT,
                        LookStatus::Provisional,
                        looklab_schema::Applicability::Current,
                    )],
                ),
                look(
                    "ineligible-target",
                    LookVisibility::Public,
                    vec![ineligible],
                ),
            ],
            scenes: Vec::new(),
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        };

        let bases = base_look_versions(&index);
        let names: Vec<&str> = bases.iter().map(|base| base.look.as_str()).collect();
        assert_eq!(
            names,
            vec!["layered", "source-preview", "ineligible-target"]
        );
        assert!(bases[0].matchable);
        assert!(!bases[1].matchable);
        assert!(bases[2].matchable);
    }

    #[test]
    fn an_absent_tolerated_field_stays_absent() {
        assert_eq!(parse_optional_tolerated(""), Ok(None));
        assert_eq!(parse_optional_tolerated("any"), Ok(Some(Tolerated::Any)));
    }
}
