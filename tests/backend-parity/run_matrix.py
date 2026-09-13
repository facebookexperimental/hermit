#!/usr/bin/env python3
"""Run and ratchet Hermit's cross-backend compatibility matrix."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import os
import shlex
import signal
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
from typing import NamedTuple


SCRIPT_DIR = Path(__file__).resolve().parent
REPOSITORY = SCRIPT_DIR.parent.parent
VERIFICATION_REPORT_BIN = Path(
    os.environ.get(
        "VERIFICATION_REPORT_BIN", REPOSITORY / "target/debug/verification-report"
    )
)
BACKENDS = ("ptrace", "dbt", "kvm")
RUNS = 3

DBT_PRIVATE_TMP_SCRIPT = """\
private_tmp=$1
mount_count=$2
shift 2
mount --make-rprivate /
while [ "$mount_count" -gt 0 ]; do
    mount --bind "$1" "$2"
    shift 2
    mount_count=$((mount_count - 1))
done
mount --rbind "$private_tmp" /tmp
cd /tmp
export TMPDIR=/tmp
exec "$@"
"""

# The compatibility scorecard is measurement state, not Hermit source.  When
# this checkout is nested in dev-hermit, live observations are written to one
# ignored per-run file under compat-envelope/ignored/backend-parity/.  The
# tracked compat-envelope/scorecard.csv is advanced only by the parent
# publisher, publish-scorecard.py, so measuring never mutates it.  Standalone
# Hermit clones simply skip that side effect unless --parent-scorecard is
# supplied.  Artifact routing never selects or disables a comparison contract.
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
    "stdout_parity",
    "output_hash",
    "duration_ms",
    "max_rss_kb",
    "reason",
    "ref_output_hash",
    "parity_comparator",
    "parity_tier",
    "comparison_tier",
    "stack_parity",
    "heap_parity",
    "verify_compare",
    "bitwise_parity",
    "compared_log_messages",
    "tier",
)

# Accepted spellings for the stdout-parity column, in preference order.
#
# The parent renderer already accepts both while `parity` -> `stdout_parity` is
# in flight.  Accepting both here too is the point of this whole mechanism: the
# rename must not be able to break this gate the way `verify_compare` did.
PARITY_COLUMNS = ("stdout_parity", "parity")

# The columns this producer actually fills.  Everything else the file carries is
# written blank.
#
# WHY THIS IS SEPARATE FROM SCORECARD_HEADER, and it is the whole bug: the outer
# scorecard's schema is owned by the PARENT workspace, not by Hermit.  The parent
# added `verify_compare` (dev-hermit commit 7080d68) and every Hermit validate
# that reached test.dbt_parity then died on an exact-tuple header comparison --
# with no Hermit-side change, and AFTER running the full matrix, so the failure
# named a header while every parity cell had actually passed.  A consumer that
# demands schema equality makes any producer-side column addition a fleet
# outage.  So bind to the columns we WRITE and let the file carry extras.
# Columns that describe WHAT COMPARISON a row's verdict rests on.  They are
# filled when the file carries them and skipped when it does not.
#
# They are deliberately OPTIONAL rather than produced-and-required.  Requiring
# them would refuse today's 20-column parent scorecard outright -- exactly the
# fleet outage the mechanism above exists to prevent, just with a newer column
# name.  A producer may add evidence to a file; it may not demand that the file
# already know about it.
EVIDENCE_COLUMNS = (
    "verify_compare",
    "bitwise_parity",
    "compared_log_messages",
    "tier",
)

# The stdout comparison's condition columns.  New scorecards carry them, while
# a legacy parent scorecard may not.  They are optional at the file boundary so
# a parent-side schema addition cannot break Hermit validation; when either
# operand column is absent, append_parent_scorecard withholds the parity boolean
# too.  A result without both operands is UNMEASURED, never an inferred pass.
STDOUT_EVIDENCE_COLUMNS = (
    "ref_output_hash",
    "parity_comparator",
    "parity_tier",
)

COMPARISON_TIER_COLUMN = "comparison_tier"
COMPARISON_TIER_STDOUT_ONLY = "unqualified-stdout-only"
COMPARISON_TIER_SELF_VERIFY_ONLY = "unqualified-self-verify-only"
COMPARISON_TIER_NO_COMPARISON = "unqualified-no-comparison"
OPTIONAL_SCORECARD_COLUMNS = (
    *STDOUT_EVIDENCE_COLUMNS,
    COMPARISON_TIER_COLUMN,
    "stack_parity",
    "heap_parity",
)

# Comparators whose match may be read as bitwise identity. An allowlist, not a
# not-equal-to-"stripped" test: an unrecognised comparator name is an unknown
# policy, and an unknown policy cannot license the strongest claim in the ladder.
BITWISE_CAPABLE_COMPARATORS = ("canonical",)

PRODUCED_COLUMNS = tuple(
    c
    for c in SCORECARD_HEADER
    if c not in EVIDENCE_COLUMNS
    and c not in OPTIONAL_SCORECARD_COLUMNS
    and c not in PARITY_COLUMNS
)


def scorecard_fieldnames(actual_header, path):
    """Bind the writer to the FILE's schema, refusing only a column we must write.

    Returns ``(fieldnames, parity_column)``.  ``fieldnames`` is the file's own
    header, so rows are written at the file's width and order; a column the file
    has and this producer does not fill is written blank rather than short-
    writing the row.  That last part matters more than the acceptance: simply
    relaxing the old equality check while still writing ``SCORECARD_HEADER``
    would append 19-field rows under a 20-column header and silently misalign
    every value after ``reason``.

    Fail-closed is preserved, and narrowed to what it should always have been:
    a column this producer writes must exist.  The refusal names the missing
    columns and carries the header's own size, so a reader can tell "schema
    skew" from "wrong file" without opening it (#319 -- a count travels with
    the thing it counted).
    """
    actual = tuple(actual_header)
    parity_column = next((c for c in PARITY_COLUMNS if c in actual), None)
    missing = [c for c in PRODUCED_COLUMNS if c not in actual]
    if parity_column is None:
        missing.append(" or ".join(PARITY_COLUMNS))
    if missing:
        raise MatrixError(
            f"outer scorecard {path} is missing {len(missing)} column(s) this "
            f"producer writes: {', '.join(missing)}; its header has "
            f"{len(actual)} column(s): {','.join(actual)}"
        )
    return actual, parity_column

# `--verify` evidence kinds, ordered weakest to strongest. "gap" means the
# contract cannot currently be verified on that backend. "guest" means the two
# runs produced identical stdout+exit but the internal trace is not compared
# (KVM concurrent mode). "bitwise" is full L2: the two runs produced matching
# INFO streams under the canonical BitwiseInfoV1 policy (ptrace, DBT).
#
# `stripped` is the rung that was missing, and its absence is what made every
# green over-tiered: plain `--verify` DOES compare the DETLOG, but under the
# `Stripped` policy, whose own `--verify-json` reports `bitwise_parity: false`.
# Calling that `detlog` conflated "the DETLOG was compared" with "the DETLOG was
# identical".  `bitwise` is the real thing and is claimable only from a typed
# verdict (see `verify_tier_from_json`).
L2_RANK = {"gap": 0, "guest": 1, "stripped": 2, "bitwise": 3}
# Per-backend L2 values the matrix may record. KVM's concurrent verify path can
# never emit a DETLOG witness, so it is capped at guest-visible L2.
L2_ALLOWED = {
    "ptrace": {"stripped", "bitwise"},
    "dbt": {"stripped", "bitwise", "gap"},
    "kvm": {"guest", "gap"},
}


class VerifyPolicy(NamedTuple):
    """The one verification policy this matrix actually requests."""

    hermit_flags: tuple[str, ...]
    expected_non_kvm_tier: str
    comparison_claim: str

    @classmethod
    def checked(
        cls,
        hermit_flags: tuple[str, ...],
        expected_non_kvm_tier: str,
        comparison_claim: str,
    ) -> VerifyPolicy:
        if "--verify" not in hermit_flags:
            raise ValueError("a verification policy must request --verify")
        if not any(
            hermit_flags[index : index + 2] == ("--verify-allow", "both")
            for index in range(len(hermit_flags) - 1)
        ):
            raise ValueError(
                "a verification policy must preserve guest exit-status handling"
            )
        if expected_non_kvm_tier not in {"stripped", "bitwise"}:
            raise ValueError(
                "a non-KVM verification policy must expect stripped or bitwise evidence"
            )
        requests_canonical = "--verify-strict" in hermit_flags
        expects_bitwise = expected_non_kvm_tier == "bitwise"
        if requests_canonical != expects_bitwise:
            raise ValueError(
                "--verify-strict and the bitwise evidence tier must move together"
            )
        return cls(hermit_flags, expected_non_kvm_tier, comparison_claim)

    def displayed_flags(self) -> tuple[str, ...]:
        return ("--strict", *self.hermit_flags)

    def assurance_label(self) -> str:
        return "L2" if self.expected_non_kvm_tier == "bitwise" else "below L2"

    def mode_summary(self) -> str:
        return (
            f"MODE: verification ({shlex.join(self.displayed_flags())}); "
            f"requested policy is {self.assurance_label()}: "
            f"{self.comparison_claim}"
        )


DEFAULT_VERIFY_POLICY = VerifyPolicy.checked(
    hermit_flags=("--verify", "--verify-allow", "both"),
    expected_non_kvm_tier="stripped",
    comparison_claim=(
        "Stripped DETLOG comparison "
        "(numbers/addresses/paths normalized; NOT bitwise)"
    ),
)


class MatrixError(Exception):
    """An invalid case catalog or failed regression contract."""


def tmp_destination(path: Path, host_tmp: Path) -> tuple[Path, Path] | None:
    """Map one normalized host /tmp path beneath a command-local /tmp root."""
    normalized = Path(os.path.normpath(path))
    if not normalized.is_absolute():
        return None
    try:
        relative = normalized.relative_to("/tmp")
    except ValueError:
        return None
    host_tmp = host_tmp.resolve(strict=True)
    destination = host_tmp / relative
    resolved_destination = destination.resolve(strict=False)
    try:
        resolved_destination.relative_to(host_tmp)
    except ValueError as error:
        raise MatrixError(
            f"refusing to stage {normalized} outside {host_tmp}: "
            f"destination resolves to {resolved_destination}"
        ) from error
    return normalized, destination


def command_in_private_tmp(
    command: list[str], host_tmp: Path, preserve: tuple[Path, ...] = ()
) -> list[str]:
    """Run one command with its host directory mounted over /tmp."""
    mounts: list[str] = []
    host_tmp = Path(os.path.abspath(host_tmp))
    for requested in preserve:
        mapping = tmp_destination(requested, host_tmp)
        if mapping is None:
            continue
        source, destination = mapping
        if not source.exists():
            raise MatrixError(f"cannot preserve missing path beneath /tmp: {source}")
        resolved_source = source.resolve(strict=True)
        if source.is_dir() and host_tmp.is_relative_to(resolved_source):
            raise MatrixError(
                f"cannot replace /tmp while preserving {source}: "
                f"it contains the command directory {host_tmp}"
            )
        if source.is_dir():
            destination.mkdir(parents=True, exist_ok=True)
        elif source.is_file():
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.touch()
        else:
            raise MatrixError(f"cannot preserve unsupported path beneath /tmp: {source}")
        mounts.extend((str(resolved_source), str(destination)))
    return [
        "unshare",
        "--user",
        "--map-root-user",
        "--mount",
        "sh",
        "-ceu",
        DBT_PRIVATE_TMP_SCRIPT,
        "hermit-dbt-private-tmp",
        str(host_tmp),
        str(len(mounts) // 2),
        *mounts,
        *command,
    ]


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
        self._host_tmp_sequence = 0

    def host_tmp(self, backend: str, name: str) -> Path:
        self._host_tmp_sequence += 1
        path = (
            self.root
            / "host-tmp"
            / f"{backend}-{name}-{self._host_tmp_sequence}"
        )
        path.mkdir(parents=True)
        return path

    def expose_tmp_paths(
        self, backend: str, guest: list[str], host_tmp: Path
    ) -> list[str]:
        """Keep absolute /tmp file arguments visible to the selected backend."""
        for argument in guest:
            mapping = tmp_destination(Path(argument), host_tmp)
            if mapping is None:
                continue
            source, destination = mapping
            if not source.is_file():
                continue
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, destination)
        return [*guest]

    def binary(self, name: str) -> Path:
        if name in self._binaries:
            return self._binaries[name]

        local = SCRIPT_DIR / "fixtures"
        sources: dict[str, tuple[Path, tuple[str, ...]]] = {
            "pthread_lifecycle": (local / "pthread_lifecycle.c", ("-pthread",)),
            "process_wait_lifecycle": (
                REPOSITORY / "tests/c/dbt_wait_lifecycle.c",
                ("-D_GNU_SOURCE",),
            ),
            "mmap_exec": (REPOSITORY / "tests/c/dbt_mmap_exec.c", ()),
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
    def __init__(self) -> None:
        self._host_tmp_sequence = 0
        self.exposed_tmp_paths: list[tuple[str, tuple[str, ...], Path]] = []

    def host_tmp(self, backend: str, name: str) -> Path:
        self._host_tmp_sequence += 1
        return (
            Path("/backend-parity-catalog")
            / "host-tmp"
            / f"{backend}-{name}-{self._host_tmp_sequence}"
        )

    def expose_tmp_paths(
        self, backend: str, guest: list[str], host_tmp: Path
    ) -> list[str]:
        self.exposed_tmp_paths.append((backend, tuple(guest), host_tmp))
        return [*guest]

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
            b"dbt-mmap-exec-ok\n",
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
    # ("dbt", "file_metadata") was retired by PR #1851
    # (https://github.com/rrnewton/hermit/pull/1851).  Its stated exit condition
    # was "declared a gap until DBT determinizes fchown"; that PR moves chown,
    # fchown, fchownat and lchown from PassThrough to Determinized in the shared
    # classification, so the unprivileged chown-to-root EPERM this cell existed
    # for no longer occurs.  Confirmed with this harness's own probe at that
    # head: `run_matrix.py --backend dbt --probe-gaps` reports
    # "XPASS dbt/file_metadata: candidate for promotion from gap: 3/3 runs
    # matched".  The L2 row for the same case is NOT retired; see L2_GAPS.
    ("dbt", "pthread_lifecycle"): (
        "Portable release DynamoRIO can stall or exit during native pthread "
        "startup before Detcore readiness"
    ),
    ("kvm", "process_wait_lifecycle"): (
        "KVM records serialized child exits and implements wait4/waitid, but "
        "does not synthesize guest SIGCHLD handler delivery"
    ),
}
L2_GAPS = {
    ("dbt", "file_metadata"): (
        "NOT inherited from the L1 gap any more -- that L1 gap was retired by "
        "PR #1851 (https://github.com/rrnewton/hermit/pull/1851) and the old "
        "reason here, 'the fchown EPERM aborts the guest before any --verify "
        "double-run', is no longer true: the guest now runs to completion and "
        "prints 'syscall-file-metadata-ok count=20'.  The row still fails, for "
        "an unrelated and pre-existing reason, so the cell stays declared with "
        "its cause corrected.  Measured at that head: with the runner's exact "
        "flags the L2 double-run SUCCEEDS ('Success: deterministic. Determinism "
        "verified', DBT guest-memory hashes equal) -- EXCEPT that adding "
        "--verify-json makes it exit 1 with 'DBT canonical evidence contained "
        "no INFO records'.  That failure is not specific to this case or to "
        "chown: bisected to --verify-json alone, and reproduced on /bin/true, "
        "which issues no chown at all.  It affects every DBT case the runner "
        "probes under --verify, so the --verify dbt ratchet is currently "
        "measuring that defect rather than per-case determinism"
    ),
    ("dbt", "exit_status"): (
        "hermit --verify runs the DBT guest only once when the first run exits "
        "non-zero (--verify-allow both), so the double-run DETLOG comparison "
        "never executes for this non-zero-exit contract"
    ),
    ("dbt", "pthread_lifecycle"): ("DynamoRIO startup stall prevents an L2 verify run"),
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
    # `stripped`, not `bitwise`: this is the tier the probe's own comparator can
    # actually earn today.  Raising it to `bitwise` is a RATCHET that belongs
    # with the INFO-tier comparator work, not with this correction -- asserting
    # it now would red every ptrace/DBT cell for a comparator limitation rather
    # than a guest defect, which is the mirror image of the bug being fixed.
    return (
        "guest" if backend == "kvm" else DEFAULT_VERIFY_POLICY.expected_non_kvm_tier
    ), "-"


def case_command(name: str, fixtures: Fixtures) -> tuple[list[str], int, bytes | None]:
    cases = case_catalog(fixtures)
    try:
        return cases[name]
    except KeyError as error:
        raise MatrixError(f"case catalog has no implementation for {name}") from error


def backend_block(backend: str, hermit: Path, strict: bool) -> str | None:
    if backend == "dbt":
        smoke_command = [str(hermit), "run", "--backend", "dbt"]
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
            return "DBT smoke timed out"
        if smoke.returncode != 0:
            diagnostic = smoke.stderr.decode(errors="replace").strip()
            return f"DBT smoke exited {smoke.returncode}: {diagnostic[-300:]}"
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
    host_tmp: Path,
    verify: bool = False,
    verify_json: Path | None = None,
) -> list[str]:
    command = [str(hermit), "run"]
    if backend != "ptrace":
        command.extend(["--backend", backend])
    if strict:
        command.append("--strict")
    if verify:
        # hermit runs the guest twice internally and compares them.  `--verify`
        # ALONE is the `Stripped` comparison, NOT a bitwise one: it strips the
        # wall-clock prefix and applies
        # `unsafe-numeric-address-and-path-normalization/v1`, which normalises
        # numbers generally -- so a differing read() return length, a differing
        # pointer argument and a differing openat path all collapse to the same
        # token.  Mutation testing measured 3 of 5 planted defects surviving it
        # (dev-hermit experiments/strict-certification-mutation-sweep_20260806).
        # Whatever this run earns is read off `--verify-json` below; it is not
        # assumed from the flag and it is not scraped from the banner.
        #
        # `--verify-allow both` keeps the guest's own exit status (including
        # deliberate non-zero cases such as exit_status) flowing through so the
        # runner can still enforce exit-status parity.
        command.extend(DEFAULT_VERIFY_POLICY.hermit_flags)
        if verify_json is not None:
            command.append(f"--verify-json={verify_json}")
    command.extend(["--base-env=minimal", "--max-timeslice=disabled"])
    if backend == "dbt":
        # DBT does not enter Hermit's mount namespace. Put the whole launcher in
        # a rootless mount namespace whose /tmp is this command's directory, so
        # fixed /tmp names are isolated even when the guest ignores TMPDIR.
        command.append("--tmp=/tmp")
        command.append("--env=TMPDIR=/tmp")
    else:
        command.append(f"--tmp={host_tmp}")
    if backend == "ptrace" and name != "cpuid_policy":
        command.append("--no-virtualize-cpuid")
    command.extend(["--", *guest])
    if backend == "dbt":
        preserve = [hermit.parent]
        built_install = hermit.parent.parent / "install_pkg"
        if (built_install / "rsrcs").is_dir():
            preserve.append(built_install)
        if verify_json is not None:
            preserve.append(verify_json.parent)
        if install_dir := os.environ.get("HERMIT_INSTALL_DIR"):
            preserve.append(Path(install_dir))
        return command_in_private_tmp(command, host_tmp, tuple(preserve))
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


# Dynamic-output rows are cross-backend exact-output contracts only when they
# already name such a contract explicitly.  The fixed-output rows carry their
# contract in ``expected_stdout``; virtual_pid is the one marker-only row whose
# complete output is defined as a virtual identity, and DBT random_sources has
# a pre-existing ptrace root-stream comparison.  Memory-layout and clock rows
# deliberately remain repeatability-within-one-backend contracts: their raw
# addresses/timing deltas are not cross-backend byte oracles.
DYNAMIC_EXACT_STDOUT_PARITY_CASES = frozenset({"virtual_pid"})


def exact_stdout_parity_contract(
    backend: str, name: str, expected_stdout: bytes | None
) -> bool:
    return (
        expected_stdout is not None
        or name in DYNAMIC_EXACT_STDOUT_PARITY_CASES
        or (backend == "dbt" and name == "random_sources")
    )


def stdout_parity_evidence(
    candidate: bytes | None, reference: bytes | None
) -> dict[str, str]:
    """Return the exact operands and their derived stdout-parity verdict.

    ``None`` means that side was not measured.  Empty stdout is deliberately
    different: ``b""`` is a real operand with the well-known SHA-256 digest and
    can hold or differ just like any other byte stream.  The verdict is derived
    only when both operands exist, so this helper cannot manufacture a pass from
    a missing reference or from the enclosing test's PASS status.
    """
    evidence: dict[str, str] = {}
    if candidate is not None:
        evidence["output_hash"] = hashlib.sha256(candidate).hexdigest()
    if reference is not None:
        evidence["ref_output_hash"] = hashlib.sha256(reference).hexdigest()
    if candidate is not None and reference is not None:
        evidence["stdout_parity"] = (
            "1" if evidence["output_hash"] == evidence["ref_output_hash"] else "0"
        )
        evidence["parity_comparator"] = "stdout-sha256-exact-v1"
        evidence["parity_tier"] = "stdout-exact"
    return evidence


CPUID_BLOCK_REASON = "host kernel/CPU lacks CPUID faulting"
HOST_CAPABILITIES = frozenset(("cpuid-faulting", "kvm"))


def parse_host_capabilities(raw: bytes) -> dict[str, dict[str, object]]:
    try:
        report = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise MatrixError(f"host-capabilities record is not valid JSON: {error}") from error
    if not isinstance(report, dict) or report.get("schema") != 1:
        raise MatrixError("host-capabilities record must use schema 1")
    capabilities = report.get("host_capabilities")
    if not isinstance(capabilities, dict) or set(capabilities) != HOST_CAPABILITIES:
        raise MatrixError(
            "host-capabilities record must contain the complete closed set "
            f"{sorted(HOST_CAPABILITIES)!r}"
        )
    for name, verdict in capabilities.items():
        if (
            not isinstance(verdict, dict)
            or set(verdict) != {"present", "evidence"}
            or not isinstance(verdict.get("present"), bool)
            or not isinstance(verdict.get("evidence"), str)
            or not verdict["evidence"].strip()
        ):
            raise MatrixError(f"host-capabilities {name!r} verdict is malformed")
    return capabilities


def read_host_capabilities(hermit: Path) -> dict[str, dict[str, object]]:
    result = run_with_timeout([str(hermit), "host-capabilities", "--json"])
    if result is None:
        raise MatrixError("hermit host-capabilities probe timed out")
    if result.returncode != 0:
        detail = result.stderr.decode(errors="replace").strip()
        raise MatrixError(
            f"hermit host-capabilities probe failed with exit {result.returncode}: {detail}"
        )
    return parse_host_capabilities(result.stdout)


def cpuid_policy_is_blocked(
    backend: str,
    name: str,
    host_capabilities: dict[str, dict[str, object]],
) -> bool:
    return (
        backend == "ptrace"
        and name == "cpuid_policy"
        and host_capabilities["cpuid-faulting"]["present"] is False
    )


def capture_ptrace_reference(
    hermit: Path,
    guest: list[str],
    name: str,
    strict: bool,
    expected_status: int,
    expected_stdout: bytes | None,
    host_tmp: Path,
    host_capabilities: dict[str, dict[str, object]],
) -> tuple[bytes | None, str, bool]:
    """Capture the plain-run ptrace stdout used as the cross-backend reference."""
    reference = run_with_timeout(
        hermit_command(hermit, "ptrace", guest, name, strict, host_tmp)
    )
    if reference is None:
        return None, "ptrace reference timed out", False
    if reference.returncode != expected_status:
        diagnostic = reference.stderr.decode(errors="replace").strip()
        if cpuid_policy_is_blocked("ptrace", name, host_capabilities):
            return None, CPUID_BLOCK_REASON, True
        return (
            None,
            f"ptrace reference exited {reference.returncode}, expected "
            f"{expected_status}: {diagnostic[-300:]}",
            False,
        )
    if expected_stdout is not None and reference.stdout != expected_stdout:
        return (
            None,
            f"ptrace reference stdout={reference.stdout!r}, "
            f"expected={expected_stdout!r}",
            False,
        )
    return reference.stdout, "", False


# Two distinct `--verify` success witnesses, and they are NOT the same assurance:
#
#  * DETLOG-bitwise (ptrace, DBT): hermit re-runs the guest and finds the two
#    DETLOG streams bitwise-identical after normalization. This is full L2 -- the
#    internal syscall/scheduling trace is itself reproducible.
#  * guest-visible (KVM): reverie-kvm runs concurrently and states outright that
#    "internal syscall trace order is not deterministic", so `--verify` compares
#    only guest stdout and exit status across the two runs. That is a strictly
#    weaker guest-visible L2; do not report it as DETLOG determinism.
#
def verify_tier_from_json(path: Path) -> dict[str, str] | None:
    """Read the tier a `--verify` run actually earned from its typed verdict.

    This is the whole point of the correction.  The banner strings above are a
    PROXY: `":: Success: deterministic. Determinism verified."` is printed by a
    run whose own `--verify-json` says `bitwise_parity: false`, so scraping it
    cannot distinguish a stripped match from a bitwise one.  `--verify-json`
    carries the condition with the value -- strictness, the parity boolean, and
    the counts that make the boolean falsifiable -- so read that instead.

    `bitwise` requires a terminal `matched` verdict, `verified=true`,
    `bitwise_parity=true`, a CANONICAL log comparison, and equal positive integer
    counts on both sides.  The count is not redundant: an empty-vs-empty log
    comparison reports "no difference" under the strictest possible spec, so
    without it a run that produced no DETLOG at all would certify as bitwise
    parity.

    ⚠️ EVERY CONJUNCT IS LOAD-BEARING, and the weaker form this replaces
    (`bool(bitwise_parity) and bool(left) and bool(right)`) admitted records that
    contradict themselves.  Measured against that form:

      * `"bitwise_parity": "0"` certified BITWISE, because `bool("0")` is `True`
        in Python.  So did the string `"false"`.
      * `bitwise_parity: true` under `strictness: "stripped"` certified BITWISE
        -- the exact conflation of "the DETLOG was compared" with "the DETLOG was
        identical" that the tier ladder above exists to prevent.
      * `verdict: "diverged"` with `bitwise_parity: true` certified BITWISE.
      * counts of `-1|-1`, `"239"|"239"`, `true|true`, and `239|240` all
        certified BITWISE.

    `type(x) is int` rather than `isinstance`: `bool` is a subclass of `int`, so
    `{"left": true}` would otherwise read as a count.

    NO LIVE PRODUCER IS KNOWN FOR ANY OF THESE.  `verify.rs` declares
    `pub bitwise_parity: bool` and `#[serde(rename_all = "snake_case")]` on both
    `Verdict` and `LogCompareStrictness`, so Hermit's own `--verify-json` emits
    real booleans and the literals `matched`/`canonical`; the string and boolean
    count cases are unreachable from it, and the rest require a self-contradictory
    record.  This is a DEFENSIVE parser reading a file off disk -- per `AGENTS.md`
    an unreadable or truncated comparison must refuse rather than report a match --
    so it is hardened as defence in depth, not to fix an observed over-tiering.

    Tightening only ever moves a record DOWN the ladder, never up: anything that
    fails these conjuncts falls to `stripped` or `guest` by the branches below,
    which are the rungs it already belonged on.

    The producer-owned Rust type is the vocabulary authority. Python reads the
    evidence fields from `verification-report --json matched`, which parses the
    complete current shape and checks the closed `Verdict` enum. Its JSON is the
    same report that was checked, without a second read of a mutable file. A
    non-match never earns a positive tier; a typed infrastructure error retains
    its cause beside `gap` so the caller can report `ERROR`.

    Returns ``None`` for absent, malformed, contradictory or other non-match
    reports. A well-formed infrastructure error returns `gap`, never a match.
    """
    try:
        typed = subprocess.run(
            [str(VERIFICATION_REPORT_BIN), "--json", "matched", str(path)],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as error:
        print(
            f"run_matrix: REFUSED typed verification report {path}: {error}",
            file=sys.stderr,
        )
        return None
    if typed.returncode not in (0, 1):
        detail = typed.stderr.strip() or f"reader exited {typed.returncode}"
        print(
            f"run_matrix: REFUSED typed verification report {path}: {detail}",
            file=sys.stderr,
        )
        return None
    try:
        record = json.loads(typed.stdout)
    except ValueError:
        return None
    if not isinstance(record, dict):
        return None
    if typed.returncode != 0 and record.get("verdict") != "infrastructure_error":
        print(
            f"run_matrix: REFUSED typed verification report {path}: {typed.stderr.strip()}",
            file=sys.stderr,
        )
        return None
    infrastructure_error = ""
    if record.get("verdict") == "infrastructure_error":
        if record.get("verified") is not False or record.get("bitwise_parity") is not False:
            print(
                f"run_matrix: REFUSED infrastructure_error receipt {path}: "
                "verified and bitwise_parity must both be false",
                file=sys.stderr,
            )
            return None
        cause = record.get("infrastructure_error")
        if not isinstance(cause, dict):
            print(
                f"run_matrix: REFUSED infrastructure_error receipt {path}: missing cause",
                file=sys.stderr,
            )
            return None
        count = cause.get("count")
        if cause.get("kind") != "skid_overshoot" or type(count) is not int or count <= 0:
            print(
                f"run_matrix: REFUSED infrastructure_error receipt {path}: "
                f"invalid cause {cause!r}",
                file=sys.stderr,
            )
            return None
        infrastructure_error = (
            f"verification recorded {count} HERMIT_SKID_OVERSHOOT report(s)"
        )
    comparison = record.get("comparison") or {}
    counts = record.get("compared_log_messages") or {}
    left, right = counts.get("left"), counts.get("right")
    compared = f"{left}|{right}" if left is not None and right is not None else ""
    strictness = str(comparison.get("strictness") or "")
    positive_count = (
        type(left) is int
        and type(right) is int
        and left > 0
        and right > 0
        and left == right
    )
    bitwise = (
        record.get("verified") is True
        and record.get("verdict") == "matched"
        and record.get("bitwise_parity") is True
        and strictness in BITWISE_CAPABLE_COMPARATORS
        and comparison.get("compare_logs") is True
        and positive_count
    )
    # ⚠️ A REFUSAL NOBODY CAN SEE IS NOT A CHECK. Silently degrading the tier is
    # correct arithmetic and useless evidence: the operator reads `guest`, which
    # is also what an honest output-only run reports, and never learns that a
    # record CLAIMED parity and was rejected. Those two are the same row and
    # different facts.
    #
    # So a record that does NOT claim parity and is not bitwise stays silent --
    # that is the ordinary case and saying anything would be noise. A record that
    # DOES claim parity and is refused is named, with the conjunct that refused
    # it, because that record is either a producer defect or a contradiction and
    # somebody has to be told which.
    #
    # This is the shape of the defect it detects: the KVM comparator reported
    # `matched` while `compare_logs` was false -- a verdict disagreeing with its
    # own evidence. Degrading that to `guest` without comment would file a
    # self-contradictory record under the same label as a correct one.
    if record.get("bitwise_parity") and not bitwise:
        why = []
        if record.get("bitwise_parity") is not True:
            why.append(f"bitwise_parity is {record.get('bitwise_parity')!r}, not a JSON true")
        if record.get("verified") is not True:
            why.append(f"verified is {record.get('verified')!r}")
        if record.get("verdict") != "matched":
            why.append(f"verdict is {record.get('verdict')!r}, not 'matched'")
        if strictness != "canonical":
            why.append(f"comparator is {strictness or 'unset'!r}, not canonical")
        if comparison.get("compare_logs") is not True:
            why.append(
                f"compare_logs is {comparison.get('compare_logs')!r} -- parity is "
                "claimed over a log stream that was not compared"
            )
        if not positive_count:
            why.append(f"compared counts are {left!r}|{right!r}, not equal positive integers")
        print(
            f"run_matrix: REFUSED the bitwise tier for {path}: the record claims "
            f"bitwise_parity but {'; '.join(why)}.",
            file=sys.stderr,
        )

    if infrastructure_error:
        tier = "gap"
    elif bitwise:
        tier = "bitwise"
    elif comparison.get("compare_logs"):
        tier = "stripped"
    else:
        # Verified without comparing the log stream at all: stdout+exit only.
        tier = "guest"
    return {
        "tier": tier,
        "verify_compare": strictness,
        "bitwise_parity": "1" if bitwise else "0",
        "compared_log_messages": compared,
        "infrastructure_error": infrastructure_error,
    }


def run_case_verify(
    hermit: Path,
    backend: str,
    name: str,
    guest: list[str],
    expected_status: int,
    expected_l2: str,
    host_tmp: Path,
    host_capabilities: dict[str, dict[str, object]],
    evidence: dict[str, str] | None = None,
) -> tuple[str, str, float]:
    """Verification probe: one `hermit run --strict --verify` invocation.

    `--verify` runs the guest twice inside hermit and diverts the guest's own
    stdout into per-run temp logs, so this path cannot compare guest stdout the
    way the L1 path does. The contract it enforces instead is: the guest exit
    status matches, and Hermit's internal double-run comparison reports success
    at *at least* the evidence tier the matrix records (`expected_l2`). A
    Stripped DETLOG result satisfies a `guest` contract because it compares more
    observations; the reverse fails. Only a bitwise result establishes L2.
    """
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="hermit-verify-json-") as verify_dir:
        verdict_path = Path(verify_dir) / "verdict.json"
        command = hermit_command(
            hermit,
            backend,
            guest,
            name,
            strict=True,
            host_tmp=host_tmp,
            verify=True,
            verify_json=verdict_path,
        )
        result = run_with_timeout(command)
        observed_evidence = (
            verify_tier_from_json(verdict_path) if verdict_path.exists() else None
        )
    if evidence is not None and observed_evidence:
        evidence.update(observed_evidence)
        evidence[COMPARISON_TIER_COLUMN] = COMPARISON_TIER_SELF_VERIFY_ONLY
    if result is None:
        return "FAIL", "verify run timed out", time.monotonic() - started
    if observed_evidence and observed_evidence["infrastructure_error"]:
        return (
            "ERROR",
            observed_evidence["infrastructure_error"],
            time.monotonic() - started,
        )
    diagnostic = result.stderr.decode(errors="replace").strip()
    if result.returncode != expected_status:
        if cpuid_policy_is_blocked(backend, name, host_capabilities):
            return (
                "BLOCKED",
                CPUID_BLOCK_REASON,
                time.monotonic() - started,
            )
        return (
            "FAIL",
            f"verify exited {result.returncode}, expected {expected_status}: "
            f"{diagnostic[-300:]}",
            time.monotonic() - started,
        )
    if observed_evidence is None:
        return (
            "FAIL",
            "verify produced no usable current typed verification report: "
            f"{diagnostic[-300:]}",
            time.monotonic() - started,
        )
    # Typed verdict: authoritative.
    observed = observed_evidence["tier"]
    # A gap being probed (--probe-gaps) has no positive contract to meet; report
    # what it actually reached so it can be evaluated for promotion.
    if expected_l2 != "gap" and L2_RANK[observed] < L2_RANK[expected_l2]:
        return (
            "FAIL",
            f"reached verification tier {observed} but contract requires {expected_l2}",
            time.monotonic() - started,
        )
    # Each label states the comparison it EARNED.  The old "detlog" entry read
    # "L2 DETLOG-bitwise: --verify double-run matched" for a Stripped compare
    # whose own verdict says bitwise_parity:false -- that claim is what this
    # correction removes.
    label = {
        "bitwise": (
            "L2 DETLOG-bitwise: verify-json reported bitwise_parity over a "
            "nonzero compared-message count"
        ),
        "stripped": (
            "Stripped DETLOG: --verify double-run matched under the Stripped "
            "policy (numbers/addresses/paths normalized; NOT bitwise)"
        ),
        "guest": (
            "Guest-visible verification: output+exit matched "
            "(internal trace not compared)"
        ),
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
    host_capabilities: dict[str, dict[str, object]] | None = None,
    evidence: dict[str, str] | None = None,
) -> tuple[str, str, float]:
    if host_capabilities is None:
        raise MatrixError("run_case requires the producer-owned host-capabilities record")
    guest, expected_status, expected_stdout = case_command(name, fixtures)
    reference_guest = [*guest]
    if evidence is not None:
        evidence[COMPARISON_TIER_COLUMN] = COMPARISON_TIER_NO_COMPARISON
    if backend == "dbt" and name == "random_sources":
        guest = [*guest, "--root-only"]
        # Root-only output is the existing DBT cross-backend contract, so both
        # operands deliberately receive this shared narrowing argument.
        reference_guest = guest
    if backend == "kvm" and name == "memory_advice":
        # This selects the KVM-only fixture path for the candidate.  The ptrace
        # reference must retain the portable invocation captured above.
        guest = [*guest, "--kvm"]
    if verify:
        host_tmp = fixtures.host_tmp(backend, f"{name}-verify")
        return run_case_verify(
            hermit,
            backend,
            name,
            fixtures.expose_tmp_paths(backend, guest, host_tmp),
            expected_status,
            expected_l2,
            host_tmp,
            host_capabilities,
            evidence,
        )
    baseline: bytes | None = None
    started = time.monotonic()
    reference_stdout: bytes | None = None
    reference_problem = ""
    reference_blocked = False
    requires_ptrace_reference = backend == "dbt" and name == "random_sources"
    requires_exact_stdout_parity = exact_stdout_parity_contract(
        backend, name, expected_stdout
    )
    if requires_exact_stdout_parity or requires_ptrace_reference:
        reference_tmp = fixtures.host_tmp("ptrace", f"{name}-reference")
        (
            reference_stdout,
            reference_problem,
            reference_blocked,
        ) = capture_ptrace_reference(
            hermit,
            fixtures.expose_tmp_paths("ptrace", reference_guest, reference_tmp),
            name,
            strict,
            expected_status,
            expected_stdout,
            reference_tmp,
            host_capabilities,
        )
        if evidence is not None:
            evidence.update(stdout_parity_evidence(None, reference_stdout))
        if reference_blocked:
            return "BLOCKED", reference_problem, time.monotonic() - started
        # Preserve the pre-existing DBT random-stream contract even when the
        # caller explicitly disables scorecard output.  General stdout parity
        # may remain UNMEASURED when its reference is unavailable; this named
        # functional comparison may not.
        if requires_ptrace_reference and reference_stdout is None:
            return "FAIL", reference_problem, time.monotonic() - started
        if requires_exact_stdout_parity and reference_stdout is None:
            # The requested comparison itself is part of this cell's contract.
            # A missing side or comparator failure is RED, not an unmeasured
            # success: no equality verdict exists to support a green row.
            return "FAIL", reference_problem, time.monotonic() - started
    ptrace_random = (
        root_random_output(reference_stdout)
        if backend == "dbt" and name == "random_sources" and reference_stdout is not None
        else None
    )
    for iteration in range(RUNS):
        host_tmp = fixtures.host_tmp(backend, f"{name}-run-{iteration + 1}")
        command = hermit_command(
            hermit,
            backend,
            fixtures.expose_tmp_paths(backend, guest, host_tmp),
            name,
            strict,
            host_tmp,
        )
        result = run_with_timeout(command)
        if result is None:
            return "FAIL", f"run {iteration + 1} timed out", time.monotonic() - started

        # Record the candidate before interpreting its status or expected output.
        # A real stdout divergence must leave unequal operands and parity=0 in
        # the row, even though the enclosing test returns FAIL immediately.
        if iteration == 0 and requires_exact_stdout_parity:
            comparison = stdout_parity_evidence(result.stdout, reference_stdout)
            if evidence is not None:
                evidence.update(comparison)
                evidence[COMPARISON_TIER_COLUMN] = COMPARISON_TIER_STDOUT_ONLY
            if comparison.get("stdout_parity") == "0":
                return (
                    "FAIL",
                    "run 1 stdout differed from ptrace reference",
                    time.monotonic() - started,
                )

        if result.returncode != expected_status:
            diagnostic = result.stderr.decode(errors="replace").strip()
            if cpuid_policy_is_blocked(backend, name, host_capabilities):
                return (
                    "BLOCKED",
                    CPUID_BLOCK_REASON,
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
    detail = f"{RUNS}/{RUNS} runs matched"
    return "PASS", detail, time.monotonic() - started


# The columns the `--output` TSV carries.  `evidence` is deliberately NOT one of
# them: it is a nested dict of live parity observations whose home is the outer
# scorecard (see `append_parent_scorecard`), and a dict has no faithful TSV
# rendering.  Dropping it here is a decision, not an accident, which is why it is
# named rather than absorbed by a permissive writer.
RESULT_COLUMNS = (
    "test_name",
    "backend",
    "expectation",
    "result",
    "seconds",
    "detail",
)
# Keys a result row may legitimately carry that are not columns.  Anything
# outside `RESULT_COLUMNS` and this set is skew nobody anticipated, and the
# writer must say so instead of guessing.
NON_COLUMN_RESULT_KEYS = frozenset({"evidence"})


def write_results(path: Path, results: list[dict[str, str]]) -> None:
    """Write the whole run matrix, or write nothing and name what stopped it.

    `csv.DictWriter` defaults to ``extrasaction="raise"``, so once the row
    builder learned to carry `evidence` (:1241, unlike the six-key GAP rows at
    :1214) every executed row raised mid-write.  Because `writerows` streams,
    the raise left behind a syntactically valid TSV containing the clean PREFIX
    of the rows -- measured 3 of 10 with realistic GAP-then-executed ordering,
    and 0 of 10 when the first row was skewed.

    That is the part worth defending against.  The process does exit non-zero
    (an uncaught `ValueError` exits 1), so the failure is not silent to a caller
    reading ``$?``; it is silent at the ARTIFACT boundary, because a short file
    is indistinguishable from a small result set.  A reader cannot tell "the
    matrix found three cases" from "the matrix found ten and lost seven".

    Two rules, and neither alone is sufficient:

      * a KNOWN non-column key is projected out deliberately, by name;
      * ANY other skew -- an unexpected extra key, or a missing column -- raises
        `MatrixError` identifying the row and the key, BEFORE anything is
        written.

    Validation therefore runs over every row up front, and the file is then
    written whole through a temporary file and an atomic rename.  A refusal
    leaves the previous artifact untouched rather than truncated, so there is no
    state in which a short file can be mistaken for a complete one.
    """
    for index, row in enumerate(results):
        missing = [column for column in RESULT_COLUMNS if column not in row]
        if missing:
            raise MatrixError(
                f"result row {index} ({row.get('backend', '?')}/"
                f"{row.get('test_name', '?')}) is missing required column(s) "
                f"{', '.join(missing)}; refusing to write a partial "
                f"{path} -- {len(results)} row(s) would have been lost"
            )
        unexpected = sorted(
            set(row) - set(RESULT_COLUMNS) - NON_COLUMN_RESULT_KEYS
        )
        if unexpected:
            raise MatrixError(
                f"result row {index} ({row['backend']}/{row['test_name']}) "
                f"carries unexpected field(s) {', '.join(unexpected)} that "
                f"{path} has no column for; declare them in RESULT_COLUMNS or "
                f"in NON_COLUMN_RESULT_KEYS. Refusing to write a partial file "
                f"-- {len(results)} row(s) would have been lost"
            )

    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f"{path.name}.partial")
    try:
        with temporary.open("w", newline="", encoding="utf-8") as output:
            writer = csv.DictWriter(
                output,
                fieldnames=RESULT_COLUMNS,
                delimiter="\t",
                extrasaction="ignore",
            )
            writer.writeheader()
            writer.writerows(results)
        temporary.replace(path)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    # The count travels with the artifact: a consumer that sees this line and a
    # file with a different number of data rows knows the two disagree.
    print(f"TRACKING: wrote {len(results)} result row(s) to {path}")


def write_structured_test_results(
    results: list[dict[str, str]], executed: int, filtered: int, mode: str
) -> None:
    """Publish exact terminal matrix results without trusting stdout."""
    configured = os.environ.get("DAGRUN_TEST_COUNTS_PATH")
    if not configured:
        return
    terminal = [result for result in results if result["result"] != "GAP"]
    if len(terminal) != executed:
        raise MatrixError(
            "structured DBT results disagree with the executed-case count: "
            f"{len(terminal)} terminal row(s), {executed} executed case(s)"
        )
    rows = [
        {
            "id": (
                f"backend-parity/{result['test_name']} "
                f"[{result['backend']}/{mode}]"
            ),
            "result": "pass" if result["result"] in {"PASS", "XPASS"} else "fail",
            "attempts": 1,
        }
        for result in terminal
    ]
    if len({row["id"] for row in rows}) != len(rows):
        raise MatrixError("structured DBT results contain a duplicate test identity")
    path = Path(configured)
    temporary = path.with_name(f"{path.name}.tmp.{os.getpid()}")
    payload = {
        "schema": 2,
        "executed_tests": executed,
        "filtered_tests": filtered,
        "results": rows,
    }
    try:
        temporary.write_text(
            json.dumps(payload, separators=(",", ":")) + "\n", encoding="utf-8"
        )
        temporary.replace(path)
    except OSError as error:
        temporary.unlink(missing_ok=True)
        raise MatrixError(
            f"cannot publish structured test counts to {path}: {error}"
        ) from error


DEFAULT_OBSERVATION_SUBDIR = Path("ignored") / "backend-parity"


def discover_compat_envelope() -> Path | None:
    configured = os.environ.get("DEV_HERMIT_ROOT") or os.environ.get("DEV_HERMIT")
    roots = [Path(configured)] if configured else []
    roots.extend((REPOSITORY, *REPOSITORY.parents))
    for root in roots:
        compat_dir = root / "compat-envelope"
        if compat_dir.is_dir():
            return compat_dir.resolve()
    return None


def make_run_id() -> tuple[str, int]:
    hermit_sha = git_output("rev-parse", "HEAD") or "unknown"
    epoch = int(time.time())
    return f"backend-parity-{hermit_sha[:12]}-{epoch}-{os.getpid()}", epoch


def default_observation_path(run_id: str) -> Path | None:
    compat_dir = discover_compat_envelope()
    if compat_dir is None:
        return None
    return compat_dir / DEFAULT_OBSERVATION_SUBDIR / f"{run_id}.csv"


def fold_in_command(compat_dir: Path, observation: Path) -> str:
    publisher = compat_dir / "publish-scorecard.py"
    command = (
        "python3",
        str(publisher),
        "--observation",
        str(observation),
        "--current",
        str(compat_dir / "scorecard.csv"),
        "--history",
        str(compat_dir / "history" / "scorecard-observations.csv"),
    )
    return " ".join(shlex.quote(part) for part in command)


def is_tracked_current_scorecard(path: Path, compat_dir: Path | None) -> bool:
    """Recognize the reviewed current view even without parent discovery.

    ``--parent-scorecard`` exists for a standalone Hermit checkout, where the
    requested parent may not be an ancestor and ``discover_compat_envelope``
    legitimately returns ``None``.  The publisher-only rule must still hold in
    that exact configuration, so the resolved ``compat-envelope/scorecard.csv``
    shape is itself sufficient.  A discovered parent remains the stronger
    identity check and also covers a symlinked spelling of the directory.
    """
    def has_current_shape(candidate: Path) -> bool:
        return (
            candidate.name == "scorecard.csv"
            and candidate.parent.name == "compat-envelope"
        )

    # Check the spelling as well as the resolved target.  A standalone caller
    # may point at a symlinked compat-envelope whose target has a different
    # directory name; that must not turn the publisher-only current view into
    # an appendable observation.
    if has_current_shape(path):
        return True
    resolved = path.resolve()
    if compat_dir is not None:
        if resolved == (compat_dir / "scorecard.csv").resolve():
            return True
    return has_current_shape(resolved)


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
    run_id: str | None = None,
    epoch: int | None = None,
) -> None:
    # Multiple worktrees can validate concurrently against one outer workspace.
    # Serialize whole-row appends so the shared measurement log remains valid.
    import fcntl

    path.parent.mkdir(parents=True, exist_ok=True)
    hermit_sha = git_output("rev-parse", "HEAD") or "unknown"
    dirty = bool(git_output("status", "--porcelain"))
    if run_id is None or epoch is None:
        run_id, epoch = make_run_id()
    mode = "verify" if verify else "strict" if strict else "repeat"
    rows: list[dict[str, str]] = []
    for result in results:
        status = result["result"]
        passed = status in {"PASS", "XPASS"}
        stdout_evidence = result.get("evidence") or {}
        outcome = {
            "PASS": "pass",
            "XPASS": "pass",
            "FAIL": "fail",
            "ERROR": "error",
            "GAP": "gap",
            "BLOCKED": "skip",
        }[status]
        # PASS/FAIL is the whole test's outcome, not a stdout comparison.  The
        # parity boolean is derived solely from the two hashes captured above.
        parity = stdout_evidence.get("stdout_parity", "")
        detail = result["detail"]
        if verify and result["backend"] == "kvm" and passed:
            detail = (
                "Guest-visible verification only (stdout+exit compared; internal "
                f"trace not compared): {detail}"
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
                # A determinism positive requires evidence that a comparison
                # happened AND what it was. When the run produced no typed
                # verdict, `determinism_unmeasured` is set and this stays blank:
                # the cell is genuinely unmeasured, not deterministic-by-default.
                "deterministic": (
                    ""
                    if (result.get("evidence") or {}).get("determinism_unmeasured")
                    else ("1" if passed and strict else "")
                ),
                "stdout_parity": parity,
                "output_hash": stdout_evidence.get("output_hash", ""),
                "duration_ms": str(round(float(result["seconds"]) * 1000)),
                "max_rss_kb": "",
                "reason": detail,
                **{
                    column: stdout_evidence.get(column, "")
                    for column in STDOUT_EVIDENCE_COLUMNS
                },
                # The comparison this row's verdict rests on.  `deterministic`
                # alone is the ambiguous field the audit flagged -- a bare 1
                # cannot distinguish a stripped match from a bitwise one -- so it
                # now always travels with the tier that earned it and the counts
                # that make the tier falsifiable.  Written only into files that
                # carry these columns (see EVIDENCE_COLUMNS).
                **{
                    column: stdout_evidence.get(column, "")
                    for column in EVIDENCE_COLUMNS
                },
                COMPARISON_TIER_COLUMN: stdout_evidence.get(
                    COMPARISON_TIER_COLUMN, COMPARISON_TIER_NO_COMPARISON
                ),
                # This matrix does not request the L3 memory flags.  For a new
                # run that fact is known and is false, not historical unknown.
                "stack_parity": "0",
                "heap_parity": "0",
            }
        )

    with path.open("a+", newline="", encoding="utf-8") as scorecard:
        fcntl.flock(scorecard.fileno(), fcntl.LOCK_EX)
        scorecard.seek(0)
        first_line = scorecard.readline()
        if first_line:
            actual_header = next(csv.reader([first_line]))
            fieldnames, parity_column = scorecard_fieldnames(actual_header, path)
        else:
            fieldnames, parity_column = SCORECARD_HEADER, PARITY_COLUMNS[0]
            writer = csv.DictWriter(
                scorecard, fieldnames=fieldnames, lineterminator="\n"
            )
            writer.writeheader()
        if "ref_output_hash" not in fieldnames:
            # A legacy row cannot carry both operands.  Keep its candidate hash,
            # but never emit an assertion that the row cannot re-derive.
            for row in rows:
                row["stdout_parity"] = ""
        if parity_column != "stdout_parity":
            for row in rows:
                row[parity_column] = row.pop("stdout_parity")
        scorecard.seek(0, os.SEEK_END)
        # `restval` fills a column the file HAS and we did not populate;
        # `extrasaction="ignore"` drops evidence we produced that the file does
        # NOT carry.  Both directions are required and neither is cosmetic: the
        # default `extrasaction="raise"` turns "this producer learned to record
        # more" into a hard refusal of every older scorecard -- the same outage
        # shape `verify_compare` caused, one column generation later.
        writer = csv.DictWriter(
            scorecard,
            fieldnames=fieldnames,
            restval="",
            extrasaction="ignore",
            lineterminator="\n",
        )
        writer.writerows(rows)
        scorecard.flush()
        fcntl.flock(scorecard.fileno(), fcntl.LOCK_UN)
    print(f"TRACKING: wrote {len(rows)} observation rows to {path}")


def record_parent_observations(
    results: list[dict[str, str]],
    *,
    requested_path: Path | None,
    disabled: bool,
    strict: bool,
    verify: bool,
    probe_gaps: bool,
) -> Path | None:
    if disabled or not results:
        return None
    run_id, epoch = make_run_id()
    explicit = requested_path is not None
    destination = requested_path or default_observation_path(run_id)
    if destination is None:
        print(
            "TRACKING: no enclosing dev-hermit compat-envelope/ found; "
            f"{len(results)} observation(s) NOT recorded"
        )
        return None
    compat_dir = discover_compat_envelope()
    if is_tracked_current_scorecard(destination, compat_dir):
        raise MatrixError(
            "refusing to append the tracked current scorecard; write a per-run "
            "observation and publish it through publish-scorecard.py"
        )
    append_parent_scorecard(
        destination,
        results,
        strict=strict,
        verify=verify,
        probe_gaps=probe_gaps,
        run_id=run_id,
        epoch=epoch,
    )
    if not explicit and compat_dir is not None:
        print(
            "TRACKING: per-run artifact only; current scorecard unchanged. "
            "After review, publish with:\n  "
            + fold_in_command(compat_dir, destination)
        )
    return destination


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
            "write observations to this exact artifact (default: one ignored "
            "per-run file under compat-envelope/ignored/backend-parity/; the "
            "tracked compat-envelope/scorecard.csv is always refused)"
        ),
    )
    parser.add_argument(
        "--no-parent-scorecard",
        action="store_true",
        help=(
            "disable outer dev-hermit observation output without disabling "
            "any comparison"
        ),
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
            "verify every probe twice using the "
            f"{DEFAULT_VERIFY_POLICY.comparison_claim}; this policy is "
            f"{DEFAULT_VERIFY_POLICY.assurance_label()} "
            "(implies --strict; guest stdout is diverted, so stdout parity is "
            "not checked in this mode)"
        ),
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    names = validate_catalog()
    backends = args.backends or list(BACKENDS)
    # --verify presupposes strict mode.  The default Stripped comparator remains
    # below L2; only canonical --verify-strict evidence can establish L2.
    strict = args.strict or args.verify
    if args.verify:
        print(DEFAULT_VERIFY_POLICY.mode_summary())
    elif strict:
        print("MODE: L1 (--strict), byte-identical stdout across 3 runs")
    else:
        print("MODE: compatibility (repeat-run), byte-identical stdout across 3 runs")
    baseline = len(names)
    for backend in BACKENDS:
        passing = baseline - sum(gap_backend == backend for gap_backend, _ in L1_GAPS)
        print(f"RATCHET {backend}: {passing}/{baseline} ({passing / baseline:.1%})")
    # Verification ratchet: how many contracts each backend verifies under
    # --verify, split
    # by assurance kind.  These are CONTRACTS, not earned results: the tier a run
    # actually reaches is read from its own verdict (`verify_tier_from_json`).
    # The split used to print `detlog=`, which asserted bitwise identity for a
    # comparison that only ever normalised-and-compared; it prints `stripped=`
    # now so the headline cannot overstate the corpus.
    for backend in BACKENDS:
        verified = baseline - sum(gap_backend == backend for gap_backend, _ in L2_GAPS)
        tier_counts = {"stripped": 0, "guest": 0, "bitwise": 0}
        tier = (
            "guest"
            if backend == "kvm"
            else DEFAULT_VERIFY_POLICY.expected_non_kvm_tier
        )
        tier_counts[tier] = verified
        print(
            f"RATCHET --verify {backend}: {verified}/{baseline} "
            f"({verified / baseline:.1%}) "
            f"[stripped-DETLOG={tier_counts['stripped']} "
            f"guest-visible={tier_counts['guest']} bitwise={tier_counts['bitwise']}]"
        )
    if args.check:
        return 0

    hermit = args.hermit.resolve()
    if not hermit.is_file() or not os.access(hermit, os.X_OK):
        raise MatrixError(f"Hermit executable is unavailable: {hermit}")
    if args.parent_scorecard and args.no_parent_scorecard:
        raise MatrixError(
            "--parent-scorecard and --no-parent-scorecard cannot be used together"
        )
    host_capabilities = read_host_capabilities(hermit)
    results: list[dict[str, str]] = []
    failures = 0
    executed_cases = 0
    filtered_cases = 0
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
                    filtered_cases += 1
                    continue

                evidence: dict[str, str] = {}
                executed_cases += 1
                status, detail, duration = run_case(
                    hermit,
                    backend,
                    name,
                    fixtures,
                    strict,
                    args.verify,
                    expected,
                    host_capabilities,
                    evidence,
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
                        "evidence": evidence,
                    }
                )
                if status == "ERROR" or (not is_gap and status == "FAIL"):
                    failures += 1

    if args.output:
        write_results(args.output, results)
    record_parent_observations(
        results,
        requested_path=args.parent_scorecard,
        disabled=args.no_parent_scorecard,
        strict=strict,
        verify=args.verify,
        probe_gaps=args.probe_gaps,
    )
    mode = "verify" if args.verify else "strict" if strict else "repeat"
    write_structured_test_results(results, executed_cases, filtered_cases, mode)
    return 1 if failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except MatrixError as error:
        print(f"ERROR: {error}", file=sys.stderr)
        sys.exit(2)
