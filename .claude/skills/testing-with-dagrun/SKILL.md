---
name: testing-with-dagrun
description: "Apply testing-principles with dagrun: per-step CPU budgets and wall backstops, memory boxing and admission caps, named resources and innermost retries, reading results by test id, plus the Cargo/nextest profile settings (CPU caps, retries=2 with FLAKY recording, fail-fast, build boxing). Use when defining or reviewing a dagrun validation graph or a nextest profile. How to run DAGs is the dagrun skill; the principles are testing-principles."
---

<!-- Generated from agent-utils 2ef1a9e49bfd9b6c3f1cb6001bdf8af35776790a skills/testing-with-dagrun/SKILL.md; do not edit. Change it in agent-utils, then run scripts/import-agent-utils-skills.rs. -->

# Testing with dagrun

Apply `testing-principles` through dagrun; the `dagrun` skill owns
how to run and inspect DAGs. Each step declares its CPU budget (the
real limit), a wall-clock backstop (deadlock only), and a memory
limit sized at about 1.5x its measured cold peak, with the step
memory-boxed so neighbours cannot take its memory. Total admitted
load is capped at the host level. Name scarce resources explicitly,
set priorities so repair work is not starved, and keep retries
innermost, bounded and counted — a retried step's earlier attempts
stay in the record with their own budgets and measured times. Read
results by test id from the run record; a node with no result is
`no_result`, never a pass.

Cargo/nextest (agent-utils is Rust-first): per-test CPU caps live in
the nextest profile; the retry idiom is profile-level `retries = 2`
in validate profiles only, with every FLAKY outcome recorded (run
record, ledger, scorecard, global statistics) — a per-test
`[[profile.<name>.overrides]]` entry with an exact filter only where
a named, tracked race needs more than the default. Fail fast, cap
test groups so a new test must fit the node it joins, set
`CARGO_BUILD_JOBS` deliberately, box builds (for example
`systemd-run` with memory limits), size build budgets from measured
cold builds rather than warm ones, and list tests to know what
should have run — silent test loss must fail.
