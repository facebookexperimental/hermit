# Hermit Error Catalog

This catalog maps Hermit's actionable errors and warnings to their likely cause
and a concrete next step. Match the stable text shown in **Message**; values such
as paths, thread IDs, and syscall arguments vary between runs.

Hermit prints an error chain as `Error: ...` followed by indented `> ...`
causes. Diagnose the last OS error in that chain first. A guest program's own
nonzero exit or `errno` is not necessarily a Hermit failure.

The **Class** column uses these labels:

- **Configuration**: change the command, input, permissions, or host policy.
- **Unsupported**: the guest uses behavior Hermit does not determinize.
- **Internal bug**: an invariant failed; flags may help isolate the problem but
  cannot fix it.

Diagnostic modes trade away guarantees. `--namespace-only` disables
interception and determinization. `--strace-only` keeps interception for
logging but disables virtual inputs, deterministic I/O, RCB time, and serialized
scheduling. Options beginning with `--no-` similarly weaken reproducibility.

## Startup And Configuration

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `unexpected argument`, `invalid value`, `required arguments ... not provided`, or `unrecognized subcommand` | Configuration | Clap rejected the command line before Hermit started. | Run `hermit --help` or `hermit <subcommand> --help`. Put `--` before `PROGRAM` when guest arguments begin with `-`. |
| `unable to parse name=value from '...'` or `unable to parse env var name or name=value from '...'` | Configuration | `--env`/`-e` was not `NAME` or `NAME=VALUE`, or used an invalid variable name. | Use `-e NAME` to pass through a host value or `-e NAME=VALUE` to set one. |
| `Attempt to pass through env var ..., but it is not set in host environment` | Configuration | `-e NAME` named a variable absent from Hermit's environment. | Export it first, use `-e NAME=VALUE`, or remove the option. |
| `--no-virtualize-time also requires --no-virtualize-metadata` | Configuration | Host time was requested while deterministic metadata remained enabled. | Add `--no-virtualize-metadata`, or remove `--no-virtualize-time`. The former weakens determinism. |
| `--sched-sticky-random-param must be between 0 and 1 inclusive` | Configuration | The sticky scheduler probability was outside its valid range. | Pass a value from `0` through `1`. |
| `Cannot set both --replay-preemptions-from and --replay-schedule-from` | Configuration | Two mutually exclusive replay sources were supplied. | Keep exactly one replay option. |
| `unable to parse <thread_id>:<logical_time> from '...'` | Configuration | A thread/time option did not contain two valid integers separated by `:`. | Use `THREAD_ID:LOGICAL_TIME`, with both fields expressed as unsigned decimal integers. |
| `Unable to parse string as exit code constraint` | Configuration | Analyze received an invalid `--target-exit-code`. | Use a signed integer, `nonzero`, `none`, or `any`. |
| `Program ... does not exist or is not accessible` | Configuration | The executable path is absent in the guest view, including the isolated `/tmp`. | Correct the path/mount. For a host `/tmp` binary, use `--bind=SOURCE:TARGET` or, diagnostically, `--tmp=/tmp`. |
| `Program ... is a directory` | Configuration | `PROGRAM` resolved to a directory. | Select the executable file. |
| `Program ... is not a regular executable file` | Configuration | `PROGRAM` is a device, socket, FIFO, or other non-regular file. | Use a regular executable or a supported interpreter invocation. |
| `Program ... is not executable` | Configuration | Execute permission is missing. | Run `chmod +x PROGRAM` or invoke its interpreter explicitly. |
| `Program ... has an empty shebang interpreter` | Configuration | A script begins with `#!` but contains no interpreter. | Correct the script's first line. |
| `uses shebang interpreter ..., but the interpreter does not exist` | Configuration | The script interpreter is absent in the guest filesystem. | Install, mount, or correct the interpreter path. |
| `uses shebang interpreter ..., but it is not an executable file` | Configuration | The shebang target exists but cannot be executed. | Fix its type/permissions or select another interpreter. |
| `Could not resolve program ... in guest PATH` | Configuration | A bare program name was not found using the guest environment. | Pass an absolute path, set `PATH`, or use `--base-env=host`. |
| `--bind source ... does not exist` or `--mount source ... does not exist` | Configuration | A requested host mount source is absent. | Create/correct the source before starting Hermit. |
| `--bind target ... is outside guest /tmp, so this option has no effect` | Configuration | `--bind` was used for a target it cannot expose. | Use Docker-style `--mount` for targets outside `/tmp`. |
| `Failed to create temporary log files` | Configuration | `--verify` could not create its temporary artifacts. | Check `TMPDIR`, directory permissions, free space, and file-descriptor limits. |
| `Failed to open log file` or `Unable to open output: ...` | Configuration | A log, `bnz`, or report path cannot be created. | Make the parent directory writable, remove a conflicting directory, and check disk space. |
| `Failed to fork capability probe` or `Failed to wait for capability probe` | Configuration | Hermit's ptrace/seccomp startup probe could not fork or reap its child. | Check process limits and container policy; retry `--namespace-only` to isolate interception from namespace setup. |
| `Hermit cannot use ptrace` / `PTRACE_TRACEME probe was denied` | Configuration | Container seccomp or the host Yama/LSM policy denied parent-child ptrace. | Permit same-UID parent-child ptrace. Use `--namespace-only` only as a smoke test; it does not determinize. |
| `Hermit cannot install its tracee seccomp filter` | Configuration | The host denied seccomp or `prctl(PR_SET_NO_NEW_PRIVS)`. | Permit those operations in the container profile. Use `--namespace-only` only to diagnose policy. |
| namespace, mount, UID/GID-map, `EPERM`, or `Sandbox container exited unexpectedly` | Configuration | User/PID/mount namespaces are unavailable or their setup was denied. | Verify `unshare --user --map-root-user --pid --mount --fork true`; enable unprivileged namespaces and required mounts in host/container policy. |
| `Container exited unexpectedly` | Configuration | A record/analyze helper container died before returning its result. | Read the preceding cause/log. Check namespaces, mounts, executable visibility, and resource limits. |
| `First run during --verify exited in error` | Configuration | The first verification execution did not complete normally. | Fix that execution's earlier error before comparing determinism. |
| `--preemption-timeout requires user-space perf counters ... continuing with timer preemption disabled` | Configuration | `perf_event_open` was blocked by `perf_event_paranoid`, seccomp, or missing VM PMU exposure. | Enable user PMU access, or pass `--preemption-timeout=disabled`. Disabling it can let CPU-bound threads run until another event. |
| `Hardware perf counters are not supported on this machine. Records/Replays may randomly fail` | Configuration | Record/replay metadata setup could not use the PMU and disabled preemption. | Enable user PMU access before recording and replay on equivalent hardware, or expect weaker schedule fidelity. |

## Option Warnings

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `--imprecise timers with --replay-preemptions-from ... won't replay precisely` | Configuration | Approximate timers were enabled during precise preemption replay. | Remove `--imprecise-timers`. |
| `--stop-after-turn will have no effect if --no-sequentialize-threads is enabled` | Configuration | A scheduler stop condition was combined with no scheduler. | Remove `--no-sequentialize-threads` or the ineffective stop option. |
| `--stop-after-iter will have no effect if --no-sequentialize-threads is enabled` | Configuration | An iteration stop condition was combined with no scheduler. | Remove `--no-sequentialize-threads` or the ineffective stop option. |
| `--debug-externalize-sockets will have no effect if --no-sequentialize-threads is enabled` | Configuration | Socket externalization needs the deterministic scheduler. | Re-enable thread sequentialization or remove the debug option. |
| `-s/--stacktrace-event has no effect if not recording/replaying events` | Configuration | A trace-event stack request was used without a trace source/sink. | Add the appropriate recording/replay option or remove `--stacktrace-event`. |
| `manually set log lvl ... but need DEBUG for selfcheck/verbose functionality` | Configuration | Analyze self-checks need debug events that the chosen log level omits. | Put `--log=debug` before `analyze`. |
| `run without any --filter arguments, so accepting ALL runs` | Configuration | Analyze has no target predicate. | Add an exit, stdout, stderr, or other target filter. |
| `performing --search with system randomness` | Configuration | Analyze search has no reproducible seed. | Re-run with the printed `--analyze-seed=N`. |
| `WARNING: DESYNCs found` | Configuration | Verification found replay events that diverged. | Treat the recording, binary, filesystem, environment, and Hermit revision as one immutable set; inspect the desync details. |
| `WARNING: ... looks like a hardware emulator (VMM). Hermit's host-time virtualization exposes mutually inconsistent clock sources ...` | Configuration | A `qemu-system-*` program was launched with Hermit's virtual clock enabled. Its emulated guest derives TSC from a synthetic RDTSC but PIT/PM/APIC/RTC from virtualized `clock_gettime`, and those bases are not mutually coherent. | If the nested guest reports `Unable to calibrate against PIT`, `Marking TSC unstable`, or `No current clocksource`, re-run with `--no-virtualize-time --no-virtualize-metadata`, or make QEMU use one instruction-derived clock via `-icount shift=0,sleep=off`. See `docs/QEMU_BOOT.md`. |

## Unsupported Guest Behavior

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| guest receives `ENOSYS` from `futimesat` | Unsupported | The obsolete `futimesat(2)` syscall is intentionally not implemented. | Update the guest to `utimensat(2)` or `futimens(3)`. `--strace-only` is a compatibility diagnostic, not a deterministic fix. |
| `Not handling deprecated syscall: epoll_wait_old(...)` or `epoll_ctl_old(...)` | Unsupported | A guest used an obsolete pre-2.6 epoll ABI. | Rebuild/update the guest to `epoll_wait`/`epoll_pwait` and `epoll_ctl`. |
| `refusing to execute FUTEX_FD, which was removed in Linux 2.6.26` | Unsupported | The guest requested removed futex operation `FUTEX_FD`. | Update the guest or its runtime; there is no supported deterministic equivalent. |
| `futex op not handled yet: N` | Unsupported | The guest used a futex operation outside Hermit's WAIT/WAKE and BITSET support. | Reduce to a reproducer and update the guest to supported futex operations, or add Detcore support. `--strace-only` can confirm the diagnosis. |
| `clone() with CLONE_VFORK ... not currently supported and will not work` | Unsupported | A guest called `clone` with `CLONE_VFORK`. | Prefer `fork` plus `exec`, or a runtime path that does not request `CLONE_VFORK`. |
| `unsupported syscall: ...` | Unsupported | `--panic-on-unsupported-syscalls` converted Detcore's normal passthrough fallback into a diagnostic panic. | Remove that diagnostic flag to pass the syscall through, accepting possible nondeterminism, or implement/model the syscall. |
| `cpuid leaf 0x... not in deterministic table; returning zero result` | Unsupported | CPUID virtualization has no table entry for the requested leaf. | Prefer changing the guest feature probe. `--no-virtualize-cpuid` exposes the host CPU for diagnosis but weakens reproducibility. |
| `Analyze Networking: Non-zero port detected` | Unsupported | `--analyze-networking` observed a fixed bind port. | Bind port `0` and discover the assigned port. `--network=host` may avoid namespace constraints but introduces host/external nondeterminism. |
| guest receives `EADDRINUSE` after deterministic bind retries | Unsupported | Hermit's deterministic isolated-network port range is exhausted or occupied. | Close stale listeners, bind port `0`, or retry in a fresh container. Host networking is diagnostic and nondeterministic. |

Unlisted syscalls are passed through by default. Successful passthrough is not
an error, but it may introduce a host-dependent result. Use
`hermit --log=info run --panic-on-unsupported-syscalls -- PROGRAM` only to find
the first passthrough candidate.

## Recordings, Replays, And Files

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `Failed to open ...metadata.json`, `Failed to parse ...metadata.json`, or `... is not a file` | Configuration | The recording is missing, unreadable, corrupt, or points at the wrong data directory. | Use the same `--data-dir`/`HERMIT_DATA_DIR`, confirm with `hermit record list`, restore permissions, or re-record. Do not edit recording files. |
| `Failed to find last recording ID` | Configuration | No readable `last` recording marker exists. | Supply an explicit recording ID or create a new recording in the selected data directory. |
| `Failed to create recording directory: ...` | Configuration | Hermit could not initialize the selected recording data directory. | Create a writable parent, check disk space and file limits, or select another `--data-dir`. |
| `Failed to update .../last`, `Failed to serialize metadata`, or `Failed to record ...` | Configuration | Recording storage is unwritable, full, or failed while capturing. | Check parent permissions, disk space, file limits, and the nested cause; then create a fresh recording. |
| `Failed to delete recording ...` | Configuration | `clean`/`remove` could not remove recording data. | Check ownership and permissions, then retry with the same data directory. |
| `Version mismatch, recording version ..., replayer version ...` | Configuration | The recording and replay executable use incompatible Hermit versions. | Replay with the recording version or make a new recording. |
| `Failed to create chroot environment` | Configuration | Replay could not prepare its captured root. | Check the recording contents, namespace/mount policy, path permissions, and the nested OS error. |
| `Failed to run gdb command. Please make sure it is in your $PATH.` | Configuration | Interactive record/replay requested GDB but it is missing or failed. | Install/fix GDB, or use replay `--autopilot` when debugging is not needed. |
| `Failed to create ... for thread ...` or `Failed to open ... for thread ...` | Configuration | A per-thread event file could not be created/read. | Check recording permissions and completeness. Re-record if a thread stream is missing or truncated. |
| `Got unexpected event: ...` | Internal bug | Recorder and replayer event subscriptions or event order disagree. | Reproduce with one Hermit revision and an untouched recording, then report the command and recording metadata. |
| desync summary followed by `See the report generated at: ...` | Configuration | Replay verification observed a syscall different from the recorded event. | Inspect the report; restore the original binary, inputs, environment, mounts, and Hermit version, or re-record. |
| `Failed to generate desync error report` | Configuration | A desync occurred and its report path was unwritable. | Fix recording-directory permissions/free space, then repeat to obtain the underlying report. |
| `Replay mode desynchronized from trace, bailing out` | Configuration | Schedule replay diverged while `--die-on-desync` was set. | Correct the trace/workload mismatch. Removing `--die-on-desync` continues diagnostically but does not make the replay faithful. |
| `Replay trace ran out, stopping at unknown event ...` | Configuration | The schedule ended early while `--replay-exhausted-panic` was set. | Use the matching complete trace or re-record. Removing the flag permits unmatched execution and weakens replay guarantees. |
| `Error reading file ...`, `Error parsing PreemptionRecord from JSON`, or `Invalid PreemptionRecord when loading ...` | Configuration | A preemption file is absent, unreadable, malformed, or violates ordering/priority rules. | Restore the original generated JSON or re-record it; do not hand-edit it. Report a reproducer if Hermit generated the invalid file. |
| `Cannot write_to_disk because this PreemptionWriter was created without a backing file` | Internal bug | An in-memory writer was incorrectly asked to flush to disk. | Report the command and backtrace; no CLI option repairs this API misuse. |
| `Error while dropping PreemptionWriter` | Configuration | The final automatic write failed. | Check the destination directory, disk space, and the nested I/O error. |

## Verification And Log Diff

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `Mismatch in stdout between runs`, `Mismatch in stderr between runs`, or `Mismatch in exit status` | Configuration | `run --verify` observed different guest results. | Inspect retained artifacts; use immutable inputs, minimal environment, isolated networking, supported syscalls, and unchanged virtualization settings. |
| `Log differences found between runs` / `Mismatch between run1 and run2 outputs (logs retained)` | Configuration | Deterministic logs differ even if user-visible output may match. | Inspect both retained logs at the first divergent event and remove external inputs or unsupported passthroughs. |
| `Nonzero exit not allowed.` | Configuration | `hermit-verify` ran a guest that exited nonzero without permission. | Fix the guest failure or pass `--allow-nonzero-exit` when that status is expected. |
| `Verification check failed` | Configuration | A selected `hermit-verify` use case returned false. | Read the preceding stdout/stderr/schedule comparison; its first mismatch is the primary error. |
| `expecting "global" key in the target json file` or `... has unexpected format` | Configuration | A verification schedule is not a Hermit schedule JSON object. | Supply an unmodified schedule/preemption file generated by the matching Hermit version. |
| `unknown value ... for DetLogFilter` | Configuration | A log-diff filter was not `syscall`, `syscallresult`, or `other`. | Use one of those values with `--include-detlogs`. |
| `Log line without expected tag: ...` | Configuration | `logdiff` input contains a nonempty message without an `ERROR`, `WARN`, `INFO`, `DEBUG`, or `TRACE` prefix. | Compare raw stdout separately or regenerate both files through Hermit's tracing logger. |

## Analyze And Schedule Search

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `cannot search through executions with --no-sequentialize-threads. Determinism required` | Configuration | Analyze was asked to search without deterministic scheduling. | Remove `--no-sequentialize-threads`. |
| `FAILED. The run did not match the target criteria. Try --search.` | Configuration | A single analyze execution did not satisfy its target predicate. | Correct the predicate/input or add `--search`. |
| `First run matched criteria but second run did not` | Configuration | A supposed target execution is not reproducible. | Fix external inputs and unsupported calls before minimizing. Pin `--analyze-seed` and workload inputs. |
| `--selfcheck requires perfect reproducibility` | Configuration | Repeated logs differed during analyzer self-check. | Stabilize the workload. Removing `--selfcheck` only disables the diagnostic. |
| `preemptions recorded ... did not match replayed ... (no fixed point)` | Configuration | Recorded preemption points could not be replayed exactly. | Disable `--imprecise-timers`, use the same binary/inputs/host PMU setup, and record again. |
| `Expectations not met ... baseline run matched target criteria` | Configuration | The target filter also accepts the baseline, so search has no opposite outcomes. | Tighten or correct the target predicate. |
| `Final run expected match=..., observed opposite` | Internal bug | Analyze's final replay contradicted its chosen pole. | Preserve report/log/preemption artifacts and report the reproducer. |
| `--run1-schedule` or `--run2-schedule` ends in `not implemented` | Unsupported | These accepted analyzer inputs have no implementation. | Use `--run1-preemptions`, `--run2-preemptions`, or seed-based runs instead. |
| `Final preemption record still does not match target criteria` | Configuration | Minimization produced a record that no longer reproduces the target. | Increase reproducibility first, then repeat from a fresh record. |
| `provided nonmatching preemption record appears corrupt` | Configuration | A comparison record failed validation or replay expectations. | Replace it with an unmodified generated record. |
| `Timestamps failed to monotonically increase` or `time series ... contained duplicate entries` | Configuration | A preemption series is unordered or has duplicate timestamps. | Re-record or regenerate it; report a bug if Hermit produced it. |
| `Hermit analyzer produced corrupt preemption record` | Internal bug | The minimizer itself constructed an invalid record. | Keep the printed corrupt and last-good records and report the reproducer. |
| `Jittered schedule replay was not stable after N attempts` | Configuration | The same requested schedule realized different traces/outcomes beyond the jitter threshold. | Stabilize inputs and replay conditions. `--jitter-dist=N` changes the tolerated distance, but the attempt limit is currently internal. |
| `Event-Level Search Failed - No convergence after N passes` | Configuration | Refinement did not reach adjacent schedules within its internal pass limit. | Stabilize replay and inspect unmatched events with `--verbose`; the pass limit is not currently a CLI option. |
| `opposite-outcome schedules have no remaining reorderable event distance` | Configuration | Outcomes differ but the matched schedules contain no event reorder to explain them. | Investigate nondeterministic inputs or unsupported syscalls rather than schedule order. |
| `schedules are one swap apart but still contain ... unmatched event edits` | Configuration | Search poles include inserted/deleted events, not only a reorder. | Stabilize control flow and inputs; inspect verbose unmatched-event output. |
| `Needleman-Wunsch matrix dimensions overflowed` | Configuration | Schedule alignment dimensions overflowed or are impractically large. | Use shorter traces/narrower search endpoints; lower workload duration before retrying. |
| `Expected tmp_dir to be set at this point`, `write/copy ... preempts file to succeed`, `New run to succeed`, or `Unable to write report file` | Internal bug | Analyze violated an internal phase invariant or converted an I/O failure into a panic. | Check artifact-directory permissions first; otherwise preserve the command/artifacts and report it. |

## Intentional Stops And Runtime Diagnostics

| Message | Class | Trigger | Fix |
| --- | --- | --- | --- |
| `Fatal: Exiting hermit container immediately upon SIGINT` | Configuration | SIGINT arrived while `--sigint-instakill` was enabled. | This is the requested behavior. Rerun without the option if the guest should handle SIGINT. |
| `Could not read backtrace!` | Configuration | A requested stacktrace could not be unwound. | Install matching debug information/unwind support and preserve the executable; omit stacktrace diagnostics if unnecessary. |
| `Failed to open preemption stacktrace log file` | Configuration | `--preemption-stacktrace-log-file` is unwritable. | Correct the path/permissions or remove the file option to log to stderr. |
| `Sequentializing but not virtualizing ... absolute clock_nanosleep ... just yielding` | Unsupported | Serialized scheduling was combined with host time for an absolute sleep Hermit cannot model. | Keep virtual time enabled, or use `--no-sequentialize-threads` only as a nondeterministic compatibility test. |
| `Deadlock detected: thread(s) waiting on futex, but no runnable threads left` | Configuration | Every modeled thread is asleep on a futex and no modeled wake is possible. | Check the guest synchronization protocol first. Compare `--strace-only`; if the native guest progresses, report the minimized program as a Detcore bug. |
| guest hangs after PMU warning | Configuration | Timer preemption is disabled and a CPU-bound thread reaches no intercepted scheduling event. | Enable PMU access or add explicit guest synchronization/yields. `--no-sequentialize-threads` is diagnostic and weakens determinism. |

## Internal Bug Panics

The following message families have no valid user-level fix. Re-run with
`RUST_BACKTRACE=1`, capture `hermit --log=debug --log-file=hermit.log ...`, and
report the exact command, Hermit commit, host kernel/CPU, and smallest guest.
Do not work around them by trusting the output of the failed run.

| Message family | Trigger |
| --- | --- |
| `LogicalTime::duration_since ... future`, `update_global_time ... before start`, `Attempted to update ... time ... already ...`, or `Trying to extract time for thread ... no entry` | Detcore's logical clock moved backward or lost a registered thread. |
| `Couldn't read clock`, `Missed expected preemption`, `end_of_timeslice is None`, `Ended time slice ... still beyond`, `Timer invariant broken`, or `Failed to set timer` | PMU/RCB timeslice state violated an invariant or the backend rejected a timer. |
| `Cannot set end of timeslice ... current ... already ...` | A replayed preemption point is not later than current thread time. This can originate in a mismatched/corrupt preemption record; regenerate it before reporting. |
| `thread time should never go down`, `Global time is before epoch start`, or `bump_global_time ... went backwards` | Global and per-thread logical clocks disagree. |
| `Detcore Default impl should not be called` or `Detcore GlobalState Default impl should not be called` | Reverie initialized Detcore through an invalid default path. |
| `Missing DetPid`, `clone_flags must be set by parent`, `parent ... does not exist in thread_to_leader`, or `no entry for dettid ... next_turns` | Thread creation/lifecycle bookkeeping is inconsistent. |
| `Child thread ... not found in preemption history` | A replay trace lacks a newly created thread. This is a warning with fallback priority; use a matching complete trace. |
| `bad initial_priority`, `bad priority argument`, or `not an acceptable priority value` | A corrupt trace or scheduler bug supplied a priority outside the allowed range. Regenerate the trace once before reporting. |
| `Ivar multiple put exception`, `Ivar ... could not lock`, or `Join failed` | Scheduler synchronization state was poisoned or written twice. |
| `TimedEvents::insert ... already in the set`, `multiple entries for dtid ... in TimedEvents`, or `inner set cannot be empty` | Timed-wait bookkeeping contains duplicates or an impossible empty bucket. |
| `Tried to add ... to runqueue, but it's already present`, tentative-selection assertions, or monotonic-turn assertions | Run-queue transaction state is corrupt. |
| `Requests for more than one resource ... not supported`, `mixed in with other resource requests`, or `multiple resource ids in resource request` | An internal caller composed a resource request the scheduler cannot represent. |
| `pause should never return ... except by interruption`, `futex wake doesn't have a timeout`, or `signal_guest ... nonexistent thread` | An impossible scheduler response or signal target occurred. |
| `signal delivery ... group leader thread has exited` | Signal routing hit an unimplemented exited-leader case. |
| `Failed to get fd`, `invalid syscall / unknown fd`, `cannot simulate nonblocking`, or `sockets/pipes ... physically nonblocking` | Detcore's virtual and physical file-descriptor state disagree. |
| `Expect that when virtualize_metadata, DetFd's stat is populated` | Metadata virtualization lost the file's cached stat record. |
| `PreemptionRecord: cannot re-register`, `before registering thread`, or priority/time assertions | Preemption recording and thread registration are out of order. |
| `Thread state should be explicitly initialized in init_thread_state` | Recorder/replayer thread-local state was requested before initialization. |
| `Serialization is not yet implemented` from `event_stream` | Internal code attempted to serialize an event stream object directly; normal record/replay serializes individual events. |
| `FINISHME` from `mvar`, bare `not implemented`, or `internal invariant`/`impossible` assertions | An unfinished or supposedly unreachable Detcore path ran. |
| Rust's `called Option::unwrap() on a None value`, `called Result::unwrap() on an Err value`, assertion failure, poisoned-lock panic, or integer overflow | An unchecked internal assumption failed. Use the backtrace's first Hermit frame to identify the subsystem and report it. |

## Coverage And Maintenance

This catalog covers production paths in `hermit-cli`, `detcore`,
`detcore-model`, and `hermit-verify` that:

- return an explicit `anyhow` error or context;
- emit an actionable warning/error;
- return `ENOSYS`/unsupported status from Detcore; or
- panic through `panic!`, `unimplemented!`, `expect`, `unwrap`, assertions, or
  poisoned locks.

Test-only assertions, developer scripts, errors printed by the guest, and the
text of arbitrary OS/I/O errors are excluded. OS errors are reported through
the operation-specific context above; apply their standard remedy (path,
permission, space, process/file limit, or host policy) to the final cause in
the chain.

When adding or changing an error path, search at least:

```bash
rg -n 'bail!|anyhow!|context\(|with_context\(' \
  hermit-cli/src detcore/src detcore-model/src hermit-verify/src
rg -n 'panic!|unimplemented!|todo!|\.expect\(|\.unwrap\(|assert' \
  hermit-cli/src detcore/src detcore-model/src hermit-verify/src
rg -n 'ENOSYS|ENOTSUP|EOPNOTSUPP|warn!|error!|WARNING:|Fatal:' \
  hermit-cli/src detcore/src detcore-model/src hermit-verify/src
```

Also update this file when a message or option name changes. For broader setup
and reproducibility guidance, see [USER_GUIDE.md](USER_GUIDE.md#troubleshooting).
