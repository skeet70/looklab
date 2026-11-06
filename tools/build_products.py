#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Build-product identities and receipts for the looklab static site; standard library only."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass
from enum import Enum
from functools import lru_cache
from pathlib import Path
from typing import Callable, TypeVar

ROOT = Path(__file__).resolve().parent.parent
WASM_DIST = ROOT / "site/generated/wasm"
SCHEMA = 1
WASM_TARGET = "wasm32-unknown-unknown"
IGNORED_PARTS = {"target", ".git", "__pycache__"}
GENERATOR_EXAMPLES = {
    "looklab-publish": "site_data.rs",
    "looklab-wasm": "publish_match_fixture.rs",
}
SCENE_FIXTURES = ("3115", "1955", "1824", "1797", "3094", "1603")
WASM_COMPILE_TIME_TREES = ("data", "fixtures/pattern")
WASM_COMPILE_TIME_FILES = ("fixtures/manifest.toml",)
DATA_COMPILE_TIME_TREES = ("data",)
COMMON_ROOT_FILES = (
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "flake.nix",
    "flake.lock",
    ".cargo/config.toml",
    "tools/build_products.py",
)
UNSUPPORTED_EXAMPLE = re.compile(
    r"^\s*(?:pub\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*;",
    re.MULTILINE,
)
INCLUDE_MACRO = re.compile(r"\binclude(?:_str|_bytes)?!\s*\(")


class Command(str, Enum):
    WASM = "wasm"
    SITE_DATA = "site-data"
    WASM_IDENTITY = "wasm-key"
    SITE_DATA_IDENTITY = "site-data-key"


class Kind(str, Enum):
    WASM = "wasm"
    SITE_DATA = "site-data"


class CargoProfile(str, Enum):
    DEV = "dev"
    RELEASE = "release"
    TEST = "test"


PROFILE_PARENT: dict[CargoProfile, CargoProfile] = {CargoProfile.TEST: CargoProfile.DEV}


class WasmProduct(str, Enum):
    PREP = "prep"
    MATCH = "match"
    SCENE = "scene"


@dataclass(frozen=True)
class WasmSpec:
    output: str
    package: str
    crate: str
    no_default_features: bool
    features: tuple[str, ...]


WASM_PRODUCTS: dict[WasmProduct, WasmSpec] = {
    WasmProduct.PREP: WasmSpec(
        output="looklab_prep_module.wasm",
        package="looklab-prep-module",
        crate="looklab_prep_module",
        no_default_features=False,
        features=(),
    ),
    WasmProduct.MATCH: WasmSpec(
        output="looklab_match.wasm",
        package="looklab-wasm",
        crate="looklab_wasm",
        no_default_features=True,
        features=("worker",),
    ),
    WasmProduct.SCENE: WasmSpec(
        output="looklab_scene_module.wasm",
        package="looklab-scene-module",
        crate="looklab_scene_module",
        no_default_features=False,
        features=(),
    ),
}


@dataclass(frozen=True)
class ReceiptOutput:
    path: str
    sha256: ContentDigest
    bytes: int


@dataclass(frozen=True)
class Receipt:
    kind: Kind
    product: WasmProduct | None
    dest: str | None
    key: IdentityKey
    profile: dict
    tools: dict[str, str]
    components: dict[str, str]
    outputs: tuple[ReceiptOutput, ...]


class BuildError(Exception):
    pass


class Stale(Exception):
    pass


class Sha256Hex(str):
    """Validated lowercase sha256 hex digest."""

    __slots__ = ()

    def __new__(cls, value: str) -> "Sha256Hex":
        if not re.fullmatch(r"[0-9a-f]{64}", value):
            raise ValueError("not a lowercase sha256 hex digest")
        return super().__new__(cls, value)


class ContentDigest(Sha256Hex):
    """Digest of captured file or tree bytes."""

    __slots__ = ()


class IdentityKey(Sha256Hex):
    """Digest over a build-product identity."""

    __slots__ = ()


class DestinationKey(Sha256Hex):
    """Digest over a tagged destination identity."""

    __slots__ = ()


def display(path: Path) -> str:
    try:
        return path.resolve().relative_to(ROOT).as_posix()
    except ValueError:
        return str(path)


def sha256_file(path: Path) -> ContentDigest:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return ContentDigest(digest.hexdigest())


def digest_json(value: object) -> ContentDigest:
    payload = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    return ContentDigest(hashlib.sha256(payload.encode("utf-8")).hexdigest())


def hash_tree(directory: Path) -> ContentDigest:
    files = []
    for path in directory.rglob("*"):
        relative = path.relative_to(directory)
        if any(part in IGNORED_PARTS for part in relative.parts):
            continue
        if path.is_file():
            files.append((relative.as_posix(), path))
    files.sort()
    digest = hashlib.sha256()
    for name, path in files:
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(sha256_file(path).encode("ascii"))
        digest.update(b"\n")
    return ContentDigest(digest.hexdigest())


def write_json_atomic(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.tmp.{os.getpid()}")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def copy_file_atomic(source: Path, destination: Path) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_name(f"{destination.name}.tmp.{os.getpid()}")
    try:
        shutil.copyfile(source, temporary)
        os.replace(temporary, destination)
    finally:
        temporary.unlink(missing_ok=True)


def load_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError) as error:
        raise Stale(f"{display(path)} is unreadable: {error}") from error


def load_toml(path: Path) -> dict:
    import tomllib

    try:
        with path.open("rb") as handle:
            return tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise BuildError(f"{display(path)}: {error}") from error


def run_capture(
    command: list[str], *, cwd: Path | None = None, env: dict | None = None
) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(command, cwd=cwd, env=env, capture_output=True, text=True)
    except FileNotFoundError as error:
        raise BuildError(
            f"{command[0]} is not on PATH; run this script inside `nix develop`"
        ) from error


def tool_version(command: list[str]) -> str:
    result = run_capture(command)
    if result.returncode != 0:
        raise BuildError(
            f"`{shlex.join(command)}` failed: {result.stderr.strip() or result.stdout.strip()}"
        )
    return result.stdout.strip()


def tool_versions(kind: Kind) -> dict[str, str]:
    versions = {
        "rustc": tool_version(["rustc", "-vV"]),
        "cargo": tool_version(["cargo", "-V"]),
    }
    if kind is Kind.WASM:
        versions["wasm-opt"] = tool_version(["wasm-opt", "--version"])
        versions["wasm-tools"] = tool_version(["wasm-tools", "-V"])
    return versions


def profile_env_prefix(profile: CargoProfile) -> str:
    return f"CARGO_PROFILE_{profile.value.upper().replace('-', '_')}_"


def env_component(profile: CargoProfile) -> dict[str, str]:
    names = {
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    }
    own_prefix = profile_env_prefix(profile)
    parent = PROFILE_PARENT.get(profile)
    parent_prefix = profile_env_prefix(parent) if parent is not None else None
    prefixes = ("CARGO_BUILD_", "CARGO_UNSTABLE_", own_prefix)
    pairs = {}
    for name, value in sorted(os.environ.items()):
        if name in {"CARGO_TARGET_DIR", "CARGO_BUILD_JOBS"}:
            continue
        if name in names or name.startswith(prefixes):
            pairs[name] = value
        elif parent_prefix is not None and name.startswith(parent_prefix):
            if own_prefix + name[len(parent_prefix) :] not in os.environ:
                pairs[name] = value
        elif name.startswith("CARGO_TARGET_") and name != "CARGO_TARGET_DIR":
            pairs[name] = value
    return pairs


@lru_cache(maxsize=1)
def cache_root() -> Path:
    configured = os.environ.get("LOOKLAB_BUILD_ROOT")
    if configured:
        return (Path(configured).expanduser().resolve() / "target" / "build-products")
    result = run_capture(["git", "-C", str(ROOT), "rev-parse", "--git-common-dir"])
    if result.returncode != 0:
        raise BuildError(
            "cannot resolve the shared build root: "
            + (result.stderr.strip() or "git rev-parse --git-common-dir failed")
        )
    common = Path(result.stdout.strip())
    if not common.is_absolute():
        common = (ROOT / common).resolve()
    if common.name != ".git":
        raise BuildError(f"unexpected git common directory {common}")
    return common.parent / "target" / "build-products"


@lru_cache(maxsize=None)
def cargo_metadata_for(manifest: Path) -> dict:
    result = run_capture(
        [
            "cargo",
            "metadata",
            "--no-deps",
            "--offline",
            "--locked",
            "--format-version",
            "1",
            "--manifest-path",
            str(manifest),
        ],
        cwd=ROOT,
    )
    if result.returncode != 0:
        raise BuildError(
            f"cargo metadata failed for {display(manifest)}: "
            + (result.stderr.strip() or result.stdout.strip())
        )
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise BuildError(f"cargo metadata returned invalid JSON: {error}") from error
    if not isinstance(data, dict) or not isinstance(data.get("packages"), list):
        raise BuildError("cargo metadata returned no package records")
    return data


class Workspace:
    def __init__(self) -> None:
        self.root_manifest = load_toml(ROOT / "Cargo.toml")
        self.by_dir: dict[Path, dict] = {}
        self.by_name: dict[str, dict] = {}
        for record in cargo_metadata_for(ROOT / "Cargo.toml")["packages"]:
            self._register(record)

    def _register(self, record: dict) -> dict:
        name = record.get("name")
        manifest = record.get("manifest_path")
        if not isinstance(name, str) or not isinstance(manifest, str):
            raise BuildError("cargo metadata package record is missing name or manifest_path")
        directory = Path(manifest).resolve().parent
        dependencies = [
            dependency
            for dependency in record.get("dependencies", [])
            if isinstance(dependency, dict)
        ]
        package = {"name": name, "dir": directory, "dependencies": dependencies}
        existing = self.by_name.get(name)
        if existing is not None and existing["dir"] != directory:
            raise BuildError(f"cargo metadata has two packages named {name}")
        self.by_dir[directory] = package
        self.by_name[name] = package
        return package

    def package_at(self, directory: Path) -> dict:
        directory = directory.resolve()
        if directory not in self.by_dir:
            manifest = directory / "Cargo.toml"
            if not manifest.is_file():
                raise BuildError(f"local dependency {display(directory)} has no Cargo.toml")
            for record in cargo_metadata_for(manifest)["packages"]:
                self._register(record)
            if directory not in self.by_dir:
                raise BuildError(f"cargo metadata does not describe {display(directory)}")
        return self.by_dir[directory]

    def package(self, name: str) -> dict:
        try:
            return self.by_name[name]
        except KeyError as error:
            raise BuildError(f"workspace has no package named {name}") from error

    def closure(self, start: list[str]) -> list[dict]:
        pending = [self.package(name) for name in start]
        seen = set()
        packages = []
        while pending:
            package = pending.pop()
            if package["name"] in seen:
                continue
            seen.add(package["name"])
            packages.append(package)
            for dependency in package["dependencies"]:
                path = dependency.get("path")
                if isinstance(path, str):
                    pending.append(self.package_at(Path(path)))
        return sorted(packages, key=lambda package: package["name"])

    def profile(self, name: CargoProfile) -> dict:
        return self.root_manifest.get("profile", {}).get(name.value, {})


def assert_supported_example(path: Path) -> None:
    text = path.read_text(encoding="utf-8", errors="replace")
    match = UNSUPPORTED_EXAMPLE.search(text)
    if match is not None:
        raise BuildError(
            f"{display(path)} uses {match.group(0).strip()!r}; "
            "extend build_products source selection instead of relying on an implicit module"
        )


def include_literal(rest: str, path: Path) -> str:
    if not rest.startswith('"'):
        raise BuildError(
            f"{display(path)} uses an unsupported include argument; "
            "only a single string literal is supported"
        )
    body = []
    escaped = False
    for character in rest[1:]:
        if escaped:
            body.append("\\" + character)
            escaped = False
        elif character == "\\":
            escaped = True
        elif character == '"':
            try:
                return ast.literal_eval('"' + "".join(body) + '"')
            except (SyntaxError, ValueError) as error:
                raise BuildError(f"{display(path)}: bad include literal") from error
        else:
            body.append(character)
    raise BuildError(f"{display(path)}: unterminated include literal")


def assert_supported_sources(files: list[Path], package_dir: Path) -> None:
    allowed = [
        (package_dir / "src").resolve(),
        (ROOT / "data").resolve(),
        (ROOT / "fixtures/pattern").resolve(),
        (ROOT / "fixtures/manifest.toml").resolve(),
    ]
    for path in files:
        if path.suffix != ".rs":
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        if re.search(r"#\[path\s*=", text):
            raise BuildError(
                f"{display(path)} uses #[path]; extend the documented build_products source trees"
            )
        for match in INCLUDE_MACRO.finditer(text):
            literal = include_literal(text[match.end() :].lstrip(), path)
            resolved = (path.parent / literal).resolve()
            if not resolved.is_file():
                raise BuildError(f"{display(path)}: include target {literal} is missing")
            inside = any(
                resolved == root or (root.is_dir() and resolved.is_relative_to(root))
                for root in allowed
            )
            if not inside:
                raise BuildError(
                    f"{display(path)}: include target {display(resolved)} is outside the "
                    "documented source trees (package src, data, fixtures/pattern, fixtures/manifest.toml)"
                )


def package_target_files(package: dict, example: str | None) -> list[Path]:
    directory = package["dir"]
    files = set()
    source = directory / "src"
    if not source.is_dir():
        raise BuildError(f"{display(directory)} has no src directory")
    for path in source.rglob("*"):
        relative = path.relative_to(directory)
        if any(part in IGNORED_PARTS for part in relative.parts):
            continue
        if path.is_file():
            files.add(path)
    for name in ("Cargo.toml", "build.rs"):
        path = directory / name
        if path.is_file():
            files.add(path)
    if example is not None:
        example_path = directory / "examples" / example
        if not example_path.is_file():
            raise BuildError(f"missing generator example {display(example_path)}")
        assert_supported_example(example_path)
        files.add(example_path)
    ordered = sorted(files)
    assert_supported_sources(ordered, directory)
    return ordered


def package_component(package: dict, example: str | None) -> ContentDigest:
    digest = hashlib.sha256()
    for path in package_target_files(package, example):
        relative = path.relative_to(package["dir"]).as_posix()
        digest.update(relative.encode("utf-8"))
        digest.update(b"\0")
        digest.update(sha256_file(path).encode("ascii"))
        digest.update(b"\n")
    return ContentDigest(digest.hexdigest())


def common_components(scripts: tuple[str, ...]) -> dict[str, str]:
    components = {}
    for relative in COMMON_ROOT_FILES + scripts:
        path = ROOT / relative
        if path.is_file():
            components[relative] = sha256_file(path)
        else:
            components[f"{relative}#absent"] = "absent"
    return components


def add_tree(components: dict[str, str], relative: str, *, required: bool) -> None:
    directory = ROOT / relative
    if not directory.is_dir():
        if required:
            raise BuildError(f"missing input directory {relative}")
        components[f"{relative}#absent"] = "absent"
        return
    components[relative] = hash_tree(directory)


def add_file(components: dict[str, str], relative: str) -> None:
    path = ROOT / relative
    if not path.is_file():
        raise BuildError(f"missing input file {relative}")
    components[relative] = sha256_file(path)


def wasm_identity(product: WasmProduct) -> dict:
    spec = WASM_PRODUCTS[product]
    workspace = Workspace()
    packages = workspace.closure([spec.package])
    components = common_components(("site/scripts/wasm-assets.sh",))
    for package in packages:
        components[f"crate/{package['name']}"] = package_component(package, None)
    for relative in WASM_COMPILE_TIME_TREES:
        add_tree(components, relative, required=relative == "data")
    for relative in WASM_COMPILE_TIME_FILES:
        add_file(components, relative)
    components["environment"] = digest_json(env_component(CargoProfile.RELEASE))
    profile = {
        "package": spec.package,
        "target": WASM_TARGET,
        "cargo_profile": CargoProfile.RELEASE.value,
        "profile_settings": workspace.profile(CargoProfile.RELEASE),
        "no_default_features": spec.no_default_features,
        "features": sorted(spec.features),
    }
    identity = {
        "schema": SCHEMA,
        "kind": Kind.WASM.value,
        "product": product.value,
        "profile": profile,
        "tools": tool_versions(Kind.WASM),
        "components": components,
    }
    identity["key"] = IdentityKey(digest_json(identity))
    return identity


def verify_named_fixtures(manifest: dict) -> dict[str, str]:
    entries = {entry.get("file"): entry for entry in manifest.get("fixture", [])}
    digests = {}
    for identifier in SCENE_FIXTURES:
        filename = f"scene-{identifier}.dng"
        path = ROOT / "fixtures/cache" / filename
        entry = entries.get(filename)
        if entry is None:
            raise BuildError(f"{filename} is missing from fixtures/manifest.toml")
        if not path.is_file():
            raise BuildError(
                f"missing {display(path)}; run `nix develop -c tools/fetch-fixtures.sh`"
            )
        digest = sha256_file(path)
        if digest != entry.get("sha256") or path.stat().st_size != entry.get("size"):
            raise BuildError(
                f"{display(path)} does not match fixtures/manifest.toml; "
                "run `nix develop -c tools/fetch-fixtures.sh`"
            )
        digests[filename] = digest
    return digests


def site_data_identity(cargo_profile: CargoProfile) -> dict:
    workspace = Workspace()
    packages = workspace.closure(["looklab-publish", "looklab-wasm"])
    components = common_components(("site/scripts/site-data.sh",))
    for package in packages:
        components[f"crate/{package['name']}"] = package_component(
            package, GENERATOR_EXAMPLES.get(package["name"])
        )
    for relative in DATA_COMPILE_TIME_TREES:
        add_tree(components, relative, required=True)
    add_file(components, "fixtures/manifest.toml")
    manifest = load_toml(ROOT / "fixtures/manifest.toml")
    for filename, digest in verify_named_fixtures(manifest).items():
        components[f"fixtures/cache/{filename}"] = digest
    components["environment"] = digest_json(env_component(cargo_profile))
    profile = {
        "cargo_profile": cargo_profile.value,
        "profile_settings": workspace.profile(cargo_profile),
        "generators": ["looklab-publish:site_data", "looklab-wasm:publish_match_fixture"],
        "features": {"looklab-wasm": ["fixtures"]},
    }
    identity = {
        "schema": SCHEMA,
        "kind": Kind.SITE_DATA.value,
        "profile": profile,
        "tools": tool_versions(Kind.SITE_DATA),
        "components": components,
    }
    identity["key"] = IdentityKey(digest_json(identity))
    return identity


def receipt_path(product: WasmProduct, key: IdentityKey) -> Path:
    return cache_root() / "wasm" / f"{WASM_PRODUCTS[product].output}.{key}.json"


def store_path(product: WasmProduct, digest: ContentDigest) -> Path:
    return cache_root() / "wasm" / f"{WASM_PRODUCTS[product].output}.{digest}.wasm"


def destination_identity(dest: Path) -> str:
    resolved = dest.resolve()
    try:
        return resolved.relative_to(ROOT).as_posix() or "."
    except ValueError:
        return "external-" + hashlib.sha256(str(resolved).encode("utf-8")).hexdigest()


def destination_key(dest: Path) -> DestinationKey:
    resolved = dest.resolve()
    try:
        scope = ["repository", resolved.relative_to(ROOT).as_posix() or "."]
    except ValueError:
        scope = ["external", str(resolved)]
    return DestinationKey(digest_json(scope))


def dest_lock_name(dest: Path) -> str:
    return hashlib.sha256(str(dest.resolve()).encode("utf-8")).hexdigest()[:32]


def site_receipt_path(dest: Path, key: IdentityKey) -> Path:
    return cache_root() / "site-data" / f"{destination_key(dest)}.{key}.json"


def describe_component_changes(receipt: Receipt, identity: dict) -> str:
    old = receipt.components
    new = identity["components"]
    changed = [name for name in sorted(set(old) | set(new)) if old.get(name) != new.get(name)]
    if not changed:
        return "receipt key does not match current inputs"
    shown = ", ".join(changed[:8])
    if len(changed) > 8:
        shown += f" and {len(changed) - 8} more"
    return f"inputs changed: {shown}"


def receipt_mapping(value: object, label: str) -> dict:
    if not isinstance(value, dict):
        raise Stale(f"{label}: expected an object")
    return value


def receipt_string(mapping: dict, name: str, label: str) -> str:
    value = mapping.get(name)
    if not isinstance(value, str) or not value:
        raise Stale(f"{label}: {name} must be a non-empty string")
    return value


def receipt_string_map(value: object, label: str) -> dict[str, str]:
    mapping = receipt_mapping(value, label)
    if not all(isinstance(key, str) and isinstance(item, str) for key, item in mapping.items()):
        raise Stale(f"{label}: expected string values")
    return dict(mapping)


HexDigest = TypeVar("HexDigest", bound=Sha256Hex)


def receipt_digest(
    mapping: dict, name: str, label: str, parse: Callable[[str], HexDigest]
) -> HexDigest:
    value = mapping.get(name)
    if not isinstance(value, str):
        raise Stale(f"{label}: {name} must be a sha256 hex digest")
    try:
        return parse(value)
    except ValueError as error:
        raise Stale(f"{label}: {name} is not a lowercase sha256 hex digest") from error


def parse_receipt(path: Path, kind: Kind, product: WasmProduct | None = None) -> Receipt:
    if not path.is_file():
        raise Stale(f"no receipt at {display(path)}")
    label = f"receipt {display(path)}"
    raw = receipt_mapping(load_json(path), label)
    if raw.get("schema") != SCHEMA:
        raise Stale(f"{label}: schema {raw.get('schema')}, want {SCHEMA}")
    try:
        receipt_kind = Kind(raw.get("kind"))
    except (TypeError, ValueError) as error:
        raise Stale(f"{label}: unknown kind {raw.get('kind')!r}") from error
    if receipt_kind is not kind:
        raise Stale(f"{label}: kind {receipt_kind.value}, want {kind.value}")
    receipt_product = None
    receipt_dest = None
    if receipt_kind is Kind.WASM:
        try:
            receipt_product = WasmProduct(raw.get("product"))
        except (TypeError, ValueError) as error:
            raise Stale(f"{label}: unknown product {raw.get('product')!r}") from error
        if product is not None and receipt_product is not product:
            raise Stale(f"{label}: product {receipt_product.value}, want {product.value}")
    else:
        receipt_dest = receipt_string(raw, "dest", label)
    key = receipt_digest(raw, "key", label, IdentityKey)
    profile = receipt_mapping(raw.get("profile"), f"{label} profile")
    tools = receipt_string_map(raw.get("tools"), f"{label} tools")
    components = receipt_string_map(raw.get("components"), f"{label} components")
    raw_outputs = raw.get("outputs")
    if not isinstance(raw_outputs, list) or not raw_outputs:
        raise Stale(f"{label}: outputs must be a non-empty list")
    outputs = []
    for index, entry in enumerate(raw_outputs):
        output_label = f"{label} output {index}"
        output = receipt_mapping(entry, output_label)
        relative = receipt_string(output, "path", output_label)
        if Path(relative).is_absolute():
            raise Stale(f"{output_label}: path must be relative")
        digest = receipt_digest(output, "sha256", output_label, ContentDigest)
        size = output.get("bytes")
        if not isinstance(size, int) or isinstance(size, bool) or size < 0:
            raise Stale(f"{output_label}: bytes must be a non-negative integer")
        outputs.append(ReceiptOutput(path=relative, sha256=digest, bytes=size))
    if len({output.path for output in outputs}) != len(outputs):
        raise Stale(f"{label}: duplicate output paths")
    return Receipt(
        kind=receipt_kind,
        product=receipt_product,
        dest=receipt_dest,
        key=key,
        profile=profile,
        tools=tools,
        components=components,
        outputs=tuple(outputs),
    )


def wasm_imports(path: Path) -> list[str]:
    result = run_capture(["wasm-tools", "print", "--skeleton", str(path)])
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().splitlines()
        raise BuildError(
            f"wasm-tools print failed for {display(path)}" + (f": {detail[-1]}" if detail else "")
        )
    return [
        line.strip()
        for line in result.stdout.splitlines()
        if line.strip().startswith("(import")
    ]


def wasm_policy_errors(path: Path) -> list[str]:
    errors = []
    result = run_capture(["wasm-tools", "validate", "--features=-relaxed-simd", str(path)])
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip().splitlines()
        errors.append("wasm-tools validate failed" + (f": {detail[-1]}" if detail else ""))
    imports = wasm_imports(path)
    if imports:
        errors.append("module still imports after wasm-opt --remove-imports: " + imports[0])
    return errors


def raw_import_errors(path: Path) -> list[str]:
    return [
        f"unexpected raw import (only wasm-bindgen type descriptions may be stripped): {line}"
        for line in wasm_imports(path)
        if "__wbindgen" not in line
    ]


def verify_output(path: Path, expected: ReceiptOutput) -> None:
    if not path.is_file():
        raise Stale(f"missing output {display(path)}")
    if path.stat().st_size != expected.bytes:
        raise Stale(f"output size changed: {display(path)}")
    if sha256_file(path) != expected.sha256:
        raise Stale(f"output bytes changed: {display(path)}")


def verify_wasm(product: WasmProduct, identity: dict) -> None:
    spec = WASM_PRODUCTS[product]
    receipt_file = receipt_path(product, identity["key"])
    receipt = parse_receipt(receipt_file, Kind.WASM, product)
    if receipt.key != identity["key"]:
        raise Stale(describe_component_changes(receipt, identity))
    expected_path = f"site/generated/wasm/{spec.output}"
    if len(receipt.outputs) != 1 or receipt.outputs[0].path != expected_path:
        raise Stale(f"receipt {display(receipt_file)} lists unexpected outputs")
    output = ROOT / expected_path
    verify_output(output, receipt.outputs[0])
    errors = wasm_policy_errors(output)
    if errors:
        raise Stale("; ".join(errors))
    verify_output(output, receipt.outputs[0])
    if wasm_identity(product)["key"] != identity["key"]:
        raise Stale("inputs changed while verifying")


def seal_wasm(product: WasmProduct, identity: dict) -> None:
    spec = WASM_PRODUCTS[product]
    output = WASM_DIST / spec.output
    relative = output.relative_to(ROOT).as_posix()
    digest = sha256_file(output)
    copy_file_atomic(output, store_path(product, digest))
    receipt = {
        **identity,
        "outputs": [{"path": relative, "sha256": digest, "bytes": output.stat().st_size}],
    }
    write_json_atomic(receipt_path(product, identity["key"]), receipt)


def materialize_wasm(product: WasmProduct, identity: dict) -> bool:
    spec = WASM_PRODUCTS[product]
    try:
        receipt = parse_receipt(receipt_path(product, identity["key"]), Kind.WASM, product)
        if receipt.key != identity["key"] or len(receipt.outputs) != 1:
            return False
        expected = receipt.outputs[0]
        expected_path = f"site/generated/wasm/{spec.output}"
        if expected.path != expected_path:
            return False
        stored = store_path(product, expected.sha256)
        if not stored.is_file():
            return False
        if stored.stat().st_size != expected.bytes or sha256_file(stored) != expected.sha256:
            return False
        output = ROOT / expected_path
        output.parent.mkdir(parents=True, exist_ok=True)
        staged = output.with_name(f".materialize-{output.name}.{os.getpid()}")
        try:
            shutil.copyfile(stored, staged)
            if staged.stat().st_size != expected.bytes or sha256_file(staged) != expected.sha256:
                return False
            if raw_import_errors(staged) or wasm_policy_errors(staged):
                return False
            if wasm_identity(product)["key"] != identity["key"]:
                raise BuildError(
                    f"{product.value}: worker inputs changed before materializing; "
                    "not publishing."
                )
            os.replace(staged, output)
        finally:
            staged.unlink(missing_ok=True)
    except Stale:
        return False
    verify_output(output, expected)
    print(f"materialized {display(output)}", flush=True)
    return True


def cargo_target_dir() -> Path:
    configured = os.environ.get("CARGO_TARGET_DIR")
    target = Path(configured) if configured else ROOT / "target"
    if not target.is_absolute():
        target = ROOT / target
    return target


def cargo_environment() -> dict[str, str]:
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(cargo_target_dir())
    return environment


def run_checked(command: list[str], *, cwd: Path, env: dict | None = None) -> None:
    print(f"+ {shlex.join(command)}", flush=True)
    result = subprocess.run(command, cwd=cwd, env=env)
    if result.returncode != 0:
        raise BuildError(f"`{shlex.join(command)}` exited with {result.returncode}")


def build_wasm(product: WasmProduct, identity: dict) -> None:
    spec = WASM_PRODUCTS[product]
    command = [
        "cargo",
        "build",
        "--locked",
        "--manifest-path",
        str(ROOT / "Cargo.toml"),
        "-p",
        spec.package,
        "--target",
        WASM_TARGET,
        "--release",
    ]
    if spec.no_default_features:
        command.append("--no-default-features")
    if spec.features:
        command.extend(["--features", ",".join(spec.features)])
    temporary_root = cache_root() / "tmp"
    temporary_root.mkdir(parents=True, exist_ok=True)
    WASM_DIST.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=temporary_root, prefix=f"wasm-{product.value}-") as temporary:
        workspace = Path(temporary)
        raw = workspace / "raw.wasm"
        optimized = workspace / "optimized.wasm"
        canonical = cargo_target_dir() / WASM_TARGET / "release" / f"{spec.crate}.wasm"
        with file_lock(cache_root() / "locks" / "cargo-target.lock"):
            run_checked(command, cwd=ROOT, env=cargo_environment())
            if not canonical.is_file():
                raise BuildError(f"cargo did not produce {display(canonical)}")
            shutil.copyfile(canonical, raw)
        if wasm_identity(product)["key"] != identity["key"]:
            raise BuildError(
                f"{product.value}: worker inputs changed while building; not publishing. Rerun."
            )
        errors = raw_import_errors(raw)
        if errors:
            raise BuildError(f"{product.value}: " + "; ".join(errors))
        run_checked(
            ["wasm-opt", "--remove-imports", "-O3", str(raw), "-o", str(optimized)],
            cwd=ROOT,
        )
        errors = wasm_policy_errors(optimized)
        if errors:
            raise BuildError(f"{product.value}: " + "; ".join(errors))
        if wasm_identity(product)["key"] != identity["key"]:
            raise BuildError(
                f"{product.value}: worker inputs changed before publication; not publishing."
            )
        staged = WASM_DIST / f".publish-{spec.output}.{os.getpid()}"
        try:
            shutil.copyfile(optimized, staged)
            os.replace(staged, WASM_DIST / spec.output)
        finally:
            staged.unlink(missing_ok=True)
    if wasm_identity(product)["key"] != identity["key"]:
        raise BuildError(
            f"{product.value}: worker inputs changed before sealing; receipt not written."
        )
    seal_wasm(product, identity)
    print(f"published {display(WASM_DIST / spec.output)}", flush=True)


def walk_site_outputs(dest: Path) -> list[str]:
    data = dest / "data"
    if not data.is_dir():
        return []
    files = []
    for path in data.rglob("*"):
        if path.is_file():
            files.append(path.relative_to(dest).as_posix())
    root = dest / "site-root.txt"
    if root.is_file():
        files.append("site-root.txt")
    return sorted(files)


def verify_site_outputs(dest: Path, receipt: Receipt) -> None:
    expected = {entry.path: entry for entry in receipt.outputs}
    current = walk_site_outputs(dest)
    if not current:
        raise Stale(f"missing output tree {display(dest / 'data')}")
    missing = sorted(set(expected) - set(current))
    extra = sorted(set(current) - set(expected))
    if missing:
        raise Stale("missing outputs: " + ", ".join(missing[:8]))
    if extra:
        raise Stale("stale outputs not in receipt: " + ", ".join(extra[:8]))
    for relative in current:
        verify_output(dest / relative, expected[relative])


def site_path(dest: Path, relative: object, label: str) -> Path:
    if not isinstance(relative, str) or not relative:
        raise Stale(f"{label}: missing path")
    path = Path(relative)
    if path.is_absolute():
        raise Stale(f"{label}: absolute path {relative}")
    resolved = (dest / path).resolve()
    try:
        resolved.relative_to(dest.resolve())
    except ValueError as error:
        raise Stale(f"{label}: path escapes {display(dest)}: {relative}") from error
    return resolved


def verify_hashed_file(
    dest: Path, relative: object, expected_hash: object, expected_bytes: object, label: str
) -> Path:
    path = site_path(dest, relative, label)
    if not path.is_file():
        raise Stale(f"{label}: missing {path.relative_to(dest.resolve()).as_posix()}")
    if path.stat().st_size != expected_bytes:
        raise Stale(f"{label}: size mismatch {path.relative_to(dest.resolve()).as_posix()}")
    if sha256_file(path) != expected_hash:
        raise Stale(f"{label}: hash mismatch {path.relative_to(dest.resolve()).as_posix()}")
    return path


def site_json(path: Path, label: str) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise Stale(f"{label}: {error}") from error
    if not isinstance(value, dict):
        raise Stale(f"{label}: expected an object")
    return value


def verify_site_integrity(dest: Path) -> None:
    try:
        _verify_site_integrity(dest)
    except Stale:
        raise
    except (AttributeError, KeyError, OSError, TypeError, ValueError) as error:
        raise Stale(f"manifest tree is malformed: {error}") from error


def _verify_site_integrity(dest: Path) -> None:
    index_path = dest / "data/index.json"
    root_path = dest / "site-root.txt"
    if not index_path.is_file() or not root_path.is_file():
        raise Stale(f"missing {display(dest / 'data/index.json')} or site-root.txt")
    index_bytes = index_path.read_bytes()
    if root_path.read_text(encoding="utf-8").strip() != hashlib.sha256(index_bytes).hexdigest():
        raise Stale("site-root.txt does not hash data/index.json")
    index = site_json(index_path, "data/index.json")
    if index.get("format") != "looklab-site/1":
        raise Stale("data/index.json has an unexpected format")
    for look in index.get("looks", []):
        slug = look.get("slug")
        if not isinstance(slug, str):
            raise Stale("data/index.json look is missing a slug")
        for version in look.get("versions", []):
            version_name = version.get("version")
            label = f"look {slug} {version_name}"
            manifest_path = verify_hashed_file(
                dest,
                version.get("manifest"),
                version.get("manifest_sha256"),
                version.get("manifest_bytes"),
                label,
            )
            manifest = site_json(manifest_path, f"{label} manifest")
            if manifest.get("look") != slug or manifest.get("version") != version_name:
                raise Stale(f"{label}: manifest identity does not match data/index.json")
            if manifest.get("status") != version.get("status"):
                raise Stale(f"{label}: manifest status does not match data/index.json")
            artifacts = {}
            base = manifest_path.parent.relative_to(dest.resolve()).as_posix()
            for artifact in manifest.get("artifacts", []):
                name = artifact.get("name")
                if not isinstance(name, str):
                    raise Stale(f"{label}: manifest artifact has no name")
                if name in artifacts:
                    raise Stale(f"{label}: duplicate artifact {name}")
                verify_hashed_file(
                    dest,
                    f"{base}/{name}",
                    artifact.get("sha256"),
                    artifact.get("bytes"),
                    f"{label} artifact {name}",
                )
                artifacts[name] = artifact
            primary = version.get("artifact")
            if primary not in artifacts:
                raise Stale(f"{label}: primary artifact {primary} is not in the manifest")
            if (
                artifacts[primary].get("sha256") != version.get("sha256")
                or artifacts[primary].get("bytes") != version.get("bytes")
            ):
                raise Stale(f"{label}: primary artifact hash does not match data/index.json")
    for scene in index.get("scenes", []):
        label = f"scene {scene.get('name')}"
        verify_hashed_file(
            dest,
            scene.get("path"),
            scene.get("sha256"),
            scene.get("bytes"),
            label,
        )
        thumbnail = scene.get("thumbnail")
        if isinstance(thumbnail, dict):
            verify_hashed_file(
                dest,
                thumbnail.get("path"),
                thumbnail.get("sha256"),
                thumbnail.get("bytes"),
                f"{label} thumbnail",
            )
    reference = index.get("reference_preview")
    if isinstance(reference, dict):
        verify_hashed_file(
            dest,
            reference.get("path"),
            reference.get("sha256"),
            reference.get("bytes"),
            "reference preview",
        )
    for body in index.get("bodies", []):
        body_id = body.get("id")
        if not isinstance(body_id, str):
            raise Stale("data/index.json body is missing an id")
        body_path = site_path(dest, f"data/bodies/{body_id}.json", f"body {body_id}")
        if not body_path.is_file():
            raise Stale(f"body {body_id}: missing body page")
        if site_json(body_path, f"body {body_id}").get("id") != body_id:
            raise Stale(f"body {body_id}: body page id does not match data/index.json")
    scenes_index_path = dest / "data/scenes/index.json"
    if scenes_index_path.is_file():
        scenes_index = site_json(scenes_index_path, "data/scenes/index.json")
        if scenes_index.get("scenes") != index.get("scenes"):
            raise Stale("data/scenes/index.json does not match data/index.json scenes")
    conformance_path = dest / "data/developers/conformance-v1.json"
    if conformance_path.is_file():
        conformance = site_json(conformance_path, "conformance metadata")
        archive = site_path(dest, conformance.get("archive"), "conformance archive")
        if not archive.is_file() or sha256_file(archive) != conformance.get("sha256"):
            raise Stale("conformance archive does not match its metadata")


def verify_site_data(dest: Path, identity: dict, profile: CargoProfile) -> None:
    receipt_file = site_receipt_path(dest, identity["key"])
    receipt = parse_receipt(receipt_file, Kind.SITE_DATA)
    if receipt.dest != destination_identity(dest):
        raise Stale(f"receipt {display(receipt_file)} is for {receipt.dest}")
    if receipt.key != identity["key"]:
        raise Stale(describe_component_changes(receipt, identity))
    verify_site_outputs(dest, receipt)
    verify_site_integrity(dest)
    if site_data_identity(profile)["key"] != identity["key"]:
        raise Stale("inputs changed while verifying")


def seal_site_data(dest: Path, identity: dict) -> None:
    current = walk_site_outputs(dest)
    if not current:
        raise BuildError(f"refusing to seal empty output tree {display(dest / 'data')}")
    outputs = [
        {
            "path": relative,
            "sha256": sha256_file(dest / relative),
            "bytes": (dest / relative).stat().st_size,
        }
        for relative in current
    ]
    receipt = {**identity, "dest": destination_identity(dest), "outputs": outputs}
    write_json_atomic(site_receipt_path(dest, identity["key"]), receipt)


def write_bytes_atomic(path: Path, data: bytes) -> None:
    temporary = path.with_name(f"{path.name}.tmp.{os.getpid()}")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def exchange_paths(left: Path, right: Path) -> None:
    result = run_capture(["mv", "--exchange", "--no-copy", "-T", str(left), str(right)])
    if result.returncode != 0:
        raise BuildError(
            "atomic directory exchange failed; GNU coreutils mv with --exchange is required: "
            + (result.stderr.strip() or result.stdout.strip())
        )


@contextmanager
def termination_as_exception():
    previous = {}

    def interrupt(signum, frame):
        raise KeyboardInterrupt(f"signal {signum}")

    for handled in (signal.SIGTERM, signal.SIGINT):
        previous[handled] = signal.getsignal(handled)
        signal.signal(handled, interrupt)
    try:
        yield
    finally:
        for handled, handler in previous.items():
            signal.signal(handled, handler)


@contextmanager
def blocked_termination():
    handled = {signal.SIGTERM, signal.SIGINT}
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, handled)
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def publish_site_tree(
    dest: Path, stage: Path, identity: dict, profile: CargoProfile
) -> None:
    staged_data = stage / "data"
    staged_root = stage / "site-root.txt"
    if not staged_data.is_dir() or not (staged_data / "index.json").is_file():
        raise BuildError(f"generators did not write {display(staged_data)}/index.json")
    if not (staged_data / "scenes" / "index.json").is_file():
        raise BuildError(f"generators did not write {display(staged_data)}/scenes/index.json")
    if not staged_root.is_file():
        raise BuildError(f"generators did not write {display(staged_root)}")
    if site_data_identity(profile)["key"] != identity["key"]:
        raise BuildError("site data inputs changed before publication; not publishing.")
    dest.mkdir(parents=True, exist_ok=True)
    destination_data = dest / "data"
    destination_root = dest / "site-root.txt"
    receipt = site_receipt_path(dest, identity["key"])
    old_receipt = receipt.read_bytes() if receipt.is_file() else None
    exchanged = False
    published = False
    root_exchanged = False
    root_published = False
    try:
        with blocked_termination():
            if destination_data.exists():
                exchange_paths(staged_data, destination_data)
                exchanged = True
            else:
                os.replace(staged_data, destination_data)
                published = True
        with blocked_termination():
            if destination_root.exists():
                exchange_paths(staged_root, destination_root)
                root_exchanged = True
            else:
                os.replace(staged_root, destination_root)
                root_published = True
        if site_data_identity(profile)["key"] != identity["key"]:
            raise BuildError("site data inputs changed immediately after swap; rolling back.")
        verify_site_integrity(dest)
        seal_site_data(dest, identity)
    except BaseException:
        with blocked_termination():
            if old_receipt is None:
                receipt.unlink(missing_ok=True)
            else:
                write_bytes_atomic(receipt, old_receipt)
            if exchanged and staged_data.exists() and destination_data.exists():
                exchange_paths(staged_data, destination_data)
            elif published and destination_data.exists():
                os.replace(destination_data, staged_data)
            if root_exchanged and staged_root.exists() and destination_root.exists():
                exchange_paths(staged_root, destination_root)
            elif root_published and destination_root.exists():
                os.replace(destination_root, staged_root)
        raise


def build_site_data(dest: Path, profile: CargoProfile, identity: dict) -> None:
    profile_arguments = (
        ["--release"] if profile is CargoProfile.RELEASE else ["--profile", profile.value]
    )
    dest.mkdir(parents=True, exist_ok=True)
    staging_parent = dest / ".site-data-staging"
    staging_parent.mkdir(parents=True, exist_ok=True)
    for stale in staging_parent.iterdir():
        if stale.is_dir():
            shutil.rmtree(stale, ignore_errors=True)
    with termination_as_exception():
        stage = Path(tempfile.mkdtemp(dir=staging_parent, prefix="run-"))
        try:
            output = stage / "site"
            environment = cargo_environment()
            run_checked(
                [
                    "cargo",
                    "run",
                    "--locked",
                    *profile_arguments,
                    "-p",
                    "looklab-publish",
                    "--example",
                    "site_data",
                    "-j",
                    "6",
                    "--",
                    str(output),
                ],
                cwd=ROOT,
                env=environment,
            )
            run_checked(
                [
                    "cargo",
                    "run",
                    "--locked",
                    *profile_arguments,
                    "-p",
                    "looklab-wasm",
                    "--features",
                    "fixtures",
                    "--example",
                    "publish_match_fixture",
                    "-j",
                    "6",
                    "--",
                    str(output),
                ],
                cwd=ROOT,
                env=environment,
            )
            publish_site_tree(dest, output, identity, profile)
        finally:
            shutil.rmtree(stage, ignore_errors=True)
    print(f"published {display(dest / 'data')}", flush=True)


@contextmanager
def file_lock(path: Path):
    import fcntl

    path.parent.mkdir(parents=True, exist_ok=True)
    handle = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield
    finally:
        fcntl.flock(handle, fcntl.LOCK_UN)
        os.close(handle)


def requested_products(arguments) -> list[WasmProduct]:
    requested = [product for product in WasmProduct if getattr(arguments, product.value)]
    return requested or list(WasmProduct)


def run_wasm(arguments) -> int:
    if arguments.check and arguments.force:
        raise BuildError("--check and --force cannot be combined")
    failures = []
    for product in requested_products(arguments):
        spec = WASM_PRODUCTS[product]
        output = WASM_DIST / spec.output
        if arguments.check:
            try:
                verify_wasm(product, wasm_identity(product))
            except Stale as error:
                failures.append(f"{product.value}: {error}")
            else:
                print(f"ok {display(output)}")
            continue
        if not arguments.force:
            try:
                verify_wasm(product, wasm_identity(product))
            except Stale:
                pass
            else:
                print(f"reuse {display(output)}", flush=True)
                continue
        lock = cache_root() / "locks" / f"wasm-{spec.output}.lock"
        with file_lock(lock):
            identity = wasm_identity(product)
            reason = "forced"
            if not arguments.force:
                try:
                    verify_wasm(product, identity)
                except Stale as error:
                    reason = str(error)
                else:
                    print(f"reuse {display(output)}", flush=True)
                    continue
                if materialize_wasm(product, identity):
                    continue
            print(f"build {display(output)} ({reason})", flush=True)
            build_wasm(product, identity)
    if failures:
        raise BuildError("stale worker products:\n  " + "\n  ".join(failures))
    return 0


def run_site_data(arguments) -> int:
    if arguments.check and arguments.force:
        raise BuildError("--check and --force cannot be combined")
    dest = Path(arguments.dest)
    if not dest.is_absolute():
        dest = ROOT / dest
    dest = dest.resolve()
    profile = CargoProfile.RELEASE if arguments.release else CargoProfile.TEST
    identity = site_data_identity(profile)
    if arguments.check:
        verify_site_data(dest, identity, profile)
        print(f"ok {display(dest)}")
        return 0
    if not arguments.force:
        try:
            verify_site_data(dest, identity, profile)
        except Stale:
            pass
        else:
            print(f"reuse {display(dest / 'data')}", flush=True)
            return 0
    lock = cache_root() / "locks" / f"site-data-{dest_lock_name(dest)}.lock"
    with file_lock(lock):
        identity = site_data_identity(profile)
        reason = "forced"
        if not arguments.force:
            try:
                verify_site_data(dest, identity, profile)
            except Stale as error:
                reason = str(error)
            else:
                print(f"reuse {display(dest / 'data')}", flush=True)
                return 0
        print(f"generate {display(dest)} ({reason})", flush=True)
        build_site_data(dest, profile, identity)
    return 0


def run_wasm_key(arguments) -> int:
    identities = {
        product.value: wasm_identity(product) for product in requested_products(arguments)
    }
    print(json.dumps(identities, indent=2, sort_keys=True))
    return 0


def run_site_data_key(arguments) -> int:
    profile = CargoProfile.RELEASE if arguments.release else CargoProfile.TEST
    print(json.dumps(site_data_identity(profile), indent=2, sort_keys=True))
    return 0


def parse_arguments(argv: list[str]):
    parser = argparse.ArgumentParser(
        prog="build_products.py",
        description="Build-product identities and receipts for the looklab static site.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    wasm = commands.add_parser(
        Command.WASM.value, help="build, check or reuse the browser worker modules"
    )
    for product in WasmProduct:
        wasm.add_argument(f"--{product.value}", action="store_true", help=f"select {product.value}")
    wasm.add_argument("--check", action="store_true", help="verify products without building")
    wasm.add_argument("--force", action="store_true", help="rebuild selected products")
    data = commands.add_parser(
        Command.SITE_DATA.value, help="generate or verify the static site data"
    )
    data.add_argument("--check", action="store_true", help="verify data without generating")
    data.add_argument("--force", action="store_true", help="regenerate even when fresh")
    data.add_argument("--release", action="store_true", help="use the production release profile")
    data.add_argument(
        "dest", nargs="?", default="site", help="output root, relative to the repository"
    )
    wasm_key = commands.add_parser(
        Command.WASM_IDENTITY.value, help="print worker identities without building"
    )
    for product in WasmProduct:
        wasm_key.add_argument(
            f"--{product.value}", action="store_true", help=f"select {product.value}"
        )
    data_key = commands.add_parser(
        Command.SITE_DATA_IDENTITY.value, help="print the site-data identity"
    )
    data_key.add_argument("--release", action="store_true", help="use the production release profile")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    arguments = parse_arguments(argv)
    handlers = {
        Command.WASM: run_wasm,
        Command.SITE_DATA: run_site_data,
        Command.WASM_IDENTITY: run_wasm_key,
        Command.SITE_DATA_IDENTITY: run_site_data_key,
    }
    try:
        return handlers[Command(arguments.command)](arguments)
    except BuildError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except Stale as error:
        print(f"stale: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
