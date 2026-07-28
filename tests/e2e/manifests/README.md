<!--
Copyright (c) Meta Platforms, Inc. and affiliates.
All rights reserved.

This source code is licensed under the BSD-style license found in the
LICENSE file in the root directory of this source tree.
-->

# Centralized e2e test manifests (schema v2)

Status: **prototype / design proposal** (CI Overhaul v2). This directory defines
a centralized manifest format that supersedes the per-script
`# HERMIT_E2E_META_BEGIN … HERMIT_E2E_META_END` JSON comment blocks parsed today
by [`ci/test_harness.sh`](../../../ci/test_harness.sh). The v1 harness discovers
**only** `*.sh` under five hard-coded `tests/e2e/<category>/` directories and
finds exactly **12** annotated tests, while `tests/` contains **182 `.c`
programs** and **31 guest `.rs`** files that are exercised piecemeal by
hand-written `hermit-cli/tests/*.rs` integration tests. v2 makes every
executable test discoverable from one small set of declarative manifests.

## Why centralize

The embedded-comment approach has three structural problems:

1. **Discovery is coupled to a `.sh` wrapper.** Every test needs a bespoke
   `--prepare`/`--run` shell script even when the payload is a single `.c`
   program. That is why only 12 of 182 `.c` programs participate: the other
   170 are cc-compiled and asserted individually inside Rust integration tests,
   duplicating the "build the guest, run Hermit, diff the output" logic dozens
   of times.
2. **Metadata is scattered.** Each test's mode/backend policy lives in a JSON
   blob inside a shell comment. There is no single place to answer "which tests
   run under KVM?" or "why is DBI disabled for the threaded guests?".
3. **`WHY` is only at the mode level.** The v1 schema records a reason when a
   whole *mode* is disabled (`disabled_modes`), but it cannot say *why a
   specific backend* is disabled for a mode that is otherwise enabled. The real
   backend gaps (DBI aggregation, SaBRe `O_NONBLOCK` leak, LiteInst incomplete
   interception) are invisible to the manifest.

## Design goals

- **MODE outer, BACKEND inner, consistently.** Every mode declares a
  `backends_enabled` list and a `backends_disabled` table mapping each excluded
  backend to a one-line reason. No silent gaps.
- **Entries point at programs.** A test entry names a `program` path. The
  harness infers how to run it from the extension and builds `.c`/`.rs` guests
  implicitly — no wrapper script required. Short tests may inline a `direct`
  shell command instead.
- **K bucket manifests, not one monolith.** One `*.toml` per bucket
  (`determinism-stress.toml`, `system-utils.toml`, …). Buckets can map to CI
  lanes and be sharded across runners.
- **Faithful superset of v1.** Everything the 12 current tests express is
  representable, plus the two new modes (`naked` explicit, `custom`) and the
  per-backend `WHY`.

## File format

Each manifest is a TOML file. TOML is chosen over JSON because the manifests are
human-authored, benefit from inline comments (especially for `WHY` reasons),
and TOML is the Rust-native config format this repository already prefers.

```toml
schema = 2
bucket = "determinism-stress"        # matches the file stem; used as a CI shard key

[[test]]
id          = "determinism-stress/order-violation"
description = "Different chaos seeds expose distinct reproducible thread schedules"
lane        = "portable"             # portable | privileged
requires    = ["linux", "x86_64", "userns", "ptrace", "cc"]
timeout_seconds = 90
occasional  = false
# program: path relative to repo root. Extension selects the runner:
#   *.sh -> executed directly (supports the --prepare/--run protocol)
#   *.c  -> harness cc-compiles it implicitly, then runs the binary
#   *.rs -> harness rustc-compiles the guest, then runs the binary
# Instead of `program`, a short test may set `direct = "…shell one-liner…"`.
program     = "tests/e2e/determinism-stress/order-violation.sh"
observation = { status = true, stdout = true, stderr = false, artifacts = [] }

  # ---- MODE (outer) -> BACKEND (inner) --------------------------------------
  [test.modes.verify]
  backends_enabled = ["ptrace"]
  backends_disabled = { dbi = "…why…", kvm = "…why…", sabre = "…why…", liteinst = "…why…" }

  [test.modes.chaos]
  backends_enabled = ["ptrace"]
  seeds  = [0, 1]
  assert = { min_distinct = 2, min_passes = 1, min_failures = 1 }
  backends_disabled = { dbi = "…", kvm = "…", sabre = "…", liteinst = "…" }

  # A mode the test does not run is recorded here with an explicit reason, so a
  # missing mode is never a silent gap (mirrors v1 `disabled_modes`).
  [test.disabled_modes]
  replay = "Chaos witness reproduction is the required relation for this racy guest"
  naked  = "Native scheduling does not guarantee both outcomes within a fixed budget"
```

### Modes

`MODE` is the outer axis. Five modes are defined; `verify`/`chaos`/`replay`/
`naked` must each be either enabled (a `[test.modes.<mode>]` table) or listed in
`[test.disabled_modes]` with a reason. `custom` is opt-in and only appears when
used.

| Mode     | Meaning                                                                                   | Extra keys |
| -------- | ----------------------------------------------------------------------------------------- | ---------- |
| `verify` | `hermit run --strict --verify` — two runs must be bitwise-identical (L2).                  | —          |
| `chaos`  | `hermit run --strict --chaos --sched-heuristic=random --seed=S` — seeded schedule search. | `seeds`, `assert.{min_distinct,min_passes,min_failures}` |
| `replay` | `hermit record start --strict --verify` — record then replay+diff. ptrace-only today.      | —          |
| `naked`  | **Meta-CI sanity check**: run the program *without Hermit* N times to confirm it is genuinely nondeterministic. Not a determinism test; guards against a test that would pass vacuously. | `runs` (2–20), `assert.min_distinct` |
| `custom` | `hermit run` with test-specific extra args, for edge cases the standard modes cannot express. | `args` (extra hermit args), `assert.{runs,repeat_identical}` |

`naked` has no backend (it never loads Detcore); its inner axis is the native
host. All other modes carry the `backends_enabled` / `backends_disabled` pair.

### Backends (inner)

`BACKEND` is the inner axis: `ptrace`, `dbi`, `kvm`, `sabre`, `liteinst`.
`backends_enabled` is the list actually exercised; `backends_disabled` maps every
other backend to a concrete reason. Together they should cover all five backends
for each enabled non-`naked` mode, so an omission is a review-visible defect
rather than an accidental hole. Reasons should cite the tracking issue where one
exists (e.g. SaBRe `#1035`, LiteInst `#1047`).

### Program kinds and implicit builds

The harness dispatches on the `program` extension:

- **`.sh`** — executed directly with the existing `--prepare`/`--run` contract
  and the same `HOME`/`XDG_CONFIG_HOME`/`E2E_TMPDIR`/`E2E_FIXTURE_DIR`
  environment `ci/test_harness.sh` already provides. This keeps every one of the
  12 current tests working unchanged.
- **`.c`** — the harness compiles it implicitly
  (`cc -std=c11 -O2 -g -Wall -Wextra -Werror [-pthread]`) into the cell's
  fixture dir and runs the resulting binary. No wrapper script is needed, which
  is what unlocks the other ~170 `.c` programs.
- **`.rs`** — the harness compiles the guest with `rustc` (or a declared
  `cargo` target) and runs the binary. Covers `tests/rust/*.rs` and
  `tests/chaos/*.rs` guests.
- **`direct`** — an inline shell command for trivial cases, so a one-liner does
  not need a file at all.

A `build` table may override defaults when a program needs extra sources or
flags (see `system-utils.toml`'s use of `build.extra_sources`).

## Validation rules (harness contract)

A manifest loader must reject a manifest unless, for every `[[test]]`:

- `schema == 2` and `bucket` equals the file stem.
- `id` is unique across all manifests and begins with `<bucket>/`.
- exactly one of `program` / `direct` is set; a `program` path exists and its
  extension is one of `.sh` / `.c` / `.rs`.
- `lane` ∈ {portable, privileged}; `1 ≤ timeout_seconds ≤ 1800`.
- each of `verify`/`chaos`/`replay`/`naked` is present in `modes` **or**
  `disabled_modes` (never both, never neither).
- every enabled non-`naked` mode lists at least one enabled backend, each drawn
  from the five known backends, and its `backends_disabled` keys are disjoint
  from `backends_enabled`.
- `replay.backends_enabled ⊆ {ptrace}` (replay is ptrace-only today).

## Migration path

1. Land this format + the prototype manifests + `manifest-plan.rs` loader (this
   task).
2. Teach `ci/test_harness.sh` (or a Rust successor) to load `tests/e2e/manifests/
   *.toml` in addition to the embedded blocks, expanding each test into
   `(test × mode × enabled-backend)` cells exactly as `emit_required_plan` does
   today.
3. Port the 12 embedded-block tests into manifests and delete their comment
   blocks.
4. Fan the remaining `.c`/`.rs` guests into buckets, converting the bespoke
   `hermit-cli/tests/*.rs` build-and-run integration tests into manifest
   entries so their coverage is discoverable and de-duplicated.
5. A follow-up **audit task** confirms every executable test is reachable from a
   manifest (the coverage gate v1 lacked).

## Prototype contents

- [`determinism-stress.toml`](determinism-stress.toml) — `order-violation`
  (verify + seeded chaos) and `thread-contention` (explicit `naked` + verify,
  multi-`.c` build).
- [`system-utils.toml`](system-utils.toml) — `record-getpid` (verify + replay),
  `date-nanoseconds` (naked + verify, no build), and `clock-determinism`, a
  **new** entry that points directly at `tests/c/clock_determinism.c` (implicit
  build, no wrapper) and demonstrates the `custom` mode.
- [`manifest-plan.rs`](manifest-plan.rs) — a `rust-script` loader that parses the
  manifests, enforces the validation rules above, and prints the expanded
  execution plan. Run it with `./manifest-plan.rs` (or
  `rust-script manifest-plan.rs`).
