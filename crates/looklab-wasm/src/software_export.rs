// SPDX-License-Identifier: AGPL-3.0-or-later
//! Software-pipeline export: bake, validate and write an interface profile artifact.

use looklab_color::Mat3;
use looklab_color::dng::{CalibrationMatrix, DngColorSpec};
pub use looklab_export::CctK;
use looklab_export::{
    Artifact, BakeIlluminant, BakeOutput, Baker, CctReport, DeltaStats, DngInputTransform, Format,
    InputTransform, ValidationReport, cube_files, validate, write_haldclut, write_native_json,
};
use looklab_model::file::{LookVersion, Slug};
use looklab_model::probes;
use looklab_model::{LookDocument, MatchRefusal};
use looklab_publish::InputTransformVersion;
use looklab_schema::{
    BodyId, IlluminantHandling, InsertionPoint, InterfaceId, InterfaceProfile,
    validate_interface_profile,
};
use serde::{Deserialize, Serialize};

const TRANSFORM_FORMAT: &str = "looklab-transform/1";

/// Changes to exported bytes require a new engine identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SoftwareExportVersion {
    V1,
}

impl SoftwareExportVersion {
    pub const CURRENT: Self = Self::V1;
}

impl core::fmt::Display for SoftwareExportVersion {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::V1 => "looklab-software-export/1",
        })
    }
}

impl TryFrom<String> for SoftwareExportVersion {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "looklab-software-export/1" => Ok(Self::V1),
            _ => Err(format!("unsupported software export engine {value}")),
        }
    }
}

impl From<SoftwareExportVersion> for String {
    fn from(value: SoftwareExportVersion) -> Self {
        value.to_string()
    }
}

/// The artifact format a software pipeline accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Cube,
    Haldclut,
    NativeJson,
}

impl ExportFormat {
    fn schema(self) -> Format {
        match self {
            Self::Cube => Format::Cube,
            Self::Haldclut => Format::Haldclut,
            Self::NativeJson => Format::NativeJson,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Cube => "cube",
            Self::Haldclut => "png",
            Self::NativeJson => "json",
        }
    }
}

impl core::fmt::Display for ExportFormat {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(match self {
            Self::Cube => "cube",
            Self::Haldclut => "haldclut",
            Self::NativeJson => "native_json",
        })
    }
}

/// The core never accepts a path from the caller.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExportFileName(String);

impl ExportFileName {
    /// # Errors
    /// An empty, over-long or path-like name.
    pub fn new(value: impl Into<String>) -> Result<Self, SoftwareExportError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 255
            || value == "."
            || value.contains("..")
            || value
                .chars()
                .any(|character| character == '/' || character == '\\' || character.is_control())
        {
            return Err(SoftwareExportError::InvalidFileName(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExportFileName {
    type Error = SoftwareExportError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExportFileName> for String {
    fn from(value: ExportFileName) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportFile {
    pub name: ExportFileName,
    pub bytes: Vec<u8>,
}

/// The export request from the published profile page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoftwareExportRequest {
    pub version: SoftwareExportVersion,
    /// The published `.looklab` archive.
    pub target_look: Vec<u8>,
    /// The published interface profile YAML.
    pub profile_yaml: String,
    /// The body calibration YAML, absent for fixed scene insertions.
    #[serde(default)]
    pub transform_yaml: Option<String>,
    /// The selected camera body. Required by a nonempty `profile.bodies` and by
    /// camera-RGB insertion.
    #[serde(default)]
    pub body: Option<BodyId>,
    pub format: ExportFormat,
    /// The single-illuminant bake CCT; ignored for `dual_interpolated`.
    pub cct: CctK,
}

/// ΔE summary over the supported probes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportDeltaStats {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
}

impl From<&DeltaStats> for ExportDeltaStats {
    fn from(stats: &DeltaStats) -> Self {
        Self {
            count: stats.count,
            median: stats.median,
            p95: stats.p95,
            max: stats.max,
        }
    }
}

/// Validation at one bake or reporting CCT.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportCctReport {
    pub cct: CctK,
    pub stats: ExportDeltaStats,
    pub unsupported: usize,
    pub clipped: usize,
}

impl From<&CctReport> for ExportCctReport {
    fn from(report: &CctReport) -> Self {
        Self {
            cct: report.cct,
            stats: ExportDeltaStats::from(&report.stats),
            unsupported: report.unsupported,
            clipped: report.clipped,
        }
    }
}

/// The simulator report that gates the export.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportValidation {
    pub primary: ExportCctReport,
    pub others: Vec<ExportCctReport>,
    pub probes: usize,
    /// Probes whose target appearance needed perceptual clipping.
    pub target_clipped: usize,
    /// Only for `display_encoded_post_tone`: ΔE between the full Look and the
    /// colour-only Look the bake can realize.
    pub tone_mismatch: Option<ExportDeltaStats>,
}

impl From<&ValidationReport> for ExportValidation {
    fn from(report: &ValidationReport) -> Self {
        Self {
            primary: ExportCctReport::from(&report.primary),
            others: report.others.iter().map(ExportCctReport::from).collect(),
            probes: report.probes,
            target_clipped: report.target_clipped,
            tone_mismatch: report.tone_mismatch.as_ref().map(ExportDeltaStats::from),
        }
    }
}

/// The illuminant handling a bake used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportIlluminant {
    Single,
    DualInterpolated,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportBake {
    pub illuminant: ExportIlluminant,
    /// The CCT the validation report is keyed on.
    pub primary_cct: CctK,
    /// The CCTs the bake produced artifacts at.
    pub ccts: Vec<CctK>,
}

impl From<BakeIlluminant> for ExportBake {
    fn from(bake: BakeIlluminant) -> Self {
        Self {
            illuminant: match bake {
                BakeIlluminant::Single { .. } => ExportIlluminant::Single,
                BakeIlluminant::DualInterpolated => ExportIlluminant::DualInterpolated,
            },
            primary_cct: bake.primary(),
            ccts: bake.ccts(),
        }
    }
}

/// A validated export. `files` is empty when `flagged` is true.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SoftwareExportReply {
    pub version: SoftwareExportVersion,
    pub target_slug: Slug,
    pub target_version: LookVersion,
    pub profile_id: InterfaceId,
    pub profile_version: u32,
    pub format: ExportFormat,
    /// The consumed transform header version; `None` for fixed scene insertions.
    pub input_transform_version: Option<InputTransformVersion>,
    pub bake: ExportBake,
    pub flagged: bool,
    pub validation: ExportValidation,
    pub files: Vec<ExportFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum SoftwareExportError {
    #[error("target look: {0}")]
    Look(#[from] looklab_model::ModelError),
    #[error("unsupported software export engine {0}")]
    UnsupportedVersion(SoftwareExportVersion),
    #[error("profile: {0}")]
    Profile(String),
    #[error("profile does not accept {0}")]
    FormatNotDeclared(ExportFormat),
    #[error("profile requires a selected body")]
    MissingSelectedBody,
    #[error("profile is not declared for body {0}")]
    BodyNotDeclared(BodyId),
    #[error("the transform names body {declared}, not the selected {selected}")]
    TransformBodyMismatch { selected: BodyId, declared: BodyId },
    #[error("camera insertion needs a body transform")]
    MissingCameraTransform,
    #[error("a transform was supplied for an insertion that does not consume one")]
    UnusedTransform,
    #[error("transform: {0}")]
    Transform(String),
    #[error("target look is not matchable: {0}")]
    NotMatchable(MatchRefusal),
    #[error("probe support mask has {found} entries for {expected} probes")]
    ProbeMask { found: usize, expected: usize },
    #[error("no supported probe is representable at {cct}")]
    NoSupportedProbes { cct: CctK },
    #[error("invalid generated file name {0:?}")]
    InvalidFileName(String),
    #[error("export: {0}")]
    Export(#[from] looklab_export::ExportError),
}

/// Bakes, validates and writes one requested format.
///
/// # Errors
/// An invalid Look, profile, format, calibration or support mask, or a bake the
/// profile cannot realize. A validation flag is not an error; the reply carries
/// the report with no files.
pub fn run_export(
    request: &SoftwareExportRequest,
) -> Result<SoftwareExportReply, SoftwareExportError> {
    if request.version != SoftwareExportVersion::CURRENT {
        return Err(SoftwareExportError::UnsupportedVersion(request.version));
    }
    let document = LookDocument::decode(&request.target_look)?;
    if let Some(refusal) = document.match_refusal() {
        return Err(SoftwareExportError::NotMatchable(refusal));
    }
    let profile = validate_interface_profile(&request.profile_yaml).map_err(|errors| {
        SoftwareExportError::Profile(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    let format = request.format.schema();
    if !profile.formats.contains(&format) {
        return Err(SoftwareExportError::FormatNotDeclared(request.format));
    }

    let needs_camera = profile.insertion.point == InsertionPoint::CameraRgbWbLinear;
    if !profile.bodies.is_empty() {
        let body = request
            .body
            .as_ref()
            .ok_or(SoftwareExportError::MissingSelectedBody)?;
        if !profile.bodies.contains(body) {
            return Err(SoftwareExportError::BodyNotDeclared(body.clone()));
        }
    }
    let (input, input_transform_version) = if needs_camera {
        let selected = request
            .body
            .as_ref()
            .ok_or(SoftwareExportError::MissingSelectedBody)?;
        let yaml = request
            .transform_yaml
            .as_deref()
            .ok_or(SoftwareExportError::MissingCameraTransform)?;
        let calibration = parse_transform(yaml)?;
        if let Some(declared) = &calibration.body
            && declared != selected
        {
            return Err(SoftwareExportError::TransformBodyMismatch {
                selected: selected.clone(),
                declared: declared.clone(),
            });
        }
        let version = calibration.version;
        (calibration.transform(), Some(version))
    } else {
        if request.transform_yaml.is_some() {
            return Err(SoftwareExportError::UnusedTransform);
        }
        (ExportTransform::None, None)
    };

    let probe_set = probes::v1();
    let support = match &document.uncertainty.support {
        Some(mask) => {
            if mask.values.len() != probe_set.probes.len() {
                return Err(SoftwareExportError::ProbeMask {
                    found: mask.values.len(),
                    expected: probe_set.probes.len(),
                });
            }
            Some(
                mask.values
                    .iter()
                    .map(|value| *value > 0.5)
                    .collect::<Vec<bool>>(),
            )
        }
        None => None,
    };

    let illuminant = match profile.insertion.illuminant_handling {
        IlluminantHandling::Single => BakeIlluminant::Single { cct: request.cct },
        IlluminantHandling::DualInterpolated => BakeIlluminant::DualInterpolated,
    };
    let output = Baker::new(&profile, &document.look, &input).bake(illuminant)?;
    let report = validate(
        &output,
        &document.look,
        &input,
        &probe_set,
        support.as_deref(),
    )?;
    for cct in output.bake.ccts() {
        if report.at(cct).is_none_or(|entry| entry.stats.count == 0) {
            return Err(SoftwareExportError::NoSupportedProbes { cct });
        }
    }
    let flagged = report.flagged();
    let validation = ExportValidation::from(&report);
    let files = if flagged {
        Vec::new()
    } else {
        write_files(&document, &profile, &output, request.format)?
    };
    Ok(SoftwareExportReply {
        version: SoftwareExportVersion::CURRENT,
        target_slug: document.file.look.slug.clone(),
        target_version: document.file.look.version.clone(),
        profile_id: profile.id.clone(),
        profile_version: profile.version,
        format: request.format,
        input_transform_version,
        bake: output.bake.into(),
        flagged,
        validation,
        files,
    })
}

/// Postcard bytes for [`run_export`].
#[expect(
    clippy::expect_used,
    reason = "the request holds nothing postcard cannot encode"
)]
pub fn encode_software_export_request(request: &SoftwareExportRequest) -> Vec<u8> {
    postcard::to_allocvec(request).expect("software export request is encodable")
}

/// The reply of a worker export call.
///
/// # Errors
/// A malformed reply or trailing bytes.
pub fn decode_software_export_reply(bytes: &[u8]) -> Result<SoftwareExportReply, String> {
    let (reply, rest) = postcard::take_from_bytes::<Result<SoftwareExportReply, String>>(bytes)
        .map_err(|error| error.to_string())?;
    if !rest.is_empty() {
        return Err(format!(
            "{} trailing bytes after the worker reply",
            rest.len()
        ));
    }
    reply
}

fn write_files(
    document: &LookDocument,
    profile: &InterfaceProfile,
    output: &BakeOutput,
    format: ExportFormat,
) -> Result<Vec<ExportFile>, SoftwareExportError> {
    let stem = file_stem(document, profile);
    // `writer::written_bytes` measures cube files with this title.
    let title = output.profile_id.as_str();
    if format == ExportFormat::NativeJson {
        let text = write_native_json(output)?;
        return Ok(vec![file(format!("{stem}.json"), text.into_bytes())?]);
    }
    let published: Vec<(String, &Artifact)> = match output.bake {
        BakeIlluminant::Single { .. } => vec![(
            String::new(),
            output
                .primary_artifact()
                .ok_or(looklab_export::ExportError::NoAcceptedArtifact)?,
        )],
        BakeIlluminant::DualInterpolated => output
            .artifacts
            .iter()
            .map(|entry| (format!("-{}", entry.cct.kelvin() as u32), &entry.artifact))
            .collect(),
    };
    let mut files = Vec::new();
    for (suffix, artifact) in published {
        let lut = artifact
            .as_lut()
            .ok_or(looklab_export::ExportError::Unsupported(
                "the requested format needs a 3D LUT artifact",
            ))?;
        match format {
            ExportFormat::Cube => {
                let (grid, shaper) = cube_files(lut, title, output.insertion.range);
                files.push(file(format!("{stem}{suffix}.cube"), grid.into_bytes())?);
                if let Some(shaper) = shaper {
                    files.push(file(
                        format!("{stem}{suffix}-shaper.cube"),
                        shaper.into_bytes(),
                    )?);
                }
            }
            ExportFormat::Haldclut => {
                files.push(file(
                    format!("{stem}{suffix}.{}", format.extension()),
                    write_haldclut(lut)?,
                )?);
            }
            ExportFormat::NativeJson => {
                unreachable!("native_json is written from the whole output")
            }
        }
    }
    Ok(files)
}

fn file(name: String, bytes: Vec<u8>) -> Result<ExportFile, SoftwareExportError> {
    Ok(ExportFile {
        name: ExportFileName::new(name)?,
        bytes,
    })
}

fn file_stem(document: &LookDocument, profile: &InterfaceProfile) -> String {
    format!(
        "{}-{}-{}",
        sanitize(document.file.look.slug.as_str()),
        sanitize(document.file.look.version.as_str()),
        sanitize(profile.id.as_str()),
    )
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

struct ParsedTransform {
    body: Option<BodyId>,
    version: InputTransformVersion,
    calibration: Calibration,
}

enum Calibration {
    Dng(DngInputTransform),
    Fixed(Mat3),
}

impl ParsedTransform {
    fn transform(&self) -> ExportTransform {
        match &self.calibration {
            Calibration::Dng(transform) => ExportTransform::Dng(transform.clone()),
            Calibration::Fixed(matrix) => ExportTransform::Fixed(*matrix),
        }
    }
}

enum ExportTransform {
    Dng(DngInputTransform),
    Fixed(Mat3),
    None,
}

impl InputTransform for ExportTransform {
    fn camera_to_xyz(&self, cct: CctK) -> Option<Mat3> {
        match self {
            Self::Dng(transform) => transform.camera_to_xyz(cct),
            Self::Fixed(matrix) => Some(*matrix),
            Self::None => None,
        }
    }
}

fn parse_transform(yaml: &str) -> Result<ParsedTransform, SoftwareExportError> {
    let file: TransformYaml = serde_norway::from_str(yaml)
        .map_err(|error| SoftwareExportError::Transform(error.to_string()))?;
    if !file.format.is_empty() && file.format != TRANSFORM_FORMAT {
        return Err(SoftwareExportError::Transform(format!(
            "transform format {:?}, expected {TRANSFORM_FORMAT:?}",
            file.format
        )));
    }
    if file.version == 0 {
        return Err(SoftwareExportError::Transform(
            "transform version is zero".to_owned(),
        ));
    }
    let version = InputTransformVersion::new(file.version)
        .map_err(|error| SoftwareExportError::Transform(error.to_string()))?;
    if let Some(cct) = file.cct
        && !(cct.is_finite() && cct > 0.0)
    {
        return Err(SoftwareExportError::Transform(format!(
            "transform cct {cct} is not a positive temperature"
        )));
    }
    for cct in file
        .color_matrices
        .iter()
        .map(|entry| entry.cct)
        .chain(file.forward_matrices.iter().map(|entry| entry.cct))
    {
        if !(cct.is_finite() && cct > 0.0) {
            return Err(SoftwareExportError::Transform(format!(
                "calibration cct {cct} is not a positive temperature"
            )));
        }
    }
    if !file.color_matrices.is_empty() || !file.forward_matrices.is_empty() {
        let color = file
            .color_matrices
            .into_iter()
            .map(|entry| {
                CalibrationMatrix::new(entry.cct, entry.xyz_to_camera).ok_or_else(|| {
                    SoftwareExportError::Transform(format!("invalid calibration cct {}", entry.cct))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let forward = file
            .forward_matrices
            .into_iter()
            .map(|entry| {
                CalibrationMatrix::new(entry.cct, entry.camera_to_xyz).ok_or_else(|| {
                    SoftwareExportError::Transform(format!("invalid calibration cct {}", entry.cct))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let spec = DngColorSpec::new(color, forward)
            .map_err(|error| SoftwareExportError::Transform(error.to_string()))?;
        let transform = DngInputTransform::new(spec)
            .map_err(|error| SoftwareExportError::Transform(error.to_string()))?;
        return Ok(ParsedTransform {
            body: file.body,
            version,
            calibration: Calibration::Dng(transform),
        });
    }
    let matrix = file.camera_to_xyz.ok_or_else(|| {
        SoftwareExportError::Transform(
            "transform file has no camera matrices usable for a bake".to_owned(),
        )
    })?;
    if matrix.inverse().is_none() {
        return Err(SoftwareExportError::Transform(
            "the fixed camera-to-XYZ matrix is singular".to_owned(),
        ));
    }
    Ok(ParsedTransform {
        body: file.body,
        version,
        calibration: Calibration::Fixed(matrix),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransformYaml {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    body: Option<BodyId>,
    #[serde(default)]
    cct: Option<f64>,
    #[serde(default)]
    camera_to_xyz: Option<Mat3>,
    #[serde(default)]
    color_matrices: Vec<ColorMatrixYaml>,
    #[serde(default)]
    forward_matrices: Vec<ForwardMatrixYaml>,
    #[serde(default, rename = "per_image")]
    _per_image: Vec<PerImageYaml>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ColorMatrixYaml {
    cct: f64,
    xyz_to_camera: Mat3,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ForwardMatrixYaml {
    cct: f64,
    camera_to_xyz: Mat3,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PerImageYaml {
    file: String,
    camera_to_xyz: Mat3,
}
