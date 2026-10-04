---
name: code-elegance
description: "Find and fix code smells that hurt correctness and maintainability: conditional hacks, determinism-weakening shortcuts, backend escape hatches, oversized functions/argument lists, deep nesting, and low-value slow tests. Use when auditing existing code for quality, deciding what to refactor next, or reviewing whether a change is a principled fix or a bandaid."
---

# Code Elegance

**Elegance and correctness are the same audit, not two separate ones.** In a
determinism engine, an ugly conditional is rarely "just style" — it is usually
where a determinism gap, a backend divergence, or an untested edge case is
hiding. Treat every smell you find as a lead on a possible bug, and treat every
correctness fix as an opportunity to leave the code more principled than you
found it.

This skill does not replace the sibling skills — it tells you when to reach for
them. It is the standing charter for the `hermit-deslop` sweep: find one
high-value smell, fix it with a real regression test, land it as one small PR,
repeat.

Every count and command in this file was measured against this tree; the
"measured" annotations name the date so a later reader can tell a stale number
from a fresh one. **Re-run a recipe before trusting its number.**

## Core principles

1. **Eliminate conditional hacks.** A `if (special_circumstances) { do_hack();
   }` pattern is a code smell almost by definition: it means the general path
   doesn't actually handle the general case, and something upstream is papering
   over that gap. Find the real invariant and make the code satisfy it
   unconditionally, or make the special case a first-class, named, tested
   branch of the design — not a silent carve-out.
2. **Never let elegance work weaken a determinism check.** This project has
   precise, hard-won definitions of what deterministic execution means (see
   [`continuous-virtual-time-is-sacred`](../continuous-virtual-time-is-sacred/SKILL.md)
   and the assurance ladder in
   [`hermit-debugging`](../hermit-debugging/SKILL.md)). Refactoring code near a
   determinism check is exactly where an agent is tempted to "simplify" by
   stripping the nondeterministic fields it's comparing. **That is cheating,
   not simplifying** — fake green, not a fix. The canonical failure mode: an
   agent "fixes" a failing detcore log comparison by filtering out the very
   fields that would show nondeterminism, so the diff always passes. If a
   determinism-adjacent test is failing, that is a **finding to report**, not
   an obstacle between you and a merge.

   The same trap exists *without touching any comparator code*: bare `--verify`
   uses the lossy `Stripped` comparator and **cannot establish L2**
   (`AGENTS.md:232`). Only `--verify-strict` compares under `BitwiseInfoV1`.
   Quoting a `--verify`-only pass as an L2 result is a fake green by reporting
   rather than by code.

   **The final success line alone is ambiguous**: measured 2026-08-09, both
   `hermit run --strict --verify -- /bin/echo hello` and the same command with
   `--verify-strict` end in the identical line `:: Success: deterministic.
   Determinism verified.` The surrounding console output does differ (the strict
   run says it is canonicalizing host addresses and comparing INFO messages; the
   lossy one says it is normalizing known nondeterministic numerical data and
   comparing DETLOG messages), but that is prose you have to read correctly.
   `--verify-json` is the authoritative machine-readable evidence:

   ```
   # --verify --verify-strict
   {"verified":true,"bitwise_parity":true, "comparison":{"strictness":"canonical",
    "log_scope":"info","strip_lines":false,"exact_remainder":true, ...}}

   # --verify alone
   {"verified":true,"bitwise_parity":false,"comparison":{"strictness":"stripped",
    "log_scope":"deterministic","strip_lines":true,"exact_remainder":false,
    "stripped_prefixes":[...,"unsafe-numeric-address-and-path-normalization/v1"]}}
   ```

   So **always pass `--verify-json` and read `bitwise_parity`**; never rest an
   L2 claim on the success line. Use the canonical form in workflow step 5.
3. **Respect backend modularity — no private escape hatches.** Per
   [`AGENTS.md` § Backend Definition](../../../AGENTS.md) (`AGENTS.md:160`),
   every real backend runs through the same `Detcore<XxxGuest>` path and the
   same shared Detcore code. A backend must not grow a special code path that
   only it takes to dodge the shared contract.

   The discriminator between a legitimate per-backend branch and an escape
   hatch:

   | Legitimate divergence | Unprincipled escape hatch |
   | --- | --- |
   | Differs in **mechanism** — how this backend traps a nondeterministic instruction | Differs in **guarantee** — this backend skips a check the others perform |
   | Still reaches the shared `Detcore<XxxGuest>` path | Bypasses Detcore, or reimplements a determinization locally |
   | The limitation is **declared and reported** in the result (AGENTS.md's KVM `--verify` output-only fallback reports `bitwise_parity: false`) | The limitation is silent, and the weaker path still reports the same verdict as the strong one |

   A `match backend { ... }` that dispatches to backend-specific
   *implementations* of the *same* contract is fine. A branch that gives one
   backend weaker guarantees than the others, **without saying so in the
   verdict**, is the smell. For time/clock/scheduling in particular, read
   [`continuous-virtual-time-is-sacred`](../continuous-virtual-time-is-sacred/SKILL.md):
   a backend that "achieves parity" by rounding, freezing, coarsening, or
   resetting virtual time is faking it.
4. **Identify and address smells; also take tasks from the backlog.** You can
   originate work by reading code (see "Finding candidates" below), or by
   picking up a flagged item from `mb`/`tg` backlog notes. Either way, land it
   as one focused PR.
5. **Add a regression test at the lowest useful layer.** `AGENTS.md:274` says
   "Add a regression test at the lowest useful layer"; the sibling
   [`repo-cleanliness`](../repo-cleanliness/SKILL.md) skill (line 29) says "at
   the narrowest useful layer". Both are binding, and they agree: put Detcore
   unit and integration behavior under `detcore/`, and a guest program under
   `tests`/`flaky-tests`. **Escalate to an end-to-end `hermit` CLI test under
   `hermit-cli/tests/` only when the bug lives in the user-visible CLI contract
   and no lower layer can observe it** — an e2e cell is the most expensive tier
   in CI, so spending one is a decision, not a default. Add the regression test
   *before or alongside* the fix, so it fails on the old code.

   (This is deliberately the opposite default from "prefer end-to-end tests".
   Principle 6 exists to cut CI wall-clock; reaching for e2e by default would
   fight it. Reach down first, escalate on evidence.)
6. **Shrink or re-tier low-value tests — do not delete one without a proof.
   Start with the slowest.** A test suite's value is its power-to-weight ratio:
   coverage per second of CI time. When auditing, sort by wall-clock cost first
   — the most expensive tests are where a trivial or redundant one costs the
   most to keep. The sanctioned outcomes are **shrink** (same surface, less
   work) and **re-tier** (`occasional = true`), both covered by
   [`test-shrink-optimization`](../test-shrink-optimization/SKILL.md).

   **Deletion is a separate, higher gate that no sibling skill covers** — see
   "Deleting a test" below. `AGENTS.md:481` lists tests "`#[ignore]`d, masked,
   or deleted to make a checkout look green" under **Not done**.
7. **Improve modularity and generality.** Fix functions with too many
   arguments (bundle related parameters into a struct, or split the function).
   Fix functions that are too deeply nested (extract a helper, invert a
   condition to return early, replace a nested `if`/`match` pyramid with a
   flat dispatch).

## Finding candidates

Run these from the repository root. They are heuristics for *where to look*,
not verdicts — read the hit before deciding it's a smell (see "NOT a smell"
below).

### Deep nesting

```bash
# Lines whose block-opening keyword sits 20+ columns in (~5 levels at 4-space
# indent). Sort by match count to find the worst offenders first.
rg -c --type rust '^\s{20,}(if |match |for |while )' | sort -t: -k2 -rn | head -20
```

Measured 2026-08-09, top of the list: `hermit-cli/tests/relaxed_flag_matrix.rs`
(8), `detcore/src/tool_global.rs` (8), `detcore/src/syscalls/files.rs` (8),
`detcore/src/scheduler.rs` (8), `detcore/src/lib.rs` (8). The first of those is
a **false positive** — see "NOT a smell".

### Long functions

There is no `clippy::too_many_lines` lint enabled in this workspace yet, so
locate function boundaries by hand or with a quick script:

```bash
# List every fn signature with its line number; eyeball the gap to the next
# fn in the same file as a proxy for function length.
rg -n --type rust '^\s*(pub(\(\w+\))? )?(async )?fn \w+' <file>
```

For a repo-wide pass, prefer `cargo clippy` first (below) — it already flags
excessive argument counts, and manual length-scanning is best reserved for a
file you already suspect after reading it.

### Wide argument lists

```bash
# Every existing opt-out is a pre-identified candidate: someone already hit
# the default clippy::too_many_arguments threshold (7) and suppressed it
# instead of restructuring.
rg -n 'allow\(clippy::too_many_arguments\)' --type rust

# Full lint run surfaces new ones that aren't yet suppressed:
cargo clippy --workspace --all-targets -- -D warnings
```

Measured 2026-08-09: 8 suppression sites, in `detcore-dbt/src/lib.rs` (3),
`hermit-cli/src/bin/hermit/backends.rs` (2), `ci/manifest-plan/src/main.rs`,
`scripts/validate.rs`, and `scripts/lib/validate_plan.rs`.

### Conditional special-casing / hacks

```bash
# Self-admitted hacks and workarounds in comments (16 hits, 2026-08-09):
rg -ni 'hack|workaround|kludge|special.?case' --type rust
```

Code that behaves differently under test, for no product reason, is a classic
smell. There are two distinct spellings and only one of them yields candidates
here — do not conflate them.

```bash
# CANDIDATES: `#[cfg(test)]` gating something that is NOT a module — a test-only
# fn, field, or statement compiled into production code. (The atomic group skips
# intervening attributes such as `#[path = ...]`, so a cfg-gated `mod` reached
# through one is correctly excluded.)
rg -U --pcre2 -c \
  '#\[cfg\(test\)\]\n(?>(?:[ \t]*#\[[^\n]*\]\n)*)[ \t]*(?!(?:pub(?:\([a-z:]+\))? )?mod\b)' \
  --type rust | sort -t: -k2 -rn
```

Measured 2026-08-09 — 16 sites, of 116 `#[cfg(test)]` attributes in total (the
other 100 gate a module declaration, 76 of them the plain `mod tests`):

```
scripts/check-reverie-pin.rs:4
detcore/src/scheduler.rs:4
detcore/src/preemptions.rs:3
hermit-verify/src/cli_wrapper/run.rs:1
hermit-cli/src/recorder/fs.rs:1
detcore/src/scheduler/runqueue.rs:1
detcore/src/logdiff.rs:1
ci/manifest-plan/src/main.rs:1
```

These are **leads, not verdicts** — most are legitimate test affordances
(test-only constructors and inspectors in determinism-critical code). Read the
hit; do not treat the list as a work queue.

**Re-checking the clean axis.** The runtime macro `cfg!(test)` — an actual
production branch keyed on the test cfg — is the sharper smell, and this tree
currently has none:

```bash
rg -n 'cfg!\(test\)' --type rust   # expected 2026-08-09: no output, exit 1
```

Exit 1 with no output is the *good* result and is not a candidate search; any
hit is the finding. Keep the two spellings straight: `rg 'cfg\(test\)'` (no `!`)
matches the attribute, which is overwhelmingly the ordinary `#[cfg(test)] mod
tests { ... }` idiom.

```bash
# Backend-keyed branches. Note the exact enum spelling: the variants are
# Ptrace / Kvm / Dbt / Sabre / Liteinst / E9patch (hermit-cli/src/lib.rs:595).
# ripgrep is case-sensitive, so `LiteInst` matches 0 and silently hides the
# backend you were looking for. There is no `E9patch`-less shortlist: e9patch
# and liteinst are the two most likely to need a carve-out.
rg -c 'Backend::(Ptrace|Kvm|Dbt|Sabre|Liteinst|E9patch)' --type rust \
  | sort -t: -k2 -rn
```

Measured 2026-08-09 — 102 hits across exactly six files, **all of them the
expected enum definition plus CLI dispatch surface**:

```
hermit-cli/src/lib.rs:52
hermit-cli/src/bin/hermit/run.rs:34
hermit-cli/src/bin/hermit/main.rs:10
hermit-cli/src/backend_stats.rs:3
hermit-cli/src/bin/hermit/record_start.rs:2
hermit-cli/src/bin/hermit/strace.rs:1
```

Those six are the baseline, not suspects. **What is worth a second look is a
`Backend::` branch appearing anywhere else** — especially inside `detcore/`,
`detcore-model/`, or a Reverie guest crate, where the shared determinism code
is supposed to be backend-agnostic. Diff the file list against the six above
rather than eyeballing it.

(Do not filter this with `grep -v -E 'backends?\.rs'`: the only path that
matches is `hermit-cli/src/bin/hermit/backends.rs`, which has **0** `Backend::`
hits, so the filter removes nothing and just hides the fact that the real
dispatch files are present.)

### Finding slow, low-value tests

nextest's `final-status-level = "slow"` in `.config/nextest.toml` means "this
test exceeded `slow-timeout.period`" — 300s on the default profile, 600s for
four named stress overrides. It does **not** mean "the N slowest tests". If
nothing crosses the threshold that block is empty, so `| tail -n 60` of a run
is not a ranking. Rank from the `ci` profile's JUnit timings instead:

```bash
# Order-robust on purpose: nextest writes name/classname/timestamp/time, while
# the Rust test-harness writes classname/name/time. Extract each attribute
# independently instead of assuming a fixed order. (`\bname="` does not match
# inside `classname="`, and `\btime="` does not match `timestamp="`.)
rank_junit() {
  local tc; tc=$(mktemp)
  rg -o '<testcase [^>]*>' "$1" > "$tc"
  paste <(sed -E 's/.*\btime="([^"]*)".*/\1/'      "$tc") \
        <(sed -E 's/.*\bclassname="([^"]*)".*/\1/' "$tc") \
        <(sed -E 's/.*\bname="([^"]*)".*/\1/'      "$tc") \
    | sort -rn | head -20
  rm -f "$tc"
}

rank_junit target/nextest/ci/junit.xml
```

**Do not reach for `cargo nextest run --workspace --profile ci` as the default
way to feed it.** Measured 2026-08-09, `cargo nextest list --workspace` (1.6s
with everything already built) reports **1,185 tests across 130 binaries**, and
`.config/nextest.toml` pins `package(=hermit) & kind(=test)` to the
single-threaded `hermit-serialized` group. An adversarial review of this skill
aborted such a run at 107s with 373 tests still unstarted — and an aborted run
gives you a partial, misleading ranking. Get the JUnit the cheap way instead:

```bash
cargo nextest list --workspace          # 1.6s: which suites even exist, and how big
cargo nextest run --profile ci -p <package>          # scope to one package
cargo nextest run --profile ci -E 'test(/<regex>/)'  # or one filter expression
```

or rank a JUnit that a CI run already produced, rather than regenerating it
locally at all.

Verified 2026-08-09 against `cargo nextest run --profile ci -p hermit-resources`,
which wrote `target/nextest/ci/junit.xml` and ranked as (seconds, suite, test):

```
0.006	hermit-resources	tests::invoked_symlink_location_finds_colocated_resources
0.006	hermit-resources	tests::explicit_install_directory_has_priority
0.006	hermit-resources	tests::empty_explicit_directory_is_rejected
0.005	hermit-resources	tests::release_binary_finds_target_install_package
```

For E2E manifest cells, the Rust harness emits its own JUnit
(`ci/manifest-plan/src/runner.rs` `write_junit`):

```bash
cargo build -p hermit-manifest-plan --bin test-harness
target/debug/test-harness run --lane portable \
  --test c-programs/add-key-enosys --mode verify --backend ptrace \
  --junit /tmp/hermit-e2e.xml
rank_junit /tmp/hermit-e2e.xml
```

Verified 2026-08-09 on a single cell
(`--test c-programs/add-key-enosys --mode verify --backend ptrace`):

```
0.191	c-programs	c-programs/add-key-enosys/verify/ptrace
```

Once you have a ranked list, read the slowest entries first and ask: does this
test exercise unique syscall/scheduler/backend surface, or does a cheaper test
already cover the same ground?
[`test-shrink-optimization`](../test-shrink-optimization/SKILL.md) then gives
you the shrink methodology and the acceptance gates, and the choice between
keeping the entry in the regular manifest and marking it `occasional = true`.

### Deleting a test

`test-shrink-optimization` deliberately stops at shrink-vs-re-tier; it offers
no deletion path, and neither does any other sibling skill. If you believe a
test should be *removed* rather than shrunk or re-tiered, that is a
coordinator-approved decision requiring **both** of the following in the PR
description:

1. **Name the bug the test was added for**, and say why that bug can no longer
   regress (covered elsewhere, code deleted, invariant now enforced
   structurally). Local git alone is sufficient — the introducing commit's
   subject normally already carries its `(#NNNN)`:

   ```bash
   git log -S '<test fn or guest program name>' --oneline -- <path>
   # the OLDEST hit is the introducing commit; its subject usually ends in (#NNNN)
   ```

   Verified 2026-08-09:

   ```
   $ git log -S 'verify_strict_info_reports_typed_memory_parity_on_landed_fixture' \
       --oneline -- hermit-cli/tests/hermit_modes.rs
   38cf5373 verify: keep BitwiseInfoV1 within the INFO envelope (#1661)
   ```

   *Optional, network-permitting:* if the subject carries no PR number, resolve
   it with `with-proxy gh api "repos/rrnewton/hermit/commits/$SHA/pulls" --jq
   '.[].html_url'`. **Treat this as a convenience, never as the gate** — GitHub
   API egress is agent-dependent on this fleet. The same call that succeeds for
   one agent returns `Forbidden. Your destination may have been blocked by a
   destination filter.` for another (both observed 2026-08-09 on this SHA). If
   you cannot reach the API, `git log -S` plus the commit subject discharges
   this step; a gate you cannot execute is a gate that gets skipped.

2. **Prove no coverage is lost**, with a clean `--fail-on-loss` diff:

   ```bash
   scripts/hermit-code-coverage.rs collect --name <suite>-with -- <args>
   scripts/hermit-code-coverage.rs collect --name <suite>-without --no-build -- <args>
   scripts/hermit-code-coverage.rs diff \
     --baseline <suite>-with --candidate <suite>-without --fail-on-loss
   ```

Without both, shrink or re-tier it. Deleting or `#[ignore]`-ing a test to make
a checkout look green is explicitly **Not done** (`AGENTS.md:481`).

## NOT a smell

Do not "fix" these — they are intentional design, and reverting them is a
regression, not an elegance improvement:

- **A per-backend implementation of a shared interface.** `KvmGuest`,
  `PtraceGuest`, `SabreGuest`, etc. are *supposed* to differ in how they trap a
  nondeterministic instruction. The smell is a backend skipping a guarantee
  the others provide, not a backend having its own code (principle 3's table).
- **`#[cfg(test)] mod tests { ... }`.** The standard Rust idiom for compiling a
  test module out of production builds, and 100 of this tree's 116 `#[cfg(test)]`
  attribute sites (76 of them spelled exactly `mod tests`). Test-only
  constructors and inspectors in `detcore/src/scheduler.rs`,
  `detcore/src/preemptions.rs`, and `detcore/src/logdiff.rs` are in the same
  category. The smell is the runtime macro `cfg!(test)` changing production
  behavior — currently 0 hits.
- **An exhaustive combinatorial test matrix.** Nested `for` loops that
  enumerate a flag cross-product are not "deep nesting" — they are the natural
  shape of a matrix test.
  `hermit-cli/tests/relaxed_flag_matrix.rs:97-98` is the deep-nesting recipe's
  joint top hit and is exactly this: `for virtualize_cpuid in [true, false] {
  for verify in [false, true] {` inside a strict/sequentialize/deterministic-io
  /time-metadata product. Flattening it would *lose* coverage.
- **A long `match` enumerating syscalls or event kinds.** An exhaustive
  dispatch table over a closed enum is not "deep nesting" or a "hack" — it's
  the natural shape of a syscall handler. Don't flatten it into indirection
  for its own sake.
- **Fail-closed handling of an unsupported syscall/feature.** Code that
  refuses (panics/errors) rather than silently guessing at nondeterministic
  behavior is a deliberate determinism guardrail, not defensive-programming
  clutter. See `syscall-classification-two-lists-and-failclosed-gating`
  context in the parent workspace memory before "simplifying" a fail-closed
  branch away.
- **Hardware-sensitive tests that fail on *your* host.** PMU/RCB-dependent
  preemption tests, CPUID-interception tests, and the Detcore `tests_misc`
  RDRAND/RDSEED cases can fail in a restricted container or VM
  (`AGENTS.md:41-48`, `:94-96`). Under principle 6 they are the perfect false
  positive: slow *and* red on a restricted host. `AGENTS.md` is explicit — "Do
  not mark or delete a hardware-sensitive test merely to make a local VM
  green." Report the host limitation instead.
- **`--unsafe-strip-lines` and similar diagnostic-only normalizations**, used
  to *localize* a divergence in `hermit log-diff`. These are fine as debugging
  aids. They become a hack only if someone routes them into the actual
  parity/verification comparison to make a failing check pass — that crosses
  into principle 2 above.
- **A manifest entry marked `occasional = true`.** That is the sanctioned
  outcome of a test-shrink audit for an irreducibly expensive, high-value test
  — not an oversight to "fix" by deleting or disabling it. Measured 2026-08-24
  there are 349 `occasional: false` entries and 2 `occasional: true`
  (`tests/e2e/manifests/applications.yaml:134` and `:191`, both KVM
  load-sensitive probes). Note that **no manifest entry currently carries a
  `slow_reason`** — the key exists only as an optional field validated in
  `ci/manifest-plan/src/main.rs:534`. If an occasional entry lacks one, *add*
  the `slow_reason`; do not read its absence as permission to delete the test.
- **`#[allow(clippy::too_many_arguments)]` you cannot yet fix in scope.** It's
  a valid candidate for a *future* focused PR (principle 7), not something to
  silently delete the annotation for without doing the restructuring.

## Workflow for a sweep PR

1. Pick **one** smell or backlog item. Don't bundle unrelated cleanups — the
   owner's standing instruction is targeted improvements, not a mess-inducing
   giant refactor.
2. Reproduce/confirm the issue with the narrowest test or command that shows
   it (a unit test, or a minimal `hermit run` repro).
3. Fix it, preferring the version that removes the special case rather than
   adding a second one.
4. Add or extend a regression test at the **lowest useful layer** (principle
   5): `detcore/` for Detcore behavior, a guest program under
   `tests`/`flaky-tests`, and `hermit-cli/tests/` only when the contract under
   test is the user-visible CLI itself.
5. Run the narrowest relevant test target. If the change is anywhere near a
   determinism check, establish the level with the canonical L2 command —
   bare `--verify` cannot (principle 2):

   ```bash
   ./target/debug/hermit run --strict --verify --verify-strict \
     --verify-json /tmp/parity.json -- <program>
   # require JSON "bitwise_parity": true
   ```

   Then `cargo fmt --all -- --check` and
   `cargo clippy --workspace --all-targets -- -D warnings`.
6. Write the PR with **Summary**, **Determinism**, **Linux Semantics**, and
   **Validation** sections (per `AGENTS.md`); if the change touches
   backend-shared code, name which backends you validated it against, and
   state the assurance level with its backend rather than an unqualified
   "passes".
7. Send it for adversarial review — you do not land your own elegance PRs.

## Related skills

- [`repo-cleanliness`](../repo-cleanliness/SKILL.md) — keeps the *repository*
  free of misplaced files; this skill keeps the *code* free of smells. Run
  both checklists before committing. It is also the co-author of principle 5's
  "narrowest useful layer" rule.
- [`test-shrink-optimization`](../test-shrink-optimization/SKILL.md) — the
  methodology for **shrinking** a slow test or **re-tiering** it to
  `occasional = true` without losing coverage, including the power/weight
  vector and the acceptance gates. It does **not** cover deletion; for that,
  use the "Deleting a test" gate above.
- [`continuous-virtual-time-is-sacred`](../continuous-virtual-time-is-sacred/SKILL.md)
  and [`determinism-regression-debugging`](../determinism-regression-debugging/SKILL.md)
  — read before touching anything near a clock, scheduler, or parity
  comparison; they catalogue exactly the "fake green" moves principle 2 above
  forbids, and `continuous-virtual-time-is-sacred` is the reference for
  principle 3's "weaker guarantee" column when the guarantee in question is
  time.
- [`backend-reality-reviewer`](../backend-reality-reviewer/SKILL.md) — a
  **completion-claim audit** (B0–B4 scoring: does `--backend X` on `main`
  actually load `Detcore<XxxGuest>`, or does it bypass Detcore?). Use it to
  decide whether a backend path is real at all. It does not adjudicate whether
  an existing per-backend branch is principled — for that, use principle 3's
  table and `AGENTS.md` § Backend Definition.
- [`hermit-debugging`](../hermit-debugging/SKILL.md) — log-first diagnosis and
  the assurance ladder (L1–L4) for describing what a test actually proved.
