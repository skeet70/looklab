// SPDX-License-Identifier: AGPL-3.0-or-later
//! `looklab validate`: schema checks for a data directory and its probe files.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Args;
use looklab_model::ProbeSetVersion;
use looklab_model::probes::{V1_PROBE_COUNT, validate_yaml};

#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Data directory, laid out as `data/` (bodies/, interfaces/, probes/).
    pub dir: PathBuf,
}

pub fn run(args: &ValidateArgs) -> Result<()> {
    let mut errors = 0usize;
    let files = looklab_schema::validate_dir(&args.dir)
        .with_context(|| format!("walking {}", args.dir.display()))?;
    for file in &files {
        print!("{file}");
        errors += file.errors.len();
    }

    let probes_dir = args.dir.join("probes");
    if probes_dir.is_dir() {
        let entries = std::fs::read_dir(&probes_dir)
            .with_context(|| format!("listing {}", probes_dir.display()))?;
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "yaml")
            {
                paths.push(path);
            }
        }
        paths.sort();
        for path in paths {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            match validate_yaml(&text) {
                Ok(file) => {
                    if file.version == ProbeSetVersion::V1 && file.probes.len() != V1_PROBE_COUNT {
                        println!(
                            "{}: probe set v1 has {} probes, expected {V1_PROBE_COUNT}",
                            path.display(),
                            file.probes.len()
                        );
                        errors += 1;
                    }
                }
                Err(error) => {
                    println!("{}: {error}", path.display());
                    errors += 1;
                }
            }
        }
    }

    if errors > 0 {
        bail!("{errors} validation error(s) in {}", args.dir.display());
    }
    println!("valid: {}", args.dir.display());
    Ok(())
}
