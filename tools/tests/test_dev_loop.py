#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Cutover and rollback behaviour of the dev UI supervisor over isolated temporary roots."""

import json
import os
import signal
import socket
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import dev_loop  # noqa: E402


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def listening_stub(path: Path, port: int) -> None:
    write(
        path,
        f"""#!/usr/bin/env python3
import socket, time
sock = socket.socket()
sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
sock.bind(("127.0.0.1", {port}))
sock.listen(16)
time.sleep(120)
""",
    )
    path.chmod(0o755)


def failing_stub(path: Path) -> None:
    write(path, "#!/bin/sh\nexit 1\n")
    path.chmod(0o755)


def make_layout(
    root: Path,
    *,
    public_port: int = dev_loop.DEFAULT_PUBLIC_PORT,
    stub_port: int = dev_loop.DEFAULT_STUB_PORT,
) -> dev_loop.Layout:
    target = root / "target-ui"
    runtime = target / "runtime"
    return dev_loop.Layout(
        origin=root,
        target=target,
        view=target / "source",
        dist=target / "dist",
        next_dist=target / "next-dist",
        stub_state=target / "stub-state",
        runtime=runtime,
        stub_binary=target / "debug" / "examples" / "stub_server",
        stub_bin=target / "stub-bin",
        record=target / "dev-generation.json",
        runtime_record=runtime / "generation.json",
        css_script=target / "source" / "site" / "scripts" / "dev-css.mjs",
        css_watch_script=root / "site" / "scripts" / "dev-css.mjs",
        public_port=public_port,
        stub_port=stub_port,
    )


class PortEnvironmentTests(unittest.TestCase):
    def layout_for(self, values: dict[str, str]) -> dev_loop.Layout:
        with tempfile.TemporaryDirectory() as temporary:
            environment = {"LOOKLAB_UI_TARGET_DIR": temporary, **values}
            with mock.patch.dict(os.environ, environment, clear=False):
                return dev_loop.Layout.from_environment()

    def test_ports_come_from_the_environment(self) -> None:
        layout = self.layout_for(
            {"LOOKLAB_DEV_PORT": "8099", "LOOKLAB_DEV_API_PORT": "8100"}
        )
        self.assertEqual(layout.public_port, 8099)
        self.assertEqual(layout.stub_port, 8100)

    def test_defaults_are_used_without_overrides(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=False):
            os.environ.pop("LOOKLAB_DEV_PORT", None)
            os.environ.pop("LOOKLAB_DEV_API_PORT", None)
            layout = self.layout_for({})
        self.assertEqual(layout.public_port, dev_loop.DEFAULT_PUBLIC_PORT)
        self.assertEqual(layout.stub_port, dev_loop.DEFAULT_STUB_PORT)

    def test_equal_ports_are_rejected(self) -> None:
        with self.assertRaises(ValueError):
            self.layout_for({"LOOKLAB_DEV_PORT": "8099", "LOOKLAB_DEV_API_PORT": "8099"})

    def test_invalid_port_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            self.layout_for({"LOOKLAB_DEV_PORT": "http"})


class ArgumentsTests(unittest.TestCase):
    def test_reuse_data_is_opt_in(self) -> None:
        self.assertFalse(dev_loop.parse_arguments([]).reuse_data)
        self.assertTrue(dev_loop.parse_arguments(["--reuse-data"]).reuse_data)


class ExchangeTests(unittest.TestCase):
    def test_exchange_leaves_previous_dist_at_staging_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            next_dist = root / "next"
            dist = root / "dist"
            write(next_dist / "index.html", "new")
            write(dist / "index.html", "old")

            dev_loop.exchange_dist(next_dist, dist)

            self.assertEqual((dist / "index.html").read_text(), "new")
            self.assertEqual((next_dist / "index.html").read_text(), "old")

    def test_exchange_without_previous_dist_moves_staging_into_place(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            next_dist = root / "next"
            dist = root / "dist"
            write(next_dist / "index.html", "new")

            dev_loop.exchange_dist(next_dist, dist)

            self.assertEqual((dist / "index.html").read_text(), "new")
            self.assertFalse(next_dist.exists())


class PublishTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.port = free_port()
        self.layout = make_layout(self.root, stub_port=self.port)
        for directory in (
            self.layout.dist,
            self.layout.runtime / "data",
            self.layout.stub_bin,
        ):
            directory.mkdir(parents=True, exist_ok=True)
        for name, value in (("GRACE_SECONDS", 2.0), ("READY_SECONDS", 5.0)):
            patcher = mock.patch.object(dev_loop, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        self.env_patch = mock.patch.object(
            dev_loop,
            "sanitized_base",
            lambda: {"PATH": os.environ.get("PATH", "")},
        )
        self.env_patch.start()
        self.addCleanup(self.env_patch.stop)
        self.supervisor = dev_loop.Supervisor(self.layout, regen_data=False)
        self.addCleanup(self.supervisor.shutdown)
        write(self.layout.dist / "index.html", "old")
        write(self.layout.dist / dev_loop.BUILD_STAMP, "gen-old\n")
        write(self.layout.next_dist / "index.html", "new")
        write(self.layout.runtime / "data" / "bodies" / "old.body.yaml", "old\n")
        listening_stub(self.layout.stub_bin / "gen-old", self.port)
        self.supervisor.launch_stub(self.layout.stub_bin / "gen-old")
        self.supervisor.wait_ready()
        self.supervisor.activate(self.layout.stub_bin / "gen-old")
        self.supervisor.active_bodies_digest = dev_loop.tree_digest(
            self.layout.runtime / "data" / "bodies"
        )

    def stage_bodies(self, text: str) -> tuple[Path, str]:
        staging = self.layout.runtime / "data" / ".bodies.next"
        write(staging / "new.body.yaml", text)
        return staging, dev_loop.tree_digest(staging)

    def test_successful_publish_swaps_dist_bodies_and_records(self) -> None:
        binary = self.layout.stub_bin / "gen-new"
        listening_stub(binary, self.port)
        staged = self.stage_bodies("new\n")
        generation = types.SimpleNamespace(digest="gen-new")

        self.supervisor.publish_generation(generation, "sig-new", binary, staged)

        self.assertEqual((self.layout.dist / "index.html").read_text(), "new")
        self.assertEqual((self.layout.dist / dev_loop.BUILD_STAMP).read_text(), "gen-new\n")
        self.assertFalse(self.layout.next_dist.exists())
        self.assertEqual(
            (self.layout.runtime / "data" / "bodies" / "new.body.yaml").read_text(),
            "new\n",
        )
        runtime = json.loads(self.layout.runtime_record.read_text())
        self.assertEqual(runtime["generation"], "gen-new")
        self.assertEqual(runtime["bodies"], staged[1])
        record = json.loads(self.layout.record.read_text())
        self.assertEqual(record, {"generation": "gen-new", "signature": "sig-new"})
        running = self.supervisor.children.process("stub")
        self.assertIsNotNone(running)
        self.assertEqual(Path(running.args[0]), binary)

    def test_failed_new_stub_restores_bodies_dist_and_old_stub(self) -> None:
        binary = self.layout.stub_bin / "gen-broken"
        failing_stub(binary)
        staged = self.stage_bodies("new\n")
        generation = types.SimpleNamespace(digest="gen-broken")

        with self.assertRaises(dev_loop.BuildError):
            self.supervisor.publish_generation(generation, "sig-broken", binary, staged)

        self.assertEqual((self.layout.dist / "index.html").read_text(), "old")
        self.assertTrue((self.layout.runtime / "data" / "bodies" / "old.body.yaml").is_file())
        self.assertFalse((self.layout.runtime_record).exists())
        running = self.supervisor.children.process("stub")
        self.assertIsNotNone(running)
        self.assertEqual(Path(running.args[0]), self.layout.stub_bin / "gen-old")

    def test_failed_exchange_restores_bodies_and_old_stub(self) -> None:
        binary = self.layout.stub_bin / "gen-new"
        listening_stub(binary, self.port)
        staged = self.stage_bodies("new\n")
        generation = types.SimpleNamespace(digest="gen-new")

        with mock.patch.object(
            dev_loop,
            "exchange_dist",
            side_effect=dev_loop.BuildError("exchange unavailable"),
        ):
            with self.assertRaises(dev_loop.BuildError):
                self.supervisor.publish_generation(
                    generation, "sig-new", binary, staged
                )

        self.assertEqual((self.layout.dist / "index.html").read_text(), "old")
        self.assertTrue((self.layout.runtime / "data" / "bodies" / "old.body.yaml").is_file())
        running = self.supervisor.children.process("stub")
        self.assertIsNotNone(running)
        self.assertEqual(Path(running.args[0]), self.layout.stub_bin / "gen-old")
        self.supervisor.children.stop("stub")
        self.assertEqual(running.returncode, -signal.SIGTERM)

    def test_identical_binary_and_bodies_keep_the_running_stub(self) -> None:
        binary = self.layout.stub_bin / "gen-copy"
        write(binary, (self.layout.stub_bin / "gen-old").read_text())
        binary.chmod(0o755)
        before = self.supervisor.children.process("stub").pid
        staging = self.layout.runtime / "data" / ".bodies.next"
        write(staging / "old.body.yaml", "old\n")
        staged = (staging, dev_loop.tree_digest(staging))
        generation = types.SimpleNamespace(digest="gen-copy")

        self.supervisor.publish_generation(generation, "sig-copy", binary, staged)

        self.assertEqual((self.layout.dist / "index.html").read_text(), "new")
        self.assertEqual(self.supervisor.children.process("stub").pid, before)
        self.assertTrue((self.layout.runtime / "data" / "bodies" / "old.body.yaml").is_file())
        self.assertFalse(staged[0].exists())

    def test_published_replacement_stub_terminates_on_soft_signal(self) -> None:
        binary = self.layout.stub_bin / "gen-new"
        listening_stub(binary, self.port)
        staged = self.stage_bodies("new\n")
        generation = types.SimpleNamespace(digest="gen-new")

        self.supervisor.publish_generation(generation, "sig-new", binary, staged)
        replacement = self.supervisor.children.process("stub")
        self.assertEqual(os.getpgid(replacement.pid), replacement.pid)

        self.supervisor.children.stop("stub")

        self.assertEqual(replacement.returncode, -signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()
