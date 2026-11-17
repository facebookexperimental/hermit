---
name: validation-evidence
description: "How to state a Hermit result: what counts as a backend, assurance levels L0 to L4 and the commands that establish them, the verify comparators, KVM L2 claims, and the backend, log level and relaxations every report names. Use before claiming that any Hermit run passes, is deterministic, or reaches a level, and when reviewing such a claim."
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
