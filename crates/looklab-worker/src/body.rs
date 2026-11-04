// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::PathBuf;

use looklab_color::Mat3;
use looklab_schema::{Body, ControlSchema, Epoch, FirmwareVersion};
use looklab_upload::{BodyId, LookSlug};
use serde::Deserialize;

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
        let body_path = root.join(format!("{stem}.body.yaml"));
        let schema_path = root.join(format!("{stem}.yaml"));
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

#[derive(Deserialize)]
struct TransformFile {
    #[serde(default)]
    camera_to_xyz: Option<Mat3>,
}

impl TransformDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn load(&self, body: &BodyId) -> Result<Mat3> {
        let path: PathBuf = self
            .root
            .join(format!("{}.yaml", body.as_str().replace('/', "-")));
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Mat3::IDENTITY);
            }
            Err(error) => return Err(error.into()),
        };
        let file: TransformFile = serde_norway::from_str(&text)?;
        file.camera_to_xyz.ok_or_else(|| {
            WorkerError::Data(format!(
                "{}: only a fixed `camera_to_xyz` matrix is supported",
                path.display()
            ))
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
