// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::path::PathBuf;

use looklab_body::{
    A0Source, BodyPrior, CalibrationIlluminant, InputTransform, TransformVersion,
    camera_to_xyz_at_illuminant, camera_to_xyz_at_wb,
};
use looklab_color::Mat3;
use looklab_color::dng::{CalibrationMatrix, DngColorSpec};
use looklab_schema::{
    Body, ControlSchema, Epoch, FirmwareVersion, SNAPSHOT_MANIFEST, SnapshotManifest, content_hash,
};
use looklab_upload::{BodyId, LookSlug};
use serde::Deserialize;
use sqlx::SqlitePool;

use crate::db::ContributionIntent;
use crate::error::{Result, WorkerError};

#[derive(Debug, Clone)]
pub struct BodyResources {
    pub body: Body,
    pub schema: ControlSchema,
}

#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

/// The server-published snapshot version and root, verified against its recorded hash.
pub async fn published_snapshot(
    db_pool: &SqlitePool,
    config: &crate::config::WorkerConfig,
) -> Result<(u64, PathBuf)> {
    let identity = {
        let mut conn = db_pool.acquire().await?;
        crate::db::published_snapshot_identity(&mut conn).await?
    };
    match identity {
        Some((version, published_hash)) => {
            let root = verified_snapshot_root(config, version, &published_hash)?;
            Ok((version, root))
        }
        None => Ok((0, config.data.clone())),
    }
}

/// The root of one recorded published snapshot, verified against its hash.
pub fn verified_snapshot_root(
    config: &crate::config::WorkerConfig,
    version: u64,
    published_hash: &str,
) -> Result<PathBuf> {
    let root = config.snapshot_dir.join(format!("v{version}"));
    if !root.is_dir() {
        return Err(WorkerError::Data(format!(
            "published snapshot {version} is missing at {}",
            root.display()
        )));
    }
    let manifest_bytes = std::fs::read(root.join(SNAPSHOT_MANIFEST)).map_err(|error| {
        WorkerError::Data(format!(
            "published snapshot {version} has no readable manifest: {error}"
        ))
    })?;
    let manifest: SnapshotManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        WorkerError::Data(format!(
            "published snapshot {version} has an invalid manifest: {error}"
        ))
    })?;
    if manifest.version != version || manifest.content_hash != published_hash {
        return Err(WorkerError::Data(format!(
            "published snapshot {version} manifest does not match its recorded identity"
        )));
    }
    let files = snapshot_files(&root)?;
    if content_hash(&files) != published_hash {
        return Err(WorkerError::Data(format!(
            "published snapshot {version} bytes do not match their recorded hash"
        )));
    }
    Ok(root)
}

pub async fn published_data_dir(
    db_pool: &SqlitePool,
    config: &crate::config::WorkerConfig,
) -> Result<DataDir> {
    let (_, root) = published_snapshot(db_pool, config).await?;
    Ok(DataDir::new(root))
}

fn snapshot_files(root: &std::path::Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    collect_snapshot_files(root, root, &mut files)?;
    Ok(files)
}

fn collect_snapshot_files(
    root: &std::path::Path,
    directory: &std::path::Path,
    files: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_snapshot_files(root, &path, files)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| WorkerError::Data("snapshot file escapes its root".to_owned()))?
            .to_string_lossy()
            .replace('\\', "/");
        if relative == SNAPSHOT_MANIFEST {
            continue;
        }
        files.push((relative, std::fs::read(&path)?));
    }
    Ok(())
}

impl DataDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn stem(body: &BodyId) -> String {
        body.as_str().replace('/', "-")
    }

    pub fn load(&self, body: &BodyId) -> Result<BodyResources> {
        let stem = Self::stem(body);
        let root = self.root.join("bodies");
        let exported = self.root.join(looklab_schema::body_export_path(body));
        // Snapshot exports encode `/`; the shipped data names bodies with `-`.
        let (body_path, schema_path) = if exported.is_file() {
            (
                exported,
                self.root.join(looklab_schema::schema_export_path(body)),
            )
        } else {
            (
                root.join(format!("{stem}.body.yaml")),
                root.join(format!("{stem}.yaml")),
            )
        };
        let body_text = std::fs::read_to_string(&body_path)
            .map_err(|error| WorkerError::Data(format!("{}: {error}", body_path.display())))?;
        let schema_text = std::fs::read_to_string(&schema_path)
            .map_err(|error| WorkerError::Data(format!("{}: {error}", schema_path.display())))?;
        let body = looklab_schema::validate_body(&body_text)
            .map_err(|errors| WorkerError::Data(format!("body {stem}: {errors:?}")))?;
        let schema = looklab_schema::validate_control_schema(&schema_text)
            .map_err(|errors| WorkerError::Data(format!("schema {stem}: {errors:?}")))?;
        Ok(BodyResources { body, schema })
    }

    pub fn bodies(&self) -> Result<Vec<BodyResources>> {
        let mut out = Vec::new();
        let root = self.root.join("bodies");
        for entry in std::fs::read_dir(&root)? {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.ends_with(".body.yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
            let body = looklab_schema::validate_body(&text)
                .map_err(|errors| WorkerError::Data(format!("{name}: {errors:?}")))?;
            let id = BodyId::new(body.id.as_str())
                .map_err(|_| WorkerError::Data(format!("body file `{name}` has an invalid id")))?;
            out.push(self.load(&id)?);
        }
        out.sort_by(|left, right| left.body.id.cmp(&right.body.id));
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct TransformDir {
    root: PathBuf,
}

pub const TRANSFORM_FORMAT: &str = "looklab-transform/1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransformFile {
    #[serde(default)]
    format: Option<String>,
    version: u32,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    cct: Option<f64>,
    #[serde(default)]
    camera_to_xyz: Option<Mat3>,
    #[serde(default)]
    color_matrices: Vec<XyzToCamera>,
    #[serde(default)]
    forward_matrices: Vec<CameraToXyz>,
    #[serde(default)]
    per_image: Vec<PerImageTransform>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct XyzToCamera {
    cct: f64,
    xyz_to_camera: Mat3,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct CameraToXyz {
    cct: f64,
    camera_to_xyz: Mat3,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerImageTransform {
    file: String,
    camera_to_xyz: Mat3,
}

/// The camera-to-XYZ matrices a body's uploads resolve against.
#[derive(Debug, Clone)]
pub struct BodyTransforms {
    version: u32,
    spec: Option<DngColorSpec>,
    fixed: Option<Mat3>,
    per_image: BTreeMap<String, Mat3>,
}

impl BodyTransforms {
    pub fn version(&self) -> u32 {
        self.version
    }

    pub(crate) fn input_transform(
        &self,
        body: &BodyId,
        version: TransformVersion,
    ) -> Result<InputTransform> {
        let prior = match (&self.spec, self.fixed) {
            (Some(spec), _) => BodyPrior {
                source: A0Source::Dng,
                a0_a: camera_to_xyz_at_illuminant(spec, CalibrationIlluminant::A)?,
                a0_d65: camera_to_xyz_at_illuminant(spec, CalibrationIlluminant::D65)?,
            },
            (None, Some(matrix)) => BodyPrior {
                source: A0Source::Db,
                a0_a: matrix,
                a0_d65: matrix,
            },
            (None, None) => {
                return Err(WorkerError::Data(
                    "a body calibration requires a shared authoritative profile".to_owned(),
                ));
            }
        };
        Ok(InputTransform::prior(
            looklab_body::BodyId::new(body.as_str())?,
            version,
            prior,
        ))
    }

    /// The image's matrix, from its file override, its as-shot illuminant, or the
    /// body's single matrix.
    pub(crate) fn matrix_for(&self, file: Option<&str>, wb: [f32; 3]) -> Result<Mat3> {
        if let Some(matrix) = file.and_then(|file| self.per_image.get(file)) {
            return Ok(*matrix);
        }
        if let Some(spec) = &self.spec {
            return Ok(camera_to_xyz_at_wb(spec, wb)?);
        }
        self.fixed
            .ok_or_else(|| WorkerError::Data("transform file has no usable matrix".to_owned()))
    }
}

impl TransformDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, body: &BodyId) -> PathBuf {
        self.root
            .join(format!("{}.yaml", body.as_str().replace('/', "-")))
    }

    pub fn load(&self, body: &BodyId) -> Result<BodyTransforms> {
        let path = self.path(body);
        let text = std::fs::read_to_string(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkerError::TransformUnavailable { path: path.clone() }
            } else {
                WorkerError::Io(error)
            }
        })?;
        Self::parse(&text, &path, body)
    }

    /// The parsed transforms and the exact bytes they were parsed from.
    pub fn load_with_bytes(&self, body: &BodyId) -> Result<(BodyTransforms, Vec<u8>)> {
        let path = self.path(body);
        let bytes = std::fs::read(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                WorkerError::TransformUnavailable { path: path.clone() }
            } else {
                WorkerError::Io(error)
            }
        })?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?;
        let transforms = Self::parse(text, &path, body)?;
        Ok((transforms, bytes))
    }

    /// `None` when the body has no transform file; a file that exists but does not
    /// parse is still an error.
    pub fn load_optional(&self, body: &BodyId) -> Result<Option<BodyTransforms>> {
        match self.load(body) {
            Ok(transforms) => Ok(Some(transforms)),
            Err(WorkerError::TransformUnavailable { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The body's transform file, validated for publication with its match bundles.
    pub fn snapshot(&self, body: &BodyId) -> Result<Vec<u8>> {
        self.load(body)?;
        let path = self.path(body);
        std::fs::read(&path)
            .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))
    }

    /// Validates published installed-prior bytes for `body` and the recorded version.
    pub fn validate(&self, body: &BodyId, version: u32, bytes: &[u8]) -> Result<()> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| WorkerError::Data("published input transform is not UTF-8".to_owned()))?;
        let parsed = Self::parse(
            text,
            std::path::Path::new("<published input transform>"),
            body,
        )?;
        if parsed.version() != version {
            return Err(WorkerError::Data(format!(
                "published input transform for {body} declares version {}, recorded {version}",
                parsed.version()
            )));
        }
        Ok(())
    }

    fn parse(text: &str, path: &std::path::Path, body: &BodyId) -> Result<BodyTransforms> {
        let file: TransformFile = serde_norway::from_str(text)?;
        if let Some(named) = &file.body
            && named.as_str() != body.as_str()
        {
            return Err(WorkerError::Data(format!(
                "{}: transform is for `{named}`, expected `{body}`",
                path.display()
            )));
        }
        if let Some(format) = &file.format
            && format != TRANSFORM_FORMAT
        {
            return Err(WorkerError::Data(format!(
                "{}: transform format `{format}`, expected `{TRANSFORM_FORMAT}`",
                path.display()
            )));
        }
        if file.version == 0 {
            return Err(WorkerError::Data(format!(
                "{}: transform version must be at least 1",
                path.display()
            )));
        }
        if let Some(cct) = file.cct
            && !(cct.is_finite() && cct > 0.0)
        {
            return Err(WorkerError::Data(format!(
                "{}: cct {cct} is not a positive temperature",
                path.display()
            )));
        }
        if file
            .color_matrices
            .iter()
            .any(|entry| !(entry.cct.is_finite() && entry.cct > 0.0))
            || file
                .forward_matrices
                .iter()
                .any(|entry| !(entry.cct.is_finite() && entry.cct > 0.0))
        {
            return Err(WorkerError::Data(format!(
                "{}: a calibration cct is not a positive temperature",
                path.display()
            )));
        }
        let spec = if file.color_matrices.is_empty() && file.forward_matrices.is_empty() {
            None
        } else {
            let color = file
                .color_matrices
                .iter()
                .filter_map(|entry| CalibrationMatrix::new(entry.cct, entry.xyz_to_camera))
                .collect();
            let forward = file
                .forward_matrices
                .iter()
                .filter_map(|entry| CalibrationMatrix::new(entry.cct, entry.camera_to_xyz))
                .collect();
            Some(
                DngColorSpec::new(color, forward)
                    .map_err(|error| WorkerError::Data(format!("{}: {error}", path.display())))?,
            )
        };
        let per_image = file
            .per_image
            .iter()
            .map(|entry| (entry.file.clone(), entry.camera_to_xyz))
            .collect();
        if spec.is_none() && file.camera_to_xyz.is_none() && file.per_image.is_empty() {
            return Err(WorkerError::Data(format!(
                "{}: transform file has no matrices",
                path.display()
            )));
        }
        Ok(BodyTransforms {
            version: file.version,
            spec,
            fixed: file.camera_to_xyz,
            per_image,
        })
    }
}

pub fn resolve_look(
    resources: &BodyResources,
    firmware: &str,
    sim_key: &str,
    intent: ContributionIntent,
    target: Option<&LookSlug>,
) -> Result<LookSlug> {
    if intent == ContributionIntent::Recipe {
        return target.cloned().ok_or_else(|| {
            WorkerError::Data("recipe contribution without a target Look".to_owned())
        });
    }
    let epoch = epoch_for(&resources.schema, firmware)?;
    let sim = epoch
        .native_sims
        .iter()
        .find(|sim| sim.key.as_str() == sim_key)
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "sim `{sim_key}` is not in epoch {} of {}",
                epoch.id, resources.schema.body
            ))
        })?;
    LookSlug::new(sim.look.as_str()).map_err(|_| {
        WorkerError::Data(format!(
            "native sim has an invalid Look `{}`",
            sim.look.as_str()
        ))
    })
}

/// Fuzzy epoch map.
pub fn epoch_for<'a>(schema: &'a ControlSchema, firmware: &str) -> Result<&'a Epoch> {
    let version: FirmwareVersion = firmware
        .parse()
        .map_err(|_| WorkerError::Data(format!("invalid firmware `{firmware}`")))?;
    let mut epochs: Vec<&Epoch> = schema.epochs.iter().collect();
    epochs.sort_by(|left, right| left.firmware.from.cmp(&right.firmware.from));
    if let Some(epoch) = epochs
        .iter()
        .find(|epoch| epoch.firmware.from <= version && version <= epoch.firmware.to)
    {
        return Ok(epoch);
    }
    let lower = epochs
        .iter()
        .rev()
        .find(|epoch| epoch.firmware.from <= version);
    lower
        .or_else(|| epochs.first())
        .copied()
        .ok_or_else(|| WorkerError::Data("body schema has no epochs".to_owned()))
}
