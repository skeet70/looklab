// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_prep::{LocalImageKey, SeedFile};
use serde::Serialize;

use crate::error::FacadeError;

/// The local seed file `looklab verify-payload` reads.
pub fn build_seed_file(
    prior_version: u32,
    entries: &[(LocalImageKey, [u8; 32])],
) -> Result<String, FacadeError> {
    let wire = SeedFileWire {
        format: SEED_FORMAT,
        prior_transform_version: prior_version,
        entries: entries
            .iter()
            .map(|(key, seed)| SeedEntryWire {
                key: key.as_str(),
                seed: to_hex(seed),
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&wire)?;
    Ok(SeedFile::from_json(&json)?.to_json()?)
}

const SEED_FORMAT: &str = "looklab-seeds/1";

#[derive(Serialize)]
struct SeedFileWire<'a> {
    format: &'a str,
    prior_transform_version: u32,
    entries: Vec<SeedEntryWire<'a>>,
}

#[derive(Serialize)]
struct SeedEntryWire<'a> {
    key: &'a str,
    seed: String,
}

fn to_hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}
