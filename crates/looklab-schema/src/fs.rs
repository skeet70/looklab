// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(clippy::disallowed_methods)]

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::ValidationError;
use crate::{
    validate_body, validate_control_schema, validate_interface_profile, validate_manual_catalog,
};

#[derive(Debug, Clone)]
pub struct FileErrors {
    pub file: PathBuf,
    pub errors: Vec<ValidationError>,
}

impl fmt::Display for FileErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for error in &self.errors {
            writeln!(f, "{}: {error}", self.file.display())?;
        }
        Ok(())
    }
}

pub fn validate_dir(root: impl AsRef<Path>) -> io::Result<Vec<FileErrors>> {
    let root = root.as_ref();
    let paths = classified_yaml_files(root)?;
    let mut failures = Vec::new();
    for path in paths {
        let Some(kind) = classify(&path) else {
            continue;
        };
        let yaml = fs::read_to_string(&path)?;
        let errors = match kind {
            Kind::Body => validate_body(&yaml).err(),
            Kind::Schema => validate_control_schema(&yaml).err(),
            Kind::Interface => validate_interface_profile(&yaml).err(),
            Kind::Manual => validate_manual_catalog(&yaml).err(),
        };
        if let Some(errors) = errors
            && !errors.is_empty()
        {
            failures.push(FileErrors {
                file: path.strip_prefix(root).unwrap_or(&path).to_path_buf(),
                errors,
            });
        }
    }
    Ok(failures)
}

pub(crate) enum Kind {
    Body,
    Schema,
    Interface,
    Manual,
}

pub(crate) fn classify(path: &Path) -> Option<Kind> {
    let name = path.file_name()?.to_str()?;
    let dir = path.parent()?.file_name()?.to_str()?;
    if !name.ends_with(".yaml") {
        return None;
    }
    match dir {
        "bodies" if name.ends_with(".body.yaml") => Some(Kind::Body),
        "bodies" => Some(Kind::Schema),
        "interfaces" => Some(Kind::Interface),
        "manuals" => Some(Kind::Manual),
        _ => None,
    }
}

pub(crate) fn classified_yaml_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    collect(root, &mut paths)?;
    paths.retain(|path| classify(path).is_some());
    paths.sort();
    Ok(paths)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect(&path, out)?;
        } else if file_type.is_file() && path.extension() == Some(std::ffi::OsStr::new("yaml")) {
            out.push(path);
        }
    }
    Ok(())
}
