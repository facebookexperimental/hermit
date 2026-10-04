# Per-test CPU measurements

`ci/run-nextest-counted.sh` installs one Nextest run wrapper in its temporary
configuration. Default and CI profiles retain their existing wall limits,
grace period, retries and test selection. The generated configuration remains
measurement-only until a clean-host Nextest CPU distribution supplies a
production budget. Focused controls exercise enforcement by invoking the
wrapper with an explicit `--cpu-timeout-usec` budget and termination grace.

Each test attempt gets an atomic record keyed by package, typed binary ID,
test name and retry number. The report reconciles every record against the
terminal Nextest events. Missing, extra, malformed, mixed-run or contradictory
records fail the command. A launch failure with no terminal test and no attempt
records remains a failure with zero executed tests and an empty CPU report.

Set `HERMIT_NEXTEST_CPU_REPORT_PATH` to retain the report. Otherwise a counted
run writes `<DAGRUN_TEST_COUNTS_PATH>.cpu.json`; without a count path it retains
no CPU report. Schema 3 includes the Nextest run ID, typed identities,
`cpu_usage_usec`, `wall_time_ms`, the CPU source, completion status and every
successful `wait4` receipt with its raw status and CPU fields. A run
with no attempt records has a null run ID. Retained stress-index events remain
readable by the count-only adapter; CPU reconciliation explicitly refuses them
because the declared Nextest version provides no corresponding wrapper identity.

The production measurement-only path retains Nextest's process group and uses
the child's final `wait4` accounting; it does not add a polling loop. An
explicit budget creates an exclusive child cgroup v2 below the wrapper's
already delegated cgroup and enrolls the test in `pre_exec`, before its program
can run or fork. The wrapper stays in the parent. The child's `cpu.stat`
`usage_usec` is the sole budget and final-total authority, so normally
auto-reaped descendants remain charged while unrelated Nextest peers stay
outside the attempt. This does not measure the small fork-to-enrollment interval,
and cgroup membership is not a security boundary against a privileged test that
deliberately moves itself. The wrapper samples twice per second. At the first
sample at or above the CPU budget, it records the boundary value, sends
`SIGTERM` to the direct child through its held pidfd, waits the configured grace
period, writes the authenticated owned `cgroup.kill` if needed, requires both an
empty cgroup and `ECHILD`, and publishes a typed `cpu_timeout` completion before
returning a failing status to Nextest. External signals and ordinary exits
remain distinct first causes. On the budgeted path the wrapper forwards one
external signal to the direct child through its held pidfd and does not let a
later signal replace an already observed CPU timeout. Missing delegation,
failed enrollment, changed cgroup identity, unreadable counters, incomplete
cleanup or a decreasing final counter refuses rather than falling back to
procfs or `wait4` accounting.

On the measurement-only path, an ordinary exit uses the final reaped `wait4`
total. `supervisor_signal` combines already-reaped `wait4` CPU with the live
procfs descendant snapshot taken at interruption, and records that distinct
`procfs-descendants+wait4` source. Nextest still owns the existing 2-second
grace, so CPU consumed after that snapshot is not in the record and must not be
read as a final total.

Production activation also needs the outer Nextest termination grace to exceed
the wrapper's child grace by a measured publication margin. The current 2-second
Nextest grace remains unchanged, so this change does not activate a CPU budget.

The committed graph prepares the normal wrapper binary along with its Nextest
executables. Prepared-record schema 2 binds the Cargo artifact's package,
normal binary target, executable bytes and mode to the recorded source,
compiler, build environment and actual target directory. Required consumers
verify those records for both inventory and execution and never build a
replacement. Standalone counted runs may explicitly build the wrapper. The
binary-map, record-directory, report and wrapper-path variables are removed
before the test child executes.
