// SPDX-License-Identifier: AGPL-3.0-or-later
//! Registered body entities that do not yet have a control schema.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use looklab_schema::{Body, BodyId, validate_body, validate_control_schema};

#[derive(Debug, thiserror::Error)]
pub enum BodyContextError {
    #[error("bodies directory: {0}")]
    Io(#[from] io::Error),
    #[error("{file} has no valid body entity: {errors}")]
    InvalidBody { file: String, errors: String },
    #[error("{file} has no valid control schema: {errors}")]
    InvalidSchema { file: String, errors: String },
}

/// Registered body entities without a control schema; errors on unreadable or invalid data.
pub fn registered_without_schema(
    bodies_dir: &Path,
    snapshot_dir: &Path,
    active: Option<u64>,
) -> Result<Vec<Body>, BodyContextError> {
    let mut bodies = BTreeMap::new();
    let mut schemas = BTreeSet::new();
    read_dir(bodies_dir, &mut bodies, &mut schemas)?;
    if let Some(version) = active {
        let dir = snapshot_dir.join(format!("v{version}")).join("bodies");
        read_dir(&dir, &mut bodies, &mut schemas)?;
    }
    Ok(bodies
        .into_values()
        .filter(|body| !schemas.contains(&body.id))
        .collect())
}

fn read_dir(
    dir: &Path,
    bodies: &mut BTreeMap<BodyId, Body>,
    schemas: &mut BTreeSet<BodyId>,
) -> Result<(), BodyContextError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        if name.ends_with(".body.yaml") {
            let body = validate_body(&text).map_err(|errors| BodyContextError::InvalidBody {
                file: name.to_owned(),
                errors: join_errors(&errors),
            })?;
            bodies.insert(body.id.clone(), body);
        } else {
            let schema = validate_control_schema(&text).map_err(|errors| {
                BodyContextError::InvalidSchema {
                    file: name.to_owned(),
                    errors: join_errors(&errors),
                }
            })?;
            schemas.insert(schema.body);
        }
    }
    Ok(())
}

fn join_errors(errors: &[looklab_schema::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}
