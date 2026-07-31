---
name: determinism-regression-debugging
description: "Fix a determinism REGRESSION — something that ran deterministically (or booted, or replayed) before and is now broken/wedged/nondeterministic. Differential method: bisect from a known-good SHA to the culprit, diff a known-good vs broken full log to the first divergence, classify step-back vs fix-forward, and never re-fake the broken behavior to make a test pass. Use when a previously-working run regressed; for first-time determinism bring-up use hermit-debugging instead."
---

# Debugging Determinism Regressions

**Thesis: a regression is a *difference*, so debug it differentially, not from
first principles.** Something worked at commit `G` and is broken at commit `B`.
You already have a known-good reference — use it. Bisect to the culprit and diff
a good run against a broken run; do not start by reading `scheduler.rs`.

This skill is the regression-specific overlay. The general mechanics —
`--log info`, DETLOG/COMMIT lines, `hermit log-diff`, the assurance ladder —
live in [`hermit-debugging`](../hermit-debugging/SKILL.md); read it for how to
capture and read a trace. Here we add only what changes when you have a
*before*: bisect, good-vs-broken trace-diff, culprit classification, and the
sacred-time trap.

**When to use this** vs [`hermit-debugging`](../hermit-debugging/SKILL.md):

- Use *this* skill when a specific run *regressed*: a demo that used to boot now
  wedges, a program that passed `--verify` now diverges, a replay that matched
  now desyncs. You have (or can find) a known-good SHA.
- Use *hermit-debugging* when a program has *never* been deterministic under
  hermit — there is no good reference to diff against.

## Step 1 — Bisect from a known-good SHA to the culprit

Get an exact `(good, bad)` SHA pair before touching source. Known-good anchors:
a pinned demo/release SHA, the parent gitlink last known to boot, a green
`--verify` in CI, or the commit before a suspect merge.

```bash
git bisect start
git bisect bad  <broken-SHA>
git bisect good <known-good-SHA>
# each step: rebuild, run the SAME repro, mark good/bad
git bisect run ./repro.sh     # if the repro exits 0=good / nonzero=bad
```

Keep the repro *identical* across steps — same guest command, same backend,
same flags, same host conditions. A load-sensitive wedge can bisect to the
wrong commit; pin down whether the symptom is a code change or the environment
first (a boot wedge that reproduces on the *good* SHA under the same host load
is not a code regression — see the demo5 case below). The output of Step 1 is
one culprit commit, not a hunch.

## Step 2 — Diff a known-good vs broken FULL log to the first divergence

This is the move that turns a multi-hour hunt into one diff. Capture a full
trace of the good run and the broken run, **redirect each to a file** — never
stream a full-log run to your terminal, it floods — then diff to the exact first
point they part.

```bash
# Global --log flag BEFORE the subcommand; stderr to a file (see hermit-debugging §0).
<good-hermit>   --log info run -- <repro>  2>/tmp/good.log
<broken-hermit> --log info run -- <repro>  2>/tmp/broken.log
wc -l /tmp/good.log /tmp/broken.log

# Prefer the built-in comparator — it normalizes known noise (pointers, tmp
# paths, /proc/<pid>, elapsed time) and reports the FIRST divergence:
hermit log-diff --strip-lines --syscall-history 5 /tmp/good.log /tmp/broken.log
```

When the two logs come from different binaries/versions and `log-diff` can't
pair them, fall back to canonicalizing (strip timestamps/PIDs/pointers) and
plain `diff`, then read **only the first divergence** — everything after it is
downstream noise. Anchor on the last matching `COMMIT turn`/`inbound syscall`
and the first line that differs: a diverging `(turn, dettid)` is a *schedule*
divergence; matching COMMITs with a differing DETLOG value is an *unvirtualized
source*. This is exactly how the demo5 boot wedge was localized — a good boot
(~7.4M lines) vs a broken one (~347k lines) diffed straight to the first
divergence at the starved QEMU thread's syscall, instead of guessing in the
scheduler. See the case study:
`ai_docs/demo5-good-vs-broken-trace-diff-divergence_20260731.md` in the
`dev-hermit` parent workspace.

## Step 3 — Classify the culprit: step-backward vs latent-bug-exposed

The culprit commit is one of two kinds, and they get opposite fixes:

- **Step-backward (accidental regression).** The commit did not intend to change
  scheduling/time/determinism but did — a refactor that dropped a case, a
  reordering, an off-by-one in virtual time. Fix = **revert or a tight targeted
  fix** that restores the prior deterministic behavior. Add a regression test so
  it can't silently come back.
- **Wanted change that exposed a latent bug (fix-forward).** The commit is
  correct and desirable, and it merely *surfaced* a pre-existing defect (a race,
  a clock-domain split, an unhandled wakeup). Reverting the good change hides the
  bug again. Fix = **fix forward** on the real defect; keep the culprit commit.
  (Example: broadening exec handling that exposed a pre-existing scheduler
  tentative-pop race — the exposure is not the bug.)

Decide with evidence from Step 2: does the culprit's diff *mechanically* explain
the first divergence, or does it just change timing/ordering enough to reveal
something older? Say which class in the handoff; it dictates revert vs fix.

## Step 4 — The sacred-time trap: never re-fake the broken behavior

The most damaging regression "fix" is one that restores a *fake* determinism.
Virtual time and virtualized results are sacred; do not blunt them to make a
test pass. This is the same rule as
[`continuous-virtual-time-is-sacred`](../continuous-virtual-time-is-sacred/SKILL.md)
— read it for the full anti-pattern catalogue (freezing/quantizing time,
per-exec resets, first-read-epoch parity). The regression-specific pitfalls:

- **Do not re-introduce the broken behavior to satisfy an assertion.** If a test
  now fails because it was asserting the *old, wrong* behavior (the classic
  "`return 3`" cheat — hardcode the expected value; or PR #1095's clock
  normalization that made the first post-exec read match by construction, a
  tautology that *contributed* to the demo5 wedge), the test is the thing that's
  wrong. Making time lie again to turn the test green re-creates the regression
  under a green check.
- **If a test asserted broken behavior, UPDATE the test, then defer honestly.**
  Change the assertion to require the *correct* behavior. If the correct fix is
  bigger than this task, mark the test appropriately (or split it) and record
  the deferred real fix as a follow-up with an exact description — do not silence
  it by weakening the product. This is the sabre `date.sh` case: the honest move
  was to assert the correct time behavior and defer the harder backend fix, not
  to re-fake the date so the old assertion passed.
- **First-sample / round-origin parity is a red flag, not a witness.** A
  byte-identical `.000000000` on the first read after an exec/reset proves the
  origin is tidy, not that time evolves deterministically. Demand
  repeated-read and cross-exec parity in the trace.

Report the assurance level you actually restored (L1/L2/L3, backend,
relaxations) bound to the fixed SHA — see the ladder in
[`hermit-debugging`](../hermit-debugging/SKILL.md) §5.

## Checklist

1. Pin an exact `(good, bad)` SHA pair; confirm the symptom is code, not host
   load.
2. `git bisect run` the identical repro to one culprit commit.
3. Capture good vs broken full logs to *files*; `log-diff` (or canonicalize +
   diff) to the **first** divergence.
4. Classify: step-backward → revert/targeted fix; latent-bug-exposed →
   fix-forward.
5. Never re-fake broken time/results to pass a test; fix the test to assert
   correct behavior and defer the real fix honestly.
6. Add a regression test; report the restored level/backend/relaxations at the
   fixed SHA.

## Related

- [`hermit-debugging`](../hermit-debugging/SKILL.md) — log capture, DETLOG/COMMIT
  reading, `log-diff`, assurance ladder (the mechanics this skill builds on).
- [`continuous-virtual-time-is-sacred`](../continuous-virtual-time-is-sacred/SKILL.md)
  — the full sacred-time anti-pattern catalogue and reviewer checklist.
- [`deadlock-debugging`](../deadlock-debugging.md) — when the regression is a
  hang/wedge rather than a value divergence.
- Case study: `ai_docs/demo5-good-vs-broken-trace-diff-divergence_20260731.md`
  (in the `dev-hermit` parent) — the demo5 boot-wedge trace-diff that localized
  the divergence in one diff.
