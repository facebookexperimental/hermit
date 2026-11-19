---
schema: syscall-pages/v3
syscall: select
nr_x86_64: 23
related:
  - https://github.com/rrnewton/hermit/issues/424
discussion:
  kind: new
  url: https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+select&body=Discussion+for+the+%60select%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fselect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60select%60%2C+or+in+what+the+page+says%3F%0A
# code facts: tool-owned (syscall-pages.rs facts --apply); never hand-edit
code_facts:
  as_of: 403e350e9e82ee540529f097daccae6a60832d07
  detcore_class: determinized
  dispatch: "typed"
  refused: false
  refused_only_when_fail_closed: false
  subscribed_run: true
  subscribed_record_replay: true
# prose: human/agent-authored, machine-checked (syscall-pages.rs check); v3 only
prose:
  status: checked
  reviewed_at: 2026-11-17
  based_on: 7ac736d54e14d3eb8e190ae1849481e5ff41ea1a
---

## In short

Natively, what this call returns and when it wakes are settled by host descriptor readiness, host elapsed time, and whichever signal arrives first. Hermit determinizes the wait by polling scratch descriptor sets at scheduler turns against a virtual-time deadline, and on that emulated path writes back a virtual remaining time. Known gaps: 4.

## Determinism strategy

**What is nondeterministic natively.** Linux waits until a descriptor in the read, write, or exception sets is ready, the caller-supplied timeout expires by the host clock, or a signal interrupts, whichever comes first.[^linux] The result therefore depends on host scheduling and device timing: the returned count, the ready subset written back into the sets, whether the call blocks at all, and the time not slept that Linux writes back into the timeout all vary between runs.[^linux]

**What Hermit does.** Detcore determinizes the call: typed dispatch reaches a handler that, in a sequentialized run, emulates the wait as repeated scheduler-polled probes rather than one host-timed sleep.[^c1][^c2][^c4] The handler snapshots the caller's sets, probes a scratch copy with a zero timeout on each granted turn, and copies the probe result back only when the probe succeeds, so each observation happens at a schedule point rather than at a host wakeup.[^c5][^c6] A finite timeout becomes a deadline on logical time, and on that emulated path the remaining time written back is derived from that same virtual clock rather than from the host's unslept remainder.[^c5][^c8]

Some calls are deliberately left with the kernel because they cannot block or cannot be probed faithfully this way: a zero timeout is served by a single kernel call once expired virtual timer descriptors are made readable, a negative descriptor count is passed through for the kernel's own answer, and a count beyond one machine word stays a shielded kernel wait so the guest bitmap is never over-read.[^c4] Without sequentialized threads the kernel likewise owns the wait behind a signal shield, so the probe emulation above is the sequentialized-run path.[^c4]

**Interface fidelity.** The `timeval` timeout is read the way current Linux reads it: a negative seconds or microseconds field fails with `EINVAL`, while microseconds amounting to a whole second or more fold into the seconds instead of failing, and a seconds sum that would overflow also fails with `EINVAL`.[^c3] This folding is where this call differs from its `pselect6` sibling, whose `timespec` is refused when its nanoseconds are out of range.[^linux]

The descriptor sets are treated as Linux treats their raw form: their length follows only the descriptor count, a set the handler cannot read fails with `EFAULT` before any wait, and scratch probing leaves the caller's sets untouched until a successful probe or an expired deadline copies an outcome back.[^c10][^c5][^c6][^c7] At an emulated exit the remaining timeout is written back, and if that write faults the settled result is kept, with the interrupted case named under Known gaps.[^c8] That write-back is attempted even on the set-fault error path: when a caller's set cannot be read, the handler attempts the remaining-time write before returning that fault.[^c5]

Interruption follows the kernel's own order in the main path: each turn probes before its pending-signal check, so a descriptor that becomes ready in the same turn as a signal is reported instead of losing to it, while a signal that does end the wait returns the restart-without-a-handler form that a handler turns into `EINTR`, with the remaining time already written back for a restart to resume from.[^c6][^c7] Where the backend reports no kernel signal state, a scheduler wake still ends the call before its probe, returning `EINTR` itself rather than the restart form above, so that ordering and that result both have an exception there.[^c6]

**Record & replay.** The call is subscribed in both run and record/replay modes (code facts in the Appendix).[^c2] In record/replay modes a zero timeout takes an ordinary scheduler turn and the recorded result is served back, while a call that may wait runs as a blocking external operation whose descriptor sets and remaining time the recorder captures, so replay returns the recorded outcome instead of asking the kernel about descriptors it did not recreate.[^c9][^c4]

## Known gaps

- G1 (unimplemented): a descriptor closed while another thread waits on it is not reported ready as on Linux — each probe is a fresh call, so the wait instead fails, and there is no fallback inside the call.[^c6]
- G2 (by design): a descriptor count beyond one machine word does not take the scheduler-probed path at all — the wait stays with the kernel as a shielded wait, to avoid over-reading a bitmap whose copied length the kernel clamps to its own descriptor table, and the same kernel ownership holds for a wait without sequentialized threads and for a recorded wait; on every such kernel-owned path the kernel writes back its own unslept remainder into the guest timeout, so that remainder follows the host clock, not virtual time, with replay serving the recorded remainder back.[^c4][^linux]
- G3 (unimplemented): where the backend reports kernel signal state, a pending caught `SIGCHLD` can end this wait at a turn host timing chooses, because this wait holds no signal back until it returns; holding it as other waits do is not done here.[^c11]
- G4 (unimplemented): with a finite timeout, when a signal ends the emulated wait in its restart form and the remaining-time write then faults, Hermit keeps that restart form, so with no handler the call resumes from a remainder the guest never received; Linux instead reports `EINTR` there, since the call cannot then be restarted from an updated timeout.[^c8][^linux]

## Discussion

[Start a discussion about `select`](https://github.com/facebookexperimental/hermit/issues/new?title=Syscall+discussion%3A+select&body=Discussion+for+the+%60select%60+syscall+page%3A+https%3A%2F%2Fgithub.com%2Ffacebookexperimental%2Fhermit%2Fblob%2Fmain%2Fdocs%2Fsyscalls%2Fselect.md%0A%0AWhat+should+change+in+how+Detcore+handles+%60select%60%2C+or+in+what+the+page+says%3F%0A) (opens a prefilled issue form; nothing is filed until you submit it).

## Appendix

### Code facts

Detcore classifies `select` (23) as determinized via typed dispatch; deterministic refusal: false; run subscription: true; record/replay subscription: true.

- Reviewed list: `detcore/src/syscall_classification.rs:1892`
- Dispatch: `detcore/src/lib.rs:8428`

### Tests

That this call stays in the determinized class: `detcore/src/syscall_classification.rs` (`representative_policies_stay_in_their_reviewed_sections`) — what Hermit does. The `timeval` folding and refusal rules: `detcore/src/syscalls/io.rs` (`select_timeout_folds_whole_seconds_out_of_microseconds`) — interface fidelity. That each select form ends only for a signal its mask lets in, under the in-guest delivery gate: `detcore/src/tool_global/tests/gated_wait_turn_tests/temporary_mask_turn_tests.rs` (`under_the_gate_each_select_form_ends_only_for_a_signal_its_mask_lets_in`) — interface fidelity.

### Citations

[^linux]: Linux `select(2)`:
  https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/select.c?h=v7.1.3
[^c1]: detcore/src/syscall_classification.rs:1892 sha256:195ee7715b5ea74a
[^c2]: detcore/src/lib.rs:8425-8428 sha256:305752807dff67b4
[^c3]: detcore/src/syscalls/io.rs:933-955 sha256:d57d41fd861108cb
[^c4]: detcore/src/syscalls/io.rs:2565-2625 sha256:3f9b9b0478b76c76
[^c5]: detcore/src/syscalls/io.rs:2628-2668 sha256:77ef220cbb7a3547
[^c6]: detcore/src/syscalls/io.rs:2725-2795 sha256:28387d4452ce707c
[^c7]: detcore/src/syscalls/io.rs:2797-2842 sha256:f38e122776c86496
[^c8]: detcore/src/syscalls/io.rs:2875-2910 sha256:517b1a7de0288498
[^c9]: detcore/src/syscalls/io.rs:1508-1526 sha256:838b939afa88f8d9
[^c10]: detcore/src/syscalls/io.rs:609-613 sha256:872e84d1217549b3; detcore/src/syscalls/io.rs:779-799 sha256:0cbe3253f3da7ded
[^c11]: detcore/src/syscalls/io.rs:2669-2682 sha256:f50fc44da85d1a56

## Status

- 2026-11-15 · rev 54bd03547df0 · facts-only page generated from Detcore code facts at this revision.
- 2026-11-16 · rev 7ac736d54e14 · facts refreshed from Detcore at this revision.
- 2026-11-17 · rev 7ac736d54e14 · checked determinism-strategy prose added (rubric v3); citations verified at this revision.
- 2026-11-18 · rev 403e350e9e82 · facts refreshed from Detcore at this revision.
