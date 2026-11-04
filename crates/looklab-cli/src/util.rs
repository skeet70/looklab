// SPDX-License-Identifier: AGPL-3.0-or-later
//! Helpers shared by the subcommands.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use looklab_model::LookDocument;

pub fn load_document(path: &Path) -> Result<LookDocument> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    LookDocument::decode(&bytes).with_context(|| format!("decoding {}", path.display()))
}

/// `"from-metadata"` resolves to the transform file prep wrote beside the samples.
pub fn transform_path(arg: &str, samples: &Path) -> Option<PathBuf> {
    match arg {
        "from-metadata" | "metadata" => Some(samples.join("transform.yaml")),
        _ => None,
    }
}

pub fn samples_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "llsamp")
        {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        bail!("no .llsamp files in {}", dir.display());
    }
    Ok(files)
}

/// RFC 3339 UTC.
pub fn now_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub fn replace_extension(path: &Path, suffix: &str, extension: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".to_owned());
    path.with_file_name(format!("{stem}{suffix}.{extension}"))
}
