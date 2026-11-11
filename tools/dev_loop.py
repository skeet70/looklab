#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Single-threaded development UI supervisor with serial builds and transactional cutover."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import shutil
import signal
import socket
import stat
import subprocess
import sys
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
sys.path.insert(0, str(TOOLS))

try:
    from source_generations import (
        GIT_REPOSITORY_ENVIRONMENT,
        capture,
        lane_lock,
        source_signature,
    )
except ImportError as error:  # pragma: no cover - configuration error
    sys.exit(f"dev_loop: cannot import source_generations: {error}")

DEFAULT_PUBLIC_PORT = 8081
DEFAULT_STUB_PORT = 8098
BUILD_STAMP = "dev-build-stamp.txt"
RECORD = "dev-generation.json"
RUNTIME_RECORD = "generation.json"
POLL_SECONDS = 1.0
GRACE_SECONDS = 10.0
READY_SECONDS = 15.0
FICLONE = 0x40049409
TERMINATION_SIGNALS = frozenset({signal.SIGTERM, signal.SIGINT, signal.SIGHUP})
DATA_DIRECTORIES = ("bodies", "manuals")


class BuildError(RuntimeError):
    pass


class Stopped(Exception):
    pass


@dataclass(frozen=True)
class Layout:
    origin: Path
    target: Path
    view: Path
    dist: Path
    next_dist: Path
    stub_state: Path
    runtime: Path
    stub_binary: Path
    stub_bin: Path
    record: Path
    runtime_record: Path
    css_script: Path
    css_watch_script: Path
    public_port: int
    stub_port: int

    @classmethod
    def from_environment(cls) -> "Layout":
        origin = Path(
            os.environ.get("LOOKLAB_SOURCE_ORIGIN") or TOOLS.parent
        ).resolve()
        raw_target = os.environ.get("LOOKLAB_UI_TARGET_DIR") or os.environ.get(
            "CARGO_TARGET_DIR"
        )
        target = Path(raw_target) if raw_target else origin / "target-ui"
        if not target.is_absolute():
            target = origin / target
        target = target.resolve()
        runtime = target / "runtime"
        public_port = port_from_environment("LOOKLAB_DEV_PORT", DEFAULT_PUBLIC_PORT)
        stub_port = port_from_environment("LOOKLAB_DEV_API_PORT", DEFAULT_STUB_PORT)
        if public_port == stub_port:
            raise ValueError("LOOKLAB_DEV_PORT and LOOKLAB_DEV_API_PORT must differ")
        return cls(
            origin=origin,
            target=target,
            view=target / "source",
            dist=target / "dist",
            next_dist=target / "next-dist",
            stub_state=target / "stub-state",
            runtime=runtime,
            stub_binary=target / "debug" / "examples" / "stub_server",
            stub_bin=target / "stub-bin",
            record=target / RECORD,
            runtime_record=runtime / RUNTIME_RECORD,
            css_script=target / "source" / "site" / "scripts" / "dev-css.mjs",
            css_watch_script=origin / "site" / "scripts" / "dev-css.mjs",
            public_port=public_port,
            stub_port=stub_port,
        )


def port_from_environment(name: str, default: int) -> int:
    raw = os.environ.get(name)
    if raw is None:
        return default
    try:
        port = int(raw)
    except ValueError:
        raise ValueError(f"{name} must be a port number, got {raw!r}") from None
    if not 1 <= port <= 65535:
        raise ValueError(f"{name} must be between 1 and 65535, got {port}")
    return port


def toml_string(value: object) -> str:
    return json.dumps(str(value))


stopping = False
current_process: subprocess.Popen | None = None


def request_stop(_signum: int, _frame: object) -> None:
    global stopping
    stopping = True
    process = current_process
    if process is not None and process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass


@contextmanager
def blocked_termination():
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, TERMINATION_SIGNALS)
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def unblock_termination_in_child() -> None:
    signal.pthread_sigmask(signal.SIG_UNBLOCK, TERMINATION_SIGNALS)


def start_process(
    argv: list[object],
    *,
    cwd: Path | None = None,
    env: dict[str, str] | None = None,
) -> subprocess.Popen:
    return subprocess.Popen(
        [os.fspath(argument) for argument in argv],
        cwd=None if cwd is None else os.fspath(cwd),
        env=env,
        start_new_session=True,
        preexec_fn=unblock_termination_in_child,
    )


def stop_group(process: subprocess.Popen) -> None:
    leader_alive = process.poll() is None
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    if leader_alive:
        try:
            process.wait(timeout=GRACE_SECONDS)
            return
        except subprocess.TimeoutExpired:
            pass
    else:
        time.sleep(0.05)
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        return
    if leader_alive:
        try:
            process.wait(timeout=GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            pass


class Children:
    def __init__(self) -> None:
        self.processes: dict[str, subprocess.Popen] = {}

    def start(
        self,
        name: str,
        argv: list[object],
        *,
        cwd: Path | None = None,
        env: dict[str, str] | None = None,
    ) -> None:
        self.stop(name)
        self.processes[name] = start_process(argv, cwd=cwd, env=env)
        print(f"[dev] started {name} (pid {self.processes[name].pid})", flush=True)

    def stop(self, name: str) -> None:
        process = self.processes.pop(name, None)
        if process is not None:
            stop_group(process)

    def stop_all(self) -> None:
        for name in list(self.processes):
            self.stop(name)

    def running(self, name: str) -> bool:
        process = self.processes.get(name)
        return process is not None and process.poll() is None

    def process(self, name: str) -> subprocess.Popen | None:
        return self.processes.get(name)

    def check(self) -> None:
        for name, process in self.processes.items():
            code = process.poll()
            if code is not None:
                raise RuntimeError(f"{name} exited with status {code}")


def run_step(
    label: str,
    argv: list[object],
    *,
    cwd: Path,
    env: dict[str, str],
) -> None:
    global current_process
    if stopping:
        raise Stopped()
    print(f"[dev] {label}", flush=True)
    process = start_process(argv, cwd=cwd, env=env)
    current_process = process
    try:
        code = process.wait()
    finally:
        current_process = None
    if stopping:
        raise Stopped()
    if code != 0:
        raise BuildError(f"{label} exited with status {code}")


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_digest(root: Path) -> str:
    if not root.is_dir():
        return "missing"
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        digest.update(path.relative_to(root).as_posix().encode("utf-8") + b"\0")
        digest.update(file_digest(path).encode("ascii"))
    return digest.hexdigest()


def write_atomic(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(text, encoding="utf-8")
    os.replace(temporary, path)


def reflink_or_copy(source: Path, destination: Path) -> None:
    try:
        with source.open("rb") as origin, destination.open("wb") as copy:
            fcntl.ioctl(copy.fileno(), FICLONE, origin.fileno())
    except OSError:
        destination.unlink(missing_ok=True)
        shutil.copy2(source, destination)
    os.chmod(destination, stat.S_IMODE(os.stat(source).st_mode))


def exchange_dist(next_dist: Path, dist: Path) -> None:
    if not (next_dist / "index.html").is_file():
        raise BuildError(f"{next_dist} has no index.html")
    if not dist.exists():
        os.rename(next_dist, dist)
        return
    result = subprocess.run(
        ["mv", "--exchange", "--no-copy", "-T", os.fspath(next_dist), os.fspath(dist)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise BuildError(
            "atomic directory exchange failed; GNU coreutils mv with --exchange is required: "
            + (result.stderr.strip() or result.stdout.strip() or "unknown error")
        )


def sanitized_base() -> dict[str, str]:
    environment = os.environ.copy()
    for name in GIT_REPOSITORY_ENVIRONMENT:
        environment.pop(name, None)
    return environment


def build_environment(layout: Layout, generation) -> dict[str, str]:
    environment = generation.child_environment(layout.target, ui=True)
    environment["CARGO_PROFILE_DEV_DEBUG"] = "0"
    return environment


class Supervisor:
    def __init__(
        self, layout: Layout, *, regen_data: bool, reuse_data: bool = False
    ) -> None:
        self.layout = layout
        self.regen_data = regen_data
        self.reuse_data = reuse_data
        self.children = Children()
        self.active_binary: Path | None = None
        self.active_binary_digest: str | None = None
        self.active_data_digests = {name: "missing" for name in DATA_DIRECTORIES}
        self.environment = sanitized_base()

    def prepare(self) -> None:
        for directory in (
            self.layout.target,
            self.layout.view,
            self.layout.stub_state,
            self.layout.runtime / "data",
            self.layout.next_dist,
        ):
            directory.mkdir(parents=True, exist_ok=True)

    def stage_data(self, name: str) -> tuple[Path, str] | None:
        source = self.layout.view / "data" / name
        if not source.is_dir():
            return None
        staging = self.layout.runtime / "data" / f".{name}.next"
        shutil.rmtree(staging, ignore_errors=True)
        shutil.copytree(source, staging)
        return staging, tree_digest(staging)

    def stage_site_data(self) -> None:
        source = self.layout.origin / "site" / "data"
        if not source.is_dir():
            raise BuildError("--reuse-data needs site/data in the source checkout")
        destination = self.layout.view / "site" / "data"
        index = source / "index.json"
        if destination.is_dir() and (destination / "index.json").is_file():
            if (destination / "index.json").read_bytes() == index.read_bytes():
                return
        shutil.rmtree(destination, ignore_errors=True)
        shutil.copytree(source, destination)

    def replace_data(self, name: str, staging: Path) -> None:
        data = self.layout.runtime / "data"
        destination = data / name
        retired = data / f".{name}.old"
        shutil.rmtree(retired, ignore_errors=True)
        if destination.exists():
            os.rename(destination, retired)
        os.rename(staging, destination)

    def restore_data(self, name: str, had_data: bool) -> None:
        data = self.layout.runtime / "data"
        destination = data / name
        retired = data / f".{name}.old"
        failed = data / f".{name}.failed"
        shutil.rmtree(failed, ignore_errors=True)
        if destination.exists():
            os.rename(destination, failed)
        if retired.exists():
            os.rename(retired, destination)
        elif not had_data:
            shutil.rmtree(destination, ignore_errors=True)
        shutil.rmtree(failed, ignore_errors=True)

    def drop_data_backup(self, name: str) -> None:
        shutil.rmtree(self.layout.runtime / "data" / f".{name}.old", ignore_errors=True)

    def install_binary(self, digest: str) -> Path:
        if not self.layout.stub_binary.is_file():
            raise BuildError(f"cargo did not leave {self.layout.stub_binary}")
        self.layout.stub_bin.mkdir(parents=True, exist_ok=True)
        destination = self.layout.stub_bin / digest
        temporary = self.layout.stub_bin / f".{digest}.tmp"
        temporary.unlink(missing_ok=True)
        reflink_or_copy(self.layout.stub_binary, temporary)
        os.replace(temporary, destination)
        self.prune_binaries(destination)
        return destination

    def prune_binaries(self, keep: Path | None) -> None:
        if not self.layout.stub_bin.is_dir():
            return
        for entry in self.layout.stub_bin.iterdir():
            if entry not in (keep, self.active_binary):
                entry.unlink(missing_ok=True)

    def launch_stub(self, binary: Path) -> None:
        if not binary.is_file():
            raise BuildError(f"stub binary is missing: {binary}")
        self.children.start(
            "stub",
            [
                binary,
                "--dist",
                self.layout.dist,
                "--port",
                str(self.layout.stub_port),
                "--origin",
                f"http://localhost:{self.layout.public_port}",
                "--state",
                self.layout.stub_state,
            ],
            cwd=self.layout.runtime,
            env=self.environment,
        )

    def wait_ready(self) -> None:
        process = self.children.process("stub")
        if process is None:
            raise BuildError("stub was not started")
        deadline = time.monotonic() + READY_SECONDS
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise BuildError(f"stub exited with status {process.returncode}")
            try:
                with socket.create_connection(
                    ("127.0.0.1", self.layout.stub_port), timeout=0.2
                ):
                    return
            except OSError:
                time.sleep(0.05)
        raise BuildError("stub did not start listening")

    def activate(self, binary: Path, digest: str | None = None) -> None:
        self.active_binary = binary
        self.active_binary_digest = digest if digest is not None else file_digest(binary)

    def read_runtime_record(self) -> dict | None:
        try:
            value = json.loads(
                self.layout.runtime_record.read_text(encoding="utf-8")
            )
        except (OSError, ValueError):
            return None
        return value if isinstance(value, dict) else None

    def write_runtime_record(self, generation: str) -> None:
        record = {"generation": generation}
        record.update(self.active_data_digests)
        write_atomic(
            self.layout.runtime_record,
            json.dumps(record, sort_keys=True) + "\n",
        )

    def start_persisted_runtime(self) -> bool:
        record = self.read_runtime_record()
        if record is None:
            return False
        generation = record.get("generation")
        if not isinstance(generation, str):
            return False
        digests = {name: record.get(name) for name in DATA_DIRECTORIES}
        if not all(isinstance(digest, str) for digest in digests.values()):
            return False
        binary = self.layout.stub_bin / generation
        if not binary.is_file():
            return False
        for name, digest in digests.items():
            if tree_digest(self.layout.runtime / "data" / name) != digest:
                return False
        try:
            self.launch_stub(binary)
            self.wait_ready()
        except (BuildError, OSError) as error:
            print(f"[dev] persisted stub failed to start: {error}", file=sys.stderr, flush=True)
            self.children.stop("stub")
            return False
        self.activate(binary)
        self.active_data_digests = digests
        return True

    def start_helpers(self) -> None:
        self.children.start(
            "css",
            ["node", self.layout.css_watch_script, "watch", self.layout.dist, self.layout.origin],
            env=self.environment,
        )
        self.children.start(
            "proxy",
            [
                "socat",
                f"TCP4-LISTEN:{self.layout.public_port},bind=127.0.0.1,reuseaddr,fork",
                f"TCP4:127.0.0.1:{self.layout.stub_port}",
            ],
            env=self.environment,
        )

    def rollback_stub(
        self,
        old_binary: Path | None,
        swaps: dict[str, tuple[Path, str]],
        had_data: dict[str, bool],
    ) -> None:
        self.children.stop("stub")
        for name in swaps:
            self.restore_data(name, had_data[name])
        if old_binary is None:
            return
        try:
            self.launch_stub(old_binary)
            self.wait_ready()
        except (BuildError, OSError) as error:
            print(
                f"[dev] previous stub did not restart: {error}", file=sys.stderr, flush=True
            )

    def run_generation(
        self, generation, environment: dict[str, str]
    ) -> tuple[Path, dict[str, tuple[Path, str]]]:
        layout = self.layout
        view = layout.view
        staged = {
            name: staged_dir
            for name in DATA_DIRECTORIES
            if (staged_dir := self.stage_data(name)) is not None
        }
        if not self.reuse_data:
            run_step(
                f"site data ({generation.digest[:12]})",
                ["sh", view / "site" / "scripts" / "site-data.sh"]
                + (["--force"] if self.regen_data else []),
                cwd=view,
                env=environment,
            )
            self.regen_data = False
        else:
            self.stage_site_data()
        run_step(
            f"stub server ({generation.digest[:12]})",
            [
                "cargo",
                "build",
                "--locked",
                "-p",
                "looklab-web",
                "--example",
                "stub_server",
                "--features",
                "stub",
            ],
            cwd=view,
            env=environment,
        )
        binary = self.install_binary(generation.digest)
        build_env = dict(environment)
        build_env["CARGO_NET_OFFLINE"] = "true"
        run_step(
            f"site build ({generation.digest[:12]})",
            [
                "sh",
                view / "site" / "scripts" / "build-site.sh",
                "--dist",
                layout.next_dist,
            ],
            cwd=view,
            env=build_env,
        )
        run_step(
            f"reload injector ({generation.digest[:12]})",
            ["node", layout.css_script, "inject", layout.next_dist / "index.html"],
            cwd=layout.origin,
            env=environment,
        )
        run_step(
            f"live stylesheets ({generation.digest[:12]})",
            ["node", layout.css_script, "publish", layout.next_dist, layout.origin],
            cwd=layout.origin,
            env=environment,
        )
        return binary, staged

    def publish_generation(
        self,
        generation,
        signature: str,
        binary: Path,
        staged: dict[str, tuple[Path, str]],
    ) -> None:
        layout = self.layout
        binary_digest = file_digest(binary)
        swaps = {
            name: staged_dir
            for name, staged_dir in staged.items()
            if staged_dir[1] != self.active_data_digests[name]
        }
        restart = (
            not self.children.running("stub")
            or binary_digest != self.active_binary_digest
            or bool(swaps)
        )
        old_binary = self.active_binary
        had_data = {
            name: self.active_data_digests[name] not in (None, "missing")
            for name in DATA_DIRECTORIES
        }
        with blocked_termination():
            write_atomic(
                layout.next_dist / BUILD_STAMP, f"{generation.digest}\n"
            )
            if restart:
                self.children.stop("stub")
                try:
                    for name, (staging, _) in swaps.items():
                        self.replace_data(name, staging)
                    self.launch_stub(binary)
                    self.wait_ready()
                except BaseException:
                    self.rollback_stub(old_binary, swaps, had_data)
                    raise
            try:
                exchange_dist(layout.next_dist, layout.dist)
            except BaseException:
                if restart:
                    self.rollback_stub(old_binary, swaps, had_data)
                raise
            if restart:
                self.activate(binary, binary_digest)
                for name, (_, digest) in swaps.items():
                    self.active_data_digests[name] = digest
                try:
                    self.write_runtime_record(generation.digest)
                except OSError as error:
                    print(
                        f"[dev] could not record runtime generation: {error}",
                        file=sys.stderr,
                        flush=True,
                    )
            try:
                write_record(layout, signature, generation.digest)
            except OSError as error:
                print(
                    f"[dev] could not record served generation: {error}",
                    file=sys.stderr,
                    flush=True,
                )
            shutil.rmtree(layout.next_dist, ignore_errors=True)
            for name, (staging, _) in staged.items():
                if name not in swaps:
                    shutil.rmtree(staging, ignore_errors=True)
            for name in swaps:
                self.drop_data_backup(name)
            self.prune_binaries(binary)

    def shutdown(self) -> None:
        self.children.stop_all()
        data = self.layout.runtime / "data"
        for name in DATA_DIRECTORIES:
            for suffix in ("next", "old", "failed"):
                shutil.rmtree(data / f".{name}.{suffix}", ignore_errors=True)


def read_record(layout: Layout) -> dict | None:
    try:
        record = json.loads(layout.record.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(record, dict):
        return None
    return record


def view_digest(layout: Layout) -> str | None:
    try:
        value = json.loads(
            (layout.view / ".looklab-generation.json").read_text(encoding="utf-8")
        )
    except (OSError, ValueError):
        return None
    if isinstance(value, dict) and isinstance(value.get("digest"), str):
        return value["digest"]
    return None


def usable_record(layout: Layout, record: dict | None, signature: str) -> bool:
    if record is None or record.get("signature") != signature:
        return False
    if view_digest(layout) != record.get("generation"):
        return False
    if not (layout.dist / "index.html").is_file():
        return False
    if not (layout.stub_bin / str(record.get("generation"))).is_file():
        return False
    try:
        stamp = (layout.dist / BUILD_STAMP).read_text(encoding="utf-8").strip()
    except OSError:
        return False
    return stamp == record.get("generation")


def write_record(layout: Layout, signature: str, generation: str) -> None:
    write_atomic(
        layout.record,
        json.dumps({"signature": signature, "generation": generation}, sort_keys=True)
        + "\n",
    )


class Observer:
    def __init__(self, layout: Layout) -> None:
        self.layout = layout
        self.last_error: str | None = None

    def signature(self) -> str | None:
        try:
            value = source_signature(self.layout.origin, ui=True)
        except Exception as error:  # partial writes must not stop the server
            message = f"{type(error).__name__}: {error}"
            if message != self.last_error:
                print(
                    f"[dev] source observation failed; keeping the served generation: {message}",
                    file=sys.stderr,
                    flush=True,
                )
                self.last_error = message
            return None
        self.last_error = None
        return value


def attempt(layout: Layout, supervisor: Supervisor, observer: Observer, observed: str):
    generation = None
    signature = observed
    with lane_lock(layout.target):
        try:
            observed_now = observer.signature()
            if observed_now is None:
                return "observe", None, None, None
            signature = observed_now
            generation = capture(
                layout.origin, layout.view, exclude=(layout.target,)
            )
            if stopping:
                raise Stopped()
        except Stopped:
            raise
        except Exception as error:  # capture races are generation failures
            return "failed", generation, signature, error
        environment = build_environment(layout, generation)
        try:
            binary, staged = supervisor.run_generation(generation, environment)
            supervisor.publish_generation(generation, signature, binary, staged)
        except Stopped:
            raise
        except (BuildError, OSError) as error:
            return "failed", generation, signature, error
    return "ok", generation, signature, None


def loop(layout: Layout, supervisor: Supervisor, *, once: bool) -> int:
    supervisor.prepare()
    supervisor.start_helpers()
    observer = Observer(layout)
    record = read_record(layout)
    served = False if once else supervisor.start_persisted_runtime()
    initial = observer.signature()
    reused = (
        served
        and not supervisor.regen_data
        and initial is not None
        and usable_record(layout, record, initial)
    )
    if reused:
        print(f"[dev] reusing generation {record['generation']}", flush=True)
        built = initial
    else:
        built = None
    failed: str | None = None
    print(
        f"[dev] UI http://localhost:{layout.public_port}/ (api 127.0.0.1:{layout.stub_port}); "
        f"view {layout.view}; styles hot, generations serial"
        + ("; fixed generation" if once else ""),
        flush=True,
    )
    while not stopping:
        supervisor.children.check()
        signature = observer.signature()
        if signature is None:
            if once:
                print("[dev] --once could not observe the source generation", file=sys.stderr, flush=True)
                return 1
            time.sleep(POLL_SECONDS)
            continue
        if signature == built or signature == failed:
            time.sleep(POLL_SECONDS)
            continue
        status, generation, observed, error = attempt(
            layout, supervisor, observer, signature
        )
        if stopping:
            break
        if status == "observe":
            if once:
                print("[dev] --once could not observe the source generation", file=sys.stderr, flush=True)
                return 1
            time.sleep(POLL_SECONDS)
            continue
        if status == "ok":
            built = observed
            failed = None
            print(f"[dev] serving generation {generation.digest}", flush=True)
            if once:
                print(
                    f"[dev] fixed generation {generation.digest} serving at "
                    f"http://localhost:{layout.public_port}/",
                    flush=True,
                )
                while not stopping:
                    supervisor.children.check()
                    time.sleep(POLL_SECONDS)
                return 0
            continue
        failed = observed if observed is not None else signature
        label = "capture" if generation is None else generation.digest
        print(f"[dev] build failed for {label}: {error}", file=sys.stderr, flush=True)
        if once:
            print("[dev] --once generation failed; not serving", file=sys.stderr, flush=True)
            return 1
        if failed == signature:
            print(
                f"[dev] source unchanged after failure; keeping {layout.dist} "
                "until the next edit",
                file=sys.stderr,
                flush=True,
            )
    return 0


def parse_arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="dev_loop.py", description="Serve the looklab UI from captured source generations."
    )
    parser.add_argument(
        "--regen-data", action="store_true", help="regenerate the static site data"
    )
    parser.add_argument(
        "--reuse-data",
        action="store_true",
        help="copy site/data from the checkout instead of regenerating it",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="build and serve exactly one captured generation, then stop watching sources",
    )
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    arguments = parse_arguments(argv)
    if arguments.regen_data and arguments.reuse_data:
        print("[dev] --regen-data and --reuse-data cannot be combined", file=sys.stderr)
        return 1
    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    signal.signal(signal.SIGHUP, request_stop)
    supervisor = None
    try:
        layout = Layout.from_environment()
        supervisor = Supervisor(
            layout,
            regen_data=arguments.regen_data,
            reuse_data=arguments.reuse_data,
        )
        return loop(layout, supervisor, once=arguments.once)
    except Stopped:
        return 0
    except (RuntimeError, OSError, ValueError) as error:
        print(f"[dev] {error}", file=sys.stderr, flush=True)
        return 1
    finally:
        if supervisor is not None:
            supervisor.shutdown()


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
