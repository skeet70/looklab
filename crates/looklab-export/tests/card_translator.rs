// SPDX-License-Identifier: AGPL-3.0-or-later
// Integration tests run the built binary and write fixture cards; the library has no I/O.
#![allow(clippy::disallowed_methods)]
//! P3.6: the C99 translator in `tools/card-translator-c99` against cards from
//! this crate. The synthetic looks are exactly matrix + per-channel curves.

use looklab_export::CardMeta;
use looklab_export::card::{CardSections, FitRecord, Grid, GridSide, write_card_sections};
use looklab_model::file::identity;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::LazyLock;

const CURVE_POINTS: usize = 28;
const NEUTRAL_POINTS: usize = 257;
const MID_GRAY: f64 = 0.18;
const HEADER_LENGTH: usize = 64;

static BINARIES: LazyLock<PathBuf> = LazyLock::new(|| {
    let tools = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/card-translator-c99")
        .canonicalize()
        .expect("translator directory");
    let out = Path::new(env!("CARGO_TARGET_TMPDIR")).join("card-translator");
    fs::create_dir_all(&out).expect("build directory");
    let output = Command::new("nix")
        .args(["shell", "nixpkgs#gcc", "nixpkgs#clang", "-c", "make", "-C"])
        .arg(&tools)
        .arg(format!("OUTDIR={}", out.display()))
        .arg("check")
        .output()
        .expect("run nix shell");
    assert!(
        output.status.success(),
        "translator build failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    out
});

fn binaries() -> &'static PathBuf {
    &BINARIES
}

fn fixture_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("card-translator-cards");
    fs::create_dir_all(&dir).expect("fixture directory");
    dir
}

fn run(binary: &str, card: &Path) -> Output {
    Command::new(binaries().join(binary))
        .arg(card)
        .output()
        .expect("run translator")
}

fn translate(card: &Path) -> Value {
    let output = run("llk-translate-gcc", card);
    assert!(
        output.status.success(),
        "translator rejected {}: {}",
        card.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("translator output is JSON")
}

struct Truth {
    matrix: [[f64; 3]; 3],
    curve_ev: [f64; CURVE_POINTS],
    curve: [[f64; CURVE_POINTS]; 3],
}

fn knot_index(j: usize) -> usize {
    (j * (NEUTRAL_POINTS - 1) + (CURVE_POINTS - 1) / 2) / (CURVE_POINTS - 1)
}

impl Truth {
    fn new(matrix: [[f64; 3]; 3], gain: [f64; 3], gamma: [f64; 3]) -> Self {
        let curve_ev = std::array::from_fn(|j| {
            -12.0 + 18.0 * knot_index(j) as f64 / (NEUTRAL_POINTS - 1) as f64
        });
        let curve = std::array::from_fn(|ch| {
            std::array::from_fn(|j| 0.18 * gain[ch] * 2f64.powf(gamma[ch] * curve_ev[j]))
        });
        Self {
            matrix,
            curve_ev,
            curve,
        }
    }

    /// Piecewise linear in EV, held outside the knots.
    fn curve_at(&self, channel: usize, ev: f64) -> f64 {
        if ev <= self.curve_ev[0] {
            return self.curve[channel][0];
        }
        if ev >= self.curve_ev[CURVE_POINTS - 1] {
            return self.curve[channel][CURVE_POINTS - 1];
        }
        let mut segment = 0;
        while segment < CURVE_POINTS - 2 && ev > self.curve_ev[segment + 1] {
            segment += 1;
        }
        let low = self.curve_ev[segment];
        let high = self.curve_ev[segment + 1];
        let fraction = (ev - low) / (high - low);
        let a = self.curve[channel][segment];
        let b = self.curve[channel][segment + 1];
        a + (b - a) * fraction
    }

    fn apply(&self, rgb: [f64; 3]) -> [f64; 3] {
        std::array::from_fn(|channel| {
            let row = self.matrix[channel];
            let v = row[0] * rgb[0] + row[1] * rgb[1] + row[2] * rgb[2];
            if v <= 0.0 {
                self.curve[channel][0]
            } else {
                self.curve_at(channel, (v / MID_GRAY).log2())
            }
        })
    }

    fn neutral(&self) -> Vec<[f32; 3]> {
        (0..NEUTRAL_POINTS)
            .map(|i| {
                let ev = -12.0 + 18.0 * i as f64 / (NEUTRAL_POINTS - 1) as f64;
                let y = MID_GRAY * 2f64.powf(ev);
                self.apply([y, y, y])
            })
            .map(|value| value.map(|channel| channel as f32))
            .collect()
    }

    fn fit(&self) -> Vec<FitRecord> {
        let mut inputs = Vec::new();
        for step in 0..25 {
            let x = MID_GRAY * 2f64.powf(-6.0 + 9.0 * f64::from(step) / 24.0);
            inputs.push([x, x, x]);
        }
        for r in 0..5 {
            for g in 0..5 {
                for b in 0..5 {
                    let value = |i: usize| 0.03 + 0.87 * i as f64 / 4.0;
                    inputs.push([value(r), value(g), value(b)]);
                }
            }
        }
        inputs.push([0.9, 0.02, 0.02]);
        inputs.push([0.02, 0.9, 0.02]);
        inputs.push([0.02, 0.02, 0.9]);
        inputs.push([0.7, 0.7, 0.02]);

        let mut accepted = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            let forward: Vec<f64> = (0..3)
                .map(|ch| {
                    let row = self.matrix[ch];
                    row[0] * input[0] + row[1] * input[1] + row[2] * input[2]
                })
                .collect();
            if forward.iter().any(|v| *v <= 0.0) {
                continue;
            }
            let output = self.apply(*input);
            if output.iter().any(|v| *v > 1.0) {
                continue;
            }
            accepted.push((index, *input, output));
        }
        let weight = 1.0 / accepted.len() as f32;
        accepted
            .into_iter()
            .map(|(index, input, output)| FitRecord {
                input: input.map(|v| v as f32),
                output: output.map(|v| v as f32),
                sigma: 0.02,
                weight,
                supported: true,
                probe: index as u32,
            })
            .collect()
    }
}

fn card_bytes(truth: &Truth, slug: &str, with_grid: bool) -> Vec<u8> {
    let grid = with_grid.then(|| {
        Grid::new(
            GridSide::S33,
            vec![[0.5_f32, 0.5, 0.5]; GridSide::S33.nodes()],
        )
        .expect("grid")
    });
    let sections = CardSections {
        identity: identity(slug, "1.0.0").expect("identity"),
        meta: CardMeta::new("Translator fixture", "TRN"),
        neutral: truth.neutral(),
        fit: truth.fit(),
        grid,
    };
    write_card_sections(&sections).expect("write card")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_recovered(name: &str, truth: &Truth, json: &Value) {
    let matrix: Vec<Vec<f64>> =
        serde_json::from_value(json["matrix"].clone()).expect("matrix in output");
    for (channel, row) in truth.matrix.iter().enumerate() {
        for (component, want) in row.iter().enumerate() {
            let got = matrix[channel][component];
            assert!(
                (got - want).abs() <= 0.01,
                "{name}: matrix[{channel}][{component}] {got} vs {want}"
            );
        }
    }
    for (channel, key) in ["r", "g", "b"].iter().enumerate() {
        let values: Vec<f64> =
            serde_json::from_value(json["curve"][key].clone()).expect("curve in output");
        assert_eq!(values.len(), CURVE_POINTS);
        let scale = truth.curve[channel]
            .iter()
            .fold(0.0_f64, |acc, value| acc.max(value.abs()));
        for (j, want) in truth.curve[channel].iter().enumerate() {
            let got = values[j];
            assert!(
                (got - want).abs() <= 0.005 * scale,
                "{name}: curve {key}[{j}] {got} vs {want}"
            );
        }
    }
    let lab_rms = json["fit"]["lab_rms"].as_f64().expect("lab_rms");
    assert!(lab_rms < 1.0, "{name}: fit residual {lab_rms}");
}

#[test]
fn recovers_matrix_and_curve() {
    let truth_a = Truth::new(
        [[0.95, 0.03, 0.02], [0.02, 1.02, -0.04], [-0.01, 0.05, 0.96]],
        [1.0, 1.0, 1.0],
        [0.95, 1.0, 1.05],
    );
    let truth_b = Truth::new(
        [[1.02, -0.03, 0.01], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]],
        [1.03, 0.99, 0.97],
        [1.1, 1.0, 0.9],
    );
    let dir = fixture_dir();
    for (name, truth, grid) in [("plain", &truth_a, false), ("grid", &truth_b, true)] {
        let bytes = card_bytes(truth, &format!("translator-{name}"), grid);
        let path = dir.join(format!("{name}.llk"));
        fs::write(&path, &bytes).expect("write fixture");
        let json = translate(&path);
        assert_eq!(
            json["content_id"].as_str().expect("content id"),
            hex(&Sha256::digest(
                &bytes[HEADER_LENGTH + 16 * section_count(&bytes)..]
            ))
        );
        assert_recovered(name, truth, &json);
    }
}

#[test]
fn valid_card_uses_no_allocator() {
    let truth = Truth::new(
        [[0.98, 0.01, 0.01], [0.01, 0.99, 0.0], [0.0, 0.02, 0.98]],
        [1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0],
    );
    let path = fixture_dir().join("no-allocator.llk");
    fs::write(&path, card_bytes(&truth, "translator-no-alloc", false)).expect("fixture");
    for binary in ["llk-translate-guard", "llk-translate-asan"] {
        let output = run(binary, &path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{binary} failed: {stderr}");
        assert!(
            !stderr.contains("AddressSanitizer") && !stderr.contains("runtime error"),
            "{binary}: {stderr}"
        );
    }
}

fn translation_curve_at(ev: &[f64], curve: &[f64], value: f64) -> f64 {
    if value <= ev[0] {
        return curve[0];
    }
    if value >= ev[ev.len() - 1] {
        return curve[curve.len() - 1];
    }
    let mut segment = 0;
    while segment < ev.len() - 2 && value > ev[segment + 1] {
        segment += 1;
    }
    let fraction = (value - ev[segment]) / (ev[segment + 1] - ev[segment]);
    curve[segment] + (curve[segment + 1] - curve[segment]) * fraction
}

fn assert_reproduces(name: &str, truth: &Truth, json: &Value) {
    let matrix: Vec<Vec<f64>> =
        serde_json::from_value(json["matrix"].clone()).expect("matrix in output");
    let ev: Vec<f64> = serde_json::from_value(json["curve"]["ev"].clone()).expect("ev");
    let curves: Vec<Vec<f64>> = ["r", "g", "b"]
        .iter()
        .map(|key| serde_json::from_value(json["curve"][key].clone()).expect("curve"))
        .collect();
    let scale = truth
        .curve
        .iter()
        .flatten()
        .fold(0.0_f64, |acc, value| acc.max(value.abs()));
    let mut compared = 0;
    for record in truth.fit() {
        let input = record.input.map(f64::from);
        let expected = truth.apply(input);
        if (0..3).any(|ch| expected[ch] > truth.curve[ch][CURVE_POINTS - 1]) {
            continue;
        }
        let forward: Vec<f64> = (0..3)
            .map(|ch| {
                matrix[ch][0] * input[0] + matrix[ch][1] * input[1] + matrix[ch][2] * input[2]
            })
            .collect();
        for channel in 0..3 {
            let got =
                translation_curve_at(&ev, &curves[channel], (forward[channel] / MID_GRAY).log2());
            assert!(
                (got - expected[channel]).abs() <= 0.01 * scale,
                "{name}: probe {channel} {got} vs {}",
                expected[channel]
            );
        }
        compared += 1;
    }
    assert!(compared >= 20, "{name}: only {compared} probes compared");
    let lab_rms = json["fit"]["lab_rms"].as_f64().expect("lab_rms");
    assert!(lab_rms < 1.0, "{name}: fit residual {lab_rms}");
}

#[test]
fn non_unit_row_sums_shift_the_neutral_sampling_and_reproduce_the_look() {
    let truth = Truth::new(
        [[1.04, 0.02, 0.01], [0.01, 0.96, -0.01], [-0.01, 0.03, 1.03]],
        [1.01, 0.99, 1.02],
        [1.04, 0.98, 1.06],
    );
    assert!(
        truth
            .matrix
            .iter()
            .any(|row| (row.iter().sum::<f64>() - 1.0).abs() > 0.02)
    );
    let path = fixture_dir().join("non-unit-rows.llk");
    fs::write(&path, card_bytes(&truth, "translator-row-sums", false)).expect("fixture");
    let json = translate(&path);
    assert_reproduces("non-unit rows", &truth, &json);
}

#[test]
fn a_matrix_whose_rows_lose_gain_still_reproduces_the_look() {
    let truth = Truth::new(
        [[0.96, 0.02, 0.01], [0.02, 1.03, -0.02], [-0.02, 0.03, 0.95]],
        [0.98, 1.02, 0.96],
        [0.96, 1.05, 0.99],
    );
    let path = fixture_dir().join("row-gain.llk");
    fs::write(&path, card_bytes(&truth, "translator-row-gain", false)).expect("fixture");
    let json = translate(&path);
    assert_reproduces("row gain", &truth, &json);
}

#[derive(Clone, Copy)]
struct Entry {
    kind: u32,
    offset: u32,
    length: u32,
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn section_count(bytes: &[u8]) -> usize {
    usize::from(read_u16(bytes, 12))
}

fn table_entries(bytes: &[u8]) -> Vec<Entry> {
    (0..section_count(bytes))
        .map(|i| {
            let base = HEADER_LENGTH + 16 * i;
            Entry {
                kind: read_u32(bytes, base),
                offset: read_u32(bytes, base + 4),
                length: read_u32(bytes, base + 8),
            }
        })
        .collect()
}

fn table_end(bytes: &[u8]) -> usize {
    HEADER_LENGTH + 16 * section_count(bytes)
}

struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xffff_ffff)
    }

    fn update(&mut self, data: &[u8]) {
        for byte in data {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(self.0 & 1);
                self.0 = (self.0 >> 1) ^ (0xedb8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        !self.0
    }
}

fn fix_header_crc(bytes: &mut [u8]) {
    let mut crc = Crc32::new();
    crc.update(&bytes[..16]);
    crc.update(&[0, 0, 0, 0]);
    crc.update(&bytes[20..HEADER_LENGTH]);
    crc.update(&bytes[HEADER_LENGTH..table_end(bytes)]);
    write_u32(bytes, 16, crc.finish());
}

fn fix_content_id(bytes: &mut [u8]) {
    let digest = Sha256::digest(&bytes[table_end(bytes)..]);
    bytes[0x14..0x34].copy_from_slice(&digest);
}

fn fix_section_crc(bytes: &mut [u8], index: usize) {
    let entry = table_entries(bytes)[index];
    let mut crc = Crc32::new();
    crc.update(&bytes[entry.offset as usize..(entry.offset + entry.length) as usize]);
    write_u32(bytes, HEADER_LENGTH + 16 * index + 12, crc.finish());
}

fn entry_index(bytes: &[u8], kind: u32) -> usize {
    table_entries(bytes)
        .iter()
        .position(|entry| entry.kind == kind)
        .expect("section kind")
}

fn malformed_cases(valid: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    let mut push = |name: &str, mutate: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = valid.to_vec();
        mutate(&mut bytes);
        cases.push((name.to_string(), bytes));
    };

    let neutral = entry_index(valid, 3);
    push("empty", &|bytes| bytes.clear());
    push("short-header", &|bytes| bytes.truncate(63));
    push("magic", &|bytes| bytes[0] ^= 1);
    push("version", &|bytes| bytes[4] = 2);
    push("header-length", &|bytes| bytes[6] = 65);
    push("file-length", &|bytes| {
        let length = read_u32(bytes, 8).wrapping_add(1);
        write_u32(bytes, 8, length);
        fix_header_crc(bytes);
    });
    push("section-count", &|bytes| {
        bytes[12] = 0xff;
        bytes[13] = 0xff;
    });
    push("reserved-u16", &|bytes| bytes[14] = 1);
    push("reserved-header", &|bytes| bytes[0x34] = 1);
    push("header-crc", &|bytes| bytes[16] ^= 1);
    push("content-id", &move |bytes| {
        let offset = table_entries(bytes)[neutral].offset as usize;
        bytes[offset + 8] ^= 1;
    });
    push("section-crc", &move |bytes| {
        let offset = table_entries(bytes)[neutral].offset as usize;
        bytes[offset + 8] ^= 1;
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });
    push("truncated-section", &|bytes| {
        let length = bytes.len();
        bytes.truncate(length - 7);
    });

    push("table-crc", &move |bytes| {
        bytes[HEADER_LENGTH + 16 * neutral + 12] ^= 1;
    });
    push("neutral-count", &move |bytes| {
        let offset = table_entries(bytes)[neutral].offset as usize;
        write_u32(bytes, offset, 256);
        fix_section_crc(bytes, neutral);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });
    push("neutral-nonfinite", &move |bytes| {
        let offset = table_entries(bytes)[neutral].offset as usize;
        write_u32(bytes, offset + 4, 0x7fc0_0000);
        fix_section_crc(bytes, neutral);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });

    let semantics = entry_index(valid, 2);
    push("semantics-value", &move |bytes| {
        let offset = table_entries(bytes)[semantics].offset as usize;
        bytes[offset] = 2;
        fix_section_crc(bytes, semantics);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });

    let fit = entry_index(valid, 4);
    push("fit-count", &move |bytes| {
        let offset = table_entries(bytes)[fit].offset as usize;
        write_u32(bytes, offset, 1);
        fix_section_crc(bytes, fit);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });
    push("fit-nonfinite", &move |bytes| {
        let offset = table_entries(bytes)[fit].offset as usize;
        write_u32(bytes, offset + 4, 0x7f80_0000);
        fix_section_crc(bytes, fit);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });
    push("fit-negative-weight", &move |bytes| {
        let offset = table_entries(bytes)[fit].offset as usize;
        write_u32(bytes, offset + 4 + 28, 0xbf80_0000);
        fix_section_crc(bytes, fit);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });

    let meta = entry_index(valid, 1);
    push("meta-uppercase-slug", &move |bytes| {
        let offset = table_entries(bytes)[meta].offset as usize;
        bytes[offset + 3] = b'B';
        fix_section_crc(bytes, meta);
        fix_content_id(bytes);
        fix_header_crc(bytes);
    });

    push("unsorted-kinds", &|bytes| {
        let first = read_u32(bytes, HEADER_LENGTH);
        let second = read_u32(bytes, HEADER_LENGTH + 16);
        write_u32(bytes, HEADER_LENGTH, second);
        write_u32(bytes, HEADER_LENGTH + 16, first);
        fix_header_crc(bytes);
    });
    push("duplicate-kind", &|bytes| {
        let first = read_u32(bytes, HEADER_LENGTH);
        write_u32(bytes, HEADER_LENGTH + 16, first);
        fix_header_crc(bytes);
    });
    push("overlap", &|bytes| {
        let first = read_u32(bytes, HEADER_LENGTH + 4);
        write_u32(bytes, HEADER_LENGTH + 16 + 4, first);
        fix_header_crc(bytes);
    });
    push("misaligned", &|bytes| {
        let offset = read_u32(bytes, HEADER_LENGTH + 16 + 4).wrapping_add(2);
        write_u32(bytes, HEADER_LENGTH + 16 + 4, offset);
        fix_header_crc(bytes);
    });
    push("inside-table", &|bytes| {
        write_u32(bytes, HEADER_LENGTH + 16 + 4, HEADER_LENGTH as u32);
        fix_header_crc(bytes);
    });
    push("missing-fit", &|bytes| {
        write_u32(bytes, HEADER_LENGTH + 16 * 3, 7);
        fix_header_crc(bytes);
    });
    cases
}

#[test]
fn malformed_cards_are_rejected() {
    let truth = Truth::new(
        [[0.97, 0.02, 0.01], [0.02, 1.0, -0.02], [0.0, 0.03, 0.97]],
        [1.0, 1.0, 1.0],
        [1.0, 1.0, 1.0],
    );
    let valid = card_bytes(&truth, "translator-malformed", false);
    assert!(
        run("llk-translate-asan", &{
            let path = fixture_dir().join("malformed-valid.llk");
            fs::write(&path, &valid).expect("fixture");
            path
        })
        .status
        .success(),
        "the unmutated card must translate"
    );

    let dir = fixture_dir();
    for (name, bytes) in malformed_cases(&valid) {
        let path = dir.join(format!("malformed-{name}.llk"));
        fs::write(&path, &bytes).expect("fixture");
        let output = run("llk-translate-asan", &path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "case {name} was not rejected: {stderr}"
        );
        assert!(
            !stderr.contains("AddressSanitizer") && !stderr.contains("runtime error"),
            "case {name}: {stderr}"
        );
        let inspect = Command::new(binaries().join("llk-translate-asan"))
            .arg("--inspect")
            .arg(&path)
            .output()
            .expect("run inspect");
        assert_eq!(
            inspect.status.code(),
            Some(2),
            "case {name} passed --inspect: {}",
            String::from_utf8_lossy(&inspect.stderr)
        );
    }
}

#[test]
fn an_unknown_section_with_a_bad_crc_is_rejected() {
    use looklab_export::card::{CardError, read_card};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/card/conformance-v1");
    let mut bytes = fs::read(root.join("valid/unknown-section.llk")).expect("unknown section card");
    let index = entry_index(&bytes, 77);
    let crc_offset = HEADER_LENGTH + 16 * index + 12;
    let crc = read_u32(&bytes, crc_offset) ^ 1;
    write_u32(&mut bytes, crc_offset, crc);
    fix_header_crc(&mut bytes);
    assert!(matches!(read_card(&bytes), Err(CardError::SectionCrc(77))));
    let path = fixture_dir().join("unknown-bad-crc.llk");
    fs::write(&path, &bytes).expect("fixture");
    for binary in ["llk-translate-gcc", "llk-translate-asan"] {
        let output = Command::new(binaries().join(binary))
            .arg("--inspect")
            .arg(&path)
            .output()
            .expect("run inspect");
        assert_eq!(output.status.code(), Some(2), "{binary} accepted it");
    }
}

#[test]
fn conformance_set_under_sanitizers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/card/conformance-v1");
    let index: Value =
        serde_json::from_slice(&fs::read(root.join("index.json")).expect("index")).expect("json");
    let asan = binaries().join("llk-translate-asan");
    for entry in index["files"].as_array().expect("files") {
        let path = root.join(entry["path"].as_str().expect("path"));
        let outcome = entry["outcome"].as_str().expect("outcome");
        let output = Command::new(&asan)
            .arg(&path)
            .output()
            .expect("run translator");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("AddressSanitizer") && !stderr.contains("runtime error"),
            "{}: {stderr}",
            path.display()
        );
        if outcome == "valid" {
            assert!(output.status.success(), "{}: {stderr}", path.display());
            let json: Value = serde_json::from_slice(&output.stdout).expect("json");
            let matrix = json["matrix"].as_array().expect("matrix");
            assert!(
                matrix
                    .iter()
                    .flat_map(|row| row.as_array().expect("row"))
                    .all(|value| value.as_f64().is_some_and(f64::is_finite)),
                "{}: non-finite matrix",
                path.display()
            );
        } else {
            assert_eq!(
                output.status.code(),
                Some(2),
                "{}: {stderr}",
                path.display()
            );
        }
    }
}
