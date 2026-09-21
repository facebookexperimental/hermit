# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Shared harness for the Hermit debugger (gdb / lldb) integration tests.

These tests drive a real debugger against Hermit's built-in gdbserver:

    hermit run   --gdbserver ...   -> guest stops, waits for a debugger to attach
    hermit replay ... (spawns gdb) -> deterministic replay under a debugger

The harness is intentionally dependency-free (Python stdlib only) and every
precondition is checked so the suite *skips* rather than *fails* on hosts that
cannot run Hermit (no PMU / no user namespaces / no CPUID interception) or that
lack a debugger. Real functional gaps are asserted, not skipped.
"""

import os
import re
import shutil
import socket
import subprocess
import time
import unittest
from pathlib import Path

# tests/debugger/harness.py -> repo root is two levels up.
REPO_ROOT = Path(__file__).resolve().parents[2]
GUEST_SRC = Path(__file__).resolve().parent / "guests" / "debuggee.c"

# Build guests and recordings under target/ (git-ignored) and, crucially, *not*
# under host /tmp: Hermit refuses to run a guest whose path is under host /tmp.
BUILD_DIR = REPO_ROOT / "target" / "debugger-tests"

# Known-deterministic values produced by guests/debuggee.c with x=7, y=6.
EXPECT_A = 7
EXPECT_B = 6
EXPECT_SUM = 13  # a + b
EXPECT_RESULT = 55  # (a + b) + (a * b)


def hermit_bin() -> Path | None:
    """Locate the built hermit binary (env override wins)."""
    env = os.environ.get("HERMIT_BIN")
    if env and Path(env).is_file():
        return Path(env)
    for cand in (
        REPO_ROOT / "target" / "debug" / "hermit",
        REPO_ROOT / "target" / "release" / "hermit",
    ):
        if cand.is_file():
            return cand
    return None


def have_gdb() -> bool:
    return shutil.which("gdb") is not None


def have_lldb_module() -> bool:
    try:
        import lldb  # noqa: F401

        return True
    except Exception:
        return False


def pick_free_port() -> int:
    """Ask the OS for a free TCP port. Racy by nature, but the window is small
    and Hermit binds the port immediately on startup."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def compile_guest() -> Path:
    """Compile the debuggee once (cached across tests). Non-PIE with -O0 -g."""
    BUILD_DIR.mkdir(parents=True, exist_ok=True)
    out = BUILD_DIR / "debuggee"
    if out.is_file() and out.stat().st_mtime >= GUEST_SRC.stat().st_mtime:
        return out
    cc = os.environ.get("CC", "cc")
    subprocess.run(
        [cc, "-g", "-O0", "-no-pie", "-fno-pie", "-o", str(out), str(GUEST_SRC)],
        check=True,
    )
    return out


def _port_is_listening(port: int) -> bool:
    """True if a socket is in LISTEN state on `port`, WITHOUT opening a
    connection. Hermit's gdbserver accepts exactly one client, so we must not
    consume that accept slot with a readiness probe -- we inspect kernel state
    instead (via `ss`, falling back to /proc/net/tcp{,6})."""
    ss = shutil.which("ss")
    if ss:
        try:
            out = subprocess.run(
                [ss, "-ltn"], stdout=subprocess.PIPE, timeout=5
            ).stdout.decode(errors="replace")
            return f":{port} " in out
        except Exception:  # noqa: BLE001
            pass
    hexport = f"{port:04X}"
    for path in ("/proc/net/tcp", "/proc/net/tcp6"):
        try:
            with open(path) as f:
                next(f)  # header
                for line in f:
                    cols = line.split()
                    # cols[1] = local_addr "IP:PORT" (hex); cols[3] = state,
                    # 0A == TCP_LISTEN.
                    if cols[1].split(":")[1] == hexport and cols[3] == "0A":
                        return True
        except (OSError, StopIteration, IndexError):
            continue
    return False


def _wait_for_port(port: int, proc: subprocess.Popen, timeout: float = 30.0) -> bool:
    """Poll until localhost:port is LISTENing, the process dies, or we time
    out."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if proc.poll() is not None:
            return False
        if _port_is_listening(port):
            return True
        time.sleep(0.2)
    return False


class HermitGdbserver:
    """Context manager: start `hermit run --gdbserver` and wait until the
    gdbserver port is reachable. Force host networking so the port is reachable
    from the host debugger (see hermit-cli run.rs / PR #144)."""

    def __init__(self, hermit: Path, guest: Path, port: int):
        self.hermit = hermit
        self.guest = guest
        self.port = port
        self.proc: subprocess.Popen | None = None
        self.log = BUILD_DIR / f"hermit_run_{port}.log"

    def __enter__(self) -> "HermitGdbserver":
        logf = open(self.log, "wb")
        self._logf = logf
        self.proc = subprocess.Popen(
            [
                str(self.hermit),
                "run",
                "--gdbserver",
                f"--gdbserver-port={self.port}",
                "--network=host",
                "--",
                str(self.guest),
            ],
            stdout=logf,
            stderr=subprocess.STDOUT,
            stdin=subprocess.DEVNULL,
        )
        if not _wait_for_port(self.port, self.proc):
            self._cleanup()
            raise RuntimeError(
                f"hermit gdbserver never became reachable on port {self.port}; "
                f"log:\n{self.read_log()}"
            )
        return self

    def read_log(self) -> str:
        try:
            return self.log.read_text(errors="replace")
        except OSError:
            return ""

    def _cleanup(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=10)
        try:
            self._logf.close()
        except Exception:
            pass

    def __exit__(self, *exc):
        self._cleanup()
        return False


def gdb_batch(guest: Path, commands: list[str], timeout: float = 120.0) -> str:
    """Run `gdb -batch -nx` with the given -ex commands against `guest` and
    return its combined stdout/stderr. `-nx` avoids host ~/.gdbinit surprises;
    'set breakpoint pending on' avoids interactive prompts."""
    argv = ["gdb", "-batch", "-nx", "-ex", "set breakpoint pending on"]
    for c in commands:
        argv += ["-ex", c]
    argv.append(str(guest))
    r = subprocess.run(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        timeout=timeout,
    )
    return r.stdout.decode(errors="replace")


def record(hermit: Path, guest: Path, data_dir: Path) -> str:
    """`hermit record start` the guest and return the recording id."""
    data_dir.mkdir(parents=True, exist_ok=True)
    r = subprocess.run(
        [str(hermit), "record", "start", f"--data-dir={data_dir}", "--", str(guest)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        timeout=180,
    )
    out = r.stdout.decode(errors="replace")
    if r.returncode != 0:
        raise RuntimeError(f"hermit record failed:\n{out}")
    # Output contains: "hermit replay <32-hex-id>"
    m = re.search(r"hermit replay\s+([0-9a-f]{16,})", out)
    if not m:
        raise RuntimeError(f"could not parse recording id from:\n{out}")
    return m.group(1)


def replay_under_gdb(
    hermit: Path,
    data_dir: Path,
    recording_id: str,
    gdbex: list[str],
    port: int,
    timeout: float = 180.0,
) -> str:
    """Replay a recording with gdb attached to the replay gdbserver.

    `hermit replay` spawns gdb itself (there is no serve-only replay mode), so
    we drive it via `--gdbex` commands and feed stdin from /dev/null: gdb runs
    the -ex commands and then hits EOF at its prompt and exits, making the run
    non-interactive and CI-safe."""
    argv = [
        str(hermit),
        "replay",
        recording_id,
        f"--data-dir={data_dir}",
        f"--gdbserver-port={port}",
    ]
    for c in gdbex:
        argv += ["--gdbex", c]
    r = subprocess.run(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        stdin=subprocess.DEVNULL,
        timeout=timeout,
    )
    return r.stdout.decode(errors="replace")


def can_run_hermit(hermit: Path) -> tuple[bool, str]:
    """Try a trivial `hermit run -- /bin/true`. Returns (ok, reason).

    Hosts without PMU / user namespaces / CPUID interception cannot run Hermit;
    on those the whole debugger suite skips instead of reporting false failures.
    """
    try:
        r = subprocess.run(
            [str(hermit), "run", "--", "/bin/true"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=120,
        )
    except Exception as e:  # noqa: BLE001
        return False, f"hermit failed to launch: {e}"
    if r.returncode != 0:
        return False, "hermit run -- /bin/true failed:\n" + r.stdout.decode(
            errors="replace"
        )
    return True, ""


class DebuggerTestBase(unittest.TestCase):
    """Common precondition checks + fixtures for the debugger tests."""

    require_gdb = False
    require_lldb = False

    @classmethod
    def setUpClass(cls):
        cls.hermit = hermit_bin()
        if cls.hermit is None:
            raise unittest.SkipTest(
                "hermit binary not found (build it or set HERMIT_BIN)"
            )
        if cls.require_gdb and not have_gdb():
            raise unittest.SkipTest("gdb not found on PATH")
        if cls.require_lldb and not have_lldb_module():
            raise unittest.SkipTest(
                "lldb python module not importable "
                "(set PYTHONPATH=\"$(lldb -P)\"); see run_debugger_tests.sh"
            )
        ok, reason = can_run_hermit(cls.hermit)
        if not ok:
            raise unittest.SkipTest("host cannot run Hermit: " + reason)
        try:
            cls.guest = compile_guest()
        except Exception as e:  # noqa: BLE001
            raise unittest.SkipTest(f"could not compile guest ({e})")
