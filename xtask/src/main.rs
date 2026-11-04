// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// Mirrors the core float allow-list in AGENTS.md "Numerics".
const FLOAT_METHOD_ALLOW: &[&str] = &[
    "sqrt",
    "abs",
    "min",
    "max",
    "clamp",
    "floor",
    "ceil",
    "round",
    "trunc",
    "mul_add",
    "signum",
    "copysign",
    "to_bits",
    "from_bits",
    "is_nan",
    "is_finite",
    "is_infinite",
    "is_normal",
    "is_subnormal",
    "is_sign_positive",
    "is_sign_negative",
    "to_le_bytes",
    "from_le_bytes",
    "total_cmp",
];

const CORE_CRATES: &[&str] = &[
    "looklab-color",
    "looklab-samples",
    "looklab-upload",
    "looklab-model",
    "looklab-fit",
    "looklab-match",
    "looklab-export",
    "looklab-trust",
    "looklab-prep",
    "looklab-meta",
    "looklab-raw",
    "looklab-schema",
    "looklab-report",
    "looklab-wasm",
    "looklab-prep-module",
];

const SPDX_LINE: &str = "// SPDX-License-Identifier: AGPL-3.0-or-later";

fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("float-methods") => float_methods(),
        Some("spdx") => spdx(),
        Some("bitid") => bitid(),
        Some("wasm-core") => wasm_core(),
        Some("prep-module") => prep_module(),
        Some(other) => bail!("unknown xtask subcommand {other:?}"),
        None => bail!("usage: cargo xtask <float-methods|spdx|bitid|wasm-core|prep-module>"),
    }
}

fn workspace_root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask directory has no parent")
}

fn cargo(root: &Path) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(root);
    command
}

fn run_status(command: &mut Command, what: &str) -> Result<()> {
    let status = command.status().with_context(|| what.to_string())?;
    if !status.success() {
        bail!("{what} failed with {status}");
    }
    Ok(())
}

fn run_capture(command: &mut Command, what: &str) -> Result<String> {
    let output = command.output().with_context(|| what.to_string())?;
    if !output.status.success() {
        bail!(
            "{what} failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{what}: stdout is not UTF-8"))
}

fn float_methods() -> Result<()> {
    let root = workspace_root()?;
    let sysroot = rust_sysroot()?;
    let disallowed = read_disallowed_float_paths(&root.join("clippy.toml"))?;
    let mut missing = BTreeSet::new();
    for ty in ["f64", "f32"] {
        let mut methods = BTreeSet::new();
        for file in [
            sysroot.join(format!("lib/rustlib/src/rust/library/core/src/num/{ty}.rs")),
            sysroot.join(format!("lib/rustlib/src/rust/library/std/src/num/{ty}.rs")),
        ] {
            let source = fs::read_to_string(&file).with_context(|| {
                format!(
                    "reading {} (is the rust-src component installed?)",
                    file.display()
                )
            })?;
            methods.extend(parse_stable_inherent_methods(&source, ty));
        }
        let stable = methods
            .into_iter()
            .filter(|method| !FLOAT_METHOD_ALLOW.contains(&method.as_str()))
            .map(|method| format!("{ty}::{method}"))
            .collect();
        missing.extend(missing_stable_methods(stable, &disallowed));
    }
    if !missing.is_empty() {
        let list = missing.into_iter().collect::<Vec<_>>().join("\n  ");
        bail!("root clippy.toml is missing these stable f64/f32 methods:\n  {list}");
    }
    println!(
        "float-methods: root clippy.toml covers every stable f64/f32 method outside the allow-list"
    );
    Ok(())
}

fn rust_sysroot() -> Result<PathBuf> {
    let output = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .context("running rustc --print sysroot")?;
    if !output.status.success() {
        bail!(
            "rustc --print sysroot failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text =
        String::from_utf8(output.stdout).context("rustc --print sysroot is not valid UTF-8")?;
    Ok(PathBuf::from(text.trim()))
}

fn read_disallowed_float_paths(path: &Path) -> Result<BTreeSet<String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_disallowed_paths(&text)?
        .into_iter()
        .filter(|path| path.starts_with("f64::") || path.starts_with("f32::"))
        .collect())
}

fn parse_disallowed_paths(text: &str) -> Result<BTreeSet<String>> {
    let config: toml::Value = toml::from_str(text).context("parsing clippy.toml")?;
    let entries = config
        .get("disallowed-methods")
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(entries
        .into_iter()
        .filter_map(|entry| match entry {
            toml::Value::String(path) => Some(path),
            toml::Value::Table(table) => table
                .get("path")
                .and_then(toml::Value::as_str)
                .map(str::to_string),
            _ => None,
        })
        .collect())
}

fn missing_stable_methods(
    stable: BTreeSet<String>,
    disallowed: &BTreeSet<String>,
) -> BTreeSet<String> {
    stable
        .into_iter()
        .filter(|path| !disallowed.contains(path))
        .collect()
}

/// Method names of `pub [const] [unsafe] fn` items inside `impl <ty> { ... }` blocks,
/// excluding items carrying an `#[unstable` attribute.
fn parse_stable_inherent_methods(source: &str, ty: &str) -> Vec<String> {
    let header = format!("impl {ty} {{");
    let mut methods = Vec::new();
    let mut inside = false;
    let mut impl_indent = 0;
    let mut attrs = String::new();
    let mut in_multiline_attr = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if !inside {
            if trimmed == header {
                inside = true;
                impl_indent = line.len() - line.trim_start().len();
            }
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == impl_indent && trimmed == "}" {
            inside = false;
            attrs.clear();
            in_multiline_attr = false;
            continue;
        }
        if in_multiline_attr {
            attrs.push_str(trimmed);
            attrs.push(' ');
            if trimmed.contains(']') {
                in_multiline_attr = false;
            }
            continue;
        }
        if trimmed.starts_with("#[") {
            attrs.push_str(trimmed);
            attrs.push(' ');
            if !trimmed.contains(']') {
                in_multiline_attr = true;
            }
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if let Some(name) = method_name(trimmed)
            && !attrs.contains("#[unstable")
        {
            methods.push(name.to_string());
        }
        attrs.clear();
    }
    methods
}

fn method_name(declaration: &str) -> Option<&str> {
    let rest = declaration.strip_prefix("pub ")?;
    let rest = rest.strip_prefix("const ").unwrap_or(rest);
    let rest = rest.strip_prefix("unsafe ").unwrap_or(rest);
    let rest = rest.strip_prefix("fn ")?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    (end > 0).then(|| &rest[..end])
}

fn spdx() -> Result<()> {
    let root = workspace_root()?;
    let mut checked = 0usize;
    let mut missing = Vec::new();
    for dir in ["crates", "xtask", "fuzz"] {
        let dir = root.join(dir);
        if dir.is_dir() {
            collect_rs(&dir, &mut checked, &mut missing)?;
        }
    }
    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n  ");
        bail!(
            "{} Rust files lack the SPDX header:\n  {list}",
            missing.len()
        );
    }
    println!("spdx: {checked} Rust files carry the SPDX header");
    Ok(())
}

fn collect_rs(dir: &Path, checked: &mut usize, missing: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            collect_rs(&path, checked, missing)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            *checked += 1;
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            if text.lines().next() != Some(SPDX_LINE) {
                missing.push(path);
            }
        }
    }
    Ok(())
}

struct BitidExample {
    package: String,
    stem: String,
}

fn bitid() -> Result<()> {
    let root = workspace_root()?;
    let examples = discover_bitid_examples(&root)?;
    if examples.is_empty() {
        println!("bitid: no crates/*/examples/bitid*.rs found; nothing to compare");
        return Ok(());
    }
    let target_dir =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let mut rows = Vec::new();
    let mut failures = Vec::new();
    for example in &examples {
        let packages = ["-p", example.package.as_str()].map(str::to_string);
        let stem = example.stem.as_str();

        let mut build = cargo(&root);
        build
            .args(["build", "--quiet", "--release", "--example", stem])
            .args(packages.iter().map(String::as_str));
        run_status(
            &mut build,
            &format!("building {}/{} for the host", example.package, example.stem),
        )?;
        let mut run = cargo(&root);
        run.args(["run", "--quiet", "--release", "--example", stem])
            .args(packages.iter().map(String::as_str));
        let native = run_capture(
            &mut run,
            &format!("running {}/{} natively", example.package, example.stem),
        )?;

        let mut build = cargo(&root);
        build
            .args([
                "build",
                "--quiet",
                "--release",
                "--example",
                stem,
                "--target",
                "wasm32-wasip1",
            ])
            .args(packages.iter().map(String::as_str));
        run_status(
            &mut build,
            &format!(
                "building {}/{} for wasm32-wasip1",
                example.package, example.stem
            ),
        )?;
        let wasm = target_dir
            .join("wasm32-wasip1/release/examples")
            .join(format!("{stem}.wasm"));
        let mut wasmtime = Command::new("wasmtime");
        wasmtime.arg("run").arg(&wasm);
        let wasm_output = run_capture(
            &mut wasmtime,
            &format!(
                "running {}/{} under wasmtime",
                example.package, example.stem
            ),
        )?;

        let native_lines = native.lines().collect::<Vec<_>>();
        let wasm_lines = wasm_output.lines().collect::<Vec<_>>();
        let matches = native_lines == wasm_lines;
        if !matches {
            failures.push(format!(
                "{}/{}: first difference {}",
                example.package,
                example.stem,
                first_difference(&native_lines, &wasm_lines)
            ));
        }
        rows.push((
            example.package.clone(),
            example.stem.clone(),
            native_lines.len(),
            wasm_lines.len(),
            matches,
        ));
    }

    println!(
        "{:<24} {:<16} {:>6} {:>6}  result",
        "package", "example", "native", "wasm"
    );
    for (package, stem, native, wasm, matches) in &rows {
        println!(
            "{package:<24} {stem:<16} {native:>6} {wasm:>6}  {}",
            if *matches { "match" } else { "MISMATCH" }
        );
    }
    if !failures.is_empty() {
        bail!(
            "native and wasm32-wasip1 output differ:\n  {}",
            failures.join("\n  ")
        );
    }
    Ok(())
}

fn discover_bitid_examples(root: &Path) -> Result<Vec<BitidExample>> {
    let mut examples = Vec::new();
    let crates = root.join("crates");
    if !crates.is_dir() {
        return Ok(examples);
    }
    for entry in fs::read_dir(&crates).with_context(|| format!("reading {}", crates.display()))? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let examples_dir = path.join("examples");
        if !examples_dir.is_dir() {
            continue;
        }
        let package = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("crate directory name is not UTF-8")?
            .to_string();
        for entry in fs::read_dir(&examples_dir)
            .with_context(|| format!("reading {}", examples_dir.display()))?
        {
            let file = entry?.path();
            let stem = match (
                file.extension().and_then(|extension| extension.to_str()),
                file.file_stem().and_then(|stem| stem.to_str()),
            ) {
                (Some("rs"), Some(stem)) if stem.starts_with("bitid") => stem.to_string(),
                _ => continue,
            };
            examples.push(BitidExample {
                package: package.clone(),
                stem,
            });
        }
    }
    examples.sort_by(|a, b| {
        (a.package.as_str(), a.stem.as_str()).cmp(&(b.package.as_str(), b.stem.as_str()))
    });
    Ok(examples)
}

fn first_difference(native: &[&str], wasm: &[&str]) -> String {
    for (index, (left, right)) in native.iter().zip(wasm).enumerate() {
        if left != right {
            return format!("line {}: native {left:?}, wasm {right:?}", index + 1);
        }
    }
    format!("line count: native {}, wasm {}", native.len(), wasm.len())
}

fn prep_module() -> Result<()> {
    let root = workspace_root()?;
    let target_dir =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), |dir| root.join(dir));
    let mut build = cargo(&root);
    build.args([
        "build",
        "--release",
        "--target",
        "wasm32-unknown-unknown",
        "-p",
        "looklab-prep-module",
    ]);
    run_status(
        &mut build,
        "building looklab-prep-module for wasm32-unknown-unknown",
    )?;
    let wasm = target_dir.join("wasm32-unknown-unknown/release/looklab_prep_module.wasm");
    if !wasm.is_file() {
        bail!("expected the module at {}", wasm.display());
    }

    // chrono and getrandom hold wasm-bindgen type-description imports; they are
    // unreachable, so wasm-opt replaces them with nops. Any other import fails.
    let imports = module_imports(&wasm)?;
    for line in &imports {
        if !line.contains("__wbindgen") {
            bail!("prep module imports {line}");
        }
    }
    if !imports.is_empty() {
        let optimized = wasm.with_extension("opt.wasm");
        let mut optimize = Command::new("wasm-opt");
        optimize
            .arg("--remove-imports")
            .arg(&wasm)
            .arg("-o")
            .arg(&optimized);
        run_status(&mut optimize, "removing wasm-bindgen imports")?;
        fs::rename(&optimized, &wasm).with_context(|| format!("replacing {}", wasm.display()))?;
    }

    let mut validate = Command::new("wasm-tools");
    validate
        .args(["validate", "--features=-relaxed-simd"])
        .arg(&wasm);
    run_status(&mut validate, "validating the module without relaxed-simd")?;

    if let Some(line) = module_imports(&wasm)?.first() {
        bail!("prep module imports {line}");
    }

    let bytes = fs::read(&wasm).with_context(|| format!("reading {}", wasm.display()))?;
    let digest = Sha256::digest(&bytes);
    println!("prep-module: validated without relaxed-simd, no imports; sha256 {digest:x}");
    Ok(())
}

fn module_imports(wasm: &Path) -> Result<Vec<String>> {
    let mut print = Command::new("wasm-tools");
    print.args(["print", "--skeleton"]).arg(wasm);
    let skeleton = run_capture(&mut print, "printing the module skeleton")?;
    Ok(skeleton
        .lines()
        .filter(|line| line.trim_start().starts_with("(import"))
        .map(|line| line.trim().to_owned())
        .collect())
}

fn wasm_core() -> Result<()> {
    let root = workspace_root()?;
    let mut build = cargo(&root);
    build.args(["build", "--target", "wasm32-unknown-unknown"]);
    for package in CORE_CRATES {
        build.args(["-p", package]);
    }
    println!(
        "wasm-core: building {} crates for wasm32-unknown-unknown",
        CORE_CRATES.len()
    );
    run_status(
        &mut build,
        "building core crates for wasm32-unknown-unknown",
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{missing_stable_methods, parse_disallowed_paths, parse_stable_inherent_methods};

    #[test]
    fn parser_reads_only_stable_inherent_methods() {
        let source = "\
impl f64 {
    /// Documented.
    #[stable(feature = \"rust1\", since = \"1.0.0\")]
    pub const fn abs(self) -> f64 {
        self
    }

    #[unstable(feature = \"float_gamma\", issue = \"99842\")]
    pub fn gamma(self) -> f64 {
        self
    }

    #[unstable(
        feature = \"float_minimum_maximum\",
        issue = \"91079\"
    )]
    pub fn maximum(self, other: f64) -> f64 {
        self
    }

    pub fn from_bits(v: u64) -> Self {
        Self { v }
    }

    pub const RADIX: u32 = 2;

    pub unsafe fn to_int_unchecked<Int>(self) -> Int {
        unimplemented!()
    }
}

impl f64 {
    pub fn outside_any_allow_list(self) -> f64 {
        self
    }
}
";
        assert_eq!(
            parse_stable_inherent_methods(source, "f64"),
            [
                "abs",
                "from_bits",
                "to_int_unchecked",
                "outside_any_allow_list"
            ]
        );
    }

    #[test]
    fn commented_out_ban_is_reported_missing() {
        let config = "\
disallowed-methods = [
  # { path = \"f64::recip\", reason = \"commented out\" },
  { path = \"f64::ln\", reason = \"transcendental\" },
]
";
        let disallowed = parse_disallowed_paths(config).expect("valid clippy.toml");
        assert!(disallowed.contains("f64::ln"));
        let stable = ["f64::recip".to_string()].into_iter().collect();
        assert_eq!(
            missing_stable_methods(stable, &disallowed),
            BTreeSet::from(["f64::recip".to_string()])
        );
    }
}
