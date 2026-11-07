// SPDX-License-Identifier: AGPL-3.0-or-later
//! Prepares the CC0 photo scene directory the worker reads from `<data>/scenes`.
//!
//! Run `tools/fetch-fixtures.sh` first. The worker never decodes RAW or reaches the network.
#![allow(clippy::disallowed_methods)]

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let mut manifest = PathBuf::from("fixtures/manifest.toml");
    let mut cache = PathBuf::from("fixtures/cache");
    let mut out = PathBuf::from("data/scenes");
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || {
            arguments
                .next()
                .ok_or_else(|| format!("{argument} needs a path"))
        };
        match argument.as_str() {
            "--manifest" => manifest = value()?.into(),
            "--cache" => cache = value()?.into(),
            "--out" => out = value()?.into(),
            other => return Err(format!("unknown argument `{other}`").into()),
        }
    }

    let scenes =
        looklab_publish::photo::prepare_photo_scenes(&std::fs::read_to_string(&manifest)?, &cache)?;
    looklab_publish::write_scene_sources(&out, &scenes)?;
    println!(
        "wrote {} photo scenes and their provenance to {}",
        scenes.len(),
        out.display()
    );
    Ok(())
}
