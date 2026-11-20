---
name: validation-evidence
description: "How to state a Hermit result: what counts as a backend, assurance levels L0 to L4 and the commands that establish them, the verify comparators, KVM L2 claims, what a validate run establishes, and the backend, log level and relaxations every report names. Use before claiming that any Hermit run passes, is deterministic, or reaches a level, and when reviewing such a claim."
---

# Validation evidence

`AGENTS.md` keeps the backend definition and the L0 to L4 table; this skill
holds the rules for claims built on them. For auditing a backend completion
claim use `backend-reality-reviewer`; for time and clock changes,
`continuous-virtual-time-is-sacred`.

## Backends

A **backend** is a complete execution path that loads the shared Detcore tool
through Reverie as `Detcore<XxxGuest>`, where `XxxGuest` is the
backend-specific guest implementation. Every real backend runs the same copy of
the Detcore determinism code; a separate reimplementation, or a command that
merely launches a program, is not a backend.

- `ptrace` is the default and best-tested path. DBT and KVM are real backends
  only where they execute this full Detcore tool path.
- e9patch is **not** a backend: it is binary-rewriting preprocessing used with
  the ptrace backend, whatever a CLI spelling such as `--backend=e9patch`
  suggests.
- A prototype, stub, launch adapter, preprocessing tool or compatibility shim
  is **not** a backend. Report it by its actual category and say whether it
  ever loads Detcore as a Reverie tool.
- There is one shared copy of Detcore behavior. Never describe backend-local
  syscall emulation or a second determinism engine as Detcore parity.

## Claims

Never make an unqualified "pass", "deterministic" or "L2" claim. Every result
names the exact backend and the exact test or guest command, with relevant
flags. For preprocessing and prototype results, say what executed underneath:
"e9patch preprocessing with the ptrace backend", not "e9patch backend".

A feature is **done** only when the exact test meets its declared level across
**all in-scope backends**. A pass on one backend is evidence for that backend
only.

Every reported run also states:

- **Backend**: `ptrace`, `DBT` or `KVM`.
- **Log level**: the `--log`/`RUST_LOG` level, or "default" when unset.
- **Relaxations**: any flag that weakens determinism, for example
  `--no-strict` or `--no-sequentialize-threads`, or "none". A non-strict
  result is not a determinism guarantee; label it and never present it as one.

Example: "passes at L2 (ptrace backend, default log level, no relaxations)".

## Levels and comparators

The ladder in `AGENTS.md` is cumulative; each level presupposes those below.
`--strict` refuses a run whose epoch would come from the host clock, so every
strict command passes `--epoch` or `HERMIT_EPOCH`; tests and E2E cells use
`detcore_model::config::CANONICAL_TEST_EPOCH`.

Default `--verify` uses the lossy `Stripped` comparator and cannot establish
L2. `--verify-strict` compares exit status, stdout and stderr byte for byte and
INFO events under the `BitwiseInfoV1` policy: it removes only the real
wall-clock prefix, ordinalizes host addresses while preserving identity, order
and aliasing, and compares the full remainder exactly. Virtual time,
retired-branch counts, syscall values, sizes, flags and other numeric payloads
are never stripped. State this canonical envelope rather than calling the raw
log files literally byte-identical.

## KVM

KVM `--verify` compares the retained internal logs as well as exit status,
stdout and stderr, under the same retained-log comparison. Claim L2 for a KVM
cell only when its strict typed report says `bitwise_parity: true` (which
requires syscall output-buffer hashes) and names nonzero compared INFO-message
counts. An unreadable or truncated comparison must refuse with
`verdict: no_result` rather than report a match. A backend's capability is never
evidence that a particular guest passed.

## What a validate run establishes

`validate.sh` runs `scripts/validate.rs`. Facts about it that bear on a claim
(checked 2026-11-19):

- **Commit anchoring.** Unless the gate is skipped (its skip flag, or an
  off-the-record run), it refuses a working tree with unstaged or untracked
  changes (exit 2). Staged-only changes run, but the record carries
  `commit_anchored: false`, which earns neither the tree-keyed cache nor a
  receipt. An off-the-record run
  (`--allow-local-off-the-record-run`) writes no ledger row, publishes no
  receipt and cannot be cited as validation evidence.
- **Selective by default.** A bare run executes only the nodes affected since
  the last green baseline, and falls back to the full label when no
  trustworthy baseline exists (one that is not an ancestor of `HEAD` counts as
  none) or selection fails. `--all` (`--full-run`) runs the complete suite and
  is the landing form. A selective pass never answers a full cache lookup or
  publishes a full receipt, even when it fell back to the full label.
- **Caps.** Every node in `ci/dag/validate.json` carries both a CPU-time cap
  and a wall cap, and a breach fails the run (except in nodes a level marks
  nonblocking); `--run-timeout` bounds the whole invocation. Success,
  failure, refusal, timeout and interruption all print a wall and CPU summary
  line.
- **No performance gate.** No performance baseline or timing-regression
  threshold fails a run; `--envelope-compare FILE` compares working-envelope
  counts and fails when one falls below the baseline. A green run says nothing
  about performance (`benchmark`).
- **E2E determinism goes through Hermit's verifier.** E2E manifest verify
  cells use `--verify-strict` unless the cell declares `comparator: stripped`,
  which is below L2 and recorded as a relaxation, so each cell's level comes
  from its own report. Some Rust integration tests compare two runs' output
  directly; that is an output-only comparison, below L2.
