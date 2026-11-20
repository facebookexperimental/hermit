---
name: compat-ratcheting
description: "Ratchet Hermit's Linux compatibility on every backend: what a compat-matrix cell is and how it is selected, flipped green, measured and published (the manifests, cells.json, the expected plan, SCORECARD.md, the compatibility tracker), the per-backend gVisor scoreboard, ptrace as the reference, the owner's standing rules for flipping and counting cells, cross-backend reproduction, the right fix at the right level, architecture-first sequencing, and the evidence each fix carries. Use for backend compatibility work in Hermit, Reverie or LiteInst: fixing a gap, flipping a cell, reporting counts, or reviewing such a change."
---

# Compatibility ratcheting

Compatibility ratcheting raises, one cell at a time, how much real Linux
behaviour Hermit gets right on every backend, without letting the backends
drift apart. Flipping cells green is the whole goal of a backend lane (owner,
2026-10-03). Per-backend fixes have accumulated divergence and duplicated logic
that violate DRY and the project's vision; this skill exists to stop adding
more (owner, 2026-11-19). It applies to backend development in Hermit, Reverie
and LiteInst (owner, 2026-11-19); Reverie states the layering half in its own
terms in its `reverie-layering` skill. The layering rules are the Architectural
principles in `docs/ARCHITECTURE.md`; what counts as a backend and how to state
a result is in `validation-evidence`.

## The compat matrix

- **A cell** is one test, mode and backend. Tests live in the schema-v3
  manifests `tests/e2e/manifests/*.yaml`: every test declares all five modes,
  and every mode other than `naked` splits the backend axis into enabled
  backends and disabled ones with a reason (`tests/e2e/manifests/README.md`).
  The manifests are the only source; `ci/sync-cell-config.sh` regenerates
  every file a cell flip derives from them.
- **Selection.** A cell is selected by full, that is required by full
  validation, when it is listed in `ci/expected-e2e-plan.json`. An applicable
  cell the plan does not list (for example an enabled backend with
  `ci: false`) is not selected by full; a `ci: false` cell's recorded reason
  stays visible.
  A disabled backend is not applicable. `SCORECARD.md` and
  `ci/compat-envelope/cells.json` hold this catalogue; selection is not a test
  result.
- **Measurement.** A full validate requires a fresh exact-HEAD pass for every
  selected cell; in the maintainers' environment its publisher then records
  the per-cell results in the `hermit_test_ledger` repository, from which the
  compatibility tracker (<https://hermetic-infra.org/compatibility/latest/>)
  is built. Selection counts and recorded results answer different questions:
  say which one a number is.
- **Host capability.** A cell whose host provably lacks a needed capability
  (for example `kvm`, or `cpuid-faulting` for `liteinst` and `in-guest-trap`)
  is `HOST-INAPPLICABLE`: it stays in the denominator and is never a pass.

## ptrace is the reference

- Any active mode must include ptrace, so the reference behaviour is
  established before another backend ratchets it.
- Cross-backend parity comes only from explicit ptrace-versus-backend
  comparisons over the parity population (`ci/compat-envelope/parity-cells/`:
  every test whose ptrace `verify` cell is selected). A same-backend
  `bitwise_parity` result establishes determinism, never parity, and
  cross-backend credit needs identical guest paths on both backends.
- The tracker's backend percentages use the population recorded green on
  ptrace as their denominator.
- LiteInst results are credited under the Subset rule in
  `docs/ARCHITECTURE.md`: the same test and mode must pass on in-guest-trap.

## The gVisor scoreboard

- **Corpus:** gVisor's syscall tests, vendored in
  `third-party/gvisor-syscall-tests/` and registered as the `gvisor` bucket in
  `tests/e2e/manifests/gvisor.yaml`. Every test enables `verify` on ptrace;
  KVM, LiteInst and in-guest-trap are enabled test by test.
- **Scoreboard:** passing binaries and cases per backend (ptrace, KVM,
  LiteInst, in-guest-trap), each with its denominator
  (`presenting-quantitative-data`), its level (`validation-evidence`) and its
  mode. Say whether a number comes from Hermit cells, with Detcore loaded, or
  from a standalone Reverie run with a no-op tool, which carries no Hermit
  assurance level.
- **Main work list:** divergence, meaning cases that pass on ptrace but fail on
  KVM, LiteInst or in-guest-trap.
- **Report at every increment.** Owner (2026-11-16): "what's the passing test
  count at! These key metrics I want to be updated on at every increment of
  work." Before reporting a change as landed, confirm it on `main` by freshly
  fetched ancestry (`contributing`). Separate what is new this run from the
  reconfirmed baseline, stating each denominator
  (`presenting-quantitative-data`).

## Flipping a cell green

- **A config edit flips a cell.** Owner (2026-10-03): "A backend lane should
  always be able to flip green a cell its working on just by editing a config
  file." Edit only the manifest mode (enable the backend, set its `ci` true,
  drop its disabled reason), run `ci/sync-cell-config.sh`, and commit the
  manifest with every file it rewrites. The diff of
  `ci/expected-e2e-plan.json` is the record; `ci/sync-cell-config.sh --check`
  and validation refuse drift. The script does not update the exact
  population pin in `scripts/validate.rs`, a known defect: until it is
  replaced, a flip that changes that population updates it in the same
  change.
- **Measure before promoting.** Measure an applicable cell the plan does not
  list with `ci/compat-envelope/pressure-test.rs run --test <test> --mode
  <mode> --backend <backend>` (or `test-harness run --include-manual`), and a
  not-applicable cell with the `--probe-disabled` selector. A passing
  measurement is evidence for a later manifest change, never a promotion by
  itself. A pressure sample is a promotion candidate only at exactly 10
  repetitions, every one passing on its first attempt.
- **The paved path.** Owner (2026-10-02): "We should never have a green cells
  count that omits a bunch of OTHER green cells that we know about." Put
  forward candidate cells, pressure-test them, promote the passers so full
  validate runs them, and count them in a newly published scorecard.
- **Ratchet by cell identity, never by count.** Moving a cell out of the
  selected plan is not a fix: `scorecard.rs update` refuses a selected cell's
  removal without `--allow-green-removal REASON`, and a red `super` cell is a
  regression. Do not add exact count pins (`testing-principles`).
- **Blockage is P0.** Owner (2026-10-03): "Ledger or CI blockage is a p0
  emergency to fix. Not passively accept." A lane's config flips never queue
  behind another lane's landing.

## Ratcheting tactics

The owner calls these "different ratcheting tactics" (2026-11-19).

- **Parity first.** Owner (2026-11-19): "we could look at simple test programs
  that fail parity with ptrace and fix the first things first. Try to expand
  the set of cells that have parity or the prefix of ones that partially do
  (partial credit)." Start from the simplest programs that diverge from
  ptrace and fix the first divergence first. Report the full-parity count
  beside the partial credit: the parity post-pass scores each comparison as
  the matching prefix before the first divergence,
  `credit = matched_prefix / max(left_len, right_len)`
  (`ci/manifest-plan/src/parity.rs`), which is 1.0 only for a full match.
- **Promote known passers:** the paved path above (owner, 2026-10-02).
- **Divergence from the scoreboard:** gVisor cases that pass on ptrace and
  fail elsewhere, above (owner, 2026-11-19).

## Cross-backend first

Before fixing any bug, reproduce it on every backend it could touch (ptrace,
KVM, LiteInst, in-guest-trap) and record the per-backend result in the Test
Plan (the `Validation` section of a pull request). Owner (2026-11-19): "If
there's a bug, say unshare errno bugs, it would be good to FIRST look to see if
it affects multiple backends then write the fix, often at the detcore level...
or backend specific, but make sure it's the right fix at the right level." A
bug in Detcore's shared logic hits every backend and gets one Detcore fix.

## The right fix at the right level

- Fix at the layer that owns the behaviour. Owner-approved rule (2026-11-19):
  "Linux semantics live in Detcore or in Hermit-independent crates that
  Detcore uses, never in a backend." A backend-specific fix is only for
  genuine backend mechanism.
- Behaviour Hermit models is written once, in Detcore or a crate it uses
  (principle 3). Kernel
  behaviour that a backend's injected syscalls reach belongs to the Linux
  provider under `Guest::inject`: the host kernel for ptrace and the in-guest
  backends, `reverie-linuxemu` for KVM (principle 2). It never goes in a
  backend crate.
- A feature a backend does not support never reaches the guest as a silent
  `ENOSYS` or other plausible errno: the backend records Reverie's
  `UnsupportedRefusal` with the errno it returns, and the run stops naming it
  (principle 6; the approved `--allow-unsupported-syscalls` opt-in prints the
  same refusal first).
- For cells that already pass under ptrace, ptrace's behaviour is the
  reference (owner, 2026-11-19: "we assume the ptrace is already correct").
  When a gap exists only on KVM or LiteInst, the fix makes the shared path
  serve that backend, refactoring the ptrace backend and Detcore where needed
  to "fix the gap PROPERLY", or makes that backend's Linux provider agree with
  Linux. Never add a parallel per-backend patch.
- Reviewers check "right fix at the right level" explicitly, beside the
  goalpost-moving check, and refuse a fix that makes a backend diverge unless
  it is pure mechanism (the approved exceptions in `docs/ARCHITECTURE.md`;
  `code-elegance`).

## Sequencing: architecture first, the ratchet never stalls

Owner (2026-11-19): "Prioritize getting a common architecture, DRY, correct
layering of abstractions. But then always get back to ratcheting intelligently
and never just let it stall." While cross-cutting architectural work is in
flight (in November 2026, the signal refactor and other consolidation,
including the targets listed under "Consolidation (October 2026)" in
`docs/ARCHITECTURE.md`), the ratchet is measurement, the per-backend
scoreboard, plus fixes for gaps that do not collide with the in-flight
refactors; colliding work queues behind them. Full-speed ratcheting on all
backends resumes once the cross-cutting architecture settles. Principle 7
(ratcheting waits until the foundation follows rules 1-6) and that
Consolidation section (new per-backend results are paused until the
consolidation is done) of `docs/ARCHITECTURE.md` still say otherwise; their
wording awaits the owner's update.

## Evidence each fix carries

The Test Plan has one reproduction line per backend the bug could touch,
naming the backend and the exact test or command as `validation-evidence`
requires, with the result before and after the fix. A backend that could not
run the case is listed with the reason, never left out.
