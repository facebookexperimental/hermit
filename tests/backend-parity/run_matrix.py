#!/usr/bin/env python3
"""Run and ratchet Hermit's cross-backend compatibility matrix."""

from __future__ import annotations

import argparse
import csv
import os
import signal
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
BACKENDS = ("ptrace", "dbi", "kvm")
RUNS = 3

# The compatibility scorecard is measurement state, not Hermit source.  When
# this checkout is nested in dev-hermit, live observations are appended to the
# outer workspace's canonical scorecard.  Standalone Hermit clones simply skip
# that side effect unless --parent-scorecard is supplied.
SCORECARD_HEADER = (
    "run_id",
    "run_utc",
    "hermit_sha",
    "reverie_sha",
    "dirty",
    "run_mode",
    "lane",
    "bucket",
    "test_id",
    "test_mode",
    "backend",
    "cell_state",
    "outcome",
    "deterministic",
    "parity",
    "output_hash",
    "duration_ms",
    "max_rss_kb",
    "reason",
)

# L2 (--verify) assurance kinds, ordered weakest to strongest. "gap" means the
# contract cannot currently be verified at L2 on that backend. "guest" is
# guest-visible L2: the two --verify runs produced identical stdout+exit but the
# internal trace is not compared (KVM concurrent mode). "detlog" is full L2: the
# two runs produced a bitwise-identical DETLOG after normalization (ptrace, DBI).
L2_RANK = {"gap": 0, "guest": 1, "detlog": 2}
# Per-backend L2 values the matrix may record. KVM's concurrent verify path can
# never emit a DETLOG witness, so it is capped at guest-visible L2.
L2_ALLOWED = {
    "ptrace": {"detlog"},
    "dbi": {"detlog", "gap"},
    "kvm": {"guest", "gap"},
}


class MatrixError(Exception):
    """An invalid case catalog or failed regression contract."""


def compile_fixture(source: Path, output: Path, *flags: str) -> Path:
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise MatrixError("C compiler unavailable (set CC or install cc)")
    command = [
        compiler,
        "-O2",
        "-g",
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        *flags,
        str(source),
        "-o",
        str(output),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise MatrixError(
            f"fixture compilation failed: {command!r}\n{result.stdout}{result.stderr}"
        )
    return output


class Fixtures:
    def __init__(self, root: Path) -> None:
        self.root = root
        self._binaries: dict[str, Path] = {}

    def binary(self, name: str) -> Path:
        if name in self._binaries:
            return self._binaries[name]

        local = SCRIPT_DIR / "fixtures"
        sources: dict[str, tuple[Path, tuple[str, ...]]] = {
            "pthread_lifecycle": (local / "pthread_lifecycle.c", ("-pthread",)),
            "process_wait_lifecycle": (
                REPOSITORY / "tests/c/dbi_wait_lifecycle.c",
                ("-D_GNU_SOURCE",),
            ),
            "mmap_exec": (REPOSITORY / "tests/c/dbi_mmap_exec.c", ()),
            "syscall_file_io": (
                REPOSITORY / "tests/c/syscall_file_io.c",
                (),
            ),
            "syscall_file_metadata": (
                REPOSITORY / "tests/c/syscall_file_metadata.c",
                (),
            ),
            "io_uring_fallback": (
                REPOSITORY / "tests/c/io_uring_fallback.c",
                ("-D_GNU_SOURCE",),
            ),
            "listmount_enosys": (
                REPOSITORY / "tests/c/listmount_enosys.c",
                (),
            ),
            "process_vm_readv_refusal": (
                REPOSITORY / "tests/c/process_vm_readv_refusal_probe.c",
                (),
            ),
            "process_vm_writev_refusal": (
                REPOSITORY / "tests/c/process_vm_writev_refusal_probe.c",
                (),
            ),
            "madvise_determinism": (
                REPOSITORY / "tests/c/madvise_determinism.c",
                (),
            ),
            "mmap_determinism": (
                REPOSITORY / "tests/c/mmap_determinism.c",
                (),
            ),
            "cpuid_probe": (local / "cpuid_probe.c", ()),
            "clock_determinism": (
                REPOSITORY / "tests/c/clock_determinism.c",
                ("-D_GNU_SOURCE",),
            ),
            "random_sources": (
                REPOSITORY / "tests/c/random_sources.c",
                ("-D_GNU_SOURCE", "-pthread"),
            ),
            "pid_probe": (local / "pid_probe.c", ()),
            "scheduler_policy_queries": (
                REPOSITORY / "tests/c/scheduler_policy_queries.c",
                (),
            ),
            "signal_disposition": (
                REPOSITORY / "tests/c/signal_disposition.c",
                ("-D_GNU_SOURCE",),
            ),
            "sigaction_state": (
                local / "sigaction_state.c",
                ("-D_GNU_SOURCE",),
            ),
            "sigprocmask_state": (
                local / "sigprocmask_state.c",
                ("-D_GNU_SOURCE",),
            ),
            "sigaltstack_state": (
                local / "sigaltstack_state.c",
                ("-D_GNU_SOURCE",),
            ),
        }
        source, flags = sources[name]
        binary = compile_fixture(source, self.root / name, *flags)
        self._binaries[name] = binary
        return binary


class CatalogFixtures:
    def binary(self, name: str) -> Path:
        return Path("/backend-parity-catalog") / name


def case_catalog(
    fixtures: Fixtures | CatalogFixtures,
) -> dict[str, tuple[list[str], int, bytes | None]]:
    fixture_input = SCRIPT_DIR / "fixtures/input.txt"
    return {
        "hello_stdout": (["/bin/echo", "hello world"], 0, b"hello world\n"),
        "argument_forwarding": (
            ["/usr/bin/printf", "%s|%s\n", "alpha", "two words"],
            0,
            b"alpha|two words\n",
        ),
        "exit_zero": (["/bin/true"], 0, b""),
        "exit_status": (["/bin/sh", "-c", "exit 23"], 23, b""),
        "file_read": (["/bin/cat", str(fixture_input)], 0, fixture_input.read_bytes()),
        "file_mutation": (
            [str(fixtures.binary("syscall_file_io"))],
            0,
            b"syscall-file-io-ok count=5\n",
        ),
        "file_metadata": (
            [str(fixtures.binary("syscall_file_metadata"))],
            0,
            b"syscall-file-metadata-ok count=20\n",
        ),
        "io_uring_fallback": (
            [str(fixtures.binary("io_uring_fallback"))],
            0,
            b"io_uring blocked; epoll fallback ready\n",
        ),
        "listmount_unavailable": (
            [str(fixtures.binary("listmount_enosys"))],
            0,
            b"listmount deterministically unavailable\n",
        ),
        "process_vm_readv_refusal": (
            [str(fixtures.binary("process_vm_readv_refusal"))],
            0,
            b"process-vm-readv-refused-ok\n",
        ),
        "process_vm_writev_refusal": (
            [str(fixtures.binary("process_vm_writev_refusal"))],
            0,
            b"process-vm-writev-refused-ok\n",
        ),
        "executable_mmap": (
            [str(fixtures.binary("mmap_exec"))],
            0,
            b"dbi-mmap-exec-ok\n",
        ),
        "memory_advice": (
            [str(fixtures.binary("madvise_determinism"))],
            0,
            b"madvise-ok\n",
        ),
        "heap_growth": (
            [str(fixtures.binary("mmap_determinism")), "heap"],
            0,
            None,
        ),
        "anonymous_mmap_layout": (
            [str(fixtures.binary("mmap_determinism")), "multiple"],
            0,
            None,
        ),
        "shared_anonymous_mmap": (
            [str(fixtures.binary("mmap_determinism")), "shared"],
            0,
            None,
        ),
        "pthread_lifecycle": (
            [str(fixtures.binary("pthread_lifecycle"))],
            0,
            b"threads=4 total=10\n",
        ),
        "process_wait_accounting": (
            [str(fixtures.binary("process_wait_lifecycle")), "--accounting-only"],
            0,
            b"wait4=7 waitid=9 reaped=2 cpu=zero\n",
        ),
        "process_wait_lifecycle": (
            [str(fixtures.binary("process_wait_lifecycle"))],
            0,
            b"wait4=7 waitid=9 sigchld=observed reaped=2 cpu=zero\n",
        ),
        "cpuid_policy": (
            [str(fixtures.binary("cpuid_probe"))],
            0,
            b"CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n",
        ),
        "virtual_clock": ([str(fixtures.binary("clock_determinism"))], 0, None),
        "random_sources": ([str(fixtures.binary("random_sources"))], 0, None),
        "virtual_pid": ([str(fixtures.binary("pid_probe"))], 0, None),
        "scheduler_policy_queries": (
            [str(fixtures.binary("scheduler_policy_queries"))],
            0,
            b"scheduler-policy-queries-ok\n",
        ),
        "signal_disposition": (
            [str(fixtures.binary("signal_disposition"))],
            0,
            b"signal-disposition-ok\n",
        ),
        "sigaction_state": (
            [str(fixtures.binary("sigaction_state"))],
            0,
            b"sigaction ok=5\n",
        ),
        "sigprocmask_state": (
            [str(fixtures.binary("sigprocmask_state"))],
            0,
            b"sigprocmask ok=5\n",
        ),
        "sigaltstack_state": (
            [str(fixtures.binary("sigaltstack_state"))],
            0,
            b"sigaltstack ok=4\n",
        ),
    }


# New cases are green contracts by default.  Only stable, diagnosed exceptions
# belong here; live pass/fail evidence is written to the outer scorecard.
L1_GAPS = {
    ("dbi", "file_metadata"): (
        "PR #1549 determinizes credential queries (getuid/getgid/getresuid/"
        "getresgid) to virtual-root identity 0; DBI forwards fchown(fd,0,0) to "
        "the real kernel with no CLONE_NEWUSER uid map, so the guest performs an "
        "unprivileged chown-to-root and gets EPERM, whereas ptrace remaps it "
        "through the user namespace. fchown is not correctly implemented under "
        "DBI, and an assertion against a half-implemented syscall could pass by "
        "accident and prove nothing; declared a gap until DBI determinizes "
        "fchown (see the determinize_fchown_under_dbi TODO)"
    ),
    ("dbi", "pthread_lifecycle"): (
        "Portable release DynamoRIO can stall or exit during native pthread "
        "startup before Detcore readiness"
    ),
    ("kvm", "process_wait_lifecycle"): (
        "KVM records serialized child exits and implements wait4/waitid, but "
        "does not synthesize guest SIGCHLD handler delivery"
    ),
}
L2_GAPS = {
    ("dbi", "file_metadata"): (
        "Inherited from the L1 DBI file_metadata gap: the fchown EPERM aborts "
        "the guest before any --verify double-run, so no L2 determinism witness "
        "can be produced"
    ),
    ("dbi", "exit_status"): (
        "hermit --verify runs the DBI guest only once when the first run exits "
        "non-zero (--verify-allow both), so the double-run DETLOG comparison "
        "never executes for this non-zero-exit contract"
    ),
    ("dbi", "pthread_lifecycle"): ("DynamoRIO startup stall prevents an L2 verify run"),
    ("kvm", "process_wait_accounting"): (
        "under --verify the concurrent double-run races child reaping: waitid "
        "on the already-reaped child returns ECHILD"
    ),
    ("kvm", "process_wait_lifecycle"): (
        "no guest SIGCHLD frame synthesis, so there is no L2 run to verify"
    ),
}


def validate_catalog() -> list[str]:
    cases = case_catalog(CatalogFixtures())
    if not cases:
        raise MatrixError("backend-parity case catalog is empty")
    for gaps in (L1_GAPS, L2_GAPS):
        for (backend, name), reason in gaps.items():
            if backend not in BACKENDS or backend == "ptrace":
                raise MatrixError(f"invalid known-gap backend: {backend!r}")
            if name not in cases:
                raise MatrixError(f"known gap has no case implementation: {name!r}")
            if not reason:
                raise MatrixError(f"{name}/{backend}: known gap needs a reason")
    for backend, name in L1_GAPS:
        if (backend, name) not in L2_GAPS:
            raise MatrixError(f"{name}/{backend}: an L1 gap must also be an L2 gap")
    return list(cases)


def expectation(backend: str, name: str, verify: bool) -> tuple[str, str]:
    gaps = L2_GAPS if verify else L1_GAPS
    reason = gaps.get((backend, name))
    if reason is not None:
        return "gap", reason
    if not verify:
        return "pass", "-"
    return ("guest" if backend == "kvm" else "detlog"), "-"


def case_command(name: str, fixtures: Fixtures) -> tuple[list[str], int, bytes | None]:
    cases = case_catalog(fixtures)
    try:
        return cases[name]
    except KeyError as error:
        raise MatrixError(f"case catalog has no implementation for {name}") from error


def backend_block(backend: str, hermit: Path, strict: bool) -> str | None:
    if backend == "dbi":
        smoke_command = [str(hermit), "run", "--backend", "dbi"]
        if strict:
            smoke_command.append("--strict")
        smoke_command.extend(["--", "/bin/true"])
        try:
            smoke = subprocess.run(
                smoke_command,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=30,
                check=False,
            )
        except subprocess.TimeoutExpired:
            return "DBI smoke timed out"
        if smoke.returncode != 0:
            diagnostic = smoke.stderr.decode(errors="replace").strip()
            return f"DBI smoke exited {smoke.returncode}: {diagnostic[-300:]}"
    elif backend == "kvm":
        kvm = Path("/dev/kvm")
        if not kvm.exists() or not os.access(kvm, os.R_OK | os.W_OK):
            return "/dev/kvm is not readable and writable"
    return None


def hermit_command(
    hermit: Path,
    backend: str,
    guest: list[str],
    name: str,
    strict: bool,
    verify: bool = False,
) -> list[str]:
    command = [str(hermit), "run"]
    if backend != "ptrace":
        command.extend(["--backend", backend])
    if strict:
        command.append("--strict")
    if verify:
        # L2: hermit runs the guest twice internally and asserts a
        # bitwise-identical DETLOG. `--verify-allow both` keeps the guest's own
        # exit status (including deliberate non-zero cases such as exit_status)
        # flowing through so the runner can still enforce exit-status parity.
        command.extend(["--verify", "--verify-allow", "both"])
    command.extend(
        [
            "--base-env=minimal",
            "--max-timeslice=disabled",
            "--tmp=/tmp",
        ]
    )
    if backend == "ptrace" and name != "cpuid_policy":
        command.append("--no-virtualize-cpuid")
    command.extend(["--", *guest])
    return command


def run_with_timeout(command: list[str]) -> subprocess.CompletedProcess[bytes] | None:
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=30)
    except subprocess.TimeoutExpired:
        print(f"timed-out command: {command!r}", file=sys.stderr)
        for proc in sorted(
            Path("/proc").glob("[0-9]*"), key=lambda path: int(path.name)
        ):
            try:
                stat = (proc / "stat").read_text(encoding="utf-8").split()
                if int(stat[4]) != process.pid:
                    continue
                command_line = (
                    (proc / "cmdline")
                    .read_bytes()
                    .replace(b"\0", b" ")
                    .decode(errors="replace")
                )
                wait_channel = (proc / "wchan").read_text(encoding="utf-8").strip()
                print(
                    f"timed-out process: pid={proc.name} state={stat[2]} "
                    f"wchan={wait_channel} command={command_line}",
                    file=sys.stderr,
                )
                for task in sorted((proc / "task").glob("[0-9]*")):
                    try:
                        task_stat = (task / "stat").read_text(encoding="utf-8").split()
                        task_wait = (task / "wchan").read_text(encoding="utf-8").strip()
                        task_syscall = (
                            (task / "syscall").read_text(encoding="utf-8").strip()
                        )
                        print(
                            f"timed-out thread: tid={task.name} state={task_stat[2]} "
                            f"wchan={task_wait} syscall={task_syscall}",
                            file=sys.stderr,
                        )
                    except (FileNotFoundError, PermissionError, ProcessLookupError):
                        continue
            except (FileNotFoundError, PermissionError, ProcessLookupError, ValueError):
                continue
        try:
            cgroup_path = next(
                line.partition("::")[2]
                for line in Path("/proc/self/cgroup").read_text().splitlines()
                if line.startswith("0::")
            )
            cgroup_dir = Path("/sys/fs/cgroup") / cgroup_path.lstrip("/")
            for name in ("pids.current", "pids.max", "pids.events"):
                value = (cgroup_dir / name).read_text(encoding="utf-8").strip()
                print(f"timed-out cgroup: {name}={value}", file=sys.stderr)
        except (FileNotFoundError, PermissionError, StopIteration):
            pass
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
        if stdout:
            print("timed-out guest stdout:", file=sys.stderr)
            sys.stderr.buffer.write(stdout[-8192:])
        if stderr:
            print("timed-out hermit stderr:", file=sys.stderr)
            sys.stderr.buffer.write(stderr[-8192:])
        sys.stderr.flush()
        return None
    return subprocess.CompletedProcess(command, process.returncode, stdout, stderr)


def root_random_output(stdout: bytes) -> bytes:
    """Select output driven by the root thread's random stream."""
    return b"\n".join(
        line for line in stdout.splitlines() if not line.startswith(b"thread-")
    )


# Two distinct `--verify` success witnesses, and they are NOT the same assurance:
#
#  * DETLOG-bitwise (ptrace, DBI): hermit re-runs the guest and finds the two
#    DETLOG streams bitwise-identical after normalization. This is full L2 -- the
#    internal syscall/scheduling trace is itself reproducible.
#  * guest-visible (KVM): reverie-kvm runs concurrently and states outright that
#    "internal syscall trace order is not deterministic", so `--verify` compares
#    only guest stdout and exit status across the two runs. That is a strictly
#    weaker guest-visible L2; do not report it as DETLOG determinism.
#
# Recording which witness fired keeps the matrix honest about what each backend
# actually proves under --verify (no false parity).
VERIFY_WITNESS_DETLOG = b"Determinism verified"
VERIFY_WITNESS_GUEST_VISIBLE = b"guest output and exit status matched"


def run_case_verify(
    hermit: Path,
    backend: str,
    name: str,
    guest: list[str],
    expected_status: int,
    expected_l2: str,
) -> tuple[str, str, float]:
    """L2 probe: one `hermit run --strict --verify` invocation.

    `--verify` runs the guest twice inside hermit and diverts the guest's own
    stdout into per-run temp logs, so this path cannot compare guest stdout the
    way the L1 path does. The L2 contract it enforces instead is: the guest exit
    status matches, and hermit's internal double-run comparison reports success
    at *at least* the assurance kind the matrix records (`expected_l2`). A run
    that only reaches guest-visible L2 fails a `detlog` contract; DETLOG L2
    satisfies a `guest` contract because it is strictly stronger.
    """
    started = time.monotonic()
    command = hermit_command(hermit, backend, guest, name, strict=True, verify=True)
    result = run_with_timeout(command)
    if result is None:
        return "FAIL", "verify run timed out", time.monotonic() - started
    diagnostic = result.stderr.decode(errors="replace").strip()
    if result.returncode != expected_status:
        if (
            backend == "ptrace"
            and name == "cpuid_policy"
            and (
                "continuing without CPUID interception" in diagnostic
                or "CPUID faulting is unavailable" in diagnostic
            )
        ):
            return (
                "BLOCKED",
                "host kernel/CPU lacks CPUID faulting",
                time.monotonic() - started,
            )
        return (
            "FAIL",
            f"verify exited {result.returncode}, expected {expected_status}: "
            f"{diagnostic[-300:]}",
            time.monotonic() - started,
        )
    if VERIFY_WITNESS_DETLOG in result.stderr:
        observed = "detlog"
    elif VERIFY_WITNESS_GUEST_VISIBLE in result.stderr:
        observed = "guest"
    else:
        return (
            "FAIL",
            f"verify produced no determinism witness: {diagnostic[-300:]}",
            time.monotonic() - started,
        )
    # A gap being probed (--probe-gaps) has no positive contract to meet; report
    # what it actually reached so it can be evaluated for promotion.
    if expected_l2 != "gap" and L2_RANK[observed] < L2_RANK[expected_l2]:
        return (
            "FAIL",
            f"reached L2 {observed} but contract requires {expected_l2}",
            time.monotonic() - started,
        )
    label = {
        "detlog": "L2 DETLOG-bitwise: --verify double-run matched",
        "guest": "L2 guest-visible: output+exit matched (internal trace nondeterministic)",
    }[observed]
    return "PASS", label, time.monotonic() - started


def run_case(
    hermit: Path,
    backend: str,
    name: str,
    fixtures: Fixtures,
    strict: bool,
    verify: bool = False,
    expected_l2: str = "gap",
) -> tuple[str, str, float]:
    guest, expected_status, expected_stdout = case_command(name, fixtures)
    if backend == "dbi" and name == "random_sources":
        guest = [*guest, "--root-only"]
    if backend == "kvm" and name == "memory_advice":
        guest = [*guest, "--kvm"]
    if verify:
        return run_case_verify(
            hermit, backend, name, guest, expected_status, expected_l2
        )
    baseline: bytes | None = None
    started = time.monotonic()
    ptrace_random: bytes | None = None
    if backend == "dbi" and name == "random_sources":
        reference = run_with_timeout(
            hermit_command(hermit, "ptrace", guest, name, strict)
        )
        if reference is None:
            return "FAIL", "ptrace reference timed out", time.monotonic() - started
        if reference.returncode != expected_status:
            diagnostic = reference.stderr.decode(errors="replace").strip()
            return (
                "FAIL",
                f"ptrace reference exited {reference.returncode}: {diagnostic[-300:]}",
                time.monotonic() - started,
            )
        ptrace_random = root_random_output(reference.stdout)
    for iteration in range(RUNS):
        command = hermit_command(hermit, backend, guest, name, strict)
        result = run_with_timeout(command)
        if result is None:
            return "FAIL", f"run {iteration + 1} timed out", time.monotonic() - started

        if result.returncode != expected_status:
            diagnostic = result.stderr.decode(errors="replace").strip()
            if (
                backend == "ptrace"
                and name == "cpuid_policy"
                and (
                    "continuing without CPUID interception" in diagnostic
                    or "CPUID faulting is unavailable" in diagnostic
                )
            ):
                return (
                    "BLOCKED",
                    "host kernel/CPU lacks CPUID faulting",
                    time.monotonic() - started,
                )
            return (
                "FAIL",
                f"run {iteration + 1} exited {result.returncode}, expected "
                f"{expected_status}: {diagnostic[-300:]}",
                time.monotonic() - started,
            )
        if expected_stdout is not None and result.stdout != expected_stdout:
            return (
                "FAIL",
                f"run {iteration + 1} stdout={result.stdout!r}, expected={expected_stdout!r}",
                time.monotonic() - started,
            )
        if expected_stdout is None:
            required_markers = {
                "virtual_clock": b"clock matrix success\n",
                "heap_growth": b"heap ",
                "anonymous_mmap_layout": b"multiple ",
                "shared_anonymous_mmap": b"shared ",
                "random_sources": b"getrandom[0]=",
                "virtual_pid": b"pid=",
            }
            marker = required_markers[name]
            if marker not in result.stdout:
                return (
                    "FAIL",
                    f"run {iteration + 1} omitted marker {marker!r}",
                    time.monotonic() - started,
                )
            if baseline is None:
                baseline = result.stdout
            elif result.stdout != baseline:
                return (
                    "FAIL",
                    f"run {iteration + 1} output differed from run 1",
                    time.monotonic() - started,
                )
            if (
                ptrace_random is not None
                and root_random_output(result.stdout) != ptrace_random
            ):
                return (
                    "FAIL",
                    f"run {iteration + 1} root random stream differed from ptrace",
                    time.monotonic() - started,
                )
    return "PASS", f"{RUNS}/{RUNS} runs matched", time.monotonic() - started


def write_results(path: Path, results: list[dict[str, str]]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(
            output,
            fieldnames=(
                "test_name",
                "backend",
                "expectation",
                "result",
                "seconds",
                "detail",
            ),
            delimiter="\t",
        )
        writer.writeheader()
        writer.writerows(results)


def discover_parent_scorecard() -> Path | None:
    configured = os.environ.get("DEV_HERMIT_ROOT") or os.environ.get("DEV_HERMIT")
    roots = [Path(configured)] if configured else []
    roots.extend((REPOSITORY, *REPOSITORY.parents))
    for root in roots:
        compat_dir = root / "compat-envelope"
        if compat_dir.is_dir():
            return compat_dir / "scorecard.csv"
    return None


def git_output(*args: str) -> str | None:
    result = subprocess.run(
        ["git", "-C", str(REPOSITORY), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return None
    return result.stdout.strip()


def append_parent_scorecard(
    path: Path,
    results: list[dict[str, str]],
    *,
    strict: bool,
    verify: bool,
    probe_gaps: bool,
) -> None:
    # Multiple worktrees can validate concurrently against one outer workspace.
    # Serialize whole-row appends so the shared measurement log remains valid.
    import fcntl

    path.parent.mkdir(parents=True, exist_ok=True)
    hermit_sha = git_output("rev-parse", "HEAD") or "unknown"
    dirty = bool(git_output("status", "--porcelain"))
    epoch = int(time.time())
    run_id = f"backend-parity-{hermit_sha[:12]}-{epoch}-{os.getpid()}"
    mode = "verify" if verify else "strict" if strict else "repeat"
    rows: list[dict[str, str]] = []
    for result in results:
        status = result["result"]
        passed = status in {"PASS", "XPASS"}
        outcome = {
            "PASS": "pass",
            "XPASS": "pass",
            "FAIL": "fail",
            "GAP": "gap",
            "BLOCKED": "skip",
        }[status]
        parity = "1" if passed else "0" if status == "FAIL" else ""
        detail = result["detail"]
        if verify and result["backend"] == "kvm" and passed:
            detail = (
                "L2 guest-visible only (stdout+exit compared; internal trace not "
                f"compared): {detail}"
            )
        rows.append(
            {
                "run_id": run_id,
                "run_utc": f"@{epoch}",
                "hermit_sha": hermit_sha,
                "reverie_sha": "unknown",
                "dirty": str(dirty).lower(),
                "run_mode": "expansion" if probe_gaps else "regression",
                "lane": "privileged" if result["backend"] == "kvm" else "portable",
                "bucket": "backend-parity",
                "test_id": f"backend-parity/{result['test_name']}",
                "test_mode": mode,
                "backend": result["backend"],
                "cell_state": (
                    "disabled" if result["expectation"] == "gap" else "enabled"
                ),
                "outcome": outcome,
                "deterministic": "1" if passed and strict else "",
                "parity": parity,
                "output_hash": "",
                "duration_ms": str(round(float(result["seconds"]) * 1000)),
                "max_rss_kb": "",
                "reason": detail,
            }
        )

    with path.open("a+", newline="", encoding="utf-8") as scorecard:
        fcntl.flock(scorecard.fileno(), fcntl.LOCK_EX)
        scorecard.seek(0)
        first_line = scorecard.readline()
        if first_line:
            actual_header = next(csv.reader([first_line]))
            if tuple(actual_header) != SCORECARD_HEADER:
                raise MatrixError(f"outer scorecard {path} has an incompatible header")
        else:
            writer = csv.DictWriter(
                scorecard, fieldnames=SCORECARD_HEADER, lineterminator="\n"
            )
            writer.writeheader()
        scorecard.seek(0, os.SEEK_END)
        writer = csv.DictWriter(
            scorecard, fieldnames=SCORECARD_HEADER, lineterminator="\n"
        )
        writer.writerows(rows)
        scorecard.flush()
        fcntl.flock(scorecard.fileno(), fcntl.LOCK_UN)
    print(f"TRACKING: appended {len(rows)} rows to outer scorecard {path}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--backend",
        action="append",
        choices=BACKENDS,
        dest="backends",
        help="backend to run (repeatable; default: all)",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate the case catalog and print expected rates without running guests",
    )
    parser.add_argument(
        "--hermit",
        type=Path,
        default=REPOSITORY / "target/debug/hermit",
        help="Hermit executable",
    )
    parser.add_argument("--output", type=Path, help="write observed result TSV")
    parser.add_argument(
        "--parent-scorecard",
        type=Path,
        help=(
            "append observations to this outer dev-hermit scorecard (default: "
            "auto-detect compat-envelope/scorecard.csv)"
        ),
    )
    parser.add_argument(
        "--no-parent-scorecard",
        action="store_true",
        help="disable the outer dev-hermit scorecard side effect",
    )
    parser.add_argument(
        "--probe-gaps",
        action="store_true",
        help="run documented gaps and report XPASS candidates",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="fail instead of reporting BLOCKED when a selected backend is unavailable",
    )
    parser.add_argument(
        "--strict",
        action="store_true",
        help="run every guest with hermit run --strict",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help=(
            "lift every probe to L2: run with hermit run --strict --verify so "
            "hermit's internal double-run asserts a bitwise-identical DETLOG "
            "(implies --strict; guest stdout is diverted, so stdout parity is "
            "not checked in this mode)"
        ),
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    names = validate_catalog()
    backends = args.backends or list(BACKENDS)
    # --verify is the L2 lift and presupposes strict mode (L2 = --strict
    # --verify); enable strict implicitly so callers can ask for L2 with one flag.
    strict = args.strict or args.verify
    if args.verify:
        print("MODE: L2 (--strict --verify), byte-identical DETLOG per probe")
    elif strict:
        print("MODE: L1 (--strict), byte-identical stdout across 3 runs")
    else:
        print("MODE: compatibility (repeat-run), byte-identical stdout across 3 runs")
    baseline = len(names)
    for backend in BACKENDS:
        passing = baseline - sum(gap_backend == backend for gap_backend, _ in L1_GAPS)
        print(f"RATCHET {backend}: {passing}/{baseline} ({passing / baseline:.1%})")
    # L2 ratchet: how many contracts each backend verifies under --verify, split
    # by assurance kind so DETLOG-bitwise L2 is never conflated with guest-visible.
    for backend in BACKENDS:
        verified = baseline - sum(gap_backend == backend for gap_backend, _ in L2_GAPS)
        detlog = verified if backend != "kvm" else 0
        guest = verified if backend == "kvm" else 0
        print(
            f"RATCHET-L2 {backend}: {verified}/{baseline} "
            f"({verified / baseline:.1%}) [detlog={detlog} guest-visible={guest}]"
        )
    if args.check:
        return 0

    hermit = args.hermit.resolve()
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise MatrixError(f"Hermit executable is unavailable: {hermit}")

    results: list[dict[str, str]] = []
    failures = 0
    with tempfile.TemporaryDirectory(prefix="hermit-backend-parity-") as tempdir:
        fixtures = Fixtures(Path(tempdir))
        for backend in backends:
            block = backend_block(backend, hermit, strict)
            if block:
                print(f"BLOCKED {backend}: {block}")
                if args.require_backend:
                    failures += 1
                continue

            for name in names:
                expected, gap_reason = expectation(backend, name, args.verify)
                is_gap = expected == "gap"
                if is_gap and not args.probe_gaps:
                    print(f"GAP {backend}/{name}: {gap_reason}")
                    results.append(
                        {
                            "test_name": name,
                            "backend": backend,
                            "expectation": expected,
                            "result": "GAP",
                            "seconds": "0.000",
                            "detail": gap_reason,
                        }
                    )
                    continue

                status, detail, duration = run_case(
                    hermit, backend, name, fixtures, strict, args.verify, expected
                )
                if is_gap and status == "PASS":
                    status = "XPASS"
                    detail = f"candidate for promotion from gap: {detail}"
                print(f"{status} {backend}/{name}: {detail}")
                results.append(
                    {
                        "test_name": name,
                        "backend": backend,
                        "expectation": expected,
                        "result": status,
                        "seconds": f"{duration:.3f}",
                        "detail": detail,
                    }
                )
                if not is_gap and status == "FAIL":
                    failures += 1

    if args.output:
        write_results(args.output, results)
    if args.parent_scorecard and args.no_parent_scorecard:
        raise MatrixError(
            "--parent-scorecard and --no-parent-scorecard cannot be used together"
        )
    if not args.no_parent_scorecard and results:
        parent_scorecard = args.parent_scorecard or discover_parent_scorecard()
        if parent_scorecard is None:
            print(
                "TRACKING: outer dev-hermit scorecard not found; "
                "use --parent-scorecard to select one"
            )
        else:
            append_parent_scorecard(
                parent_scorecard,
                results,
                strict=strict,
                verify=args.verify,
                probe_gaps=args.probe_gaps,
            )
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except MatrixError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
