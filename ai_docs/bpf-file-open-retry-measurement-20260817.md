# BPF FILE_OPEN retry measurement

Status: measured operational guidance, 2026-08-17

## Conclusion

On `devbig014.atn7.facebook.com`, a single BPF `FILE_OPEN` denial was not
evidence that the affected Hermit command could not run on the host.

The measured response is: retain the interrupted attempt as CANNOT-RUN, then
retry the same command before concluding that the environment is a blocker.
Do not convert a denied attempt into PASS or FAIL, and do not reduce the number
of clean repetitions required by the test.

This guidance is deliberately bounded. If the same operation is still denied
after three retries, this result does not explain that case. Report the denial
as persistent and investigate the host policy or execution path instead of
continuing to cite this measurement.

## Measurement

The measurement came from the combined-binary 261-cell compatibility run:

- Host: `devbig014.atn7.facebook.com`
- Hermit source: `79cc84087904bf7069cef9c585a2eac041341d40`
- Hermit binary SHA-256:
  `be5801f053c3ba00f3601c252a62aa771f3ea39cc2351cb65c37cb4776f329cb`
- Backends in the denominator: ptrace 121, DBT 32, SaBRe 89, LiteInst 19
- Mode: verify
- Evidence requirement: `verified=true`, `verdict=matched`,
  `bitwise_parity=true`, canonical comparison, `compare_logs=true`, and nonzero
  INFO counts on both sides
- Concurrency: 20 cells

The first three repetitions attempted 783 cell executions:

| Outcome | Attempts | Rate |
|---|---:|---:|
| Canonical PASS | 707 | 90.3% |
| CANNOT-RUN after BPF interference | 76 | 9.7% |
| Canonical FAIL | 0 | 0% |

The 76 missing clean repetitions belonged to 69 cells. Targeted retry produced:

| Retry result | Attempts |
|---|---:|
| Canonical PASS | 76 |
| CANNOT-RUN | 0 |
| Canonical FAIL | 0 |

All 76 missing repetitions therefore recovered. Sixty-two cells needed one
additional attempt. Seven cells had lost two of their original three
repetitions and needed two additional attempts. No retry attempt encountered a
second BPF interruption.

The original measurement roots were:

- `ignored/compat-envelope/promotion261-combined-complete1`
- `ignored/compat-envelope/promotion261-combined-complete2`
- `ignored/compat-envelope/promotion261-combined-complete3`

The retry roots were:

- `ignored/compat-envelope/promotion261-combined-retry1`
- `ignored/compat-envelope/promotion261-combined-retry2`

Those roots are intentionally ignored measurement artifacts, not CI
configuration. The tracked finding here is self-contained because ignored
artifacts are disposable.

## Denied operations observed during the work

The denials were not confined to guest execution. Observed `FILE_OPEN` targets
and operations included:

- compiler temporary files matching `/tmp/cc*.res`, `/tmp/cc*.o`, and
  `/tmp/cc*.s`;
- creation and traversal of DynamoRIO installation symlinks;
- publication or atomic replacement of per-cell `verify-1.json` verdicts;
- fixture preparation and execution paths;
- `/usr/bin/jq` when invoked by the full harness.

Verdict publication is especially misleading. Four attempts produced a typed
row saying `FAIL` only because Hermit could not publish `verify-1.json`:

- `c-programs/process-vm-writev-refusal-probe`, verify, ptrace
- `c-programs/socket-cookie-unix`, verify, ptrace
- `c-programs/futex-wake-enosys`, verify, DBT
- `c-programs/sysv-sem-enosys`, verify, ptrace

All four passed canonically on their first retry. A failed verdict-publication
write is therefore CANNOT-RUN evidence, not a guest or comparison failure.

## Practical guidance

1. Preserve the denied command, path, run identity, and host-policy notice.
2. Classify the affected attempt as CANNOT-RUN unless a complete typed verdict
   already exists.
3. Retry the same operation before declaring an environmental wall.
4. Continue to require the full number of clean canonical repetitions. Two
   passes plus one denial are not PASS x3.
5. Record the retry count. Repeated denials are evidence for a host-policy
   request; a single denial is not.
6. If the denial survives three retries, stop applying this guidance to that
   case and report it as persistent.

This measurement does not justify accepting scheduler success, a missing
receipt, zero compared INFO messages, or a partially written result.

## What retry did not fix

Retry is not a general BPF bypass.

- `/usr/bin/jq` remained denied to the full harness throughout this work. The
  successful measurements used a path that did not depend on that denied full
  harness invocation.
- `build.workspace` compiler `FILE_OPEN` blocks were cleared by the escalated
  execution path. Retry alone did not establish that build path.
- A consistently denied path, missing backend, build error, unsupported host
  capability, or genuine canonical divergence remains a blocker.

Consequently, this result does not by itself support a broad seccomp
relaxation. It supports retrying intermittent denials while keeping persistent
denials and build-path restrictions distinct.

## Operational cost before measurement

During the same owner-directed investigation, intermittent denials were
initially treated as hard environmental blockers:

- the PMU work discarded 14 preliminary runs;
- the pressure measurement remained at zero attempted cells for hours;
- the divergence work lost two full DAG attempts;
- a seccomp relaxation was escalated before the retry behavior was measured.

These events are not part of the 783-attempt denominator. They explain why the
retry rule should be applied early in future investigations.

## Scope and falsifier

This is one host, one BPF policy environment, and one measured workload on
2026-08-17. It establishes that the observed denials were intermittent and
fully recoverable by retry. It does not establish that every `FILE_OPEN` denial
is intermittent.

The direct falsifier is a denial of the same operation that survives three
retries. When that occurs, record the exact path and command and treat the case
as persistent; do not use the 76/76 recovery rate to dismiss it.
