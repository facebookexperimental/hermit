---
name: continuous-virtual-time-is-sacred
description: "Continuous, fine-grained, deterministic virtual time is a hard product requirement. Any change that blunts, coarsens, freezes, rounds, or resets virtual time — or that makes backends match / tests pass by making time LESS continuous — is a major red flag. Use when implementing or reviewing time, clock, scheduling, or backend-parity changes."
---

# Continuous Virtual Time Is Sacred

Hermit's determinism is built on a **continuous, fine-grained, deterministic
virtual clock**: virtual time advances smoothly as a function of guest progress
(retired branch counts, syscalls, scheduled events), and every guest-visible
time read is a pure function of that clock. This continuity is not an
implementation detail — it *is* the product. Faithful record/replay, chaos
scheduling, schedule search, and cross-backend parity all depend on time
evolving continuously and identically run-to-run.

## The core rule

**Real determinism = fine-grained continuous deterministic time.** Preserve the
continuous evolution of virtual time in every change. Never buy determinism,
parity, or a green test by making time *less* continuous.

## The "return 3" anti-pattern (why blunting time is fake determinism)

If a function is supposed to compute something and you "fix" a failing test by
rewriting it to `return 3`, the test passes but the function is destroyed. That
is fake correctness: you removed the behavior instead of making it correct.

Blunting virtual time is the same anti-pattern applied to determinism. Making
two runs (or two backends) agree by **erasing the information that time carries**
produces fake determinism / fake parity. The runs match because nothing is
happening, not because the clock is correct.

### Red-flag changes (treat every one as a likely defect until proven otherwise)

- **Rounding / quantizing** timestamps — e.g. snapping reads to clean round
  values like `...000000000`, bucketing to 1ms/1s granularity, or truncating
  low-order nanoseconds.
- **Freezing / stalling** the clock — returning the same time across distinct
  reads, or advancing it only on coarse events.
- **Per-process or per-exec clock resets** — resetting virtual time to an origin
  on `execve`, thread spawn, or backend re-init, so time rewinds or restarts.
- **First-read-epoch on a round origin** — seeding the clock so the *first* read
  in each run returns an identical, tidy value, then diverging afterward.
- **Any parity/determinism fix whose mechanism is "make time coarser"** — if the
  diff makes backends match by reducing time resolution or continuity, it is
  almost certainly hiding the real divergence rather than fixing it.

A legitimate time change makes the *continuous* clock identical across runs. An
illegitimate one makes runs identical by degrading the clock.

## Canonical case: PR #1095

[PR #1095 "Normalize guest clock startup across backends"]
(https://github.com/rrnewton/hermit/pull/1095) (merged 2026-07-28) is the
cautionary example. Normalizing the clock so the *first* read matched on a round
origin produced **fake parity**: backends agreed on the initial sample while the
underlying continuous evolution still diverged. The same blunting contributed to
the demo5 wedge, because collapsing fine-grained time removed the progress signal
the scheduler needs to advance. First-sample agreement on a tidy origin is not
determinism; continuous agreement across the whole run is.

## Test the continuous evolution, not the first sample

Parity and determinism tests must exercise time the way real guests do:

- **Sample repeatedly.** Read the clock many times across the run and compare the
  whole sequence, not just the first value. First-read parity is the classic
  false green — the origin can match while every later read drifts.
- **Check monotonic, fine-grained advance.** Assert that time strictly advances
  at fine granularity between reads, with no freezes, jumps back, or round-number
  plateaus.
- **Cross-exec and cross-thread.** Read after `execve`, in child processes, and
  across threads to catch per-process/per-exec resets. Time must remain
  continuous across an exec boundary, not restart at an origin.
- **Cross-backend.** For backend parity, compare the full time *trajectory*
  between ptrace and the backend under test, not a single canned sample.

If a test only asserts the first read matches, it does not test determinism.

## Reviewer checklist for time / clock / scheduling changes

1. Does virtual time still advance continuously and at fine granularity? If the
   change coarsens, rounds, freezes, or resets time, reject unless there is a
   proven, documented reason it preserves continuity.
2. Is any new parity/green achieved by reducing time resolution or continuity?
   That is the "return 3" smell — require a real root-cause fix instead.
3. Do the tests sample time continuously (repeated reads, cross-exec,
   cross-thread, cross-backend), or do they check only a first sample?
4. Does the PR's **Determinism** section give a continuity argument (why time
   stays fine-grained and continuous), not just "tests pass"?

## Relationship to the PR workflow

This lesson is why the required PR **Determinism** section must argue *continuous
fine-grained virtual time* explicitly, why **Validation** must demonstrate
continuous evolution rather than a first sample, and why core determinism / time
/ scheduling changes require dual independent adversarial review before landing.
See [post-facto-review](../post-facto-review/SKILL.md) for the full required-section
list, the review requirement, and the landing discipline.
