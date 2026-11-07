#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Capture immutable Git source views so builds see one fixed byte tree.

A capture copies every tracked and nonignored untracked file, plus fetched
fixtures declared in ``fixtures/manifest.toml``, into a stable view. The view
keeps its own ignored build outputs between captures; only files recorded by
the previous capture are replaced or removed. The returned digest is computed
from the bytes actually written into the view.

``lane_lock`` serializes capture and command execution for a shared target
root. Callers must hold it across both.
"""

from __future__ import annotations

import argparse
import contextlib
import dataclasses
import fcntl
import hashlib
import json
import os
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Mapping
from pathlib import Path

GENERATION_FILE = ".looklab-generation.json"
JOURNAL_FILE = ".looklab-generation.applying.json"
SCHEMA = 2
LANE_DIRECTORIES = {"native": "target", "ui": "target-ui"}
DEFAULT_NATIVE_COMMANDS = (
    ("sh", "site/scripts/wasm-assets.sh", "--prep", "--scene", "--match"),
    ("cargo", "test", "--locked", "--workspace", "--features", "looklab-fit/synth"),
)
UI_IGNORED_PREFIXES = ("docs/", "LICENSES/", "site/styles/", "site/checks/")
UI_IGNORED_FILES = {"site/styles.css"}
UI_IGNORED_COMPONENTS = {"tests", "benches"}

GIT_REPOSITORY_ENVIRONMENT = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
)

_ACTIVE_CHILD: subprocess.Popen | None = None


def _sanitized_environment(source: Mapping[str, str]) -> dict[str, str]:
    environment = dict(source)
    for name in GIT_REPOSITORY_ENVIRONMENT:
        environment.pop(name, None)
    return environment


def _git_environment() -> dict[str, str]:
    return _sanitized_environment(os.environ)


class SourceGenerationError(RuntimeError):
    """A source view could not be captured or resolved."""


@dataclasses.dataclass(frozen=True)
class SourceGeneration:
    """One captured byte tree; immutable until the next capture."""

    root: Path
    origin: Path
    digest: str
    inputs: tuple[str, ...] = dataclasses.field(repr=False, default=())
    hashes: tuple[tuple[str, str], ...] = dataclasses.field(repr=False, default=())

    def environment(
        self,
        target_dir: Path,
        *,
        ui: bool | None = None,
        ui_target_dir: Path | None = None,
    ) -> dict[str, str]:
        """Resolve the lane environment; ui=None infers from the configured shared UI target."""

        target = Path(target_dir).expanduser().resolve()
        build_root = _build_root(self.origin)
        configured_ui = _configured_ui_target(build_root)
        if ui_target_dir is not None:
            ui_target = Path(ui_target_dir).expanduser().resolve()
        elif ui is True:
            ui_target = target
        elif ui is False:
            ui_target = configured_ui
        else:
            ui_target = (
                target
                if target == configured_ui or target.name == "target-ui"
                else configured_ui
            )
        return {
            "LOOKLAB_BUILD_ROOT": str(build_root),
            "LOOKLAB_SOURCE_ORIGIN": str(self.origin),
            "LOOKLAB_SOURCE_GENERATION": self.digest,
            "CARGO_TARGET_DIR": str(target),
            "LOOKLAB_UI_TARGET_DIR": str(ui_target),
            "GIT_OPTIONAL_LOCKS": "0",
        }

    def child_environment(
        self,
        target_dir: Path,
        *,
        base: Mapping[str, str] | None = None,
        ui: bool | None = None,
        ui_target_dir: Path | None = None,
    ) -> dict[str, str]:
        """Return the sanitized environment for commands run inside this generation."""

        environment = _sanitized_environment(os.environ if base is None else base)
        environment.update(
            self.environment(target_dir, ui=ui, ui_target_dir=ui_target_dir)
        )
        original = environment.get("RUSTC_WORKSPACE_WRAPPER", "")
        if original == environment.get("LOOKLAB_CAPTURE_RUSTC_WRAPPER"):
            original = environment.get("LOOKLAB_ORIGINAL_RUSTC_WORKSPACE_WRAPPER", "")
        # Cargo includes the workspace-wrapper path in workspace artifact hashes.
        identity = json.dumps([str(self.root.resolve()), original]).encode()
        target = Path(environment["CARGO_TARGET_DIR"])
        target.mkdir(parents=True, exist_ok=True)
        wrapper = target / f".source-rustc-{hashlib.sha256(identity).hexdigest()}"
        command = f"{shlex.quote(original)} " if original else ""
        content = f'#!/bin/sh\nexec {command}"$@"\n'
        if not wrapper.is_file() or wrapper.read_text(encoding="utf-8") != content:
            temporary = wrapper.with_name(f"{wrapper.name}.tmp.{os.getpid()}")
            try:
                temporary.write_text(content, encoding="utf-8")
                temporary.chmod(0o755)
                os.replace(temporary, wrapper)
            finally:
                temporary.unlink(missing_ok=True)
        environment.update(
            {
                "RUSTC_WORKSPACE_WRAPPER": str(wrapper),
                "LOOKLAB_CAPTURE_RUSTC_WRAPPER": str(wrapper),
                "LOOKLAB_ORIGINAL_RUSTC_WORKSPACE_WRAPPER": original,
            }
        )
        return environment


@contextlib.contextmanager
def lane_lock(target_dir: Path):
    """Block until this target root's capture/build lane is free."""

    directory = Path(target_dir).expanduser().resolve()
    directory.mkdir(parents=True, exist_ok=True)
    lock_path = directory / "source.lock"
    descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_CLOEXEC, 0o644)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield lock_path
    finally:
        fcntl.flock(descriptor, fcntl.LOCK_UN)
        os.close(descriptor)


def capture(
    origin: Path, destination: Path, *, exclude: tuple[Path, ...] = ()
) -> SourceGeneration:
    """Copy origin's current source inputs into destination and return their generation."""

    origin = Path(origin).expanduser().resolve()
    destination = Path(destination).expanduser().resolve()
    extra_exclusions = tuple(Path(path).expanduser().resolve() for path in exclude)
    if not origin.is_dir():
        raise SourceGenerationError(f"source origin is not a directory: {origin}")
    if origin == destination:
        raise SourceGenerationError("source generation destination is the origin")
    if (origin / GENERATION_FILE).is_file():
        raise SourceGenerationError(f"source origin is already a captured view: {origin}")
    _git(origin, "rev-parse", "--git-dir")
    if _origin_from(origin) != origin:
        raise SourceGenerationError(f"source origin is not a worktree root: {origin}")

    _validate_capture_destination(origin, destination, destination.parent)
    _remove_stale_staging(destination)
    inputs = _inventory(origin, destination, extra_exclusions)
    tracked = set(_tracked_paths(origin))
    previous = _previous_inputs(destination)
    refresh = not _install_policy_current(destination)
    staging = _staging_path(destination)
    try:
        digest, captured, hashes = _stage(origin, staging, inputs)
        incoming = set(captured)
        destination.mkdir(parents=True, exist_ok=True)
        # Recovery must retain old and new input paths until removal and publication finish.
        _write_json_atomic(
            destination / JOURNAL_FILE,
            {
                "schema": SCHEMA,
                "digest": digest,
                "inputs": sorted(previous | incoming),
            },
        )
        _remove_departed(destination, previous - incoming)
        _apply(staging, destination, captured, dict(hashes), refresh)
        _write_json_atomic(
            destination / GENERATION_FILE,
            {
                "schema": SCHEMA,
                "digest": digest,
                "origin": str(origin),
                "inputs": list(captured),
            },
        )
        _write_git_view(destination, captured, tracked)
        (destination / JOURNAL_FILE).unlink(missing_ok=True)
    finally:
        shutil.rmtree(staging, ignore_errors=True)

    return SourceGeneration(
        root=destination,
        origin=origin,
        digest=digest,
        inputs=captured,
        hashes=hashes,
    )


def source_signature(origin: Path, *, ui: bool = False) -> str:
    """Return a cheap identity for the latest source edit, used to queue captures."""

    origin = Path(origin).expanduser().resolve()
    paths = sorted(set(_git_paths(origin)) | set(_fixture_paths(origin)))
    excluded = _excluded_prefixes(origin)
    entries: list[tuple] = []
    for relative in paths:
        if _excluded(relative, excluded):
            continue
        if ui and not _ui_relevant(relative):
            continue
        path = origin / relative
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            entries.append((relative, "missing", 0, 0))
            continue
        if stat.S_ISLNK(info.st_mode):
            entries.append(
                (relative, "link", os.readlink(path), info.st_ctime_ns)
            )
        elif stat.S_ISREG(info.st_mode):
            entries.append(
                (
                    relative,
                    "file",
                    stat.S_IMODE(info.st_mode),
                    info.st_size,
                    info.st_mtime_ns,
                    info.st_ctime_ns,
                )
            )
        else:
            entries.append(
                (relative, "other", stat.S_IMODE(info.st_mode), info.st_ctime_ns)
            )
    payload = json.dumps(entries, ensure_ascii=True, separators=(",", ":"))
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


def _ui_relevant(relative: str) -> bool:
    if relative in UI_IGNORED_FILES or relative.startswith(UI_IGNORED_PREFIXES):
        return False
    if relative.endswith(".md"):
        return False
    if any(part in UI_IGNORED_COMPONENTS for part in Path(relative).parts):
        return False
    return True


def _run_git(
    cwd: Path,
    arguments: list[str],
    *,
    input: bytes | None = None,
    env: dict[str, str] | None = None,
    check: bool = True,
) -> subprocess.CompletedProcess:
    command = ["git", "-C", str(cwd), *arguments]
    result = subprocess.run(command, input=input, capture_output=True, env=env)
    if check and result.returncode != 0:
        message = os.fsdecode(result.stderr.strip()) or os.fsdecode(result.stdout.strip())
        raise SourceGenerationError(f"{shlex.join(command)} failed: {message}")
    return result


def _git(cwd: Path, *arguments: str) -> subprocess.CompletedProcess:
    environment = _git_environment()
    environment["GIT_OPTIONAL_LOCKS"] = "0"
    return _run_git(cwd, list(arguments), env=environment)


def _git_paths(origin: Path) -> list[str]:
    result = _git(origin, "ls-files", "-z", "--cached", "--others", "--exclude-standard")
    return [os.fsdecode(entry) for entry in result.stdout.split(b"\0") if entry]


def _tracked_paths(origin: Path) -> list[str]:
    result = _git(origin, "ls-files", "-z", "--cached")
    return [os.fsdecode(entry) for entry in result.stdout.split(b"\0") if entry]


def _path_candidates(value: str, origin: Path, build_root: Path) -> tuple[Path, ...]:
    raw = Path(value).expanduser()
    if raw.is_absolute():
        return (raw.resolve(),)
    roots = [build_root, origin, Path.cwd()]
    return tuple(dict.fromkeys((root / raw).resolve() for root in roots))


def _configured_ui_target(build_root: Path) -> Path:
    value = os.environ.get("LOOKLAB_UI_TARGET_DIR")
    if value:
        raw = Path(value).expanduser()
        if raw.is_absolute():
            return raw.resolve()
        return (build_root / raw).resolve()
    return build_root / "target-ui"


def _lane_roots(origin: Path) -> tuple[Path, ...]:
    build_root = _build_root(origin)
    roots = [
        origin / "target",
        origin / "target-ui",
        build_root / "target",
        build_root / "target-ui",
    ]
    configured_build_root = os.environ.get("LOOKLAB_BUILD_ROOT")
    if configured_build_root:
        for root in _path_candidates(configured_build_root, origin, build_root):
            roots.extend((root / "target", root / "target-ui"))
    for name in ("CARGO_TARGET_DIR", "LOOKLAB_UI_TARGET_DIR"):
        value = os.environ.get(name)
        if value:
            roots.extend(_path_candidates(value, origin, build_root))
    resolved = []
    for root in roots:
        try:
            resolved.append(root.resolve())
        except OSError:
            continue
    return tuple(dict.fromkeys(resolved))


def _protected_views(origin: Path) -> tuple[Path, ...]:
    return tuple(root / "source" for root in _lane_roots(origin))


def _excluded_prefixes(origin: Path, extra: tuple[Path, ...] = ()) -> tuple[str, ...]:
    prefixes = set()
    for root in (*_lane_roots(origin), *extra):
        try:
            relative = root.resolve().relative_to(origin)
        except (OSError, ValueError):
            continue
        if relative.parts:
            prefixes.add(relative.as_posix())
    return tuple(sorted(prefixes))


def _excluded(relative: str, prefixes: tuple[str, ...]) -> bool:
    return any(
        relative == prefix or relative.startswith(prefix + "/") for prefix in prefixes
    )


def _build_root(origin: Path) -> Path:
    result = _git(origin, "rev-parse", "--git-common-dir")
    common = Path(os.fsdecode(result.stdout.strip()))
    if not common.is_absolute():
        common = (origin / common).resolve()
    if common.name != ".git":
        raise SourceGenerationError(f"unexpected git common directory: {common}")
    return common.parent


def _fixture_paths(origin: Path) -> list[str]:
    manifest = origin / "fixtures/manifest.toml"
    if not manifest.is_file():
        return []
    import tomllib

    try:
        data = tomllib.loads(manifest.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise SourceGenerationError(f"{manifest}: {error}") from error
    paths = []
    for fixture in data.get("fixture", []):
        if fixture.get("committed") is not False:
            continue
        name = fixture.get("file")
        if not isinstance(name, str) or Path(name).is_absolute() or ".." in Path(name).parts:
            raise SourceGenerationError(f"{manifest}: unsafe fixture file {name!r}")
        relative = (Path("fixtures/cache") / name).as_posix()
        if os.path.lexists(origin / relative):
            paths.append(relative)
    return paths


def _inventory(
    origin: Path, destination: Path, extra: tuple[Path, ...] = ()
) -> dict[str, Path]:
    excluded = _excluded_prefixes(origin, (destination, *extra))
    inventory: dict[str, Path] = {}
    for relative in sorted(set(_git_paths(origin)) | set(_fixture_paths(origin))):
        if _excluded(relative, excluded):
            continue
        if Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise SourceGenerationError(f"unsafe source path: {relative!r}")
        source = origin / relative
        try:
            info = os.lstat(source)
        except FileNotFoundError:
            continue
        if stat.S_ISDIR(info.st_mode):
            raise SourceGenerationError(f"{relative}: directories and submodules are unsupported")
        inventory[relative] = source
    for relative, source in inventory.items():
        if stat.S_ISLNK(os.lstat(source).st_mode):
            target = os.readlink(source)
            _check_internal_link(relative, target, inventory)
    return inventory


def _check_internal_link(relative: str, target: str, inventory: dict[str, Path]) -> None:
    if os.path.isabs(target):
        raise SourceGenerationError(
            f"{relative}: absolute symlink {target!r} would point outside the captured view"
        )
    resolved = os.path.normpath(os.path.join(os.path.dirname(relative), target))
    if resolved == ".." or resolved.startswith("../") or os.path.isabs(resolved):
        raise SourceGenerationError(
            f"{relative}: symlink {target!r} escapes the captured view"
        )
    if resolved not in inventory:
        raise SourceGenerationError(
            f"{relative}: symlink target {resolved!r} is not a captured source file"
        )


def _remove_stale_staging(destination: Path) -> None:
    parent = destination.parent
    if not parent.is_dir():
        return
    for stale in parent.glob(f".{destination.name}.staging-*"):
        if stale.is_dir() and not stale.is_symlink():
            shutil.rmtree(stale, ignore_errors=True)
    if destination.is_dir():
        for stale in destination.glob(".looklab-generation*.tmp.*"):
            if stale.is_file() and not stale.is_symlink():
                stale.unlink(missing_ok=True)


def _staging_path(destination: Path) -> Path:
    parent = destination.parent
    parent.mkdir(parents=True, exist_ok=True)
    staging = parent / f".{destination.name}.staging-{os.getpid()}-{time.time_ns()}"
    staging.mkdir()
    return staging


def _stage(
    origin: Path, staging: Path, inputs: dict[str, Path]
) -> tuple[str, tuple[str, ...], tuple[tuple[str, str], ...]]:
    relative_paths = tuple(sorted(inputs))
    _copy(origin, staging, relative_paths)
    captured = _captured_links(staging, relative_paths)
    records = _fingerprints(staging, captured)
    return _digest_of(records), captured, tuple(sorted(records.items()))


def _copy(origin: Path, staging: Path, relative_paths: tuple[str, ...]) -> None:
    cp = shutil.which("cp")
    if cp:
        for start in range(0, len(relative_paths), 128):
            chunk = relative_paths[start : start + 128]
            result = subprocess.run(
                [
                    cp,
                    "--reflink=auto",
                    "--no-dereference",
                    "--preserve=mode,timestamps",
                    "--parents",
                    "--",
                    *chunk,
                    str(staging),
                ],
                cwd=origin,
                capture_output=True,
            )
            if result.returncode == 0:
                continue
            for relative in chunk:
                if not os.path.lexists(staging / relative):
                    _copy_one(origin / relative, staging / relative)
        return
    for relative in relative_paths:
        _copy_one(origin / relative, staging / relative)


def _copy_one(source: Path, destination: Path) -> None:
    try:
        info = os.lstat(source)
    except FileNotFoundError:
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    if stat.S_ISLNK(info.st_mode):
        os.symlink(os.readlink(source), destination)
        return
    try:
        with source.open("rb") as source_handle, destination.open("wb") as target_handle:
            fcntl.ioctl(target_handle.fileno(), 0x40049409, source_handle.fileno())
    except OSError:
        if destination.exists():
            destination.unlink()
        shutil.copy2(source, destination)
    os.chmod(destination, stat.S_IMODE(info.st_mode))
    os.utime(destination, ns=(info.st_atime_ns, info.st_mtime_ns))


def _captured_links(staging: Path, relative_paths: tuple[str, ...]) -> tuple[str, ...]:
    available = set()
    for relative in relative_paths:
        path = staging / relative
        if not os.path.lexists(path):
            continue
        if os.path.islink(path):
            target = os.readlink(path)
            resolved = os.path.normpath(os.path.join(os.path.dirname(relative), target))
            if os.path.isabs(target) or resolved.startswith(".."):
                raise SourceGenerationError(
                    f"{relative}: symlink changed to {target!r} during capture"
                )
        available.add(relative)
    for relative in available:
        path = staging / relative
        if os.path.islink(path):
            target = os.readlink(path)
            resolved = os.path.normpath(os.path.join(os.path.dirname(relative), target))
            if resolved not in available or not os.path.lexists(staging / resolved):
                raise SourceGenerationError(
                    f"{relative}: symlink target {target!r} is absent from the captured view"
                )
    return tuple(sorted(available))


def _fingerprint(relative: str, path: Path, info: os.stat_result) -> str:
    if stat.S_ISLNK(info.st_mode):
        kind = "link"
        content = hashlib.sha256(
            b"link\0" + os.fsencode(os.readlink(path))
        ).hexdigest()
        mode = 0
    elif stat.S_ISREG(info.st_mode):
        kind = "file"
        content = _file_sha256(path)
        mode = stat.S_IMODE(info.st_mode)
    else:
        raise SourceGenerationError(f"{relative}: unsupported file type in source view")
    return f"{kind}\0{mode:o}\0{content}"


def _fingerprints(
    base: Path, relative_paths: tuple[str, ...], *, missing_ok: bool = False
) -> dict[str, str]:
    records: dict[str, str] = {}
    for relative in relative_paths:
        path = base / relative
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            if missing_ok:
                records[relative] = "missing"
                continue
            raise
        records[relative] = _fingerprint(relative, path, info)
    return records


def _digest_of(records: dict[str, str]) -> str:
    digest = hashlib.sha256()
    for relative in sorted(records):
        digest.update(os.fsencode(relative))
        digest.update(b"\0")
        digest.update(records[relative].encode("ascii"))
        digest.update(b"\n")
    return digest.hexdigest()


def _verify_generation(generation: SourceGeneration) -> None:
    expected = dict(generation.hashes)
    if not expected:
        return
    actual = _fingerprints(generation.root, generation.inputs, missing_ok=True)
    changed = [
        relative
        for relative in generation.inputs
        if actual.get(relative) != expected.get(relative)
    ]
    if changed:
        listed = ", ".join(changed[:5])
        if len(changed) > 5:
            listed += f", and {len(changed) - 5} more"
        raise SourceGenerationError(f"captured input changed during command: {listed}")


def _file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _fresh_mtime_ns(previous_ns: int | None, now_ns: int) -> int:
    """Return an mtime whose whole-second value exceeds the previous view mtime."""

    if previous_ns is None:
        return now_ns
    return max(now_ns, (previous_ns // 1_000_000_000 + 1) * 1_000_000_000)


def _apply(
    staging: Path,
    destination: Path,
    relative_paths: tuple[str, ...],
    records: dict[str, str],
    refresh: bool = False,
) -> None:
    now_ns = time.time_ns()
    for relative in relative_paths:
        source = staging / relative
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        try:
            info = os.lstat(target)
        except FileNotFoundError:
            info = None
        if info is not None:
            if stat.S_ISDIR(info.st_mode):
                raise SourceGenerationError(
                    f"{relative}: source file replaced a directory in the view"
                )
            if _fingerprint(relative, target, info) == records[relative]:
                if not (refresh and stat.S_ISREG(info.st_mode)):
                    continue
        staged = os.lstat(source)
        os.replace(source, target)
        if stat.S_ISREG(staged.st_mode):
            previous_ns = None if info is None else info.st_mtime_ns
            os.utime(
                target,
                ns=(staged.st_atime_ns, _fresh_mtime_ns(previous_ns, now_ns)),
            )


def _is_view(path: Path) -> bool:
    return (path / GENERATION_FILE).is_file() or (path / JOURNAL_FILE).is_file()


def _validate_capture_destination(
    origin: Path, destination: Path, lane_root: Path
) -> None:
    origin = Path(origin).expanduser().resolve()
    lane = Path(lane_root).expanduser().resolve()
    if destination.parent != lane:
        raise SourceGenerationError(
            f"destination {destination} is outside lane {lane}"
        )
    for view in _protected_views(origin):
        if destination == view:
            continue
        if view in destination.parents:
            raise SourceGenerationError(
                f"{destination}: destination is inside captured view {view}"
            )
        if destination in view.parents:
            raise SourceGenerationError(
                f"{destination}: destination contains captured view {view}"
            )
    for ancestor in destination.parents:
        if _is_view(ancestor) or (ancestor.parent / "source.lock").is_file():
            raise SourceGenerationError(
                f"{destination}: destination is inside captured view {ancestor}"
            )
    if not _is_view(destination):
        contained = destination / "source"
        if _is_view(contained):
            raise SourceGenerationError(
                f"{destination}: destination contains captured view {contained}"
            )
    if not destination.exists():
        return
    if not destination.is_dir():
        raise SourceGenerationError(f"{destination}: destination is not a directory")
    if _is_view(destination):
        return
    if any(destination.iterdir()):
        raise SourceGenerationError(
            f"{destination}: refusing to capture into a nonempty directory"
        )


def _install_policy_current(destination: Path) -> bool:
    path = destination / GENERATION_FILE
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    return data.get("schema") == SCHEMA


def _previous_inputs(destination: Path) -> set[str]:
    paths: set[str] = set()
    for name in (JOURNAL_FILE, GENERATION_FILE):
        path = destination / name
        if not path.is_file():
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            continue
        inputs = data.get("inputs")
        if not isinstance(inputs, list):
            continue
        for relative in inputs:
            if not isinstance(relative, str):
                continue
            candidate = Path(relative)
            if candidate.is_absolute() or ".." in candidate.parts:
                continue
            paths.add(relative)
    return paths


def _remove_departed(destination: Path, departed: set[str]) -> None:
    parents: set[Path] = set()
    for relative in sorted(departed):
        path = destination / relative
        if path.is_symlink() or path.is_file():
            path.unlink()
            parents.add(path.parent)
        elif path.is_dir():
            raise SourceGenerationError(f"{relative}: refusing to remove a view directory")
    for parent in sorted(parents, key=lambda path: len(path.parts), reverse=True):
        while parent != destination:
            try:
                parent.rmdir()
            except OSError:
                break
            parent = parent.parent


def _write_json_atomic(path: Path, value: object) -> None:
    temporary = path.with_name(f"{path.name}.tmp.{os.getpid()}")
    temporary.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    os.replace(temporary, path)


def _write_git_view(
    destination: Path,
    relative_paths: tuple[str, ...],
    tracked: set[str],
) -> None:
    git_dir = destination / ".git"
    if git_dir.is_symlink() or git_dir.is_file():
        git_dir.unlink()
    elif git_dir.is_dir():
        shutil.rmtree(git_dir)
    environment = _git_environment()
    template = destination.parent / f".source-git-template-{os.getpid()}-{time.time_ns()}"
    template.mkdir()
    try:
        _run_git(
            destination,
            [
                "-c",
                f"core.worktree={destination}",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.attributesFile=/dev/null",
                "init",
                "-q",
                "--initial-branch=main",
                f"--template={template}",
            ],
            env=environment,
        )
    finally:
        shutil.rmtree(template, ignore_errors=True)
    for name, value in (
        ("core.hooksPath", "/dev/null"),
        ("core.fsmonitor", "false"),
        ("core.worktree", str(destination)),
        ("core.attributesFile", "/dev/null"),
        ("filter.lfs.clean", "cat"),
        ("filter.lfs.smudge", "cat"),
        ("filter.lfs.process", ""),
        ("filter.lfs.required", "false"),
    ):
        _run_git(destination, ["config", name, value], env=environment)
    exclude = git_dir / "info" / "exclude"
    exclude.parent.mkdir(parents=True, exist_ok=True)
    exclude.write_text(
        f"{GENERATION_FILE}\n{JOURNAL_FILE}\n", encoding="utf-8"
    )
    (git_dir / "info" / "attributes").write_text(
        "* -filter -text -ident -working-tree-encoding\n", encoding="utf-8"
    )
    ignored = _ignored_paths(destination, relative_paths) - tracked
    staged = [relative for relative in relative_paths if relative not in ignored]
    if not staged:
        return
    with tempfile.NamedTemporaryFile(
        "wb", dir=destination.parent, prefix=".source-pathspec-", delete=False
    ) as handle:
        handle.write(b"\0".join(os.fsencode(relative) for relative in staged))
        pathspec = Path(handle.name)
    try:
        _run_git(
            destination,
            [
                "--literal-pathspecs",
                "add",
                "-f",
                "--pathspec-from-file",
                str(pathspec),
                "--pathspec-file-nul",
            ],
            env=environment,
        )
    finally:
        pathspec.unlink(missing_ok=True)


def _ignored_paths(destination: Path, relative_paths: tuple[str, ...]) -> set[str]:
    if not relative_paths:
        return set()
    environment = _git_environment()
    environment["GIT_OPTIONAL_LOCKS"] = "0"
    result = _run_git(
        destination,
        ["check-ignore", "--stdin", "-z"],
        input=b"\0".join(os.fsencode(relative) for relative in relative_paths),
        env=environment,
        check=False,
    )
    if result.returncode == 1:
        return set()
    if result.returncode != 0:
        message = os.fsdecode(result.stderr.strip())
        raise SourceGenerationError(f"git check-ignore failed: {message}")
    return {os.fsdecode(entry) for entry in result.stdout.split(b"\0") if entry}


def _origin_from(path: Path) -> Path:
    result = _git(path.expanduser().resolve(), "rev-parse", "--show-toplevel")
    return Path(os.fsdecode(result.stdout.strip())).resolve()


def _default_origin() -> Path:
    try:
        return _origin_from(Path.cwd())
    except SourceGenerationError:
        return _origin_from(Path(__file__).resolve().parent.parent)


def _run_command(command: tuple[str, ...], *, cwd: Path, environment: dict[str, str]) -> int:
    global _ACTIVE_CHILD
    print(f"+ {shlex.join(command)}", file=sys.stderr, flush=True)
    child = subprocess.Popen(command, cwd=cwd, env=environment, start_new_session=True)
    _ACTIVE_CHILD = child
    try:
        return child.wait()
    finally:
        if child.poll() is None:
            _signal_group(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                _signal_group(child.pid, signal.SIGKILL)
                child.wait()
        _ACTIVE_CHILD = None


def _signal_group(pid: int, signum: int) -> None:
    try:
        os.killpg(pid, signum)
    except (ProcessLookupError, PermissionError):
        pass


def _install_signal_handlers() -> None:
    def halt(signum: int, _frame) -> None:
        child = _ACTIVE_CHILD
        if child is not None and child.poll() is None:
            _signal_group(child.pid, signal.SIGTERM)
        raise SystemExit(128 + signum)

    for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        try:
            signal.signal(signum, halt)
        except (OSError, ValueError):
            pass


def _resolve_relative(path: Path) -> Path:
    return Path(path).expanduser().resolve()


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="source_generations.py",
        description="Capture immutable source generations and run commands against them.",
    )
    commands = parser.add_subparsers(dest="command", required=True)

    capture_parser = commands.add_parser("capture", help="capture one source generation")
    capture_parser.add_argument("--origin", type=Path)
    capture_parser.add_argument("--destination", type=Path, required=True)
    capture_parser.add_argument("--target-root", type=Path)

    signature_parser = commands.add_parser("signature", help="print a source edit signature")
    signature_parser.add_argument("--origin", type=Path)
    signature_parser.add_argument("--ui", action="store_true")

    run_parser = commands.add_parser(
        "run", help="capture the newest source, then run a command inside that view"
    )
    run_parser.add_argument("--origin", type=Path)
    run_parser.add_argument("--lane", choices=sorted(LANE_DIRECTORIES), default="native")
    run_parser.add_argument("--target-root", type=Path)
    run_parser.add_argument("--destination", type=Path)
    run_parser.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def main(argv: list[str] | None = None) -> int:
    _install_signal_handlers()
    arguments = _parser().parse_args(argv)
    try:
        if arguments.command == "capture":
            origin = (arguments.origin or _default_origin()).resolve()
            destination = _resolve_relative(arguments.destination)
            target_root = (
                _resolve_relative(arguments.target_root)
                if arguments.target_root
                else destination.parent
            )
            _validate_capture_destination(origin, destination, target_root)
            with lane_lock(target_root):
                generation = capture(origin, destination)
            print(generation.digest)
            return 0
        if arguments.command == "signature":
            origin = arguments.origin or _default_origin()
            print(source_signature(origin, ui=arguments.ui))
            return 0
        origin = (arguments.origin or _default_origin()).resolve()
        build_root = _build_root(origin)
        target_root = _resolve_relative(
            arguments.target_root
            if arguments.target_root
            else (
                _configured_ui_target(build_root)
                if arguments.lane == "ui"
                else build_root / LANE_DIRECTORIES[arguments.lane]
            )
        )
        destination = _resolve_relative(
            arguments.destination
            if arguments.destination
            else target_root / "source"
        )
        _validate_capture_destination(origin, destination, target_root)
        command = list(arguments.command)
        if command and command[0] == "--":
            command = command[1:]
        commands = (tuple(command),) if command else ()
        if not commands:
            if arguments.lane != "native":
                raise SourceGenerationError(
                    f"lane {arguments.lane!r} requires an explicit command after --"
                )
            commands = DEFAULT_NATIVE_COMMANDS
        with lane_lock(target_root):
            generation = capture(origin, destination, exclude=(target_root,))
            print(
                f"source generation {generation.digest} ({len(generation.inputs)} files) "
                f"at {generation.root}",
                file=sys.stderr,
                flush=True,
            )
            environment = generation.child_environment(
                target_root, ui=arguments.lane == "ui"
            )
            for entry in commands:
                result = _run_command(entry, cwd=generation.root, environment=environment)
                _verify_generation(generation)
                if result != 0:
                    return 128 - result if result < 0 else result
        return 0
    except SourceGenerationError as error:
        print(f"source generation: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
