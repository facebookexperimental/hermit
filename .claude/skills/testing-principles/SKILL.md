---
name: testing-principles
description: "Tool-agnostic testing rules that follow from running at high load: CPU-primary budgets with wall backstops, memory headroom and boxing, innermost counted retries with FLAKY harvested never silent, load-proof discipline, named skips and disabled tests, no weakened assertions, green-to-green landing. Use when writing, shrinking or reviewing tests, or auditing a repository's test setup. Tool application lives in testing-with-dagrun and testing-with-buck."
---

<!-- Generated from agent-utils 2ef1a9e49bfd9b6c3f1cb6001bdf8af35776790a skills/testing-principles/SKILL.md; do not edit. Change it in agent-utils, then run scripts/import-agent-utils-skills.rs. -->

# Testing principles

First principle: **our tests must always be able to run at high load.
We shouldn't have flaky tests and we should assume high load is
normal and test with high load from the start.** Everything below
follows from it. Repository-specific additions live in that
repository's own addendum skill, outside any vendored copy of this
one.

## The owner's directive (2026-11-21 11:24 AM ET)

The owner states the policy plainly:

> [owner directive, 2026-11-21 11:24 AM ET (relayed)] (1) A quiet
> host only for RARE benchmarks, and gates must NOT defer on
> host-quiet; every normal build, test and gate is designed to pass
> on a hot, high-load host (not flaky, not timing-sensitive). (2)
> Routine cost checks use LOAD-INVARIANT measures (CPU time,
> instructions executed, virtual time under a hermetic runner, counts of
> expensive events), NEVER wall time, which is dirty and unstable
> for most purposes (wall-time rows are report-only). (3) Memory
> coordination is good: don't start heavy work below a free-memory
> floor (#2062). (4) Outer caps are very generous livelock
> backstops, and inner caps catch real livelock.

Four consequences, made explicit below because earlier wording left
them implicit:

- **No deferral on a busy or loud host.** A build, test or gate does
  not wait for the host to quiet down before running. A quiet
  (low-load, low-noise) host is reserved for rare benchmarks whose
  quantity is inherently noise-bound (rare microbenchmarks and final
  calibration runs); name it per benchmark, never as a standing
  habit. Everything else is written to pass on a hot, high-load host.
- **Routine cost checks are load-invariant.** Budgets and gates
  measure things that do not move with whoever else is on the host:
  CPU time, instructions executed, virtual time under a hermetic
  runner, and counts of expensive events (DOM nodes
  touched, layout reads, network requests, retries). Wall time is
  never a gating measure; any wall-time row in a report is
  report-only and says so.
- **Memory coordination is the load limit that IS allowed.** Heavy
  work still does not start below a declared free-memory floor, and
  memory in use is boxed; the floor is measured in memory, not in
  who else is running, so it stays true on a hot host (#2062).
- **Caps are in two tiers.** The outer caps (wall backstops, host
  admission ceilings) are deliberately very generous: they exist to
  stop a livelock or a hung neighbour, never to budget ordinary
  work. The inner caps (per-test CPU and event budgets) are tight:
  they catch real livelock and real runaway work early.

- **Budgets are CPU-primary.** Wall time stretches under load; CPU
  time measures work. Budget CPU at about 1.5x the observed worst
  case, never above 60 CPU-seconds per test; wall time is only a
  deadlock backstop (8x expected, never above 600 s). Every timeout
  says whether it is wall or CPU. Memory limits get the same
  headroom: about 1.5x the measured cold peak, never hovering at the
  limit; builds and test steps are memory-boxed, and host admission
  caps total admitted load so no neighbour can eat the headroom. A
  new test must fit the per-test caps of the node it joins.
- **Retries are innermost, infrastructure-only and counted.** Never
  retry an assertion to make it pass; never rerun a whole queue. The
  one accepted blanket exception is a test-runner retry (for example
  nextest `retries = 2` in validate profiles) under all of: every
  FLAKY result is recorded in the run record, the ledger and the
  scorecard and harvested into global retry statistics; flaky never
  blocks landing and never passes silently; a nightly fixer takes
  everything that flaked in the last 24 hours; health warns on any
  unowned non-zero flaky count. A retry that eventually passes is a
  FLAKY result to fix, not a clean pass.
- **No assertion depends on timing.** Synchronize on observable
  state, not sleeps. Run synthetic-load proofs only inside a
  CPU-capped cgroup (about 64 spinners at most) and only at low host
  load; never flood a shared host.
- **Skips and disables are named.** A test that cannot run here is a
  capability-gated skip naming the missing capability, never a silent
  pass. A known-failing test is disabled by name with a P1 tracked
  task, never left red or quietly tolerated.
- **No goalpost moving.** Never meet a test by weakening its
  assertion, tolerance, comparator, label or gate. When shrinking a
  slow test, preserve the coverage it exists for.
- **No exact test/cell count pins.** Track per-test
  pass/fail/skip/omit; report totals without asserting them; a test
  that vanishes unlisted fails. (While a repository still has pins,
  its own rules govern declaring decreases.)
- **Main stays green-to-green through a gate; a red main is a P0.**
  Work the tight loop on the failing test; let validation confirm,
  not search.
- **Environment is hermetic and pinned.** Validate inside a base
  image reproducible from tracked, pinned sources (versions plus
  hashes, image definition in the repository); for open-source work
  prefer Nix closures. Keep the base minimal and layer what each test
  needs; deliver as an OCI directory or tarball.

## Compliance checklist

Run against a change or a whole repository: every new/changed test
ran under artificial load before landing (`taskset` plus spinners, or
`stress-ng`, inside the cgroup rule above); every timeout says wall
or CPU and fits the caps; every memory limit is ~1.5x its measured
cold peak, steps are boxed, admission caps the host; every skip names
its capability; every disabled test names a P1 task; retries are
innermost and counted, FLAKY recorded in run record, ledger and
scorecard; assertions not weakened; the change reaches a shared main
only through its gate; validation ran in the pinned image; counts name
what actually ran.

Also check, per the owner's 2026-11-21 directive:

- No gate defers on host-quiet; a quiet host is used only by a named,
  rare benchmark.
- Every routine cost check is load-invariant (CPU time, instructions,
  virtual time under a hermetic runner, or event counts); any wall-time figure
  in the same report is marked report-only and gates nothing.
- Heavy work below the free-memory floor (#2062) does not start.
- Outer backstops are generous and inner budgets are tight, with
  both tiers named where they are set.
