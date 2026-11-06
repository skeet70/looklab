#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Isolated behavioral tests for immutable source generations and the lane runner."""

import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock
from pathlib import Path

SOURCE_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = SOURCE_ROOT / "tools/source_generations.py"
sys.path.insert(0, str(SOURCE_ROOT / "tools"))

import source_generations  # noqa: E402


def write(path: Path, data: str | bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(data, bytes):
        path.write_bytes(data)
    else:
        path.write_text(data, encoding="utf-8")


def reap(process: subprocess.Popen) -> None:
    if process.poll() is None:
        process.kill()
        process.wait()
    for stream in (process.stdout, process.stderr):
        if stream is not None:
            stream.close()


class GenerationFixture(unittest.TestCase):
    def setUp(self) -> None:
        environment = unittest.mock.patch.dict(os.environ)
        environment.start()
        self.addCleanup(environment.stop)
        for name in (
            "LOOKLAB_BUILD_ROOT",
            "LOOKLAB_SOURCE_ORIGIN",
            "LOOKLAB_SOURCE_GENERATION",
            "LOOKLAB_UI_TARGET_DIR",
            "CARGO_TARGET_DIR",
        ):
            os.environ.pop(name, None)
        os.environ["GIT_CONFIG_GLOBAL"] = os.devnull
        os.environ["GIT_CONFIG_NOSYSTEM"] = "1"
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "--initial-branch=main", "--template=")
        self.git("config", "user.name", "source generation test")
        self.git("config", "user.email", "source-generation@example.invalid")
        write(self.repo / ".gitignore", "target/\nfixtures/cache/\nignored.txt\n")
        write(self.repo / "flake.nix", '{ outputs = _: { marker = "captured"; }; }\n')
        write(self.repo / "src/a.rs", "original\n")
        write(self.repo / "src/gone.rs", "depart\n")
        write(self.repo / "fixtures/pattern/pattern.png", b"pattern\n")
        write(self.repo / "fixtures/cache/scene.dng", b"fixture bytes\n")
        write(
            self.repo / "fixtures/manifest.toml",
            '[[fixture]]\nfile = "scene.dng"\ncommitted = false\n'
            'sha256 = "00"\nsize = 1\n',
        )
        self.git("add", "-A")
        self.assertEqual(self.git("remote", "-v").stdout, "")
        self.git("commit", "-q", "-m", "fixture")

    def git(self, *arguments: str, cwd: Path | None = None) -> subprocess.CompletedProcess:
        result = subprocess.run(
            ["git", "-C", str(cwd or self.repo), *arguments],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            self.fail(f"git {' '.join(arguments)} failed: {result.stderr or result.stdout}")
        return result

    def capture(self, destination: Path | None = None) -> source_generations.SourceGeneration:
        return source_generations.capture(
            self.repo, destination or self.repo / "target/source"
        )

    def generation_digest(self, view: Path) -> str:
        metadata = json.loads(
            (view / source_generations.GENERATION_FILE).read_text(encoding="utf-8")
        )
        return metadata["digest"]

    def imported_value(self, view: Path) -> str:
        environment = os.environ.copy()
        for name in ("PYTHONDONTWRITEBYTECODE", "PYTHONPYCACHEPREFIX"):
            environment.pop(name, None)
        result = subprocess.run(
            [
                sys.executable,
                "-c",
                "import sys; sys.path.insert(0, sys.argv[1]); "
                "import module_under_test; print(module_under_test.VALUE)",
                str(view),
            ],
            capture_output=True,
            text=True,
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def run_cli(self, arguments: list[str], **kwargs) -> subprocess.CompletedProcess:
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            capture_output=True,
            text=True,
            timeout=kwargs.pop("timeout", 120),
            **kwargs,
        )


class CaptureTests(GenerationFixture):
    def test_capture_includes_tracked_untracked_and_fetched_fixture_bytes(self) -> None:
        write(self.repo / "src/a.rs", "uncommitted\n")
        write(self.repo / "src/untracked.rs", "untracked\n")
        write(self.repo / "ignored.txt", "ignored\n")
        generation = self.capture()
        view = generation.root

        self.assertEqual((view / "src/a.rs").read_text(), "uncommitted\n")
        self.assertEqual((view / "src/untracked.rs").read_text(), "untracked\n")
        self.assertEqual((view / "fixtures/cache/scene.dng").read_bytes(), b"fixture bytes\n")
        self.assertEqual((view / "fixtures/pattern/pattern.png").read_bytes(), b"pattern\n")
        self.assertFalse((view / "ignored.txt").exists())
        self.assertEqual(len(generation.digest), 64)
        self.assertEqual(generation.origin, self.repo.resolve())
        self.assertNotEqual(
            os.stat(view / "src/a.rs").st_ino, os.stat(self.repo / "src/a.rs").st_ino
        )

    def test_git_repository_environment_overrides_cannot_reach_the_origin(self) -> None:
        index = self.repo / ".git/index"
        index_bytes = index.read_bytes()
        with unittest.mock.patch.dict(
            os.environ,
            {
                "GIT_DIR": str(self.repo / ".git"),
                "GIT_INDEX_FILE": str(index),
            },
        ):
            generation = self.capture()
        self.assertEqual(index.read_bytes(), index_bytes)
        self.assertTrue((generation.root / ".git").is_dir())
        self.assertEqual(
            subprocess.run(
                ["git", "-C", str(generation.root), "rev-parse", "--absolute-git-dir"],
                capture_output=True,
                text=True,
            ).stdout.strip(),
            str(generation.root / ".git"),
        )

    def test_view_is_immutable_until_the_next_capture(self) -> None:
        generation = self.capture()
        write(self.repo / "src/a.rs", "live edit\n")
        write(self.repo / "fixtures/cache/scene.dng", b"live fixture\n")
        self.assertEqual((generation.root / "src/a.rs").read_text(), "original\n")
        self.assertEqual(
            (generation.root / "fixtures/cache/scene.dng").read_bytes(), b"fixture bytes\n"
        )
        second = self.capture()
        self.assertEqual((second.root / "src/a.rs").read_text(), "live edit\n")
        self.assertEqual(second.root, generation.root)
        self.assertNotEqual(second.digest, generation.digest)

    def test_live_edit_during_command_observes_the_captured_generation(self) -> None:
        observed = self.root / "observed.txt"
        environment = os.environ.copy()
        environment["OUT"] = str(observed)
        environment["ORIGIN"] = str(self.repo)
        result = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--target-root",
                str(self.repo / "target"),
                "--",
                "sh",
                "-c",
                'cat src/a.rs > "$OUT"; printf "changed\\n" > "$ORIGIN/src/a.rs"',
            ],
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(observed.read_text(), "original\n")
        self.assertEqual((self.repo / "src/a.rs").read_text(), "changed\n")
        view = self.repo / "target/source"
        first_digest = self.generation_digest(view)

        observed_again = self.root / "observed-again.txt"
        environment["OUT"] = str(observed_again)
        second = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--target-root",
                str(self.repo / "target"),
                "--",
                "sh",
                "-c",
                'cat src/a.rs > "$OUT"',
            ],
            env=environment,
        )
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual(observed_again.read_text(), "changed\n")
        self.assertNotEqual(self.generation_digest(view), first_digest)

    def test_command_mutating_a_captured_input_fails_truthfully(self) -> None:
        result = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--target-root",
                str(self.repo / "target"),
                "--",
                "sh",
                "-c",
                "printf 'rewritten\\n' > src/a.rs; touch generated-output.bin",
            ]
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue((self.repo / "target/source/generated-output.bin").exists())
        self.assertEqual(
            (self.repo / "target/source/src/a.rs").read_text(), "rewritten\n"
        )

    def test_deleted_inputs_removed_while_generated_outputs_remain(self) -> None:
        generation = self.capture()
        view = generation.root
        write(view / "site/generated/output.bin", b"generated\n")
        write(view / "notes/keep.txt", "keep\n")
        write(self.repo / "src/untracked.rs", "temporary\n")
        self.capture()

        (self.repo / "src/gone.rs").unlink()
        (self.repo / "src/untracked.rs").unlink()
        self.capture()

        self.assertFalse((view / "src/gone.rs").exists())
        self.assertFalse((view / "src/untracked.rs").exists())
        self.assertEqual((view / "site/generated/output.bin").read_bytes(), b"generated\n")
        self.assertEqual((view / "notes/keep.txt").read_text(), "keep\n")
        self.assertEqual((view / "src/a.rs").read_text(), "original\n")

    def test_unchanged_bytes_keep_the_view_mtime_and_digest(self) -> None:
        source = self.repo / "src/a.rs"
        first = self.capture()
        view = first.root / "src/a.rs"
        view_mtime = view.stat().st_mtime_ns

        os.utime(source, ns=(time.time_ns() + 9_000_000_000,) * 2)
        second = self.capture()
        self.assertEqual(second.digest, first.digest)
        self.assertEqual(view.stat().st_mtime_ns, view_mtime)
        self.assertEqual(view.read_text(), "original\n")

    def test_changed_same_size_bytes_get_a_fresher_view_mtime(self) -> None:
        source = self.repo / "src/a.rs"
        timestamp = 1_600_000_000_123_456_789
        os.utime(source, ns=(timestamp, timestamp))
        first = self.capture()
        view = first.root / "src/a.rs"
        first_mtime = view.stat().st_mtime_ns
        self.assertNotEqual(first_mtime, timestamp)

        write(source, "modified\n")
        os.utime(source, ns=(timestamp, timestamp))
        second = self.capture()
        self.assertNotEqual(second.digest, first.digest)
        self.assertEqual(view.read_text(), "modified\n")
        self.assertGreater(
            view.stat().st_mtime_ns // 1_000_000_000,
            first_mtime // 1_000_000_000,
        )

    def test_python_import_sees_same_size_source_change_with_restored_mtime(self) -> None:
        source = self.repo / "module_under_test.py"
        first_source = 'VALUE = "one"\n'
        second_source = 'VALUE = "two"\n'
        self.assertEqual(len(first_source), len(second_source))
        timestamp = 1_600_000_000_123_456_789
        write(source, first_source)
        os.utime(source, ns=(timestamp, timestamp))
        first = self.capture()
        view = first.root
        self.assertEqual(self.imported_value(view), "one")
        self.assertTrue(
            list((view / "__pycache__").glob("module_under_test.*.pyc"))
        )
        view_mtime = (view / "module_under_test.py").stat().st_mtime_ns

        write(source, second_source)
        os.utime(source, ns=(timestamp, timestamp))
        second = self.capture()
        self.assertNotEqual(second.digest, first.digest)
        self.assertEqual((view / "module_under_test.py").read_text(), second_source)
        self.assertGreater(
            (view / "module_under_test.py").stat().st_mtime_ns // 1_000_000_000,
            view_mtime // 1_000_000_000,
        )
        self.assertEqual(self.imported_value(view), "two")

    def test_restored_legacy_view_refreshes_a_stale_python_cache(self) -> None:
        source = self.repo / "module_under_test.py"
        first_source = 'VALUE = "one"\n'
        second_source = 'VALUE = "two"\n'
        timestamp = 1_600_000_000_123_456_789
        write(source, first_source)
        os.utime(source, ns=(timestamp, timestamp))
        first = self.capture()
        view = first.root
        view_file = view / "module_under_test.py"
        other = view / "src/a.rs"
        self.assertEqual(self.imported_value(view), "one")
        stale_mtime = view_file.stat().st_mtime_ns
        other_mtime = other.stat().st_mtime_ns

        write(source, second_source)
        os.utime(source, ns=(timestamp, timestamp))
        write(view_file, second_source)
        os.utime(view_file, ns=(stale_mtime, stale_mtime))
        metadata_path = view / source_generations.GENERATION_FILE
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
        metadata["schema"] = source_generations.SCHEMA - 1
        metadata_path.write_text(json.dumps(metadata), encoding="utf-8")

        self.assertEqual(self.imported_value(view), "one")

        second = self.capture()
        self.assertNotEqual(second.digest, first.digest)
        self.assertEqual(view_file.read_text(), second_source)
        self.assertGreater(
            view_file.stat().st_mtime_ns // 1_000_000_000,
            stale_mtime // 1_000_000_000,
        )
        self.assertGreater(
            other.stat().st_mtime_ns // 1_000_000_000,
            other_mtime // 1_000_000_000,
        )
        self.assertEqual(self.imported_value(view), "two")

        settled_module = view_file.stat().st_mtime_ns
        settled_other = other.stat().st_mtime_ns
        third = self.capture()
        self.assertEqual(third.digest, second.digest)
        self.assertEqual(view_file.stat().st_mtime_ns, settled_module)
        self.assertEqual(other.stat().st_mtime_ns, settled_other)

    def test_capture_reuses_one_view_and_removes_staging(self) -> None:
        first = self.capture()
        second = self.capture()
        self.assertEqual(first.root, second.root)
        self.assertFalse(list((self.repo / "target").glob(".source.staging-*")))
        self.assertFalse((first.root / "target").exists())
        self.assertTrue((first.root / ".git").is_dir())

    def test_capture_rejects_a_subdirectory_origin(self) -> None:
        with self.assertRaises(source_generations.SourceGenerationError):
            source_generations.capture(self.repo / "src", self.root / "view")

    def test_capture_rejects_a_captured_view_as_origin(self) -> None:
        generation = self.capture()
        with self.assertRaises(source_generations.SourceGenerationError):
            source_generations.capture(generation.root, self.root / "nested")
        self.assertFalse((self.root / "nested").exists())

    def test_internal_symlink_is_recreated(self) -> None:
        (self.repo / "src/link.rs").symlink_to("a.rs")
        self.git("add", "src/link.rs")
        self.git("commit", "-q", "-m", "link")
        generation = self.capture()
        link = generation.root / "src/link.rs"
        self.assertTrue(link.is_symlink())
        self.assertEqual(os.readlink(link), "a.rs")
        self.assertEqual(link.read_text(), "original\n")

    def test_external_symlinks_are_rejected(self) -> None:
        (self.repo / "src/absolute.rs").symlink_to("/etc/passwd")
        self.git("add", "src/absolute.rs")
        with self.assertRaises(source_generations.SourceGenerationError):
            self.capture()
        self.assertFalse((self.repo / "target/source").exists())

        (self.repo / "src/absolute.rs").unlink()
        self.git("add", "-A")
        (self.repo / "src/escape.rs").symlink_to("../../secret")
        self.git("add", "src/escape.rs")
        with self.assertRaises(source_generations.SourceGenerationError):
            self.capture()
        self.assertFalse((self.repo / "target/source").exists())

    def test_file_and_directory_transitions_remove_departed_inputs_before_apply(self) -> None:
        write(self.repo / "shape", "first\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "shape file")
        first = self.capture()
        self.assertTrue((first.root / "shape").is_file())

        (self.repo / "shape").unlink()
        write(self.repo / "shape/child.rs", "child\n")
        self.git("add", "-A")
        second = self.capture()
        self.assertTrue((second.root / "shape").is_dir())
        self.assertEqual((second.root / "shape/child.rs").read_text(), "child\n")
        write(second.root / "site/generated/keep.bin", b"keep\n")

        (self.repo / "shape/child.rs").unlink()
        (self.repo / "shape").rmdir()
        write(self.repo / "shape", "second\n")
        self.git("add", "-A")
        third = self.capture()
        self.assertTrue((third.root / "shape").is_file())
        self.assertEqual((third.root / "shape").read_text(), "second\n")
        self.assertEqual((third.root / "site/generated/keep.bin").read_bytes(), b"keep\n")

    def test_interrupted_journal_retains_previous_inputs_for_the_next_capture(self) -> None:
        write(self.repo / "aa_departed", "gone\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "departed")
        generation = self.capture()
        view = generation.root

        (view / "aa_departed").unlink()
        write(view / "aa_departed/output.bin", b"generated\n")
        (self.repo / "aa_departed").unlink()
        self.git("add", "-A")
        write(view / "zz_old.txt", "old interrupted input\n")
        write(
            view / ".looklab-generation.applying.json",
            json.dumps(
                {
                    "schema": 1,
                    "digest": generation.digest,
                    "inputs": [*generation.inputs, "zz_old.txt"],
                }
            ),
        )

        with self.assertRaises(source_generations.SourceGenerationError):
            self.capture()
        self.assertTrue((view / ".looklab-generation.applying.json").is_file())

        shutil.rmtree(view / "aa_departed")
        self.capture()
        self.assertFalse((view / "zz_old.txt").exists())
        self.assertFalse((view / ".looklab-generation.applying.json").exists())

    def test_capture_excludes_explicit_lane_root_artifacts(self) -> None:
        custom = self.repo / "build/ui"
        write(custom / "artifact.bin", b"stale\n")
        generation = source_generations.capture(
            self.repo, custom / "source", exclude=(custom,)
        )
        self.assertNotIn("build/ui/artifact.bin", generation.inputs)
        self.assertFalse((generation.root / "build/ui/artifact.bin").exists())

    def test_custom_target_roots_do_not_feed_back_into_generations(self) -> None:
        cargo = self.repo / "build/cargo"
        ui = self.repo / "build/ui"
        write(cargo / "artifact.bin", b"one")
        write(ui / "artifact.bin", b"one")
        write(self.repo / "target-ui/artifact.bin", b"one")
        with unittest.mock.patch.dict(
            os.environ,
            {
                "CARGO_TARGET_DIR": str(cargo),
                "LOOKLAB_UI_TARGET_DIR": str(ui),
            },
        ):
            ui_signature = source_generations.source_signature(self.repo, ui=True)
            all_signature = source_generations.source_signature(self.repo)
            generation = self.capture()
            self.assertNotIn("build/cargo/artifact.bin", generation.inputs)
            self.assertNotIn("build/ui/artifact.bin", generation.inputs)
            self.assertNotIn("target-ui/artifact.bin", generation.inputs)
            self.assertFalse((generation.root / "build").exists())
            self.assertFalse((generation.root / "target-ui").exists())

            write(cargo / "artifact.bin", b"two")
            write(ui / "artifact.bin", b"two")
            write(self.repo / "target-ui/artifact.bin", b"two")
            self.assertEqual(
                source_generations.source_signature(self.repo, ui=True), ui_signature
            )
            self.assertEqual(source_generations.source_signature(self.repo), all_signature)

    def test_private_view_disables_global_hooks_templates_and_filters(self) -> None:
        marker = self.root / "global-config-ran"
        hooks = self.root / "global-hooks"
        hooks.mkdir()
        hook = hooks / "post-index-change"
        hook.write_text(f"#!/bin/sh\ntouch {marker}\n")
        hook.chmod(0o755)
        template = self.root / "global-template"
        (template / "hooks").mkdir(parents=True)
        (template / "hooks/pre-commit").write_text(f"#!/bin/sh\ntouch {marker}\n")
        filter_script = self.root / "filter.sh"
        filter_script.write_text(f"#!/bin/sh\ntouch {marker}\ncat\n")
        filter_script.chmod(0o755)
        global_config = self.root / "gitconfig"
        global_config.write_text(
            "[core]\n"
            f"    hooksPath = {hooks}\n"
            "    fsmonitor = true\n"
            f"    worktree = {self.root / 'global-worktree'}\n"
            "[init]\n"
            f"    templateDir = {template}\n"
            '[filter "trap"]\n'
            f"    clean = {filter_script}\n"
            f"    smudge = {filter_script}\n"
            "    required = true\n"
        )
        write(self.repo / ".gitattributes", "src/a.rs filter=trap\n")
        self.git("add", ".gitattributes")
        self.git("commit", "-q", "-m", "filter attribute")

        with unittest.mock.patch.dict(
            os.environ, {"GIT_CONFIG_GLOBAL": str(global_config)}
        ):
            generation = self.capture()

        self.assertFalse(marker.exists())
        self.assertEqual((generation.root / "src/a.rs").read_text(), "original\n")
        blob = subprocess.run(
            ["git", "-C", str(generation.root), "cat-file", "blob", ":src/a.rs"],
            capture_output=True,
        )
        self.assertEqual(blob.stdout, b"original\n")
        self.assertEqual(
            self.git("config", "core.hooksPath", cwd=generation.root).stdout.strip(),
            "/dev/null",
        )
        self.assertEqual(
            self.git("config", "core.fsmonitor", cwd=generation.root).stdout.strip(),
            "false",
        )
        self.assertFalse((generation.root / ".git/hooks/post-index-change").exists())
        self.assertFalse((generation.root / ".git/hooks/pre-commit").exists())

    def test_child_environment_drops_git_repository_overrides(self) -> None:
        generation = self.capture()
        overrides = {
            "GIT_DIR": "/tmp/looklab-git-dir",
            "GIT_WORK_TREE": "/tmp/looklab-work-tree",
            "GIT_INDEX_FILE": "/tmp/looklab-index",
            "GIT_COMMON_DIR": "/tmp/looklab-common",
            "GIT_OBJECT_DIRECTORY": "/tmp/looklab-objects",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": "/tmp/looklab-alternates",
            "GIT_NAMESPACE": "looklab",
            "GIT_PREFIX": "looklab",
        }
        base = os.environ.copy()
        base.update(overrides)
        environment = generation.child_environment(self.repo / "target", base=base)
        for name in overrides:
            self.assertNotIn(name, environment)
        self.assertEqual(environment["LOOKLAB_SOURCE_GENERATION"], generation.digest)
        self.assertEqual(environment["LOOKLAB_BUILD_ROOT"], str(self.repo.resolve()))
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(self.repo.resolve() / "target"))
        self.assertIn("PATH", environment)

    def test_runner_child_environment_drops_git_repository_overrides(self) -> None:
        output = self.root / "child-env.txt"
        overrides = {
            "GIT_DIR": "/tmp/looklab-git-dir",
            "GIT_WORK_TREE": "/tmp/looklab-work-tree",
            "GIT_INDEX_FILE": "/tmp/looklab-index",
            "GIT_COMMON_DIR": "/tmp/looklab-common",
            "GIT_OBJECT_DIRECTORY": "/tmp/looklab-objects",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES": "/tmp/looklab-alternates",
            "GIT_NAMESPACE": "looklab",
            "GIT_PREFIX": "looklab",
        }
        environment = os.environ.copy()
        environment.update(overrides)
        environment["OUT"] = str(output)
        script = (
            'printf "%s|%s|%s|%s|%s|%s|%s|%s\\n" '
            '"${GIT_DIR-}" "${GIT_WORK_TREE-}" "${GIT_INDEX_FILE-}" '
            '"${GIT_COMMON_DIR-}" "${GIT_OBJECT_DIRECTORY-}" '
            '"${GIT_ALTERNATE_OBJECT_DIRECTORIES-}" "${GIT_NAMESPACE-}" '
            '"${GIT_PREFIX-}" > "$OUT"'
        )
        result = self.run_cli(
            ["run", "--origin", str(self.repo), "--target-root", str(self.repo / "target"),
             "--", "sh", "-c", script],
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_text().strip(), "|||||||")

    def test_origin_git_state_is_not_mutated(self) -> None:
        index = self.repo / ".git/index"
        head = (self.repo / ".git/HEAD").read_bytes()
        index_bytes = index.read_bytes()
        index_mtime = index.stat().st_mtime_ns
        references = self.git("for-each-ref", "--format=%(refname) %(objectname)").stdout

        generation = self.capture()

        self.assertEqual(index.read_bytes(), index_bytes)
        self.assertEqual(index.stat().st_mtime_ns, index_mtime)
        self.assertEqual((self.repo / ".git/HEAD").read_bytes(), head)
        self.assertEqual(
            self.git("for-each-ref", "--format=%(refname) %(objectname)").stdout,
            references,
        )
        self.assertEqual(
            Path(self.git("rev-parse", "--show-toplevel", cwd=generation.root).stdout.strip()),
            generation.root,
        )
        self.assertEqual(
            Path(self.git("rev-parse", "--absolute-git-dir", cwd=generation.root).stdout.strip()),
            generation.root / ".git",
        )
        tracked = self.git("ls-files", cwd=generation.root).stdout.split()
        self.assertIn("src/a.rs", tracked)
        self.assertNotIn("fixtures/cache/scene.dng", tracked)
        self.assertEqual((generation.root / "src/a.rs").read_text(), "original\n")

    def test_view_git_stores_captured_bytes_without_attribute_filters(self) -> None:
        write(self.repo / ".gitattributes", "*.txt text eol=crlf\n")
        write(self.repo / "notes.txt", b"a\r\nb\r\n")
        self.git("add", ".gitattributes", "notes.txt")
        self.git("commit", "-q", "-m", "attributes")
        generation = self.capture()
        self.assertEqual((generation.root / "notes.txt").read_bytes(), b"a\r\nb\r\n")
        blob = subprocess.run(
            ["git", "-C", str(generation.root), "cat-file", "blob", ":notes.txt"],
            capture_output=True,
        )
        self.assertEqual(blob.returncode, 0, blob.stderr)
        self.assertEqual(blob.stdout, b"a\r\nb\r\n")

    def test_git_view_treats_captured_paths_literally(self) -> None:
        write(self.repo / "src/weird[1].txt", "literal\n")
        self.git("add", "src/weird[1].txt")
        self.git("commit", "-q", "-m", "literal")
        generation = self.capture()
        tracked = self.git("ls-files", cwd=generation.root).stdout.split()
        self.assertIn("src/weird[1].txt", tracked)
        self.assertEqual((generation.root / "src/weird[1].txt").read_text(), "literal\n")

    def test_tracked_but_ignored_source_is_staged_in_the_view(self) -> None:
        write(self.repo / ".gitignore", "target/\nfixtures/cache/\nignored.txt\nforced/\n")
        write(self.repo / "forced/kept.rs", "kept\n")
        self.git("add", ".gitignore")
        self.git("add", "-f", "forced/kept.rs")
        self.git("commit", "-q", "-m", "forced")
        generation = self.capture()
        tracked = self.git("ls-files", cwd=generation.root).stdout.split()
        self.assertIn("forced/kept.rs", tracked)
        blob = subprocess.run(
            ["git", "-C", str(generation.root), "cat-file", "blob", ":forced/kept.rs"],
            capture_output=True,
        )
        self.assertEqual(blob.stdout, b"kept\n")

    def test_view_git_and_nix_discover_captured_files_without_origin_writes(self) -> None:
        generation = self.capture()
        tracked = self.git("ls-files", cwd=generation.root).stdout.split()
        self.assertIn("flake.nix", tracked)

        nix = shutil.which("nix")
        if nix is None:
            self.skipTest("nix is not on PATH")
        result = subprocess.run(
            [
                nix,
                "--extra-experimental-features",
                "nix-command flakes",
                "eval",
                "--offline",
                "--raw",
                ".#marker",
            ],
            cwd=generation.root,
            capture_output=True,
            text=True,
            timeout=120,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "captured")

    def test_environment_points_children_at_shared_roots(self) -> None:
        generation = self.capture()
        environment = generation.environment(self.repo / "target")
        self.assertEqual(environment["LOOKLAB_BUILD_ROOT"], str(self.repo.resolve()))
        self.assertEqual(environment["LOOKLAB_SOURCE_ORIGIN"], str(self.repo.resolve()))
        self.assertEqual(environment["LOOKLAB_SOURCE_GENERATION"], generation.digest)
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(self.repo.resolve() / "target"))
        self.assertEqual(
            environment["LOOKLAB_UI_TARGET_DIR"], str(self.repo.resolve() / "target-ui")
        )
        self.assertEqual(environment["GIT_OPTIONAL_LOCKS"], "0")

        ui_environment = generation.environment(self.repo / "target-ui")
        self.assertEqual(
            ui_environment["CARGO_TARGET_DIR"], str(self.repo.resolve() / "target-ui")
        )
        self.assertEqual(
            ui_environment["LOOKLAB_UI_TARGET_DIR"], str(self.repo.resolve() / "target-ui")
        )

    def test_environment_handles_custom_ui_targets_explicitly(self) -> None:
        generation = self.capture()
        custom = (self.repo / "build/ui").resolve()
        native_target = (self.repo / "target").resolve()
        with unittest.mock.patch.dict(
            os.environ, {"LOOKLAB_UI_TARGET_DIR": str(custom)}
        ):
            native = generation.environment(native_target)
            self.assertEqual(native["CARGO_TARGET_DIR"], str(native_target))
            self.assertEqual(native["LOOKLAB_UI_TARGET_DIR"], str(custom))
            inferred = generation.environment(custom)
            self.assertEqual(inferred["CARGO_TARGET_DIR"], str(custom))
            self.assertEqual(inferred["LOOKLAB_UI_TARGET_DIR"], str(custom))

        explicit = generation.environment(custom, ui=True)
        self.assertEqual(explicit["CARGO_TARGET_DIR"], str(custom))
        self.assertEqual(explicit["LOOKLAB_UI_TARGET_DIR"], str(custom))
        default_native = generation.environment(native_target, ui=False)
        self.assertEqual(
            default_native["LOOKLAB_UI_TARGET_DIR"],
            str((self.repo / "target-ui").resolve()),
        )
        with unittest.mock.patch.dict(
            os.environ, {"LOOKLAB_UI_TARGET_DIR": "build/relative-ui"}
        ):
            relative = generation.environment(native_target, ui=False)
            self.assertEqual(
                relative["LOOKLAB_UI_TARGET_DIR"],
                str((self.repo / "build/relative-ui").resolve()),
            )

    def test_linked_worktree_uses_the_main_checkout_shared_roots(self) -> None:
        linked = self.root / "linked"
        self.git("worktree", "add", str(linked), "-b", "linked")
        write(linked / "src/linked.rs", "linked\n")
        generation = source_generations.capture(linked, self.root / "view")
        environment = generation.environment(self.repo / "target")
        self.assertEqual(environment["LOOKLAB_BUILD_ROOT"], str(self.repo.resolve()))
        self.assertEqual(environment["LOOKLAB_SOURCE_ORIGIN"], str(linked.resolve()))
        self.assertEqual(environment["CARGO_TARGET_DIR"], str(self.repo.resolve() / "target"))
        self.assertEqual((generation.root / "src/linked.rs").read_text(), "linked\n")
        with unittest.mock.patch.dict(
            os.environ, {"LOOKLAB_UI_TARGET_DIR": "ui-build"}
        ):
            relative = generation.environment(self.repo / "target", ui=False)
            self.assertEqual(
                relative["LOOKLAB_UI_TARGET_DIR"],
                str(self.repo.resolve() / "ui-build"),
            )

    def test_cli_capture_and_signature(self) -> None:
        destination = self.root / "view"
        captured = self.run_cli(
            ["capture", "--origin", str(self.repo), "--destination", str(destination)]
        )
        self.assertEqual(captured.returncode, 0, captured.stderr)
        self.assertEqual(captured.stdout.strip(), self.generation_digest(destination))
        ui_signature = self.run_cli(
            ["signature", "--origin", str(self.repo), "--ui"]
        )
        self.assertEqual(ui_signature.returncode, 0, ui_signature.stderr)
        self.assertEqual(
            ui_signature.stdout.strip(),
            source_generations.source_signature(self.repo, ui=True),
        )


class CargoReuseTests(GenerationFixture):
    def test_captured_cargo_never_reuses_another_source_view(self) -> None:
        write(
            self.repo / "Cargo.toml",
            '[workspace]\nmembers = ["app"]\nresolver = "2"\n',
        )
        write(
            self.repo / "app/Cargo.toml",
            '[package]\nname = "source-view-probe"\nversion = "0.0.0"\nedition = "2021"\n',
        )
        for custom_wrapper in (False, True):
            with self.subTest(custom_wrapper=custom_wrapper):
                base = os.environ.copy()
                for name in (
                    "RUSTC_WRAPPER",
                    "RUSTC_WORKSPACE_WRAPPER",
                    "LOOKLAB_CAPTURE_RUSTC_WRAPPER",
                    "LOOKLAB_ORIGINAL_RUSTC_WORKSPACE_WRAPPER",
                    "PROBE_WRAPPED",
                ):
                    base.pop(name, None)
                base["CARGO_TARGET_DIR"] = str(self.root / "compiler-output")
                base["CARGO_INCREMENTAL"] = "0"
                suffix = "plain"
                if custom_wrapper:
                    wrapper = self.root / "user-wrapper.sh"
                    write(wrapper, '#!/bin/sh\nexport PROBE_WRAPPED=custom\nexec "$@"\n')
                    wrapper.chmod(0o755)
                    base["RUSTC_WORKSPACE_WRAPPER"] = str(wrapper)
                    suffix = "custom"

                def program(value: str) -> None:
                    write(
                        self.repo / "app/src/main.rs",
                        'fn main() { println!("'
                        + value
                        + '-{}", option_env!("PROBE_WRAPPED").unwrap_or("plain")); }\n',
                    )

                def run(root: Path, environment: dict[str, str]) -> str:
                    result = subprocess.run(
                        ["cargo", "run", "--offline", "--quiet", "-p", "source-view-probe"],
                        cwd=root,
                        env=environment,
                        capture_output=True,
                        text=True,
                        timeout=30,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    return result.stdout.strip()

                program("FIRST")
                first = self.capture(self.repo / "target/first")
                program("SECOND")
                second = self.capture(self.repo / "target/second")
                program("THIRD")
                self.assertEqual(run(self.repo, base), f"THIRD-{suffix}")
                first_environment = first.child_environment(
                    self.root / "compiler-output", base=base
                )
                self.assertEqual(run(first.root, first_environment), f"FIRST-{suffix}")
                second_environment = second.child_environment(
                    self.root / "compiler-output", base=first_environment
                )
                self.assertEqual(run(second.root, second_environment), f"SECOND-{suffix}")
                self.assertEqual(run(self.repo, base), f"THIRD-{suffix}")


class SignatureTests(GenerationFixture):
    def test_ui_signature_excludes_css_docs_tests_benches_and_ignored_outputs(self) -> None:
        write(self.repo / "site/styles.css", "body {}\n")
        write(self.repo / "site/styles/theme.css", "body {}\n")
        write(self.repo / "docs/guide.md", "guide\n")
        write(self.repo / "README.md", "readme\n")
        write(self.repo / "crates/demo/tests/demo.rs", "test\n")
        write(self.repo / "crates/demo/benches/demo.rs", "bench\n")
        write(self.repo / "site/checks/e2e.mjs", "check\n")
        write(self.repo / "crates/demo/src/lib.rs", "pub fn x() {}\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "ui inputs")

        ui = source_generations.source_signature(self.repo, ui=True)
        all_inputs = source_generations.source_signature(self.repo)
        for relative in (
            "site/styles.css",
            "site/styles/theme.css",
            "docs/guide.md",
            "README.md",
            "crates/demo/tests/demo.rs",
            "crates/demo/benches/demo.rs",
            "site/checks/e2e.mjs",
        ):
            write(self.repo / relative, "changed\n")
            self.assertEqual(source_generations.source_signature(self.repo, ui=True), ui)
            changed = source_generations.source_signature(self.repo)
            self.assertNotEqual(changed, all_inputs)
            all_inputs = changed

        write(self.repo / "target/ignored-output.bin", b"ignored\n")
        self.assertEqual(source_generations.source_signature(self.repo, ui=True), ui)

        write(self.repo / "crates/demo/src/lib.rs", "pub fn y() {}\n")
        self.assertNotEqual(source_generations.source_signature(self.repo, ui=True), ui)

        fixture_ui = source_generations.source_signature(self.repo, ui=True)
        write(self.repo / "fixtures/cache/scene.dng", b"downloaded after the build\n")
        self.assertNotEqual(
            source_generations.source_signature(self.repo, ui=True), fixture_ui
        )


class LaneTests(GenerationFixture):
    def test_lane_lock_blocks_until_release(self) -> None:
        target = self.root / "target"
        helper = (
            "import sys, time\n"
            "from pathlib import Path\n"
            f"sys.path.insert(0, {str(SOURCE_ROOT / 'tools')!r})\n"
            "from source_generations import lane_lock\n"
            "with lane_lock(Path(sys.argv[1])):\n"
            "    print('locked', flush=True)\n"
            "    time.sleep(0.5)\n"
            "    print('released', flush=True)\n"
        )
        process = subprocess.Popen(
            [sys.executable, "-c", helper, str(target)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(reap, process)
        self.assertEqual(process.stdout.readline().strip(), "locked")
        started = time.monotonic()
        with source_generations.lane_lock(target):
            elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 0.25)
        self.assertEqual(process.stdout.readline().strip(), "released")
        self.assertEqual(process.wait(timeout=10), 0)
        self.assertTrue((target / "source.lock").is_file())

    def start_lane_holder(self, lane: Path, release: Path) -> subprocess.Popen:
        helper = (
            "import sys, time\n"
            "from pathlib import Path\n"
            f"sys.path.insert(0, {str(SOURCE_ROOT / 'tools')!r})\n"
            "from source_generations import lane_lock\n"
            "with lane_lock(Path(sys.argv[1])):\n"
            "    print('locked', flush=True)\n"
            "    while not Path(sys.argv[2]).exists():\n"
            "        time.sleep(0.02)\n"
        )
        process = subprocess.Popen(
            [sys.executable, "-c", helper, str(lane), str(release)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(reap, process)
        self.assertEqual(process.stdout.readline().strip(), "locked")
        return process

    def test_cli_capture_waits_for_the_active_destination_lane(self) -> None:
        lane = (self.repo / "target").resolve()
        destination = lane / "source"
        release = self.root / "release-lane"
        holder = self.start_lane_holder(lane, release)
        capture_process = subprocess.Popen(
            [
                sys.executable,
                str(SCRIPT),
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(destination),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(reap, capture_process)
        try:
            time.sleep(0.5)
            self.assertIsNone(capture_process.poll())
            self.assertFalse(destination.exists())
            self.assertFalse(list(lane.glob(".source.staging-*")))
        finally:
            release.touch()
        stdout, stderr = capture_process.communicate(timeout=120)
        self.assertEqual(capture_process.returncode, 0, stderr)
        self.assertEqual(stdout.strip(), self.generation_digest(destination))
        self.assertEqual(holder.wait(timeout=20), 0)

    def test_cli_capture_resolves_a_symlinked_destination_to_the_active_lane(self) -> None:
        lane = (self.repo / "target").resolve()
        alias = self.root / "lane-alias"
        alias.symlink_to(lane)
        release = self.root / "release-alias"
        holder = self.start_lane_holder(lane, release)
        capture_process = subprocess.Popen(
            [
                sys.executable,
                str(SCRIPT),
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(alias / "source"),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(reap, capture_process)
        try:
            time.sleep(0.5)
            self.assertIsNone(capture_process.poll())
            self.assertFalse((lane / "source").exists())
        finally:
            release.touch()
        stdout, stderr = capture_process.communicate(timeout=120)
        self.assertEqual(capture_process.returncode, 0, stderr)
        self.assertEqual(
            stdout.strip(), self.generation_digest(lane / "source")
        )
        self.assertEqual(holder.wait(timeout=20), 0)

    def test_cli_capture_rejects_a_target_root_outside_the_destination_lane(self) -> None:
        lane = (self.repo / "target").resolve()
        other = (self.repo / "target-ui").resolve()
        release = self.root / "release-mismatch"
        holder = self.start_lane_holder(lane, release)
        try:
            result = self.run_cli(
                [
                    "capture",
                    "--origin",
                    str(self.repo),
                    "--destination",
                    str(lane / "source"),
                    "--target-root",
                    str(other),
                ],
                timeout=20,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertTrue(result.stderr)
            self.assertFalse((lane / "source").exists())
            self.assertFalse((other / "source").exists())
        finally:
            release.touch()
        self.assertEqual(holder.wait(timeout=20), 0)
        matched = self.run_cli(
            [
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(lane / "source"),
                "--target-root",
                str(lane),
            ]
        )
        self.assertEqual(matched.returncode, 0, matched.stderr)
        self.assertEqual(
            matched.stdout.strip(), self.generation_digest(lane / "source")
        )

    def test_nested_cli_capture_cannot_mutate_an_active_outer_generation(self) -> None:
        view = self.capture().root
        metadata = view / source_generations.GENERATION_FILE
        before = metadata.read_bytes()
        release = self.root / "release-nested"
        holder = self.start_lane_holder(view.parent, release)
        try:
            nested = view / "crates/looklab-color"
            result = self.run_cli(
                [
                    "capture",
                    "--origin",
                    str(self.repo),
                    "--destination",
                    str(nested),
                ],
                timeout=20,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(nested.exists())
            self.assertFalse((view / "crates").exists())
            self.assertEqual(metadata.read_bytes(), before)
            self.assertEqual((view / "src/a.rs").read_text(), "original\n")
        finally:
            release.touch()
        self.assertEqual(holder.wait(timeout=20), 0)

    def test_nested_run_target_root_cannot_mutate_an_active_outer_generation(self) -> None:
        view = self.capture().root
        metadata = view / source_generations.GENERATION_FILE
        before = metadata.read_bytes()
        marker = self.root / "nested-run-ran"
        release = self.root / "release-nested-run"
        holder = self.start_lane_holder(view.parent, release)
        try:
            result = self.run_cli(
                [
                    "run",
                    "--origin",
                    str(self.repo),
                    "--target-root",
                    str(view / "crates/looklab-color"),
                    "--",
                    "sh",
                    "-c",
                    f'touch "{marker}"',
                ],
                timeout=20,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())
            self.assertFalse((view / "crates").exists())
            self.assertEqual(metadata.read_bytes(), before)
            self.assertEqual((view / "src/a.rs").read_text(), "original\n")

            equal = self.run_cli(
                [
                    "run",
                    "--origin",
                    str(self.repo),
                    "--target-root",
                    str(view),
                    "--",
                    "sh",
                    "-c",
                    f'touch "{marker}"',
                ],
                timeout=20,
            )
            self.assertNotEqual(equal.returncode, 0)
            self.assertFalse(marker.exists())
            self.assertFalse((view / "source").exists())
            self.assertEqual(metadata.read_bytes(), before)
        finally:
            release.touch()
        self.assertEqual(holder.wait(timeout=20), 0)

    def test_nested_capture_under_known_views_is_rejected(self) -> None:
        custom = (self.repo / "build/ui").resolve()
        environment = os.environ.copy()
        environment["LOOKLAB_UI_TARGET_DIR"] = str(custom)
        configured = self.run_cli(
            [
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(custom / "source/crates/looklab-color"),
            ],
            env=environment,
            timeout=20,
        )
        self.assertNotEqual(configured.returncode, 0)
        self.assertFalse((custom / "source").exists())
        self.assertFalse((custom / "source/crates").exists())

        main = self.run_cli(
            [
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(self.repo / "target/source/crates/looklab-color"),
            ],
            timeout=20,
        )
        self.assertNotEqual(main.returncode, 0)
        self.assertFalse((self.repo / "target").exists())

    def test_nested_capture_and_run_under_an_unconfigured_lane_are_rejected(self) -> None:
        lane = (self.root / "custom-lane").resolve()
        release = self.root / "release-custom"
        marker = self.root / "custom-lane-ran"
        holder = self.start_lane_holder(lane, release)
        try:
            self.assertTrue((lane / "source.lock").is_file())
            nested_capture = self.run_cli(
                [
                    "capture",
                    "--origin",
                    str(self.repo),
                    "--destination",
                    str(lane / "captured/crates/looklab-color"),
                ],
                timeout=20,
            )
            self.assertNotEqual(nested_capture.returncode, 0)
            self.assertFalse((lane / "captured").exists())

            nested_run = self.run_cli(
                [
                    "run",
                    "--origin",
                    str(self.repo),
                    "--target-root",
                    str(lane / "captured/crates/looklab-color"),
                    "--",
                    "sh",
                    "-c",
                    f'touch "{marker}"',
                ],
                timeout=20,
            )
            self.assertNotEqual(nested_run.returncode, 0)
            self.assertFalse(marker.exists())
            self.assertFalse((lane / "captured").exists())
        finally:
            release.touch()
        self.assertEqual(holder.wait(timeout=20), 0)

        sibling = self.run_cli(
            [
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(lane / "other-view"),
            ]
        )
        self.assertEqual(sibling.returncode, 0, sibling.stderr)
        self.assertTrue(
            (lane / "other-view" / source_generations.GENERATION_FILE).is_file()
        )

    def test_ancestor_capture_destination_cannot_overwrite_a_captured_view(self) -> None:
        view = self.capture().root
        lane = view.parent
        metadata = view / source_generations.GENERATION_FILE
        before = metadata.read_bytes()
        release = self.root / "release-ancestor"
        holder = self.start_lane_holder(lane, release)
        try:
            result = self.run_cli(
                [
                    "capture",
                    "--origin",
                    str(self.repo),
                    "--destination",
                    str(lane),
                ],
                timeout=20,
            )
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse((lane / source_generations.GENERATION_FILE).exists())
            self.assertFalse((lane / "src").exists())
            self.assertFalse((self.repo / "source.lock").exists())
            self.assertEqual(metadata.read_bytes(), before)
        finally:
            release.touch()
        self.assertEqual(holder.wait(timeout=20), 0)

        recaptured = self.run_cli(
            ["capture", "--origin", str(self.repo), "--destination", str(view)]
        )
        self.assertEqual(recaptured.returncode, 0, recaptured.stderr)
        self.assertEqual(recaptured.stdout.strip(), self.generation_digest(view))

    def test_capture_refuses_an_unrelated_nonempty_destination(self) -> None:
        destination = self.root / "occupied"
        write(destination / "keep.bin", b"keep\n")
        result = self.run_cli(
            [
                "capture",
                "--origin",
                str(self.repo),
                "--destination",
                str(destination),
            ]
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((destination / "keep.bin").read_bytes(), b"keep\n")
        self.assertFalse(
            (destination / source_generations.GENERATION_FILE).exists()
        )
        self.assertFalse((destination / "src").exists())

    def test_sigterm_stops_child_and_releases_lane(self) -> None:
        marker = self.root / "started"
        pidfile = self.root / "child.pid"
        environment = os.environ.copy()
        environment["MARKER"] = str(marker)
        environment["PIDFILE"] = str(pidfile)
        process = subprocess.Popen(
            [
                sys.executable,
                str(SCRIPT),
                "run",
                "--origin",
                str(self.repo),
                "--target-root",
                str(self.repo / "target"),
                "--",
                "sh",
                "-c",
                'echo $$ > "$PIDFILE"; touch "$MARKER"; sleep 30',
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
        )
        self.addCleanup(reap, process)
        deadline = time.monotonic() + 20
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue(marker.exists())
        process.terminate()
        returncode = process.wait(timeout=20)
        self.assertNotEqual(returncode, 0)

        child = int(pidfile.read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)
        self.assertFalse(list((self.repo / "target").glob(".source.staging-*")))
        descriptor = os.open(self.repo / "target/source.lock", os.O_RDWR)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            os.close(descriptor)

    def test_ui_lane_uses_the_actual_custom_ui_target_for_children(self) -> None:
        custom = (self.repo / "build/ui").resolve()
        output = self.root / "ui-env.txt"
        environment = os.environ.copy()
        environment["OUT"] = str(output)
        result = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--lane",
                "ui",
                "--target-root",
                str(custom),
                "--",
                "sh",
                "-c",
                'printf "%s\\n%s\\n" "$CARGO_TARGET_DIR" "$LOOKLAB_UI_TARGET_DIR" > "$OUT"',
            ],
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            output.read_text().splitlines(), [str(custom), str(custom)]
        )

    def test_ui_lane_defaults_to_the_configured_ui_target(self) -> None:
        custom = (self.repo / "build/ui").resolve()
        environment_file = self.root / "ui-default-env.txt"
        cwd_file = self.root / "ui-default-cwd.txt"
        environment = os.environ.copy()
        environment["LOOKLAB_UI_TARGET_DIR"] = str(custom)
        environment["OUT"] = str(environment_file)
        environment["CWD_OUT"] = str(cwd_file)
        result = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--lane",
                "ui",
                "--",
                "sh",
                "-c",
                'printf "%s\\n%s\\n%s\\n%s\\n%s\\n" "$CARGO_TARGET_DIR" '
                '"$LOOKLAB_UI_TARGET_DIR" "$LOOKLAB_BUILD_ROOT" '
                '"$LOOKLAB_SOURCE_ORIGIN" "$LOOKLAB_SOURCE_GENERATION" > "$OUT"; '
                'pwd -P > "$CWD_OUT"',
            ],
            env=environment,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        target, ui, build_root, origin, generation = (
            environment_file.read_text().splitlines()
        )
        self.assertEqual(target, str(custom))
        self.assertEqual(ui, str(custom))
        self.assertEqual(build_root, str(self.repo.resolve()))
        self.assertEqual(origin, str(self.repo.resolve()))
        self.assertEqual(generation, self.generation_digest(custom / "source"))
        self.assertEqual(Path(cwd_file.read_text().strip()), custom / "source")
        self.assertTrue(
            (custom / "source" / source_generations.GENERATION_FILE).is_file()
        )
        self.assertTrue((custom / "source.lock").is_file())
        self.assertFalse((self.repo / "target-ui").exists())

    def test_ui_lane_without_a_command_is_rejected(self) -> None:
        result = self.run_cli(
            [
                "run",
                "--origin",
                str(self.repo),
                "--lane",
                "ui",
                "--target-root",
                str(self.repo / "target-ui"),
            ]
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.repo / "target-ui").exists())


if __name__ == "__main__":
    unittest.main()
