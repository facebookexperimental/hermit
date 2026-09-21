# Finished work that surrounding machinery did not observe

Measurements recorded: 2026-08-17. Recovery reviewed: 2026-09-12.

This report was recovered from
https://github.com/rrnewton/hermit/pull/2408 at
`64cfa88c40d6084db9e3b1108733a75018fc518d`, path
`ai_docs/2026-08-17-finished-work-not-observed.md`. The original 10,179-byte
file has SHA-256
`5548324adbbba5273c8d4ac0ed09375392fe9f702d7f38b0c9d5133e59a53f33`.
The measurements and commands below are historical; they were not rerun for
this documentation change and do not establish current backend status.

The original SaBRe section called a diagnostic relocation a fix. That
interpretation is corrected below: current main retains the diagnostic at each
actual scheduler occurrence because suppressing those occurrences can hide
guest-visible nondeterminism. The KVM and DBT observations remain useful
reproduction evidence without asserting that their old ownership or resolution
state still holds.

This document records four independently measured cases with the same visible
shape: something finishes, but the machinery around it does not observe that
completion correctly.

This is **not** a claim of one root cause. The four cases live in different
components and have different mechanisms and resolution states. The common
shape is useful for review and test design; it is not evidence that one code
change can fix all four.

| Component | What finished | What was not observed | Historical finding |
|---|---|---|---|
| KVM, `signal-waitstatus-identity` | the guest logically exited after reporting wait-status failures | KVM did not complete physical teardown | KVM defect observed; ptrace completed |
| KVM, `pipe-chain` | the parent logically exited after a leaked `ERESTARTSYS` | KVM did not complete physical teardown while stage processes remained | KVM defect observed; ptrace passed |
| DBT evidence | the scheduler process returned from `runtime_background_init` and called `exit(0)` | the required final evidence frame was never constructed | missing lifecycle callback diagnosed; no repair established here |
| SaBRe shutdown evidence | the final exit-barrier release completed | two scheduler empty-state checks did not observe it in a stable order | diagnostic relocation proposed; rejected as a determinism repair |

## 1. KVM teardown: `backend-parity-c/signal-waitstatus-identity`

### Guest and backend context

The guest forks children that exit normally, die from signals, or exit from a
non-main thread. The parent decodes the result returned by `wait4` and asserts
that normal exits and signal deaths retain their Linux wait-status identity.

Measured with source commit
`39a01d4b13a98e3eff8d910c5609557cc647ac4c` and release binary SHA-256
`00c5412fa6c23360c5fc49c9d6b6789b585f2937ac7a62b9c6a442176e106cde`.

Historical KVM command:

```bash
env HERMIT_BIN="$PWD/target/release/hermit" \
  E2E_RESULT_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/hang-characterization/signal-waitstatus-kvm/evidence" \
  E2E_BUILD_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/batch-01/build" \
  E2E_RUN_ID=signal-waitstatus-kvm-retained \
  E2E_KEEP_VERIFY_LOGS=1 \
  target/debug/test-harness run \
    --test backend-parity-c/signal-waitstatus-identity \
    --mode verify --backend kvm --probe-disabled
```

This was blocked rather than CPU-bound: 3.14 user seconds plus 0.28 system
seconds over 123.39 wall seconds, or 2% aggregate CPU.

The real guest output before teardown stopped was:

```text
harness: FAIL fork/waitpid did not complete
normal_exit: UNEXPECTED neither-exited-nor-signalled
normal_exit: FAIL expected exit code=7
sigterm: exited code=143
sigterm: FAIL expected death by signal 15, got a normal exit
sigkill: exited code=137
sigkill: FAIL expected death by signal 9, got a normal exit
sigill: exited code=132
sigill: FAIL expected death by signal 4, got a normal exit
sigfpe: exited code=136
sigfpe: FAIL expected death by signal 8, got a normal exit
abort: exited code=134
abort: FAIL expected death by signal 6, got a normal exit
harness: FAIL fork/waitpid did not complete
exit_from_non_main_thread: UNEXPECTED neither-exited-nor-signalled
exit_from_non_main_thread: FAIL expected exit code=11
failures=11
```

In the final threaded-child case, the child thread calls `exit_group(11)`. The
parent's `wait4(10, ...)` returns `ERESTARTSYS` to the guest rather than
completing with the child's status. The root guest reports its failures and
calls `exit_group(1)`.

The last KVM scheduler record is COMMIT turn 175737, on previously committed
virtual time `1_767_225_643.969_376_250s`
(`1767225643969376250` virtual nanoseconds). The pending guest syscall is #60,
`exit_group(1)`. The run never produces a total-turn report because physical
teardown does not complete.

The ptrace control proves that the teardown failure is backend-specific. One
canonical control completed in 5.68 seconds and matched 332/332 INFO messages.
A retained second control also completed, but exposed the separate ptrace
child-reap divergence at turn 185 of a 205-turn reference run. Ptrace therefore
does not share the KVM teardown failure even though this guest can also expose
the already-known ptrace `wait4` nondeterminism.

**Historical state:** KVM product defect observed on the recorded source.
Current resolution was not measured for this recovery.

## 2. KVM teardown: `determinism-stress-c/pipe-chain`

### Guest and backend context

The guest creates five child stages connected through six pipes. Each stage
reads its input, appends a fixed line, forwards the result, and exits with a
fixed status from 30 through 34. The parent reads the final pipe, waits for all
five statuses, verifies the byte stream, and prints it.

Historical KVM command:

```bash
env HERMIT_BIN="$PWD/target/release/hermit" \
  E2E_RESULT_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/hang-characterization/pipe-chain-kvm/evidence" \
  E2E_BUILD_ROOT="$PWD/ignored/kvm-ratchet-39a01d4b/batch-01/build" \
  E2E_RUN_ID=pipe-chain-kvm-retained \
  E2E_KEEP_VERIFY_LOGS=1 \
  target/debug/test-harness run \
    --test determinism-stress-c/pipe-chain \
    --mode verify --backend kvm --probe-disabled
```

This was also blocked rather than CPU-bound: 3.16 user seconds plus 0.28
system seconds over 93.42 wall seconds, or 3% aggregate CPU.

The relevant INFO records are:

```text
INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: read(13, 0x3fffc8e0, 4096) = ?
INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #56: read(13, 0x3fffc8e0, 4096) = Err(Errno(ERESTARTSYS))
```

That internal restart result leaks to the guest as errno 512 instead of the
read being restarted. The guest prints:

```text
read(output): Unknown error 512
```

It then calls syscall #62, `exit_group(1)`. The last scheduler record is COMMIT
turn 39, on previously committed virtual time
`1_767_225_600.028_259_250s` (`1767225600028259250` virtual nanoseconds).
Three stage processes are still blocked in the pipe protocol when the parent
exits. The parent finishes logically, but KVM never completes physical
teardown.

The ptrace control completes in 3.52 seconds, runs 67 scheduler turns, and
matches 471/471 INFO messages canonically.

**Historical state:** KVM product defect observed on the recorded source.
Current resolution was not measured for this recovery.

## 3. DBT evidence: scheduler process exits without a final frame

### Process and evidence context

The DBT runtime starts its scheduler through `dr_create_client_thread`. That
thread is owned by a distinct scheduler process/PID rather than by the guest
process that emits the other protocol frames. Its START and DATA frames are
therefore emitted under its own `SO_PEERCRED` identity.

After `runtime_background_init` returns, that process calls `exit(0)` without a
corresponding `thread_leave` or `event_exit` callback. The producer that would
construct its FINAL frame is never called. The process has finished, but the
evidence protocol does not record its completion.

The concrete `c-programs/ioctl-fioclex` measurement showed sequence 11 rather
than 10: the scheduler process contributed one additional DATA frame for its
`/dev/null` read, but never contributed its FINAL frame. Six DBT cells ran
twice, exited zero, and matched their backend-local memory hashes while the
canonical report remained unwritten; this missing FINAL frame is one of the
ownership blockers behind that evidence gap.

The Rust entry point identified by the original investigation was
`detcore-dbt/src/lib.rs::reverie_dbt_runtime_background_init`; it runs the
external scheduler and returns after emitting `background scheduler
completed`. The missing lifecycle callback is in the distinct native DBT
client/process ownership path, not in the KVM code described above.

Historical cell selector:

```bash
target/debug/test-harness run \
  --test c-programs/ioctl-fioclex \
  --mode verify --backend dbt --include-manual
```

**Historical diagnosis:** the scheduler process lacked its FINAL-frame
producer. No repair or current ownership claim is established by this report.

## 4. SaBRe shutdown evidence: final release raced empty-state checks

### Scheduler and evidence context

The historical investigation attributed differing scheduler INFO records to
a race between SaBRe's final physical-exit-barrier release and two scheduler
empty-state checks. It observed one run containing the existing

```text
scheduler (step2_process_blocked): zero threads left anywhere, fizzling.
```

diagnostic and another run without it. Five measured cells showed this
record difference; those observations did not establish identical guest
execution. The original report interpreted relocating the
message as a fix. That conclusion was too strong: the relocation did not make
the underlying scheduler decision deterministic.

https://github.com/rrnewton/hermit/pull/2304 / commit `cff8ea3c9d3aa04eccfe50b80421a19acafd4064`
moved the diagnostic to the scheduler's single terminal exit and published the
SaBRe backend fact only after scheduler cleanup. Its commit evidence names two
concrete examples:

- `c-programs/io-uring-ring-determinism`: 124/123 INFO before the proposed relocation.
- `c-programs/periodic-setitimer-delivery`: 148/149 INFO before the proposed relocation.

The proposed regression test required exactly one scheduler-fizzle line,
exactly one scheduler-empty line, exactly one backend-evidence fact, and this
order:

```text
scheduler fizzle < scheduler empty < backend evidence < fallback completed
```

Historical selectors for the named cells were:

```bash
target/debug/test-harness run \
  --test c-programs/io-uring-ring-determinism \
  --mode verify --backend sabre --include-manual

target/debug/test-harness run \
  --test c-programs/periodic-setitimer-delivery \
  --mode verify --backend sabre --include-manual
```

**Recovery assessment:** reject the scheduler diagnostic relocation. At main
`7a829faeb88761809caddd3f2e16bb39926be237`,
`detcore/src/scheduler.rs::step2_process_blocked` emits the INFO record for
each actual occurrence as `SchedulerEmptyQueueKick`. Its comment records a
later guest-visible measurement: 355 versus 354 syscalls, including four
versus three `wait4` calls. Replacing those occurrences with one terminal
message would hide the distinction instead of satisfying the determinism
requirement. The same residual was rejected in
https://github.com/rrnewton/hermit/pull/2783#issuecomment-5649137068.

Moving the SaBRe backend WARN after `clean_up` is a separate controller-order
observation. Main still emits it before cleanup. Current `BitwiseInfoV1`
selects INFO records only (`detcore/src/logdiff.rs::is_info`), so the WARN move
would not repair current L2 verification. Later physical-exit scheduling work
is separately described at https://github.com/rrnewton/hermit/pull/2836; this
historical report is not an approval of that implementation.

## What is shared, and what is not

Shared observable shape:

1. A guest, process, scheduler, or exit barrier reaches its completion point.
2. An adjacent layer fails to observe, publish, or order that completion.
3. The outer result is a hang, missing evidence, or differing evidence rather
   than an accurate account of what already happened.

Different mechanisms:

- The two KVM cases include guest-visible syscall/status defects and incomplete
  backend teardown.
- The DBT case is lifecycle callback and evidence-frame ownership across a
  distinct scheduler PID.
- The SaBRe case was an observation-order race between a physical-exit barrier
  and scheduler empty-state checks.

These distinctions are load-bearing. This document does not assert a common
root cause or propose a common fix.


## Other observations retained from the recovered branch

The source branch also changed manifest reasons using these KVM measurements.
They are preserved here as historical results rather than restored as current
manifest policy. The branch does not bind each of these measurements to an
independent binary digest, so the source head alone must not be read as proof
that each ran against that exact tree.

| Cell | Result recorded in the source branch |
| --- | --- |
| `applications/example-timed-progress-bar` | Three of three comparisons diverged at scheduler turn 32 of 195: `lseek(2, 0, SEEK_CUR)` returned 11 versus 22. |
| `applications/kvm-python-examples` | Three of three comparisons diverged at scheduler turn 45: `lseek(2, 0, SEEK_CUR)` returned 11 versus 22. |
| `c-programs/acct-refusal-probe` | One of three comparisons passed; two diverged after `clone3`, at scheduler turn 4 of 13, with COMMIT virtual time 2,848,750 versus 3,697,500 ns. |
| `determinism-stress/example-race` | Three of three comparisons diverged in `wait4` and `InternalIOPolling` order. INFO counts were 16,183 versus 16,017; 5,821 versus 10,003; and 22,469 versus 11,755. |
| `language-runtimes/example-python-random` | Three of three comparisons diverged at scheduler turn 32 of 156: `lseek(2, 0, SEEK_CUR)` returned 11 versus 22. Both sides had 3,256 INFO messages in every repetition; equal counts did not establish parity. |

The recovered branch's runtime change enabling KVM log comparison is already
present in main, and its old scorecard totals and cell-selection changes are
superseded. This document does not change runtime code, comparator policy,
selected cells, or pass requirements.
