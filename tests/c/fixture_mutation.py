#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

"""Shared mutation harness for the identity-fixture family in tests/c.

This harness moved here from tests/backend-parity/parity_mutation.py, with its
fixtures, when the backend-parity-c manifest bucket was folded into c-programs
(https://github.com/rrnewton/hermit/issues/3301, slice S6).

The family of identity fixtures (rlimit_identity_probe.c,
sched_getaffinity_identity_probe.c, ioctl_fionread.c, ...) all make the same claim:
"every backend observes the same value the golden ptrace reference does." That
claim is only worth anything if the fixture would actually FAIL when a backend
gets the value wrong. Historically each fixture hand-rolled its own
both-direction proof, and a hand-rolled proof is a chance to write one that
cannot fail -- the vacuous-test shape. (Measured on the reverie staging batch:
5 of 5 members had tests that passed WITHOUT exercising their mechanism, and 4
of those 5 hid a real product bug.)

This harness gives the whole family ONE verification, so a new member cannot
drift into vacuity. Each member supplies only:

  * its syscall           -- the fixture .c source, and
  * its divergence        -- the name(s) of the mutable field(s) it threads
                             through the parity_mutate_*() seam in parity_probe.h.

The harness then proves, for every member, BOTH directions:

  (a) plant a divergence   -- run the fixture with HERMIT_PARITY_MUTATE naming a
      -> assert FAILURE        field. The seam perturbs that field's observed
                               value, so the fixture's (exit status, stdout)
                               must diverge from the clean golden run. If it does
                               NOT diverge, the field is not actually load-bearing
                               and the fixture is VACUOUS -- the harness fails.
  (b) run clean            -- run the fixture unperturbed under a candidate
      -> assert PASS           backend. Its (exit status, stdout) must MATCH the
                               golden ptrace reference. A mismatch is a real
                               backend-parity defect.

"Parity means matching the GOLDEN PTRACE REFERENCE," so ptrace is the default
comparison target in hermit mode, not something each fixture re-states.

Two run modes:

  * Native self-test (default; C compiler only, no hermit build). The reference
    is a clean native execution; the mutation direction proves each declared
    field is load-bearing, and the clean direction proves the fixture's contract
    holds. This is the cheap CI guard that catches a vacuous family member on
    every PR without building hermit.
  * Hermit cross-backend (with --hermit). Adds the REAL parity check: each
    candidate backend's clean run must match the golden ptrace run, and a
    divergence planted in that backend must be caught. A backend that cannot run
    (e.g. KVM without /dev/kvm) is SKIPPED with a reported reason -- never a
    silent pass -- unless --require-backend makes the skip fatal.

Native mode also runs a CPU-placement leg: every fixture's clean contract must
still hold on a host that lets it use exactly one CPU, and not its lowest one.
Validation hosts do restrict CPUs this way. The dev-hermit launcher keeps cells
off the CPUs the host PMU drivers are bound to, CPU 0 among them
(https://github.com/rrnewton/hermit/issues/3265), and a fixture that pinned
itself to CPU 0 failed there while passing everywhere else. An unprivileged
harness cannot create the cgroup cpuset that causes this, so the leg links the
fixture with fixture_cpuset_shim.c, which applies the kernel's cpuset rule to
sched_setaffinity, and first proves with that file's control program that the
rule refuses a CPU the kernel itself would have accepted.

Adding a family member is one registry entry below: source path + field names.
No bespoke verification code travels with the fixture.

The family is exactly the set of tests/c sources that include parity_probe.h.
The harness refuses to run if that set and the registry differ, so a fixture
that adopts the seam cannot go unexamined and a registry entry cannot point at
a file that left the family. Every run prints the fixtures it examined.
"""

from __future__ import annotations

import argparse
import dataclasses
import errno
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
# The fixtures live beside the harness, in tests/c.
FIXTURES_DIR = SCRIPT_DIR
REPOSITORY_ROOT = SCRIPT_DIR.parent.parent
# The shared contract header every family member includes.
PROBE_HEADER = "parity_probe.h"
# Link-time cpuset for the CPU-placement leg. It does not include PROBE_HEADER,
# so it is not a family member.
CPUSET_SHIM = "fixture_cpuset_shim.c"
CPUSET_SHIM_FLAGS = ("-Wl,--wrap=sched_setaffinity",)

# The golden reference backend. "Parity" is defined against this.
GOLDEN_BACKEND = "ptrace"
# Candidate backends checked for parity against the golden reference by default.
DEFAULT_CANDIDATES = ("dbt", "kvm")

# Guest run timeout (seconds) for a single fixture execution under hermit.
HERMIT_TIMEOUT_S = 60
NATIVE_TIMEOUT_S = 30


class HarnessError(Exception):
    """A harness configuration or contract error (not a fixture verdict)."""


@dataclasses.dataclass(frozen=True)
class FixtureSpec:
    """A single family member. Supplies ONLY its syscall and its divergence.

    source: fixture .c source path (relative to tests/c unless absolute).
    fields: the mutable field name(s) it threads through the parity_mutate_*()
            seam. Each is proven load-bearing independently.
    cflags: extra compile flags (e.g. ("-pthread",)); -D_GNU_SOURCE is always
            supplied per parity_probe.h's contract.
    """

    source: str
    fields: tuple[str, ...]
    cflags: tuple[str, ...] = ()

    def source_path(self) -> Path:
        candidate = Path(self.source)
        return candidate if candidate.is_absolute() else FIXTURES_DIR / candidate


# The family registry. Every identity fixture that includes parity_probe.h lives
# here with its field(s); nothing else. A new member is one line.
#
# rlimit_identity_probe.c and sched_getaffinity_identity_probe.c are the
# harness-only members that lived in tests/backend-parity/fixtures as
# rlimit_identity.c and sched_getaffinity_identity.c. They took a _probe suffix
# on the move because tests/c/rlimit_identity.c and
# tests/c/sched_getaffinity_identity.c are different, larger manifest programs.
FIXTURES: dict[str, FixtureSpec] = {
    "rlimit_identity": FixtureSpec(
        source="rlimit_identity_probe.c",
        fields=("nofile",),
    ),
    "sched_getaffinity_identity": FixtureSpec(
        source="sched_getaffinity_identity_probe.c",
        fields=("affinity_count",),
    ),
    "ioctl_fionread": FixtureSpec(
        source="ioctl_fionread.c",
        fields=(
            "navail",
            "fionread_ok",
            "fionbio_set",
            "nonblock_after_set",
            "fionbio_clear",
            "clear_readback_ok",
            "nonblock_after_clear",
        ),
    ),
}


# ---------------------------------------------------------------------------
# Strictness tiers
# ---------------------------------------------------------------------------
#
# "Parity" is not one predicate. A green must say WHICH streams it compared,
# because a green that does not is exactly how the predecessor reported PASS
# for a mutation it never looked at. Each tier is a superset of the one below.
#
#   TIER-1  exit status + stdout
#   TIER-2  + stderr
#   TIER-3  + the unstripped hermit INFO log
#   TIER-4  + stack/heap observations (not yet produced by this harness)
#
# RELATIONSHIP TO THE PRODUCT'S OWN L-LEVELS, stated so the two vocabularies do
# not silently drift apart. `hermit run --strict --verify --verify-strict`
# establishes L2 under the `BitwiseInfoV1` policy (detcore/src/logdiff.rs), and
# that is the authority for what "bitwise" means in this repository. But
# --verify compares a backend against ITSELF on a repeat run; it cannot compare
# one backend against another, which is what parity means here. So this harness
# is not a substitute for --verify-strict and does not reimplement it -- it
# applies the same idea across backends.
#
# TIER-2 corresponds to what KVM's --verify can currently assert (exit status,
# stdout, stderr) and TIER-3 to the full-INFO envelope, which is why the repo
# says KVM "cannot claim full L2 INFO parity until internal log comparison
# exists".
#
# KNOWN GAP, and it is deliberate rather than overlooked: BitwiseInfoV1 both
# removes the wall-clock prefix AND ordinalizes host addresses marked with the
# `<hostaddr 0x...>` wrapper, preserving identity and order. TIER-3 here does
# only the wall-clock part. That makes TIER-3 CONSERVATIVE: an unmarked host
# address differing between runs reports a parity BREAK that BitwiseInfoV1
# would forgive. It can therefore understate an achieved tier, never overstate
# one -- a false negative, not a false green. Adopting the product
# canonicalizer here is the right follow-up; until then, read a TIER-2 result
# as "TIER-2 or better".
#
# Cross-backend note, and it is the reason tiers exist rather than one boolean:
# different backends legitimately emit different INFO logs (backend name,
# interception detail), so requiring TIER-3 equality between the ptrace golden
# and a candidate would be red everywhere for reasons that are not parity
# defects. A lane that is red everywhere gets disabled wholesale. So a
# cross-backend cell REPORTS the highest tier it achieves; it does not pretend
# to a tier it cannot reach. Same-backend legs (golden vs mutated) are held to
# the fixture's top supported tier, because there the log genuinely should match.

TIER_STREAMS: tuple[tuple[int, str], ...] = (
    (1, "exit+stdout"),
    (2, "+stderr"),
    (3, "+info-log"),
)
MAX_TIER = TIER_STREAMS[-1][0]
TIER_NAMES: dict[int, str] = {n: label for n, label in TIER_STREAMS}


def tier_label(tier: int) -> str:
    parts = [label for n, label in TIER_STREAMS if n <= tier]
    return f"TIER-{tier} ({' '.join(parts)})" if parts else "TIER-0 (nothing compared)"


@dataclasses.dataclass(frozen=True)
class Observation:
    """The guest-visible result of one fixture execution: what parity compares.

    Carries every stream the strict standard names. The predecessor carried
    only (exit_status, stdout) and discarded stderr at the ``communicate``
    call, so a mutation visible solely in stderr or in the INFO log compared
    EQUAL and reported PASS. Widening the type is the fix; a comparison cannot
    be stricter than the thing it compares.
    """

    exit_status: int
    stdout: bytes
    stderr: bytes = b""
    info_log: bytes = b""

    def stream(self, tier: int) -> tuple:
        """The comparison key at a given tier. Tiers nest, so this is a prefix."""
        key: list = [self.exit_status, self.stdout]
        if tier >= 2:
            key.append(self.stderr)
        if tier >= 3:
            key.append(self.info_log)
        return tuple(key)

    def matches(self, other: "Observation", tier: int) -> bool:
        return self.stream(tier) == other.stream(tier)

    def __eq__(self, other: object) -> bool:
        """Equality is TIER-1 and is kept only for existing call sites.

        Prefer :meth:`matches` with an explicit tier: an untiered ``==`` is a
        comparison that does not record what it compared.
        """
        if not isinstance(other, Observation):
            return NotImplemented
        return self.matches(other, 1)

    def is_empty(self) -> bool:
        """True when this run emitted no identity payload at all.

        Two empty observations compare EQUAL, so without this an
        observation-free run reports parity. That is the vacuity that let a
        vdso fixture go green by emitting no bytes: nothing was compared, and
        nothing-vs-nothing matched.

        Deliberately keyed on STDOUT only: the identity payload is the fixture's
        canonical stdout line. A run that emits only a stderr warning has still
        produced no identity payload and must not qualify.
        """
        return not self.stdout.strip()

    def summary(self) -> str:
        def show(raw: bytes) -> str:
            return raw.decode("utf-8", "replace").strip().replace("\n", " | ")

        parts = [f"exit={self.exit_status}", f"stdout={show(self.stdout)!r}"]
        if self.stderr.strip():
            parts.append(f"stderr={show(self.stderr)[:120]!r}")
        if self.info_log.strip():
            parts.append(f"info-log={len(self.info_log)}B")
        return " ".join(parts)

    def first_differing_tier(self, other: "Observation") -> int | None:
        """Lowest tier at which these two disagree, or None if equal through MAX_TIER."""
        for tier, _ in TIER_STREAMS:
            if not self.matches(other, tier):
                return tier
        return None

    def achieved_tier(self, other: "Observation") -> int:
        """Highest tier at which these two agree (0 if they differ at TIER-1)."""
        first = self.first_differing_tier(other)
        return MAX_TIER if first is None else first - 1


# ---------------------------------------------------------------------------
# Compilation
# ---------------------------------------------------------------------------


_PROBE_INCLUDE_RE = re.compile(r'^\s*#\s*include\s+"parity_probe\.h"', re.M)


def family_sources(directory: Path) -> set[Path]:
    """Every C source in `directory` that includes the shared probe header."""
    return {
        source.resolve()
        for source in sorted(directory.glob("*.c"))
        if _PROBE_INCLUDE_RE.search(source.read_text(encoding="utf-8"))
    }


def check_family_registry(
    fixtures: dict[str, FixtureSpec], directory: Path = FIXTURES_DIR
) -> None:
    """Refuse unless the registry is exactly the family found on disk.

    A source that includes parity_probe.h but is not registered would never be
    examined; a registered source that is missing or no longer includes the
    header would be examined against a contract it does not carry. Either way
    the guard would report success about files it did not check.
    """
    if not (directory / PROBE_HEADER).is_file():
        raise HarnessError(f"shared probe header missing: {directory / PROBE_HEADER}")
    registered = {spec.source_path().resolve() for spec in fixtures.values()}
    discovered = family_sources(directory)
    missing = sorted(str(path) for path in registered if not path.is_file())
    unregistered = sorted(str(path) for path in discovered - registered)
    outside = sorted(str(path) for path in registered - discovered if path.is_file())
    if missing or unregistered or outside:
        raise HarnessError(
            "fixture registry does not match the parity_probe.h family in "
            f"{directory}: missing={missing} unregistered={unregistered} "
            f"not-including-{PROBE_HEADER}={outside}"
        )
    if not registered:
        raise HarnessError(
            f"fixture registry is empty; nothing in {directory} to examine"
        )


def compile_fixture(spec: FixtureSpec, output: Path) -> Path:
    """Compile a fixture with the manifest runner's flags plus -D_GNU_SOURCE.

    The manifest runner (ci/manifest-plan/src/runner.rs) compiles C fixtures
    with -std=c11 -O2 -g -Wall -Wextra -Werror and each test's build.cflags, and
    adds -D_GNU_SOURCE only when those cflags list it. This harness always
    defines it, adds -I for this directory, and appends the fixture's own cflags.
    """
    compiler = shutil.which(os.environ.get("CC", "cc"))
    if compiler is None:
        raise HarnessError("C compiler unavailable (set CC or install cc)")
    source = spec.source_path()
    if not source.is_file():
        raise HarnessError(f"fixture source missing: {source}")
    command = [
        compiler,
        "-O2",
        "-g",
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-D_GNU_SOURCE",
        f"-I{FIXTURES_DIR}",
        *spec.cflags,
        str(source),
        "-o",
        str(output),
    ]
    result = subprocess.run(command, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        raise HarnessError(
            f"fixture compilation failed: {command!r}\n{result.stdout}{result.stderr}"
        )
    return output


# Fields the fixture actually threads through the mutation seam, parsed from its
# source. Used to guard against a fixture growing an undeclared field: a field
# that is mutated but not registered would go unproven; a field registered but
# not mutated would be inert. Either is a drift the family must not permit.
_MUTATE_CALL_RE = re.compile(r"parity_mutate_(?:u64|i64|str)\s*\(\s*\"([^\"]+)\"")


def source_declared_fields(spec: FixtureSpec) -> set[str]:
    text = spec.source_path().read_text(encoding="utf-8")
    return set(_MUTATE_CALL_RE.findall(text))


# ---------------------------------------------------------------------------
# Execution
# ---------------------------------------------------------------------------


# Hermit writes its own diagnostics to stderr interleaved with the guest's, in
# the tracing default format:
#
#   2026-08-06T21:53:42.208442Z  INFO detcore::scheduler::runqueue: DETLOG ...
#   ^ RFC3339 timestamp          ^ level
#
# Splitting them is what makes TIER-2 and TIER-3 separable; lumping them
# together makes every guest stderr byte look like a log line and vice versa.
#
# NOTE the leading timestamp: an earlier version of this regex anchored the
# level at the start of the line, so EVERY hermit log line fell through to the
# "guest stderr" bucket. The visible symptom was every cross-backend cell
# reporting TIER-1 -- not because the backends differed, but because TIER-2 was
# comparing wall-clock timestamps against themselves. A tier that can never be
# reached is a vacuous tier, so this pattern is load-bearing.
_LOG_LINE_RE = re.compile(
    rb"^(?P<ts>\d{4}-\d{2}-\d{2}T[\d:.]+Z)?\s*(?:\[[^\]]*\]\s*)?"
    rb"(?:TRACE|DEBUG|INFO|WARN|ERROR)\b"
)
_TIMESTAMP_RE = re.compile(rb"\d{4}-\d{2}-\d{2}T[\d:.]+Z")


def normalize_log(raw: bytes) -> bytes:
    """Blank wall-clock timestamps in a log stream.

    "Unstripped" means we do not discard log CONTENT -- not that we compare
    wall-clock readings. A timestamp differs between any two runs, including
    two runs of the same backend, so leaving it in makes TIER-3 unreachable by
    construction and therefore meaningless. Everything else in the line, target
    and message included, is compared byte for byte.
    """
    return _TIMESTAMP_RE.sub(b"<TS>", raw)


def split_stderr(raw: bytes) -> tuple[bytes, bytes]:
    """Split a combined stderr stream into (guest stderr, hermit INFO log)."""
    guest: list[bytes] = []
    log: list[bytes] = []
    for line in raw.splitlines(keepends=True):
        (log if _LOG_LINE_RE.match(line) else guest).append(line)
    return b"".join(guest), normalize_log(b"".join(log))


def _run(
    command: list[str],
    env: dict[str, str],
    timeout: int,
    capture_log: bool = False,
    cpus: frozenset[int] | None = None,
) -> Observation | None:
    # `cpus` restricts the child's inherited affinity before exec. On its own
    # that is not a cpuset (the child may widen it again); the placement leg
    # pairs it with fixture_cpuset_shim.c, which treats it as one.
    process = subprocess.Popen(
        command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        stdin=subprocess.DEVNULL,
        start_new_session=True,
        preexec_fn=None if cpus is None else (lambda: os.sched_setaffinity(0, cpus)),
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        return None
    # stderr was previously captured and then DISCARDED here (`stdout, _ = ...`),
    # which is why a mutation visible only in stderr compared equal and passed.
    if capture_log:
        guest_stderr, info_log = split_stderr(stderr)
    else:
        guest_stderr, info_log = stderr, b""
    return Observation(process.returncode, stdout, guest_stderr, info_log)


def observe_native(
    binary: Path,
    mutate: str | None,
    cpus: frozenset[int] | None = None,
    args: tuple[str, ...] = (),
) -> Observation | None:
    """Run the compiled fixture directly, optionally planting a mutation."""
    env = dict(os.environ)
    env.pop("HERMIT_PARITY_MUTATE", None)
    if mutate is not None:
        env["HERMIT_PARITY_MUTATE"] = mutate
    return _run([str(binary), *args], env, NATIVE_TIMEOUT_S, cpus=cpus)


def _hermit_command(
    hermit: Path,
    backend: str,
    binary: Path,
    mutate: str | None,
    host_tmp: Path,
) -> list[str]:
    # --log=info is what makes TIER-3 possible at all: without it hermit emits
    # no INFO log, so "compare the unstripped INFO log" would be comparing two
    # empty strings — a vacuous pass wearing the name of the strictest tier.
    command = [str(hermit), "--log=info"]
    if backend != GOLDEN_BACKEND:
        command.extend(["--backend", backend])
    command.append("run")
    command.extend(
        [
            "--strict",
            "--base-env=minimal",
            "--max-timeslice=disabled",
            f"--tmp={host_tmp}",
        ]
    )
    if mutate is not None:
        # --base-env=minimal strips the ambient env, so the mutation must be
        # passed through explicitly for the guest to observe it.
        command.append(f"--env=HERMIT_PARITY_MUTATE={mutate}")
    command.extend(["--", str(binary)])
    return command


def observe_hermit(
    hermit: Path, backend: str, binary: Path, mutate: str | None
) -> Observation | None:
    """Run the fixture under a hermit backend, optionally planting a mutation."""
    env = dict(os.environ)
    env.pop("HERMIT_PARITY_MUTATE", None)  # only the guest, via --env, should see it
    with tempfile.TemporaryDirectory(
        prefix=f"hermit-tmp-{backend}-", dir=binary.parent
    ) as raw_host_tmp:
        return _run(
            _hermit_command(hermit, backend, binary, mutate, Path(raw_host_tmp)),
            env,
            HERMIT_TIMEOUT_S,
            capture_log=True,
        )


def backend_available(hermit: Path, backend: str) -> tuple[bool, str]:
    """Smoke-test a candidate backend with a trivial guest."""
    if backend == GOLDEN_BACKEND:
        return True, ""
    command = [
        str(hermit),
        "--backend",
        backend,
        "run",
        "--base-env=minimal",
        "--",
        "/bin/true",
    ]
    result = _run(command, dict(os.environ), HERMIT_TIMEOUT_S)
    if result is None:
        return False, "smoke test timed out"
    if result.exit_status != 0:
        detail = result.stdout.decode("utf-8", "replace").strip()[-200:]
        return False, f"smoke exit {result.exit_status}: {detail}"
    return True, ""


# ---------------------------------------------------------------------------
# Verdict accumulation
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class Report:
    checks: int = 0
    failures: list[str] = dataclasses.field(default_factory=list)
    skips: list[str] = dataclasses.field(default_factory=list)
    # Per-cell achieved strictness. A green that does not record which streams
    # it compared is the defect this harness exists to remove, so the tier
    # travels with the verdict rather than being inferred from the run mode.
    tiers: dict[str, int] = dataclasses.field(default_factory=dict)

    def ok(self, message: str) -> None:
        self.checks += 1
        print(f"  PASS {message}")

    def fail(self, message: str) -> None:
        self.checks += 1
        self.failures.append(message)
        print(f"  FAIL {message}")

    def skip(self, message: str) -> None:
        self.skips.append(message)
        print(f"  SKIP {message}")

    def tier(self, label: str, achieved: int) -> None:
        self.tiers[label] = achieved

    def tier_summary(self) -> str:
        if not self.tiers:
            return "no parity cells recorded a tier"
        buckets: dict[int, int] = {}
        for achieved in self.tiers.values():
            buckets[achieved] = buckets.get(achieved, 0) + 1
        parts = [
            f"{count} at {tier_label(t)}"
            for t, count in sorted(buckets.items(), reverse=True)
        ]
        return f"{len(self.tiers)} parity cell(s): " + "; ".join(parts)


def require_divergence(
    report: Report,
    label: str,
    golden: Observation | None,
    mutated: Observation | None,
    tier: int = 1,
) -> None:
    """Assert that a planted mutation was CAUGHT at ``tier`` or below.

    Detection at a LOWER tier counts: a mutation caught by stdout is caught.
    What must not happen is a mutation invisible through ``tier``, because that
    is a field the harness cannot see and therefore cannot police.
    """
    if golden is None or mutated is None:
        report.fail(f"{label}: run timed out (golden={golden}, mutated={mutated})")
        return
    if golden.is_empty() and mutated.is_empty():
        report.fail(
            f"{label}: VACUOUS -- neither run emitted an identity payload, so "
            f"there was nothing a mutation could perturb"
        )
        return
    caught_at = golden.first_differing_tier(mutated)
    if caught_at is None or caught_at > tier:
        report.fail(
            f"{label}: VACUOUS at {tier_label(tier)} -- mutation changed nothing "
            f"the harness compares; field is not load-bearing "
            f"(both {golden.summary()})"
        )
    else:
        report.ok(
            f"{label}: divergence caught at {tier_label(caught_at)} "
            f"(golden {golden.summary()} != mutated {mutated.summary()})"
        )


def require_parity(
    report: Report,
    label: str,
    golden: Observation | None,
    candidate: Observation | None,
    min_tier: int = 1,
) -> int:
    """Assert clean candidate == golden ptrace reference, and RECORD THE TIER.

    Returns the achieved tier (0 on failure). ``min_tier`` is the floor a cell
    must clear to count as parity at all; the achieved tier is reported
    separately so a green carries what it actually verified rather than a bare
    PASS. That distinction is the whole point: the rejected predecessor's PASS
    did not say which streams it had looked at, and the answer was "two".
    """
    if golden is None or candidate is None:
        report.fail(f"{label}: run timed out (golden={golden}, candidate={candidate})")
        return 0
    # NON-VACUITY leg. Checked BEFORE equality, because empty == empty is the
    # exact shape that reports success while comparing nothing.
    if golden.is_empty() or candidate.is_empty():
        report.fail(
            f"{label}: VACUOUS -- no identity payload to compare "
            f"(golden {golden.summary()}, candidate {candidate.summary()}); "
            f"a run that emits nothing must not report parity"
        )
        return 0
    achieved = golden.achieved_tier(candidate)
    if achieved < min_tier:
        first = golden.first_differing_tier(candidate)
        report.fail(
            f"{label}: PARITY BREAK at {tier_label(first or min_tier)} "
            f"(achieved {tier_label(achieved)}, required {tier_label(min_tier)}) -- "
            f"golden {golden.summary()} != candidate {candidate.summary()}"
        )
        return 0
    report.tier(label, achieved)
    report.ok(f"{label}: parity at {tier_label(achieved)} ({golden.summary()})")
    return achieved


# ---------------------------------------------------------------------------
# Per-fixture drivers
# ---------------------------------------------------------------------------


def check_declared_fields(report: Report, name: str, spec: FixtureSpec) -> None:
    declared = set(spec.fields)
    in_source = source_declared_fields(spec)
    missing = in_source - declared
    inert = declared - in_source
    if missing:
        report.fail(
            f"{name}: field(s) {sorted(missing)} mutated in source but not "
            f"registered -- they would go unproven"
        )
    if inert:
        report.fail(
            f"{name}: registered field(s) {sorted(inert)} never appear in the "
            f"mutation seam -- they are inert"
        )
    if not missing and not inert:
        report.ok(
            f"{name}: declared fields {sorted(declared)} match the source seam exactly"
        )


def run_native(report: Report, name: str, binary: Path, spec: FixtureSpec) -> None:
    print(f"[native] {name}")
    # (b) run clean -> assert PASS (the fixture's own contract holds).
    clean = observe_native(binary, mutate=None)
    if clean is None:
        report.fail(f"{name} [native]: clean run timed out")
        return
    if clean.exit_status != 0:
        report.fail(f"{name} [native]: clean contract FAILED ({clean.summary()})")
        return
    if not clean.stdout.strip():
        report.fail(f"{name} [native]: clean run emitted no identity line")
        return
    report.ok(f"{name} [native]: clean contract holds ({clean.summary()})")
    # (a) plant a divergence per field -> assert FAILURE is caught.
    for field in spec.fields:
        mutated = observe_native(binary, mutate=field)
        require_divergence(report, f"{name} [native] mutate({field})", clean, mutated)


def check_ioctl_fionread_getfl_failure(
    report: Report, spec: FixtureSpec, output: Path
) -> None:
    failed_readback = dataclasses.replace(
        spec,
        cflags=spec.cflags + ("-DHERMIT_TEST_FIONREAD_SECOND_GETFL_FAILURE",),
    )
    observation = observe_native(compile_fixture(failed_readback, output), mutate=None)
    expected = b"clear_readback_ok=0 nonblock_after_clear=-1"
    if (
        observation is not None
        and observation.exit_status != 0
        and expected in observation.stdout
    ):
        report.ok(
            "ioctl_fionread [native] failed second F_GETFL cannot satisfy the clear check"
        )
    else:
        report.fail(
            "ioctl_fionread [native] failed second F_GETFL was not preserved as a failed check "
            f"({observation.summary() if observation else 'timeout'})"
        )


# ---------------------------------------------------------------------------
# CPU-placement leg
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Placement:
    """The one CPU the placement leg allows, and a CPU it forbids.

    ``forbidden`` is in this harness's own affinity mask, so the kernel would
    accept a request for it: only the shim can refuse it.
    """

    allowed: frozenset[int]
    forbidden: int


def choose_placement() -> tuple[Placement | None, str]:
    """Allow only the highest CPU this process may use; forbid the lowest.

    The lowest is CPU 0 wherever CPU 0 is available, and allowing a single CPU
    rules out every other fixed CPU number a fixture could ask for as well.
    """
    mine = os.sched_getaffinity(0)
    if len(mine) < 2:
        return None, (
            f"this process may use only CPU(s) {sorted(mine)}; the leg needs two, "
            f"one to allow and one to forbid"
        )
    return Placement(frozenset({max(mine)}), min(mine)), ""


def check_cpuset_shim(report: Report, placement: Placement, workdir: Path) -> bool:
    """Negative control: the shim refuses a CPU the kernel would accept.

    Also asks for the allowed CPU, so a shim that refuses everything is caught
    here rather than showing up as a red in every fixture. Returns whether the
    control passed; the fixture legs are not run on an unproven shim.
    """
    (allowed,) = placement.allowed
    control = FixtureSpec(
        source=CPUSET_SHIM,
        fields=(),
        cflags=("-DFIXTURE_CPUSET_SHIM_CONTROL", *CPUSET_SHIM_FLAGS),
    )
    observation = observe_native(
        compile_fixture(control, workdir / "cpuset-shim-control"),
        mutate=None,
        cpus=placement.allowed,
        args=(str(placement.forbidden), str(allowed)),
    )
    expected = (
        f"cpuset-shim-control cpu{placement.forbidden} rc=-1 errno={errno.EINVAL}\n"
        f"cpuset-shim-control cpu{allowed} rc=0 errno=0\n"
    ).encode()
    label = (
        f"cpuset shim [native]: refuses CPU {placement.forbidden} and accepts "
        f"CPU {allowed} when started on CPU {allowed}"
    )
    if (
        observation is not None
        and observation.exit_status == 0
        and observation.stdout == expected
    ):
        report.ok(label)
        return True
    report.fail(
        f"{label}: FAILED ({observation.summary() if observation else 'timeout'}; "
        f"expected stdout {expected.decode()!r})"
    )
    return False


def run_native_placement(
    report: Report, name: str, spec: FixtureSpec, placement: Placement, workdir: Path
) -> None:
    """The fixture's clean contract must hold with only ``placement.allowed``.

    Only the clean direction runs here. Whether a field is load-bearing does not
    depend on which CPUs the host allows, and run_native already proves it.
    """
    (allowed,) = placement.allowed
    variant = dataclasses.replace(
        spec, cflags=spec.cflags + (str(FIXTURES_DIR / CPUSET_SHIM), *CPUSET_SHIM_FLAGS)
    )
    clean = observe_native(
        compile_fixture(variant, workdir / f"{name}-placement"),
        mutate=None,
        cpus=placement.allowed,
    )
    label = f"{name} [native, only CPU {allowed} allowed]"
    if clean is None:
        report.fail(f"{label}: clean run timed out")
    elif clean.exit_status != 0:
        report.fail(f"{label}: clean contract FAILED ({clean.summary()})")
    elif not clean.stdout.strip():
        report.fail(f"{label}: clean run emitted no identity line")
    else:
        report.ok(f"{label}: clean contract holds ({clean.summary()})")


def run_hermit(
    report: Report,
    name: str,
    binary: Path,
    spec: FixtureSpec,
    hermit: Path,
    candidates: tuple[str, ...],
    require_backend: bool,
    min_tier: int = 1,
) -> None:
    print(f"[hermit] {name}")
    # Golden ptrace reference: the default comparison target.
    golden = observe_hermit(hermit, GOLDEN_BACKEND, binary, mutate=None)
    if golden is None or golden.exit_status != 0:
        report.fail(
            f"{name} [hermit/{GOLDEN_BACKEND}]: golden reference did not pass "
            f"({golden.summary() if golden else 'timeout'})"
        )
        return
    if golden.is_empty():
        report.fail(
            f"{name} [hermit/{GOLDEN_BACKEND}]: VACUOUS -- golden reference "
            f"emitted no identity line, so every candidate that also emits "
            f"nothing would report parity against it"
        )
        return
    report.ok(
        f"{name} [hermit/{GOLDEN_BACKEND}]: golden reference ({golden.summary()})"
    )

    # GOLDEN SELF-CONSISTENCY, and it is the positive control for the whole
    # tier ladder. Run the golden backend a SECOND time and require the top
    # tier. If ptrace cannot match itself through the INFO log, then TIER-3 is
    # unreachable by construction and every "achieved TIER-1" below would be an
    # artifact of the harness rather than a statement about the candidate.
    # A ladder whose top rung nothing can stand on is decoration.
    golden_repeat = observe_hermit(hermit, GOLDEN_BACKEND, binary, mutate=None)
    require_parity(
        report,
        f"{name} [hermit/{GOLDEN_BACKEND}] self-consistency",
        golden,
        golden_repeat,
        min_tier=MAX_TIER,
    )

    # Seam works through hermit + --env passthrough (prove the mutation is
    # observable under the golden backend before trusting it as a probe).
    for field in spec.fields:
        mutated = observe_hermit(hermit, GOLDEN_BACKEND, binary, mutate=field)
        # Same backend on both sides, so the INFO log SHOULD match apart from
        # the mutation: hold this leg to the strictest tier.
        require_divergence(
            report,
            f"{name} [hermit/{GOLDEN_BACKEND}] mutate({field})",
            golden,
            mutated,
            tier=MAX_TIER,
        )

    for backend in candidates:
        if backend == GOLDEN_BACKEND:
            continue
        available, reason = backend_available(hermit, backend)
        if not available:
            message = f"{name} [hermit/{backend}]: backend unavailable ({reason})"
            if require_backend:
                report.fail(message)
            else:
                report.skip(message)
            continue
        # (b) clean candidate run -> assert parity with golden ptrace.
        clean = observe_hermit(hermit, backend, binary, mutate=None)
        require_parity(
            report, f"{name} [hermit/{backend}] clean", golden, clean, min_tier=min_tier
        )
        # (a) plant a divergence in this backend -> assert it is caught.
        for field in spec.fields:
            mutated = observe_hermit(hermit, backend, binary, mutate=field)
            require_divergence(
                report,
                f"{name} [hermit/{backend}] mutate({field})",
                golden,
                mutated,
                tier=max(min_tier, 1),
            )


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Self-test: prove the harness can FAIL
# ---------------------------------------------------------------------------
#
# A mutation harness that cannot fail is the ultimate vacuous guard, so the
# harness brackets ITSELF. Each case plants a condition directly against the
# comparators and asserts the verdict, then a positive control asserts the same
# comparator still accepts a legitimate run — a checker that rejects everything
# passes every negative and is worthless.
#
# The four bracket cases are the ones the closure of PR #1641 named. Two of
# them (stdout-only, stderr-drift) were IMPOSSIBLE to express before, because
# Observation did not carry stderr at all.


def _obs(exit_status=0, stdout=b"id: 1\n", stderr=b"", info=b"") -> Observation:
    return Observation(exit_status, stdout, stderr, info)


def run_self_test(report: Report) -> None:
    print("[self-test] harness bracket cases")

    # --- NEGATIVE: omitted. A run that emits nothing must not report parity.
    require_parity(
        report, "self-test/omitted", _obs(stdout=b""), _obs(stdout=b""), min_tier=1
    )
    # --- NEGATIVE: inert. A mutation that perturbs nothing observable is a
    #     field the harness cannot police, not a passing field.
    require_divergence(report, "self-test/inert", _obs(), _obs(), tier=MAX_TIER)

    # --- NEGATIVE: stdout-only. The named predecessor defect: an exit-0
    #     mutation whose only effect is in stdout must be CAUGHT, and a
    #     candidate differing only in stdout must NOT report parity.
    require_parity(
        report,
        "self-test/stdout-drift",
        _obs(stdout=b"id: 1\n"),
        _obs(stdout=b"id: 2\n"),
    )

    # --- NEGATIVE: stderr-drift. Unexpressible before this change. Identical
    #     exit status and identical stdout, differing ONLY in stderr: the
    #     predecessor compared equal here and reported PASS.
    require_parity(
        report,
        "self-test/stderr-drift",
        _obs(stderr=b"warn: fallback\n"),
        _obs(stderr=b""),
        min_tier=2,
    )
    require_divergence(
        report,
        "self-test/stderr-mutation",
        _obs(stderr=b""),
        _obs(stderr=b"warn: fallback\n"),
        tier=2,
    )

    # --- NEGATIVE: info-log drift is invisible at TIER-2 and caught at TIER-3.
    #     This is what makes the tier a real distinction rather than a label.
    require_parity(
        report,
        "self-test/info-drift-at-tier3",
        _obs(info=b"INFO a\n"),
        _obs(info=b"INFO b\n"),
        min_tier=3,
    )

    # --- POSITIVE CONTROLS. Without these the negatives above are satisfied by
    #     a comparator that refuses everything.
    achieved = require_parity(
        report,
        "self-test/positive-identical",
        _obs(stderr=b"w\n", info=b"INFO a\n"),
        _obs(stderr=b"w\n", info=b"INFO a\n"),
        min_tier=MAX_TIER,
    )
    if achieved != MAX_TIER:
        report.fail(
            f"self-test/positive-identical: identical observations must reach "
            f"{tier_label(MAX_TIER)}, got {tier_label(achieved)}"
        )
    else:
        report.ok(f"self-test/positive-identical: reached {tier_label(MAX_TIER)}")
    require_divergence(
        report,
        "self-test/positive-divergence",
        _obs(stdout=b"id: 1\n"),
        _obs(stdout=b"id: 2\n"),
    )
    # --- POSITIVE: a candidate that matches through stdout but drifts in the
    #     INFO log must still report parity AT TIER-2 rather than be rejected.
    #     This is the case that keeps cross-backend cells usable.
    tier2 = require_parity(
        report,
        "self-test/positive-tier2-with-log-drift",
        _obs(info=b"INFO ptrace\n"),
        _obs(info=b"INFO dbt\n"),
        min_tier=2,
    )
    if tier2 != 2:
        report.fail(
            f"self-test/positive-tier2-with-log-drift: expected TIER-2, got {tier2}"
        )


def self_test_expectations() -> dict[str, bool]:
    """label -> True if that self-test case is EXPECTED to fail."""
    return {
        "self-test/omitted": True,
        "self-test/inert": True,
        "self-test/stdout-drift": True,
        "self-test/stderr-drift": True,
        "self-test/stderr-mutation": False,
        "self-test/info-drift-at-tier3": True,
        "self-test/positive-identical": False,
        "self-test/positive-divergence": False,
        "self-test/positive-tier2-with-log-drift": False,
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--hermit",
        type=Path,
        default=None,
        help="path to the hermit binary; enables cross-backend parity checks",
    )
    parser.add_argument(
        "--backend",
        action="append",
        dest="backends",
        default=None,
        help=f"candidate backend to check against golden {GOLDEN_BACKEND} "
        f"(repeatable; default {','.join(DEFAULT_CANDIDATES)})",
    )
    parser.add_argument(
        "--fixture",
        action="append",
        dest="fixtures",
        default=None,
        help="restrict to named fixture(s) (repeatable; default all)",
    )
    parser.add_argument(
        "--native-only",
        action="store_true",
        help="skip hermit cross-backend checks even if --hermit is given",
    )
    parser.add_argument(
        "--require-backend",
        action="store_true",
        help="treat an unavailable candidate backend as a failure, not a skip",
    )
    parser.add_argument(
        "--keep",
        action="store_true",
        help="keep compiled fixture binaries for inspection",
    )
    parser.add_argument(
        "--min-tier",
        type=int,
        default=1,
        choices=[n for n, _ in TIER_STREAMS],
        help="floor a cross-backend cell must clear to count as parity "
        "(1 exit+stdout, 2 +stderr, 3 +unstripped INFO log); the ACHIEVED "
        "tier is recorded per cell regardless",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="bracket the harness itself: prove each comparator FAILS on its "
        "planted negative and still accepts its positive control. Exits "
        "nonzero if any expectation is not met.",
    )
    return parser.parse_args(argv)


def run_self_test_mode() -> int:
    """Run the bracket cases and verify each produced its EXPECTED verdict."""
    report = Report()
    run_self_test(report)
    expectations = self_test_expectations()
    failed_labels = {msg.split(":", 1)[0].strip() for msg in report.failures}

    print()
    mismatches: list[str] = []
    for label, should_fail in sorted(expectations.items()):
        did_fail = label in failed_labels
        verdict = "OK " if did_fail == should_fail else "BAD"
        want = "FAIL" if should_fail else "PASS"
        got = "FAIL" if did_fail else "PASS"
        print(f"  {verdict} {label}: expected {want}, got {got}")
        if did_fail != should_fail:
            mismatches.append(f"{label}: expected {want}, got {got}")

    negatives = sum(1 for v in expectations.values() if v)
    positives = len(expectations) - negatives
    print()
    print(
        f"fixture-mutation self-test: {len(expectations)} bracket case(s) "
        f"({negatives} planted negative(s) that MUST fail, "
        f"{positives} positive control(s) that MUST pass); "
        f"{len(mismatches)} mismatch(es)"
    )
    if mismatches:
        for bad in mismatches:
            print(f"  MISMATCH {bad}")
        return 1
    print("  harness is PROVEN ABLE TO FAIL: every planted negative was refused,")
    print("  and every positive control was still accepted (so it is not inert).")
    return 0


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    if args.self_test:
        return run_self_test_mode()

    check_family_registry(FIXTURES)
    selected = args.fixtures or list(FIXTURES)
    unknown = [name for name in selected if name not in FIXTURES]
    if unknown:
        raise HarnessError(f"unknown fixture(s): {unknown}; known: {sorted(FIXTURES)}")
    if not selected:
        raise HarnessError(
            "no fixture selected; a run that examines nothing is not a pass"
        )

    candidates = tuple(args.backends) if args.backends else DEFAULT_CANDIDATES

    report = Report()
    workdir = Path(tempfile.mkdtemp(prefix="fixture-mutation-"))
    examined: list[Path] = []
    try:
        placement, no_placement = choose_placement()
        if placement is None:
            report.skip(f"cpuset shim [native]: {no_placement}")
        elif not check_cpuset_shim(report, placement, workdir):
            no_placement = "the cpuset shim control failed above"
            placement = None
        for name in selected:
            spec = FIXTURES[name]
            examined.append(spec.source_path())
            check_declared_fields(report, name, spec)
            binary = compile_fixture(spec, workdir / name)
            run_native(report, name, binary, spec)
            if placement is None:
                report.skip(f"{name} [native, CPU placement]: {no_placement}")
            else:
                run_native_placement(report, name, spec, placement, workdir)
            if name == "ioctl_fionread":
                check_ioctl_fionread_getfl_failure(
                    report, spec, workdir / f"{name}-getfl-failure"
                )
            if args.hermit and not args.native_only:
                if not args.hermit.is_file():
                    report.fail(f"hermit binary not found: {args.hermit}")
                else:
                    run_hermit(
                        report,
                        name,
                        binary,
                        spec,
                        args.hermit,
                        candidates,
                        args.require_backend,
                        args.min_tier,
                    )
    finally:
        if not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)

    print()
    print(f"fixture-mutation: examined {len(examined)} fixture(s):")
    for source in examined:
        print(f"  examined: {source.relative_to(REPOSITORY_ROOT)}")
    print(
        f"fixture-mutation: {report.checks} checks, "
        f"{len(report.failures)} failed, {len(report.skips)} skipped"
    )
    # A green must carry what it verified. Print the achieved strictness per
    # cell, not just a count of passes.
    print(f"fixture-mutation strictness: {report.tier_summary()}")
    for label, achieved in sorted(report.tiers.items()):
        print(f"  tier: {label} -> {tier_label(achieved)}")
    for skipped in report.skips:
        print(f"  skipped: {skipped}")
    for failed in report.failures:
        print(f"  failed:  {failed}")
    return 1 if report.failures else 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except HarnessError as error:
        print(f"fixture-mutation: {error}", file=sys.stderr)
        sys.exit(2)
