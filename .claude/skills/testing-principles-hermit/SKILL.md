---
name: testing-principles-hermit
description: "Hermit's addendum to the imported testing-principles skill: guest-visible behavior is valid Linux or a clean refusal, exactness never depends on --verify, validation only in the hermetic image, identical isolation and reservation across backends, guest-observable desyncs only, rr-style record and replay, and continuous virtual time. Use with testing-principles when writing, shrinking or reviewing Hermit tests."
---

# Testing principles: Hermit addendum

`testing-principles`, `testing-with-dagrun` and `testing-with-buck` are
imported unchanged from agent-utils (`scripts/import-agent-utils-skills.rs`;
never edit those copies here). They hold the general rules: tests pass at high
load, CPU-primary budgets, counted retries with FLAKY recorded, named skips, no
weakened assertions. This addendum holds only what is specific to Hermit.

- **Guest-visible behavior is valid Linux behavior, or a clean refusal**
  (owner, 2026-10-30). A test that blesses behavior Linux would never produce
  is wrong even when it passes.
- **Exactness never depends on `--verify`** (owner, 2026-10-08): a run is
  deterministic whether or not verification is on.
- **Validation runs only inside the hermetic pinned image** the validation
  tooling selects, and the validator refuses to run outside it (owner,
  2026-10-30).
- **Backends get identical isolation** (owner, 2026-10-09) **and identical
  runtime reservation** (owner, 2026-10-24). A capability one backend lacks is
  a named, capability-gated skip, never a silent pass.
- **Only guest-observable divergence is a record/replay desync**; host-only
  poll-phase differences that resynchronize are not (owner, 2026-11-08;
  [issue 5285](https://github.com/rrnewton/hermit/issues/5285)).
- **Record and replay work like Mozilla rr** (owner, 2026-11-12): recording
  captures the host environment as part of what reproduces the run, so
  `record` needs no strict environment gate, and replay never writes output
  files.
- **Continuous virtual time is a hard requirement**: never make backends match
  or tests pass by making time less continuous
  (`continuous-virtual-time-is-sacred` skill).
