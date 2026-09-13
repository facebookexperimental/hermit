# Per-test CPU measurements

`ci/run-nextest-counted.sh` installs one Nextest run wrapper in its temporary
configuration. Default and CI profiles retain their existing wall limits,
grace period, retries and test selection. The wrapper adds measurements; it
does not set a CPU limit or calibrate the test budgets.

Each test attempt gets an atomic record keyed by package, typed binary ID,
test name and retry number. The report reconciles every record against the
terminal Nextest events. Missing, extra, malformed, mixed-run or contradictory
records fail the command. A launch failure with no terminal test and no attempt
records remains a failure with zero executed tests and an empty CPU report.

Set `HERMIT_NEXTEST_CPU_REPORT_PATH` to retain the report. Otherwise a counted
run writes `<DAGRUN_TEST_COUNTS_PATH>.cpu.json`; without a count path it retains
no CPU report. Schema 1 includes the Nextest run ID, typed identities,
`cpu_usage_usec`, `wall_time_ms`, the CPU source and completion status. A run
with no attempt records has a null run ID. Retained stress-index events remain
readable by the count-only adapter; CPU reconciliation explicitly refuses them
because the declared Nextest version provides no corresponding wrapper identity.

CPU is sampled from the wrapper's process group and descendants using the
shared procfs counter. It includes wrapper overhead and has procfs clock-tick
resolution. `supervisor_signal` is a snapshot at interruption: it excludes
later work during Nextest's cleanup grace period. It must not be treated as a
complete CPU total for that interrupted attempt. Nextest owns group signaling;
the wrapper reproduces its own exit or signal status without broadcasting an
additional signal to the test.

The committed graph prepares the normal wrapper binary along with its Nextest
executables. Prepared-record schema 2 binds the Cargo artifact's package,
normal binary target, executable bytes and mode to the recorded source,
compiler, build environment and actual target directory. Required consumers
verify those records for both inventory and execution and never build a
replacement. Standalone counted runs may explicitly build the wrapper. The
binary-map, record-directory, report and wrapper-path variables are removed
before the test child executes.
