# Syscall test-coverage measurement

This page is the public record of the test-matrix coverage measurement
that the per-syscall Detcore pages link to. Coverage is a separate
concept from Detcore strategy: strategy lives in
[`syscalls/`](syscalls/README.md), and no coverage number is merged
into that table. This is also not
[`SYSCALL_COVERAGE.md`](SYSCALL_COVERAGE.md), which audits how Detcore
traps or forwards syscalls rather than which syscalls tests exercise.

## November 2026 measurement

Method: count a syscall as exercised when any published validation
cell for the month reports it with a positive call count in its
first-run syscall histogram (the compact per-cell histograms Detcore
emits with every run summary). The November 2026 published series on
the validation host carried 1,032,274 cell rows, 755,056 of them with
a first-run histogram.

Result: **268 of the 373 syscalls in the table pinned at measurement
time were exercised** (105 never exercised). A single newest full
validation run in that series exercised 262. Per backend, the
exercised counts were: ptrace 265, KVM 262, SaBRe 250, LiteInst 249,
in-guest trap 249, and DBT 0 — DBT cells carried no histograms at all
in that series, a standing measurement gap rather than evidence that
no syscall ran.

Caveats: failing cells are included; backends whose histograms cover
a partial scope report floors, not totals; and the denominator moves
when the pinned syscall table is bumped, so later counts are not
directly comparable to 373. The underlying per-cell series is
published in the
[test ledger](https://github.com/rrnewton/hermit_test_ledger), from
which any later recount can be reproduced with the same rule.
