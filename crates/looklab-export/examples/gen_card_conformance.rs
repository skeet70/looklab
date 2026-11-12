// SPDX-License-Identifier: AGPL-3.0-or-later
//! Regenerates `data/card/conformance-v1/` (docs/04 §8.6) with
//! `cargo run -p looklab-export --example gen_card_conformance -- data/card/conformance-v1`.
#![allow(clippy::disallowed_methods)]

use std::error::Error;
use std::fs;
use std::path::PathBuf;

use looklab_export::card::{CardMeta, CardOptions, CardStatus, read_card, write_card};
use looklab_model::file::{LookIdentity, LookKind, WbPolicy};
use looklab_model::look::Look;
use looklab_model::{LookDocument, ProbeScalars, UncertaintyData};
use sha2::{Digest, Sha256};

const GRID_VECTORS: usize = 300;

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("data/card/conformance-v1"), PathBuf::from);
    fs::create_dir_all(dir.join("valid"))?;
    fs::create_dir_all(dir.join("malformed"))?;

    let count = looklab_model::probes::V1_PROBE_COUNT;
    let document = conformance_document(
        (0..count)
            .map(|index| if index % 3 == 0 { 0.0 } else { 1.0 })
            .collect(),
    )?;
    let meta = card_meta();
    fs::write(dir.join("valid/look.looklab"), document.encode()?)?;

    let card33 = write_card(&document, &meta, &CardOptions::default())?;
    let no_grid = write_card(&document, &meta, &CardOptions::with_grid(None))?;
    fs::write(dir.join("valid/card-33.llk"), &card33)?;
    fs::write(dir.join("valid/card-no-grid.llk"), &no_grid)?;

    let experimental_document = conformance_document(vec![0.0; count])?;
    fs::write(
        dir.join("valid/look-experimental.looklab"),
        experimental_document.encode()?,
    )?;
    let experimental = write_card(
        &experimental_document,
        &meta.clone().with_status(CardStatus::Experimental),
        &CardOptions::default(),
    )?;
    fs::write(dir.join("valid/card-experimental.llk"), &experimental)?;

    let sections = split_sections(&no_grid)?;
    let (valid_bytes, entries) = assemble(&sections);
    debug_assert_eq!(valid_bytes, no_grid);

    let mut files: Vec<(String, bool, &'static str)> = vec![
        ("valid/card-33.llk".to_string(), true, ""),
        ("valid/card-no-grid.llk".to_string(), true, ""),
        ("valid/card-experimental.llk".to_string(), true, ""),
    ];
    let mut write = |name: &str,
                     bytes: Vec<u8>,
                     valid: bool,
                     reason: &'static str|
     -> Result<(), Box<dyn Error>> {
        let path = if valid {
            format!("valid/{name}")
        } else {
            format!("malformed/{name}")
        };
        fs::write(dir.join(&path), bytes)?;
        files.push((path, valid, reason));
        Ok(())
    };

    write(
        "truncated-header.llk",
        no_grid[..32].to_vec(),
        false,
        "truncated header",
    )?;
    write(
        "truncated-table.llk",
        no_grid[..80].to_vec(),
        false,
        "section table runs past the file",
    )?;
    write(
        "truncated-section.llk",
        no_grid[..no_grid.len() - 2].to_vec(),
        false,
        "file length disagrees with the bytes",
    )?;

    let mut bad = no_grid.clone();
    bad[0] = b'X';
    write("bad-magic.llk", bad, false, "magic")?;

    let mut bad = no_grid.clone();
    bad[4..6].copy_from_slice(&2_u16.to_le_bytes());
    write("bad-version.llk", bad, false, "format version")?;

    let mut bad = no_grid.clone();
    bad[6..8].copy_from_slice(&63_u16.to_le_bytes());
    write("bad-header-length.llk", bad, false, "header length")?;

    let mut bad = no_grid.clone();
    let length = (no_grid.len() as u32).wrapping_add(4);
    bad[8..12].copy_from_slice(&length.to_le_bytes());
    write("bad-file-length.llk", bad, false, "file length")?;

    let mut bad = no_grid.clone();
    bad[16] ^= 1;
    write("bad-header-crc.llk", bad, false, "header CRC")?;

    let mut bad = no_grid.clone();
    bad[20] ^= 1;
    fix_header_crc(&mut bad);
    write("bad-content-id.llk", bad, false, "content id")?;

    let mut changed = entries.clone();
    let last = changed.len() - 1;
    changed[last].crc ^= 1;
    write(
        "bad-section-crc.llk",
        rebuild(&sections, changed)?,
        false,
        "section CRC",
    )?;

    let mut missing = sections.clone();
    missing.retain(|(kind, _)| *kind != 4);
    write(
        "missing-fit.llk",
        assemble(&missing).0,
        false,
        "missing required section",
    )?;

    let mut duplicate = sections.clone();
    duplicate.push((3, vec![0_u8; 12]));
    duplicate.sort_by_key(|(kind, _)| *kind);
    write(
        "duplicate-neutral.llk",
        assemble(&duplicate).0,
        false,
        "duplicate section kind",
    )?;

    let mut swapped = entries.clone();
    swapped.swap(0, 1);
    write(
        "unsorted-table.llk",
        rebuild(&sections, swapped)?,
        false,
        "section table is not sorted",
    )?;

    let mut overlap = entries.clone();
    overlap[3].offset = overlap[2].offset;
    overlap[3].length = overlap[2].length;
    overlap[3].crc = overlap[2].crc;
    write(
        "overlapping-sections.llk",
        rebuild(&sections, overlap)?,
        false,
        "sections overlap",
    )?;

    let mut out_of_range = entries.clone();
    out_of_range[3].offset = u32::try_from(no_grid.len() + 4)?;
    out_of_range[3].length = 4;
    write(
        "out-of-range-section.llk",
        rebuild(&sections, out_of_range)?,
        false,
        "section runs past the file",
    )?;

    let mut wrong_semantics = sections.clone();
    wrong_semantics[1].1[0..2].copy_from_slice(&2_u16.to_le_bytes());
    write(
        "wrong-semantics.llk",
        assemble(&wrong_semantics).0,
        false,
        "semantics id",
    )?;

    let mut grid34 = sections.clone();
    let mut payload = 34_u32.to_le_bytes().to_vec();
    payload.resize(4 + 34_usize.pow(3) * 6, 0);
    grid34.push((5, payload));
    write(
        "grid-side-34.llk",
        assemble(&grid34).0,
        false,
        "grid side not 33 or 65",
    )?;

    let mut non_finite_neutral = sections.clone();
    non_finite_neutral[2].1[4..8].copy_from_slice(&f32::NAN.to_le_bytes());
    write(
        "non-finite-neutral.llk",
        assemble(&non_finite_neutral).0,
        false,
        "non-finite value",
    )?;

    let mut non_finite_fit = sections.clone();
    non_finite_fit[3].1[4 + 48..4 + 52].copy_from_slice(&f32::INFINITY.to_le_bytes());
    write(
        "non-finite-fit.llk",
        assemble(&non_finite_fit).0,
        false,
        "non-finite value",
    )?;

    let card33_sections = split_sections(&card33)?;
    let mut non_finite_grid = card33_sections.clone();
    let grid_index = non_finite_grid
        .iter()
        .position(|(kind, _)| *kind == 5)
        .expect("card-33 carries a grid");
    non_finite_grid[grid_index].1[4..6].copy_from_slice(&0x7c00_u16.to_le_bytes());
    write(
        "non-finite-grid.llk",
        assemble(&non_finite_grid).0,
        false,
        "non-finite value",
    )?;

    let mut unknown = sections.clone();
    unknown.push((77, b"extra".to_vec()));
    write("unknown-section.llk", assemble(&unknown).0, true, "")?;

    let card = read_card(&card33)?;
    let experimental_sections = split_sections(&experimental)?;
    let mut bad_status = experimental_sections;
    let meta_index = bad_status
        .iter()
        .position(|(kind, _)| *kind == 1)
        .expect("experimental card carries META");
    let status = bad_status[meta_index]
        .1
        .windows(b"experimental".len())
        .position(|window| window == b"experimental")
        .expect("experimental card carries its status");
    bad_status[meta_index].1[status..status + b"experimental".len()]
        .copy_from_slice(b"questionable");
    write("bad-status.llk", assemble(&bad_status).0, false, "status")?;
    let index = serde_json::json!({
        "format": "looklab-card-conformance/1",
        "files": files
            .iter()
            .map(|(path, valid, reason)| serde_json::json!({
                "path": path,
                "outcome": if *valid { "valid" } else { "rejected" },
                "reason": reason,
            }))
            .collect::<Vec<_>>(),
    });
    fs::write(dir.join("index.json"), serde_json::to_vec_pretty(&index)?)?;

    fs::write(
        dir.join("grid-vectors.json"),
        serde_json::to_vec_pretty(&grid_vectors(&card))?,
    )?;

    fs::write(
        dir.join("render-tags.json"),
        serde_json::to_vec_pretty(&render_tags(&card))?,
    )?;
    Ok(())
}

fn conformance_document(support_values: Vec<f64>) -> Result<LookDocument, Box<dyn Error>> {
    let mut look = Look::colorimetric_spline()?;
    for (index, coefficient) in look
        .color
        .r
        .plane
        .iter_mut()
        .chain(look.color.r.volume.iter_mut())
        .enumerate()
    {
        *coefficient = 0.01 * ((index % 5) as f64 - 2.0);
    }
    for (index, coefficient) in look
        .color
        .lambda
        .plane
        .iter_mut()
        .chain(look.color.lambda.volume.iter_mut())
        .enumerate()
    {
        *coefficient = 0.005 * ((index % 3) as f64 - 1.0);
    }
    let identity = LookIdentity::new(
        "conformance-tone",
        "1.0.0",
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )?;
    let mut document = LookDocument::from_look(identity, look)?;
    let count = looklab_model::probes::V1_PROBE_COUNT;
    assert_eq!(support_values.len(), count);
    let support = ProbeScalars::new(support_values)?;
    let sigma = ProbeScalars::new(
        (0..count)
            .map(|index| 0.05 + 0.01 * (index % 7) as f64)
            .collect(),
    )?;
    document = document.with_uncertainty(UncertaintyData {
        support: Some(support),
        probe_sigma_post: Some(sigma),
        ..UncertaintyData::default()
    })?;
    Ok(document)
}

fn card_meta() -> CardMeta {
    CardMeta::new("Conformance Tone", "Conf Tone")
        .with_description("Synthetic tone and colour Look for card conformance vectors.")
        .with_license("Project-owned synthetic data")
}

type Sections = Vec<(u32, Vec<u8>)>;

type ExpectedTag<'a> = Option<(&'a str, &'a str, &'a str, &'a str, &'a str)>;

fn split_sections(bytes: &[u8]) -> Result<Sections, Box<dyn Error>> {
    let count = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
    let mut sections = Vec::with_capacity(count);
    for index in 0..count {
        let start = 64 + index * 16;
        let entry = &bytes[start..start + 16];
        let kind = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
        let offset = u32::from_le_bytes([entry[4], entry[5], entry[6], entry[7]]) as usize;
        let length = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        sections.push((kind, bytes[offset..offset + length].to_vec()));
    }
    Ok(sections)
}

#[derive(Clone, Copy)]
struct Entry {
    kind: u32,
    offset: u32,
    length: u32,
    crc: u32,
}

fn assemble(sections: &[(u32, Vec<u8>)]) -> (Vec<u8>, Vec<Entry>) {
    let table_end = 64 + sections.len() * 16;
    let mut body = Vec::new();
    let mut entries = Vec::with_capacity(sections.len());
    for (kind, data) in sections {
        while !(table_end + body.len()).is_multiple_of(4) {
            body.push(0);
        }
        let offset = (table_end + body.len()) as u32;
        entries.push(Entry {
            kind: *kind,
            offset,
            length: data.len() as u32,
            crc: crc32(data),
        });
        body.extend_from_slice(data);
    }
    let bytes = frame(&entries, &body);
    (bytes, entries)
}

fn rebuild(sections: &[(u32, Vec<u8>)], entries: Vec<Entry>) -> Result<Vec<u8>, Box<dyn Error>> {
    let table_end = 64 + sections.len() * 16;
    let mut body = Vec::new();
    for (_, data) in sections {
        while !(table_end + body.len()).is_multiple_of(4) {
            body.push(0);
        }
        body.extend_from_slice(data);
    }
    Ok(frame(&entries, &body))
}

fn fix_header_crc(bytes: &mut [u8]) {
    let count = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
    let table_end = 64 + count * 16;
    let mut crc = Crc32::new();
    crc.update(&bytes[..16]);
    crc.update(&[0_u8; 4]);
    crc.update(&bytes[20..table_end]);
    bytes[16..20].copy_from_slice(&crc.finish().to_le_bytes());
}

fn frame(entries: &[Entry], body: &[u8]) -> Vec<u8> {
    let mut header = vec![0_u8; 64];
    header[0..4].copy_from_slice(b"LLCF");
    header[4..6].copy_from_slice(&1_u16.to_le_bytes());
    header[6..8].copy_from_slice(&64_u16.to_le_bytes());
    let file_length = (64 + entries.len() * 16 + body.len()) as u32;
    header[8..12].copy_from_slice(&file_length.to_le_bytes());
    header[12..14].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    header[20..52].copy_from_slice(&Sha256::digest(body));
    let mut crc = Crc32::new();
    crc.update(&header);
    for entry in entries {
        for value in [entry.kind, entry.offset, entry.length, entry.crc] {
            crc.update(&value.to_le_bytes());
        }
    }
    header[16..20].copy_from_slice(&crc.finish().to_le_bytes());
    let mut out = Vec::with_capacity(header.len() + entries.len() * 16 + body.len());
    out.extend_from_slice(&header);
    for entry in entries {
        for value in [entry.kind, entry.offset, entry.length, entry.crc] {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(body);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                self.0 = if self.0 & 1 != 0 {
                    (self.0 >> 1) ^ 0xedb8_8320
                } else {
                    self.0 >> 1
                };
            }
        }
    }

    fn finish(&self) -> u32 {
        !self.0
    }
}

fn grid_vectors(card: &looklab_export::card::Card) -> serde_json::Value {
    let mut rng = SplitMix64(0x5eed_ca7d_0000_0001);
    let mut inputs = vec![
        [0.0, 0.0, 0.0],
        [1.0, 1.0, 1.0],
        [0.5, 0.5, 0.5],
        [0.25, 0.5, 0.75],
        [0.5, 0.25, 0.75],
        [0.75, 0.5, 0.25],
    ];
    for _ in 0..GRID_VECTORS {
        inputs.push([rng.unit(), rng.unit(), rng.unit()]);
    }
    let vectors: Vec<_> = inputs
        .into_iter()
        .map(|input| {
            let output = card
                .eval_grid_shaped(input)
                .expect("the conformance card has a grid");
            serde_json::json!({ "input": input, "output": output })
        })
        .collect();
    serde_json::json!({
        "format": "looklab-card-grid-vectors/1",
        "side": card.grid().expect("the conformance card has a grid").side().side(),
        "vectors": vectors,
    })
}

fn render_tags(card: &looklab_export::card::Card) -> serde_json::Value {
    let cid: String = card
        .cid_prefix()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let good =
        format!("looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid={cid}");
    let prefixed = format!("looklab-card/1 fw=a/0.1+b look=conformance-tone@1.0.0 cid={cid}");
    let cases: Vec<(&str, ExpectedTag)> = vec![
        (
            &good,
            Some(("fujifilm-x-t5", "1.00", "conformance-tone", "1.0.0", &cid)),
        ),
        (
            &prefixed,
            Some(("a", "0.1+b", "conformance-tone", "1.0.0", &cid)),
        ),
        (
            "looklab-card/1 fw=Fujifilm/1.00 look=conformance-tone@1.0.0 cid=0000000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm_x-t5/1.00 look=conformance-tone@1.0.0 cid=0000000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/ look=conformance-tone@1.0.0 cid=0000000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=Conformance@1.0.0 cid=0000000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@ cid=0000000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid=00000000000000",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid=00000000000000zz",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0",
            None,
        ),
        (
            "looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid=0000000000000000 extra",
            None,
        ),
        (
            "looklab-card/2 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid=0000000000000000",
            None,
        ),
        (
            " looklab-card/1 fw=fujifilm-x-t5/1.00 look=conformance-tone@1.0.0 cid=0000000000000000",
            None,
        ),
    ];
    let cases: Vec<_> = cases
        .into_iter()
        .map(|(input, expected)| match expected {
            Some((firmware_id, firmware_version, look_slug, look_version, cid_prefix)) => {
                serde_json::json!({
                    "input": input,
                    "result": {
                        "firmware_id": firmware_id,
                        "firmware_version": firmware_version,
                        "look_slug": look_slug,
                        "look_version": look_version,
                        "cid_prefix": cid_prefix,
                    },
                })
            }
            None => serde_json::json!({
                "input": input,
                "result": { "error": "not a well-formed render tag" },
            }),
        })
        .collect();
    serde_json::Value::Array(cases)
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}
