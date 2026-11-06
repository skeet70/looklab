#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""CLI-level cache-boundary tests over an isolated temporary workspace fixture."""

import hashlib
import json
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SOURCE_ROOT = Path(__file__).resolve().parents[2]
SCENE_FIXTURES = ("3115", "1955", "1824", "1797", "3094", "1603")
SCENES_INDEX = b'{"scenes":[]}'


def site_tree_index(variant: str | None = None) -> bytes:
    index: dict[str, object] = {"format": "looklab-site/1", "scenes": []}
    if variant is not None:
        index["variant"] = variant
    return json.dumps(index, separators=(",", ":")).encode("utf-8")


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def crate_manifest(name: str, extra: str = "") -> str:
    return f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "2021"\n{extra}'


def build_fixture(root: Path) -> None:
    (root / "tools").mkdir(parents=True)
    shutil.copyfile(SOURCE_ROOT / "tools/build_products.py", root / "tools/build_products.py")
    write(
        root / "Cargo.toml",
        '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n',
    )
    write(root / "Cargo.lock", "")
    write(root / "rust-toolchain.toml", '[toolchain]\nchannel = "stable"\n')
    write(root / "flake.nix", "{}\n")
    write(root / "flake.lock", "{}\n")
    write(root / ".cargo/config.toml", "")
    write(root / "README.md", "outside the documented include trees\n")

    write(
        root / "crates/looklab-prep-module/Cargo.toml",
        crate_manifest("looklab-prep-module", '\n[lib]\ncrate-type = ["cdylib", "rlib"]\n'),
    )
    write(
        root / "crates/looklab-prep-module/src/lib.rs",
        'const CHART: &str = include_str!("../../../data/charts/layouts.json");\n'
        "pub fn chart() -> &'static str { CHART }\n",
    )
    write(
        root / "crates/looklab-scene-module/Cargo.toml",
        crate_manifest("looklab-scene-module", '\n[lib]\ncrate-type = ["cdylib", "rlib"]\n'),
    )
    write(root / "crates/looklab-scene-module/src/lib.rs", "pub fn scene() {}\n")
    write(
        root / "crates/looklab-publish/Cargo.toml",
        crate_manifest(
            "looklab-publish",
            '\n[[example]]\nname = "site_data"\n',
        ),
    )
    write(root / "crates/looklab-publish/src/lib.rs", "pub fn data() {}\n")
    write(root / "crates/looklab-publish/examples/site_data.rs", "fn main() {}\n")
    write(
        root / "crates/looklab-wasm/Cargo.toml",
        crate_manifest(
            "looklab-wasm",
            '\n[features]\ndefault = ["ui"]\nui = []\nworker = []\nfixtures = []\n'
            '\n[[example]]\nname = "publish_match_fixture"\nrequired-features = ["fixtures"]\n',
        ),
    )
    write(root / "crates/looklab-wasm/src/lib.rs", "pub fn worker() {}\n")
    write(root / "crates/looklab-wasm/examples/publish_match_fixture.rs", "fn main() {}\n")

    write(root / "data/charts/layouts.json", "{}\n")
    write(root / "fixtures/pattern/pattern-portrait.png", "pattern\n")
    manifest = []
    for identifier in SCENE_FIXTURES:
        payload = f"scene-{identifier}\n".encode("utf-8")
        path = root / "fixtures/cache" / f"scene-{identifier}.dng"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
        manifest.append(
            '[[fixture]]\nfile = "scene-{name}.dng"\nsha256 = "{digest}"\nsize = {size}\n'.format(
                name=identifier,
                digest=hashlib.sha256(payload).hexdigest(),
                size=len(payload),
            )
        )
    write(root / "fixtures/manifest.toml", "\n".join(manifest))
    (root / "site/styles").mkdir(parents=True)


def run_cli(root: Path, *arguments: str, env: dict | None = None) -> subprocess.CompletedProcess:
    environment = dict(os.environ)
    environment["LOOKLAB_BUILD_ROOT"] = str(root)
    if env:
        environment.update(env)
    return subprocess.run(
        [sys.executable, str(root / "tools/build_products.py"), *arguments],
        cwd=root,
        capture_output=True,
        text=True,
        env=environment,
    )


def identity(root: Path, *arguments: str, env: dict | None = None) -> dict:
    result = run_cli(root, *arguments, env=env)
    if result.returncode != 0:
        raise AssertionError(
            f"identity command {arguments} failed: {result.stderr or result.stdout}"
        )
    return json.loads(result.stdout)


def wasm_receipt(cache: Path, product: dict, output: str) -> Path:
    return cache / "wasm" / f"{output}.{product['key']}.json"


def site_receipts(cache: Path) -> list[Path]:
    directory = cache / "site-data"
    return sorted(directory.glob("*.json")) if directory.is_dir() else []


def with_shim(shim: Path, env: dict | None = None) -> dict:
    environment = dict(env or {})
    environment["PATH"] = f"{shim}{os.pathsep}{os.environ['PATH']}"
    return environment


def clean_wasm_module() -> bytes:
    with tempfile.TemporaryDirectory() as temporary:
        directory = Path(temporary)
        source = directory / "clean.wat"
        module = directory / "clean.wasm"
        source.write_text('(module (func (export "probe")))\n', encoding="utf-8")
        parsed = subprocess.run(
            ["wasm-tools", "parse", str(source), "-o", str(module)],
            capture_output=True,
            text=True,
        )
        if parsed.returncode != 0:
            raise AssertionError(parsed.stderr)
        return module.read_bytes()


def cargo_shim(
    parent: Path, *, site_index: bytes | None = None, name: str = "cargo-shim"
) -> tuple[Path, Path]:
    real = shutil.which("cargo")
    if real is None:
        raise AssertionError("cargo is required; run tests inside nix develop")
    directory = parent / name
    directory.mkdir()
    log = parent / f"{name}.log"
    run = "    exit 86\n"
    if site_index is not None:
        digest = hashlib.sha256(site_index).hexdigest()
        run = (
            '    for output; do :; done\n'
            '    mkdir -p "$output/data/scenes"\n'
            "    printf '%s' "
            f"{shlex.quote(site_index.decode('utf-8'))}"
            ' > "$output/data/index.json"\n'
            "    printf '%s' "
            f"{shlex.quote(SCENES_INDEX.decode('utf-8'))}"
            ' > "$output/data/scenes/index.json"\n'
            f"    printf '%s' {shlex.quote(digest)} > \"$output/site-root.txt\"\n"
            "    exit 0\n"
        )
    script = directory / "cargo"
    script.write_text(
        "#!/bin/sh\n"
        f"printf '%s\\n' \"$*\" >> {shlex.quote(str(log))}\n"
        'case "$1" in\n'
        f"  -V|--version|metadata) exec {shlex.quote(real)} \"$@\" ;;\n"
        "  run)\n"
        + run
        + "    ;;\n"
        "esac\n"
        "exit 86\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    return directory, log


def compile_invocations(log: Path) -> list[str]:
    if not log.is_file():
        return []
    lines = log.read_text(encoding="utf-8").splitlines()
    return [line for line in lines if line.split() and line.split()[0] in {"build", "run"}]


def receipt_value(product: dict, product_name: str, root: Path, output: Path, payload: bytes) -> dict:
    return {
        "schema": 1,
        "kind": "wasm",
        "product": product_name,
        "key": product["key"],
        "profile": product["profile"],
        "tools": product["tools"],
        "components": product["components"],
        "outputs": [
            {
                "path": output.relative_to(root).as_posix(),
                "sha256": hashlib.sha256(payload).hexdigest(),
                "bytes": len(payload),
            }
        ],
    }


class BuildProductBoundaries(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        for tool in ("cargo", "rustc", "wasm-opt", "wasm-tools"):
            if shutil.which(tool) is None:
                raise RuntimeError(f"{tool} is required; run tests inside nix develop")
        cls.temporary = tempfile.TemporaryDirectory(prefix="looklab-build-products-")
        cls.root = Path(cls.temporary.name)
        try:
            build_fixture(cls.root)
            subprocess.run(["git", "-C", str(cls.root), "init", "-q"], check=True)
        except BaseException:
            cls.temporary.cleanup()
            raise
        cls.cache = cls.root / "target/build-products"

    @classmethod
    def tearDownClass(cls) -> None:
        cls.temporary.cleanup()

    def test_css_edit_leaves_worker_and_data_keys_unchanged(self) -> None:
        stylesheet = self.root / "site/styles/.cache-boundary-test.css"
        worker_before = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
        data_before = identity(self.root, "site-data-key")["key"]
        try:
            stylesheet.write_text("body { color: red; }\n", encoding="utf-8")
            worker_after = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
            data_after = identity(self.root, "site-data-key")["key"]
        finally:
            stylesheet.unlink(missing_ok=True)
        self.assertEqual(worker_before, worker_after)
        self.assertEqual(data_before, data_after)

    def test_inherited_dev_profile_flags_invalidate_test_data_only(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            view = parent / "view"
            build_fixture(view)
            on = {
                "CARGO_PROFILE_DEV_DEBUG_ASSERTIONS": "true",
                "CARGO_PROFILE_DEV_OVERFLOW_CHECKS": "true",
            }
            off = {
                "CARGO_PROFILE_DEV_DEBUG_ASSERTIONS": "false",
                "CARGO_PROFILE_DEV_OVERFLOW_CHECKS": "false",
            }
            test_on = identity(view, "site-data-key", env=dict(env, **on))
            test_off = identity(view, "site-data-key", env=dict(env, **off))
            release_on = identity(view, "site-data-key", "--release", env=dict(env, **on))
            release_off = identity(view, "site-data-key", "--release", env=dict(env, **off))
            self.assertNotEqual(test_on["key"], test_off["key"])
            self.assertEqual(release_on["key"], release_off["key"])
            self.assertEqual(
                identity(view, "wasm-key", "--prep", env=dict(env, **on))["prep"]["key"],
                identity(view, "wasm-key", "--prep", env=dict(env, **off))["prep"]["key"],
            )
            test_shim, _ = cargo_shim(
                parent,
                site_index=site_tree_index("test"),
                name="test-shim",
            )
            release_shim, _ = cargo_shim(
                parent,
                site_index=site_tree_index("release"),
                name="release-shim",
            )
            test_build = run_cli(
                view, "site-data", "env-test", env=with_shim(test_shim, dict(env, **on))
            )
            self.assertEqual(test_build.returncode, 0, test_build.stderr)
            release_build = run_cli(
                view,
                "site-data",
                "--release",
                "env-release",
                env=with_shim(release_shim, dict(env, **on)),
            )
            self.assertEqual(release_build.returncode, 0, release_build.stderr)
            receipts = site_receipts(cache)
            self.assertEqual(len(receipts), 2)
            self.assertEqual(
                {json.loads(receipt.read_text(encoding="utf-8"))["key"] for receipt in receipts},
                {test_on["key"], release_on["key"]},
            )
            test_fresh = run_cli(
                view, "site-data", "--check", "env-test", env=dict(env, **on)
            )
            self.assertEqual(test_fresh.returncode, 0, test_fresh.stderr)
            test_stale = run_cli(
                view, "site-data", "--check", "env-test", env=dict(env, **off)
            )
            self.assertEqual(test_stale.returncode, 1)
            for flags in (on, off):
                release_check = run_cli(
                    view,
                    "site-data",
                    "--check",
                    "--release",
                    "env-release",
                    env=dict(env, **flags),
                )
                self.assertEqual(release_check.returncode, 0, release_check.stderr)
            masked_on = dict(
                env,
                **on,
                CARGO_PROFILE_TEST_DEBUG_ASSERTIONS="true",
                CARGO_PROFILE_TEST_OVERFLOW_CHECKS="true",
            )
            masked_off = dict(
                env,
                **off,
                CARGO_PROFILE_TEST_DEBUG_ASSERTIONS="true",
                CARGO_PROFILE_TEST_OVERFLOW_CHECKS="true",
            )
            self.assertEqual(
                identity(view, "site-data-key", env=masked_on)["key"],
                identity(view, "site-data-key", env=masked_off)["key"],
            )

    def test_compile_time_include_edit_invalidates_worker(self) -> None:
        include = self.root / "data/charts/layouts.json"
        original = include.read_bytes()
        before = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
        try:
            include.write_bytes(original + b" ")
            after = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
        finally:
            include.write_bytes(original)
        self.assertNotEqual(before, after)

    def test_unsupported_example_module_fails_closed(self) -> None:
        example = self.root / "crates/looklab-publish/examples/site_data.rs"
        before = identity(self.root, "site-data-key")["key"]
        original = example.read_bytes()
        try:
            example.write_bytes(original + b"\nmod hidden;\n")
            result = run_cli(self.root, "site-data-key")
        finally:
            example.write_bytes(original)
        after = identity(self.root, "site-data-key")["key"]
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(before, after)

    def test_unsupported_include_fails_closed(self) -> None:
        source = self.root / "crates/looklab-prep-module/src/lib.rs"
        before = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
        original = source.read_bytes()
        try:
            source.write_bytes(
                original + b'\nconst BAD: &str = include_str!("../../../README.md");\n'
            )
            result = run_cli(self.root, "wasm-key", "--prep")
        finally:
            source.write_bytes(original)
        after = identity(self.root, "wasm-key", "--prep")["prep"]["key"]
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertEqual(before, after)

    def test_malformed_receipt_never_reuses_the_artifact(self) -> None:
        product = identity(self.root, "wasm-key", "--match")["match"]
        output = self.root / "site/generated/wasm/looklab_match.wasm"
        receipt = wasm_receipt(self.cache, product, "looklab_match.wasm")
        sentinel = b"artifact bytes that must not be blessed"
        output.parent.mkdir(parents=True, exist_ok=True)
        receipt.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(sentinel)
        try:
            receipt.write_text('{"schema": 1, "kind": "wasm",')
            original_receipt = receipt.read_bytes()
            result = run_cli(self.root, "wasm", "--check", "--match")
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stdout, "")
            self.assertEqual(output.read_bytes(), sentinel)
            self.assertEqual(receipt.read_bytes(), original_receipt)
        finally:
            output.unlink(missing_ok=True)
            receipt.unlink(missing_ok=True)

    def test_undecodable_receipts_are_stale_hints(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            view = parent / "view"
            build_fixture(view)
            worker = identity(view, "wasm-key", "--match", env=env)["match"]
            worker_output = view / "site/generated/wasm/looklab_match.wasm"
            worker_receipt = wasm_receipt(cache, worker, "looklab_match.wasm")
            site_shim, _ = cargo_shim(
                parent, site_index=site_tree_index(), name="site-shim"
            )
            site_build = run_cli(view, "site-data", env=with_shim(site_shim, env))
            self.assertEqual(site_build.returncode, 0, site_build.stderr)
            receipts = site_receipts(cache)
            self.assertEqual(len(receipts), 1)
            data_receipt = receipts[0]
            sentinel = b"artifact bytes that must not be blessed"
            malformed = b'\xff\xfe{"schema": 1'
            worker_output.parent.mkdir(parents=True, exist_ok=True)
            worker_receipt.parent.mkdir(parents=True, exist_ok=True)
            worker_output.write_bytes(sentinel)
            worker_receipt.write_bytes(malformed)
            data_receipt.write_bytes(malformed)
            for arguments in (("wasm", "--check", "--match"), ("site-data", "--check")):
                result = run_cli(view, *arguments, env=env)
                self.assertEqual(result.returncode, 1)
            self.assertEqual(worker_output.read_bytes(), sentinel)
            self.assertEqual(worker_receipt.read_bytes(), malformed)
            self.assertEqual(data_receipt.read_bytes(), malformed)
            shim, log = cargo_shim(parent, name="failing-shim")
            gated = with_shim(shim, env)
            for arguments in (("wasm", "--match"), ("site-data",)):
                attempts = len(compile_invocations(log))
                result = run_cli(view, *arguments, env=gated)
                self.assertEqual(result.returncode, 1)
                self.assertGreater(len(compile_invocations(log)), attempts)
            self.assertEqual(worker_output.read_bytes(), sentinel)
            self.assertEqual(worker_receipt.read_bytes(), malformed)
            self.assertEqual(data_receipt.read_bytes(), malformed)

    def test_unexpected_import_and_missing_artifact_are_rejected(self) -> None:
        product = identity(self.root, "wasm-key", "--scene")["scene"]
        output = self.root / "site/generated/wasm/looklab_scene_module.wasm"
        receipt = wasm_receipt(self.cache, product, "looklab_scene_module.wasm")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            clean_source = directory / "clean.wat"
            clean_module = directory / "clean.wasm"
            import_source = directory / "imports.wat"
            import_module = directory / "imports.wasm"
            clean_source.write_text('(module (func (export "probe")))\n', encoding="utf-8")
            import_source.write_text(
                '(module (import "env" "unexpected" (func (param i32))))\n',
                encoding="utf-8",
            )
            for source, module in ((clean_source, clean_module), (import_source, import_module)):
                parsed = subprocess.run(
                    ["wasm-tools", "parse", str(source), "-o", str(module)],
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(parsed.returncode, 0, parsed.stderr)
            clean_payload = clean_module.read_bytes()
            import_payload = import_module.read_bytes()
        output.parent.mkdir(parents=True, exist_ok=True)
        receipt.parent.mkdir(parents=True, exist_ok=True)
        try:
            output.write_bytes(clean_payload)
            receipt.write_text(
                json.dumps(
                    receipt_value(product, "scene", self.root, output, clean_payload), indent=2
                ),
                encoding="utf-8",
            )
            clean = run_cli(self.root, "wasm", "--check", "--scene")
            self.assertEqual(clean.returncode, 0)
            self.assertEqual(output.read_bytes(), clean_payload)

            output.write_bytes(import_payload)
            receipt.write_text(
                json.dumps(
                    receipt_value(product, "scene", self.root, output, import_payload), indent=2
                ),
                encoding="utf-8",
            )
            forbidden = run_cli(self.root, "wasm", "--check", "--scene")
            self.assertEqual(forbidden.returncode, 1)
            self.assertEqual(output.read_bytes(), import_payload)

            output.unlink()
            missing = run_cli(self.root, "wasm", "--check", "--scene")
            self.assertEqual(missing.returncode, 1)
            self.assertFalse(output.exists())
        finally:
            output.unlink(missing_ok=True)
            receipt.unlink(missing_ok=True)


    def test_distinct_source_views_keep_separate_worker_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            view_a = parent / "view-a"
            view_b = parent / "view-b"
            build_fixture(view_a)
            build_fixture(view_b)
            (view_b / "data/charts/layouts.json").write_text('{"variant": 2}\n', encoding="utf-8")
            product_a = identity(view_a, "wasm-key", "--prep", env=env)["prep"]
            product_b = identity(view_b, "wasm-key", "--prep", env=env)["prep"]
            self.assertNotEqual(product_a["key"], product_b["key"])
            payload = clean_wasm_module()
            for view, product in ((view_a, product_a), (view_b, product_b)):
                output = view / "site/generated/wasm/looklab_prep_module.wasm"
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(payload)
                receipt = wasm_receipt(cache, product, "looklab_prep_module.wasm")
                receipt.parent.mkdir(parents=True, exist_ok=True)
                receipt.write_text(
                    json.dumps(receipt_value(product, "prep", view, output, payload), indent=2),
                    encoding="utf-8",
                )
            receipt_a = wasm_receipt(cache, product_a, "looklab_prep_module.wasm")
            receipt_b = wasm_receipt(cache, product_b, "looklab_prep_module.wasm")
            self.assertNotEqual(receipt_a, receipt_b)
            original_a = receipt_a.read_bytes()
            check_a = run_cli(view_a, "wasm", "--check", "--prep", env=env)
            self.assertEqual(check_a.returncode, 0, check_a.stderr)
            check_b = run_cli(view_b, "wasm", "--check", "--prep", env=env)
            self.assertEqual(check_b.returncode, 0, check_b.stderr)
            self.assertTrue(receipt_a.is_file())
            self.assertTrue(receipt_b.is_file())
            self.assertEqual(receipt_a.read_bytes(), original_a)
            self.assertEqual(json.loads(receipt_b.read_text())["key"], product_b["key"])

    def test_distinct_source_views_keep_separate_site_data_receipts(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            view_a = parent / "view-a"
            view_b = parent / "view-b"
            build_fixture(view_a)
            build_fixture(view_b)
            (view_b / "data/charts/layouts.json").write_text('{"variant": 2}\n', encoding="utf-8")
            product_a = identity(view_a, "site-data-key", env=env)
            product_b = identity(view_b, "site-data-key", env=env)
            self.assertNotEqual(product_a["key"], product_b["key"])
            shim, _ = cargo_shim(
                parent, site_index=site_tree_index(), name="site-shim"
            )
            gated = with_shim(shim, env)
            for view in (view_a, view_b):
                built = run_cli(view, "site-data", env=gated)
                self.assertEqual(built.returncode, 0, built.stderr)
            receipts = site_receipts(cache)
            originals = {receipt: receipt.read_bytes() for receipt in receipts}
            check_a = run_cli(view_a, "site-data", "--check", env=env)
            self.assertEqual(check_a.returncode, 0, check_a.stderr)
            check_b = run_cli(view_b, "site-data", "--check", env=env)
            self.assertEqual(check_b.returncode, 0, check_b.stderr)
            for receipt, original in originals.items():
                self.assertEqual(receipt.read_bytes(), original)

    def test_slash_and_double_underscore_destinations_stay_fresh_independently(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            view = parent / "view"
            build_fixture(view)
            destinations = ("output/site", "output__site")
            for index, dest_identity in enumerate(destinations):
                shim, _ = cargo_shim(
                    parent,
                    site_index=site_tree_index(f"variant-{index}"),
                    name=f"destination-shim-{index}",
                )
                built = run_cli(
                    view, "site-data", dest_identity, env=with_shim(shim, env)
                )
                self.assertEqual(built.returncode, 0, built.stderr)
            receipts = site_receipts(cache)
            originals = {receipt: receipt.read_bytes() for receipt in receipts}
            for dest_identity in destinations:
                check = run_cli(view, "site-data", "--check", dest_identity, env=env)
                self.assertEqual(check.returncode, 0, check.stderr)
            for receipt, original in originals.items():
                self.assertEqual(receipt.read_bytes(), original)

    def test_external_and_repository_destinations_with_equal_display_stay_separate(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            view = parent / "view"
            build_fixture(view)
            external = parent / "external-site"
            external.mkdir()
            repo_dest = "external-" + hashlib.sha256(
                str(external.resolve()).encode("utf-8")
            ).hexdigest()
            for index, (dest, variant) in enumerate(
                ((repo_dest, "repository"), (str(external), "external"))
            ):
                shim, _ = cargo_shim(
                    parent,
                    site_index=site_tree_index(variant),
                    name=f"adversarial-shim-{index}",
                )
                built = run_cli(view, "site-data", dest, env=with_shim(shim, env))
                self.assertEqual(built.returncode, 0, built.stderr)
            for dest in (repo_dest, str(external)):
                check = run_cli(view, "site-data", "--check", dest, env=env)
                self.assertEqual(check.returncode, 0, check.stderr)

    def test_stored_module_materializes_in_new_view_without_compiling(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            main = parent / "main"
            env = {"LOOKLAB_BUILD_ROOT": str(main)}
            cache = main / "target/build-products"
            built = parent / "built"
            fresh = parent / "fresh"
            build_fixture(built)
            build_fixture(fresh)
            product = identity(built, "wasm-key", "--prep", env=env)["prep"]
            self.assertEqual(
                identity(fresh, "wasm-key", "--prep", env=env)["prep"]["key"], product["key"]
            )
            payload = clean_wasm_module()
            digest = hashlib.sha256(payload).hexdigest()
            built_output = built / "site/generated/wasm/looklab_prep_module.wasm"
            built_output.parent.mkdir(parents=True, exist_ok=True)
            built_output.write_bytes(payload)
            receipt = wasm_receipt(cache, product, "looklab_prep_module.wasm")
            receipt.parent.mkdir(parents=True, exist_ok=True)
            receipt.write_text(
                json.dumps(receipt_value(product, "prep", built, built_output, payload), indent=2),
                encoding="utf-8",
            )
            store = cache / "wasm" / f"looklab_prep_module.wasm.{digest}.wasm"

            fresh_output = fresh / "site/generated/wasm/looklab_prep_module.wasm"
            self.assertFalse(fresh_output.exists())

            shim, log = cargo_shim(parent)
            gated = dict(env, PATH=f"{shim}{os.pathsep}{os.environ['PATH']}")
            without_store = run_cli(fresh, "wasm", "--prep", env=gated)
            self.assertEqual(without_store.returncode, 1)
            self.assertFalse(fresh_output.exists())
            self.assertNotEqual(compile_invocations(log), [])

            store.parent.mkdir(parents=True, exist_ok=True)
            store.write_bytes(payload)
            log.unlink()
            materialized = run_cli(fresh, "wasm", "--prep", env=gated)
            self.assertEqual(materialized.returncode, 0, materialized.stderr)
            self.assertEqual(fresh_output.read_bytes(), payload)
            self.assertEqual(compile_invocations(log), [])
            self.assertTrue(any(line.startswith("metadata") for line in log.read_text().splitlines()))
            check = run_cli(fresh, "wasm", "--check", "--prep", env=env)
            self.assertEqual(check.returncode, 0, check.stderr)

            (fresh / "data/charts/layouts.json").write_text('{"variant": 2}\n', encoding="utf-8")
            stale_check = run_cli(fresh, "wasm", "--check", "--prep", env=env)
            self.assertEqual(stale_check.returncode, 1)
            self.assertEqual(fresh_output.read_bytes(), payload)
            stale_build = run_cli(fresh, "wasm", "--prep", env=gated)
            self.assertEqual(stale_build.returncode, 1)
            self.assertEqual(fresh_output.read_bytes(), payload)


if __name__ == "__main__":
    unittest.main()
