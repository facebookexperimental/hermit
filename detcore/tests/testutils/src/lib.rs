/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![feature(internal_output_capture)]

//! Testing utilities.

use std::ffi::OsStr;
use std::io;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::MutexGuard;

use detcore::Config;
use detcore::Detcore;
use detcore::SchedHeuristic;
use pretty_assertions::assert_eq;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::Tool;
use reverie::process::Container;
use reverie::process::Mount;
use reverie::process::Namespace;
use reverie::process::Output;
use reverie::process::RunError;
use reverie_ptrace::spawn_fn_with_config;
use reverie_ptrace::testing::print_tracee_output;
use reverie_ptrace::testing::test_cmd_with_config;
use tracing_subscriber::fmt::MakeWriter;

/// How many runs for each test when confirming determinism.
static TEST_REPS: u64 = 3;

const ISOLATED_WORKDIR_ENV: &str = hermit_test_workdir::REQUEST_ENV;
#[cfg(test)]
const HERMETIC_TEST_WORKDIR: &str = hermit_test_workdir::WORKDIR;

fn requested_test_workdir(value: Option<&OsStr>) -> Result<Option<&'static Path>, String> {
    hermit_test_workdir::requested_workdir(value).map_err(|error| error.to_string())
}

fn test_trace_level() -> String {
    std::env::var("DETCORE_TEST_RUST_LOG").unwrap_or_else(|_| {
        // This is a compromise. We don't want to slow things down too much, but it's nice
        // if we print some logs for failures on Sandcastle.
        "detcore=info".into()
    })
}

fn install_global_test_subscriber<W>(trace_level: &str, writer: W) -> bool
where
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    let collector = tracing_subscriber::fmt()
        .with_env_filter(trace_level)
        .with_writer(writer)
        .finish();
    tracing::subscriber::set_global_default(collector).is_ok()
}

static GLOBAL_TEST_SUBSCRIBER: LazyLock<()> =
    LazyLock::new(|| _ = install_global_test_subscriber(&test_trace_level(), std::io::stderr));

/// Run a function multiple times, instrumenting it with DetCore and ensuring
/// the outputs are the same on each run.
#[derive(Default)]
struct DetTestState {
    last_exit: Option<ExitStatus>,
    last_stdout: Option<String>,
    last_stderr: Option<String>,
    last_log: Option<Vec<String>>,
    test_run_num: u64,
}

static DEFAULT_CFG: LazyLock<Config> = LazyLock::new(Default::default);

/// Standardized test config: all options off.
/// (This is the bottom element of a lattice containing exponentially many possibly Configs.)
pub static BOTTOM_CFG: LazyLock<Config> = LazyLock::new(|| Config {
    virtualize_cpuid: false,
    cpuid_virtualized_by_backend: false,
    backend_supports_madvise: true,
    discover_live_file_metadata: false,
    use_thread_local_clock_reads: false,
    detect_host_clock_futex_timeouts: false,
    syscall_clobbers_virtualized_by_backend: false,
    cancel_killed_thread_rpcs: false,
    backend_reports_physical_process_exits: false,
    backend_serializes_fork_children: false,
    backend_dispatches_thread_tools: true,
    backend_tracks_process_children: true,
    backend_runs_exit_robust_list: true,
    backend_requires_thread_directed_process_signals: false,
    backend_supports_parked_write_signal_interruption: true,
    backend_virtualizes_capability_prctls: false,
    backend_defers_vfork_child_registration: false,
    virtualize_time: false,
    virtualize_metadata: false,
    mountinfo_root_rewrites: Vec::new(),
    mountinfo_device_rewrites: Vec::new(),
    mountinfo_mount_ids: Vec::new(),
    mountinfo_mount_ids_captured: false,
    fdinfo_unlisted_mount_ids: Vec::new(),
    sequentialize_threads: false,
    runs_post_fork: DEFAULT_CFG.runs_post_fork,
    passthru_opt: false,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch,
    deterministic_io: false,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    exit_on_unsupported_syscall: false,
    shutdown_on_unsupported_syscall: false,
    unsupported_syscall_report_fd: None,
    panic_on_rcb_overshoot: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    rng_seed: None,
    sched_seed: None,
    fuzz_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    max_timeslice: NonZeroU64::new(5000000),
    target_timeslice: None,
    sigint_instakill: false,
    warn_non_zero_binds: false,
    recordreplay_modes: false,
    sched_heuristic: SchedHeuristic::None,
    record_preemptions: false,
    record_preemptions_to: None,
    replay_preemptions_from: None,
    replay_schedule_from: None,
    replay_exhausted_panic: false,
    die_on_desync: false,
    stacktrace_event: Vec::new(),
    stacktrace_signal: None,
    preemption_stacktrace: false,
    preemption_stacktrace_log_file: None,
    stop_after_turn: None,
    stop_after_iter: None,
    debug_externalize_sockets: false,
    debug_futex_mode: DEFAULT_CFG.debug_futex_mode,
    sched_sticky_random_param: 0.0,
    no_rcb_time: false,
    detlog_heap: false,
    detlog_stack: false,
    detlog_regs: false,
    detlog_io_buffers: false,
    detlog_regs_cadence: 1,
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
    happens_before: None,
    fuzz_futexes: false,
    chaos_target_races: false,
    chaos_per_thread_slowdown: false,
    chaos_slowdown_max_factor: 10.0,
    chaos_epoch_length_ns: 0,
});

/// Standardized test config: common options on.
/// (This is drawn from the middle of the lattice of possible Configs.)
pub static MIDDLE_CFG: LazyLock<Config> = LazyLock::new(|| Config {
    virtualize_cpuid: true,
    cpuid_virtualized_by_backend: false,
    backend_supports_madvise: true,
    discover_live_file_metadata: false,
    use_thread_local_clock_reads: false,
    detect_host_clock_futex_timeouts: false,
    syscall_clobbers_virtualized_by_backend: false,
    cancel_killed_thread_rpcs: false,
    backend_reports_physical_process_exits: false,
    backend_serializes_fork_children: false,
    backend_dispatches_thread_tools: true,
    backend_tracks_process_children: true,
    backend_runs_exit_robust_list: true,
    backend_requires_thread_directed_process_signals: false,
    backend_supports_parked_write_signal_interruption: true,
    backend_virtualizes_capability_prctls: false,
    backend_defers_vfork_child_registration: false,
    virtualize_time: true, // stat* could depends on this
    virtualize_metadata: true,
    mountinfo_root_rewrites: Vec::new(),
    mountinfo_device_rewrites: Vec::new(),
    mountinfo_mount_ids: Vec::new(),
    mountinfo_mount_ids_captured: false,
    fdinfo_unlisted_mount_ids: Vec::new(),
    sequentialize_threads: false,
    runs_post_fork: DEFAULT_CFG.runs_post_fork,
    passthru_opt: false,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch,
    deterministic_io: true,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    exit_on_unsupported_syscall: false,
    shutdown_on_unsupported_syscall: false,
    unsupported_syscall_report_fd: None,
    panic_on_rcb_overshoot: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    rng_seed: None,
    sched_seed: None,
    fuzz_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    max_timeslice: NonZeroU64::new(5000000),
    target_timeslice: None,
    sigint_instakill: false,
    warn_non_zero_binds: false,
    recordreplay_modes: false,
    sched_heuristic: SchedHeuristic::None,
    record_preemptions: false,
    record_preemptions_to: None,
    replay_preemptions_from: None,
    die_on_desync: false,
    replay_schedule_from: None,
    replay_exhausted_panic: false,
    stacktrace_event: Vec::new(),
    stacktrace_signal: None,
    preemption_stacktrace: false,
    preemption_stacktrace_log_file: None,
    stop_after_turn: None,
    stop_after_iter: None,
    debug_externalize_sockets: false,
    debug_futex_mode: DEFAULT_CFG.debug_futex_mode,
    sched_sticky_random_param: 0.0,
    no_rcb_time: false,
    detlog_heap: false,
    detlog_stack: false,
    detlog_regs: false,
    detlog_io_buffers: false,
    detlog_regs_cadence: 1,
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
    happens_before: None,
    fuzz_futexes: false,
    chaos_target_races: false,
    chaos_per_thread_slowdown: false,
    chaos_slowdown_max_factor: 10.0,
    chaos_epoch_length_ns: 0,
});

/// Standardized test config: all options on.
/// (This is the top element of a lattice containing exponentially many possibly Configs.)
pub static TOP_CFG: LazyLock<Config> = LazyLock::new(|| Config {
    virtualize_cpuid: true,
    cpuid_virtualized_by_backend: false,
    backend_supports_madvise: true,
    discover_live_file_metadata: false,
    use_thread_local_clock_reads: false,
    detect_host_clock_futex_timeouts: false,
    syscall_clobbers_virtualized_by_backend: false,
    cancel_killed_thread_rpcs: false,
    backend_reports_physical_process_exits: false,
    backend_serializes_fork_children: false,
    backend_dispatches_thread_tools: true,
    backend_tracks_process_children: true,
    backend_runs_exit_robust_list: true,
    backend_requires_thread_directed_process_signals: false,
    backend_supports_parked_write_signal_interruption: true,
    backend_virtualizes_capability_prctls: false,
    backend_defers_vfork_child_registration: false,
    virtualize_time: true,
    virtualize_metadata: true,
    mountinfo_root_rewrites: Vec::new(),
    mountinfo_device_rewrites: Vec::new(),
    mountinfo_mount_ids: Vec::new(),
    mountinfo_mount_ids_captured: false,
    fdinfo_unlisted_mount_ids: Vec::new(),
    sequentialize_threads: true,
    runs_post_fork: DEFAULT_CFG.runs_post_fork,
    passthru_opt: false,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch,
    deterministic_io: true,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    exit_on_unsupported_syscall: false,
    shutdown_on_unsupported_syscall: false,
    unsupported_syscall_report_fd: None,
    panic_on_rcb_overshoot: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    rng_seed: None,
    sched_seed: None,
    fuzz_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    max_timeslice: NonZeroU64::new(5000000),
    target_timeslice: None,
    sigint_instakill: true,
    warn_non_zero_binds: false,
    recordreplay_modes: false,
    sched_heuristic: SchedHeuristic::None,
    record_preemptions: false,
    record_preemptions_to: None,
    replay_preemptions_from: None,
    replay_schedule_from: None,
    replay_exhausted_panic: false,
    die_on_desync: false,
    stacktrace_event: Vec::new(),
    stacktrace_signal: None,
    preemption_stacktrace: false,
    preemption_stacktrace_log_file: None,
    stop_after_turn: None,
    stop_after_iter: None,
    debug_externalize_sockets: false,
    debug_futex_mode: DEFAULT_CFG.debug_futex_mode,
    sched_sticky_random_param: 0.0,
    no_rcb_time: false,
    detlog_heap: false,
    detlog_stack: false,
    detlog_regs: false,
    detlog_io_buffers: false,
    detlog_regs_cadence: 1,
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
    happens_before: None,
    fuzz_futexes: false,
    chaos_target_races: false,
    chaos_per_thread_slowdown: false,
    chaos_slowdown_max_factor: 10.0,
    chaos_epoch_length_ns: 0,
});

/// A basic oracle, which expects a success exit code.
pub fn expect_success(o: &Output, _s: <Detcore as Tool>::GlobalState) {
    if o.status != reverie::ExitStatus::Exited(0) {
        eprintln!("Expected successful exit code, instead tracee output was:");
        print_tracee_output(o);
        panic!("Guest exited with non-zero status.",);
    }
}

#[macro_export]
macro_rules! make_det_test_variants {
    ( $fn:path ) => {
        $crate::make_det_test_variants!($fn, "all");
    };
    ( $fn:path, "all" ) => {
        $crate::make_det_test_variants!(@variants ["bottom" "middle" "default" "top"] $fn);
    };
    ( $fn:path, $($various:tt),*  ) => {
        $crate::make_det_test_variants!(@variants [$($various)*] $fn);
    };

    (@variants [ ] $fn:path ) => {
    };

    // Here we use good-old Lisp-style lists with no commas:
    (@variants [ $first:tt $($rest:tt)* ] $fn:path ) => {
        $crate::make_det_test_variants!(@one_variant $first, $fn);
        $crate::make_det_test_variants!(@variants [ $($rest)* ] $fn);
    };

    ( @one_variant "bottom", $fn:path ) => {
        #[test]
        fn bottom_detcore() {
            $fn(& $crate::BOTTOM_CFG);
        }
    };
    ( @one_variant "middle", $fn:path ) => {
        #[test]
        fn middle_detcore() {
            $fn(& $crate::MIDDLE_CFG);
        }
    };
    ( @one_variant "default", $fn:path ) => {
        #[test]
        fn default_detcore() {
            $fn(& ::core::default::Default::default());
        }
    };
    ( @one_variant "top", $fn:path ) => {
        #[test]
        fn top_detcore() {
            $fn(& $crate::TOP_CFG);
        }
    };
}

/// A convenient way to wrap a function to test multiple detcore execution variants, each
/// as a separate unit test. This is in contrast with `det_test_all_configs` which runs
/// multiple modes sequentially under one test target.
///
/// Arguments are `basic_det_test(function, predicate, modes...)`, where:
///   - `function` is the procedure under test.
///   - `predicate` is a function that accepts a Config and returns true if the test should
///     run deterministically in that configuration.
///
/// This generates calls to `det_test_fn_with_config`.
#[macro_export]
macro_rules! basic_det_test {
    ( $fn:path ) => {
        $crate::basic_det_test!($fn, |_| true);
    };
    ( $fn:path, $f:expr ) => {
        $crate::basic_det_test!(@gendef $fn, $f);
        $crate::make_det_test_variants!(detcore, "all");
    };
    ( $fn:path, $f:expr, $($variants:tt),+ ) => {
        $crate::basic_det_test!(@gendef $fn, $f);
        $crate::make_det_test_variants!(detcore $(,$variants)* );
    };

    (@gendef $fn:path, $f:expr ) => {
        fn detcore(cfg: & ::detcore::Config) {
            $crate::det_test_fn_with_config(
                $f(cfg),
                $fn,
                cfg.clone(),
                $crate::expect_success,
            );
        }
    }
}

/// Runs a test across MULTIPLE configurations.  This combines the multiple variants into
/// a single test execution (running them one after another).  This can be convenient at
/// times, but, when debugging, you may want to split these apart into their own test
/// targets.
///
/// The first function argument returns `true` iff the test *should* be deterministic
/// under a given configuration.
pub fn det_test_all_configs<C, T, O>(check: C, test: T, oracle: O)
where
    C: Fn(&Config) -> bool,
    T: Fn(&Config) + Sync,
    O: Fn(&Output, <Detcore as Tool>::GlobalState) + Clone,
{
    let do_cfg = |cfg: Config| {
        let cfg2 = cfg.clone();
        let test2 = || test(&cfg);
        println!("Full config: {:?}", cfg);
        println!(
            "================================================================================"
        );
        let oracle2 = |output: &Output, state: <Detcore as Tool>::GlobalState| {
            (oracle.clone())(output, state);
        };
        det_test_fn_with_config(check(&cfg), test2, cfg2, oracle2);
    };
    println!("\nDEFAULT mode");
    do_cfg(DEFAULT_CFG.clone());
    println!("\n\"Bottom\", least-strict configuration");
    do_cfg(BOTTOM_CFG.clone());
    println!("\n\"Middle\", medium-strict configuration");
    do_cfg(MIDDLE_CFG.clone());
    println!("\n\"Top\", most-strict configuration");
    do_cfg(TOP_CFG.clone());
}

/// The log messages produced by the tracer while executing the tracee, split into lines.
type TracerLogs = Vec<String>;

struct BufWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Clone for BufWriter {
    fn clone(&self) -> Self {
        BufWriter {
            buf: self.buf.clone(),
        }
    }
}

impl BufWriter {
    fn new() -> BufWriter {
        BufWriter {
            buf: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn buf(&self) -> MutexGuard<'_, Vec<u8>> {
        self.buf.lock().unwrap()
    }

    fn get_strings(&self) -> Vec<String> {
        let mut b = self.buf();
        let s = String::from_utf8_lossy(&b[..]).to_string();
        b.clear();
        s.lines().map(String::from).collect()
    }
}

impl io::Write for BufWriter {
    fn write(&mut self, msg: &[u8]) -> io::Result<usize> {
        let _ = std::io::stderr().write(msg);
        self.buf().write(msg)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        self.buf().flush()
    }
}

impl MakeWriter<'_> for BufWriter {
    type Writer = BufWriter;

    fn make_writer(&self) -> Self::Writer {
        BufWriter {
            buf: self.buf.clone(),
        }
    }
}

/// Similar to Reverie's `test_fn_with_config` that captures and returns the tracer's
/// logs as well.
fn test_fn_with_logs<T, F>(
    f: F,
    config: <T::GlobalState as GlobalTool>::Config,
    capture_output: bool,
) -> Result<(Output, T::GlobalState, TracerLogs), Error>
where
    T: Tool + 'static,
    F: FnOnce() + Send,
{
    let marker = std::env::var_os(ISOLATED_WORKDIR_ENV);
    let workdir = requested_test_workdir(marker.as_deref())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    LazyLock::force(&GLOBAL_TEST_SUBSCRIBER);
    let trace_level = test_trace_level();
    let bufwriter = BufWriter::new();
    let collector = tracing_subscriber::fmt()
        .with_env_filter(trace_level)
        .with_writer(bufwriter.clone())
        .finish();

    // Wrap function with an allocator reset for deterministic in-guest
    // allocations. This only has an effect if the global allocator has been set
    // to test_allocator.
    let f = move || {
        // The host thread mounted this run's /test before constructing the
        // runtime and forking. Only the tracee changes its working directory.
        if let Some(workdir) = workdir {
            std::env::set_current_dir(workdir).unwrap_or_else(|error| {
                panic!(
                    "cannot enter requested test workdir {}: {error}",
                    workdir.display()
                )
            });
        }
        test_allocator::GLOBAL
            // Try to skip to an arbitrary fixed offset that's likely to be far
            // past all the memory we've allocated so far.
            .skip_to_offset_if_init(test_allocator::GLOBAL_MAX_OFFSET / 2)
            .unwrap();
        f()
    };

    // Here we have to keep the collector tightly scoped to this test, because
    // we want to recapture the output of this test and no other.
    let run = move || {
        tracing::subscriber::with_default(collector, || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .unwrap();
            rt.block_on(async move {
                let tracee = spawn_fn_with_config::<T, _>(f, config, capture_output).await?;
                tracee.wait_with_output().await
            })
            // Drop joins this runtime's blocking workers before the namespace
            // thread returns. No external executor launches this tracee.
        })
    };
    let (out, state) = if workdir.is_some() {
        hermit_test_workdir::with_isolated_workdir(run)?
    } else {
        run()
    }?;
    Ok((out, state, bufwriter.get_strings()))
}

/// Runs a function as a guest with Detcore's test log filter applied both to
/// the calling thread and to tracer work performed on other threads.
pub fn test_fn_with_config<T, F>(
    f: F,
    config: <T::GlobalState as GlobalTool>::Config,
    capture_output: bool,
) -> Result<(Output, T::GlobalState), Error>
where
    T: Tool + 'static,
    F: FnOnce() + Send,
{
    test_fn_with_logs::<T, F>(f, config, capture_output)
        .map(|(output, state, _logs)| (output, state))
}

/// Runs a function as a guest with Detcore's test log filter and requires a
/// successful guest exit status.
pub fn check_fn_with_config<T, F>(
    f: F,
    config: <T::GlobalState as GlobalTool>::Config,
    capture_output: bool,
) -> T::GlobalState
where
    T: Tool + 'static,
    F: FnOnce() + Send,
{
    let (output, state) = test_fn_with_config::<T, F>(f, config, capture_output).unwrap();
    if output.status != ExitStatus::Exited(0) {
        print_tracee_output(&output);
        panic!("Got exit status {:?}", output.status);
    }
    state
}

/// Runs a function multiple times and checks to see if the output was
/// deterministic between runs. Expect successful exit code.
pub fn det_test_fn<F>(f: F)
where
    F: Fn() + Send + Sync,
{
    det_test_fn_with_config(true, f, Default::default(), expect_success)
}

/// Like `det_test_fn`, but allows passing in a non-default configuration.  Takes a
/// boolean flag indicating whether to expect a strictly deterministic behavior given the
/// current test and current config. The oracle runs in the per-repetition
/// process described by [`det_test_fn_with_config_repetitions`].
pub fn det_test_fn_with_config<F, O>(isdet: bool, f: F, config: Config, oracle: O)
where
    F: Fn() + Send + Sync,
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    det_test_fn_with_config_repetitions(TEST_REPS - 1, isdet, f, config, oracle)
}

/// Like [`det_test_fn_with_config`], but runs and compares exactly `repetitions` times.
///
/// Each repetition includes the tracer and guest in a fresh PID namespace with
/// matching procfs. Container maps the caller's effective uid/gid to the same
/// numeric values. A fresh user namespace still changes the capability/group
/// context; this is not transparent isolation for arbitrary credential tests.
/// The oracle consumes the actual global state in that child;
/// only the original output and complete captured logs return to the repetition
/// driver. The general [`test_fn_with_config`] state-returning API is unchanged.
///
/// The oracle therefore has a process boundary: mutations of captured Rust
/// memory do not reach the caller or later repetitions. External side effects
/// (files, inherited descriptors, diagnostics) remain real and require their own
/// ownership/cleanup. Callbacks must not depend on parent-memory communication
/// or locks held by other parent threads. This inherits `spawn_fn_with_config`'s
/// best-effort Rust-after-fork limitation; it is not a general fork-safety promise.
pub fn det_test_fn_with_config_repetitions<F, O>(
    repetitions: u64,
    isdet: bool,
    f: F,
    config: Config,
    oracle: O,
) where
    F: Fn() + Send + Sync,
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    assert!(repetitions > 0, "at least one test repetition is required");
    // Borrow the caller's values: only child-created configs/runtime/state are
    // consumed in children, and the caller drops its originals after the wait.
    run_function_test_driver(|| {
        compare_function_repetitions(repetitions, isdet, &f, &config, &oracle);
    });
}

fn compare_function_repetitions<F, O>(
    repetitions: u64,
    isdet: bool,
    f: &F,
    config: &Config,
    oracle: &O,
) where
    F: Fn() + Send + Sync,
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    if isdet {
        println!("Expecting determinism:");
    } else {
        println!("Not expecting determinism, but still performing multiple test runs:");
    }
    let mut dts = DetTestState::default();
    for ix in 1..repetitions {
        println!("Test Run {}:", ix);
        let (output, logs) = isolated_function_repetition(f, config, oracle).unwrap();
        println!("Oracle passed.");
        if isdet {
            println!("Comparing against prior run, if any.");
            check_output(&output, logs, &mut dts);
        }
    }
    println!("Test Run {}:", repetitions);
    let (output, logs) = isolated_function_repetition(f, config, oracle).unwrap();
    println!("Oracle passed. Full stderr:");
    println!("{}", String::from_utf8_lossy(&output.stderr));
    println!("Full stdout:");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if isdet {
        println!("Comparing against prior run, if any.");
        check_output(&output, logs, &mut dts);
    }
}

// Use libc's fork path, as spawn_fn_with_config does, before constructing a
// Container or Tokio runtime. Libtest can have a waiting main thread even with
// --test-threads=1; cloning a Container directly from its worker would bypass
// libc's atfork handling. The forked driver has one surviving thread, and each
// Container starts before that driver has created any runtime/workdir threads.
// This does not repair arbitrary inherited Rust locks; see the public contract.
fn run_function_test_driver(run: impl Fn()) {
    // A concurrent generic function test may be initializing this LazyLock.
    // Complete it in the original process, while that thread still exists;
    // otherwise the child could inherit an in-progress Once and wait forever.
    LazyLock::force(&GLOBAL_TEST_SUBSCRIBER);

    // Match spawn_fn's handling of libtest's thread-local output capture. Child
    // diagnostics go to the inherited streams, while the parent regains its
    // original capture even when fork fails. Do not drop the parent's capture
    // allocation in the child.
    let output_capture = std::io::set_output_capture(None);
    let pid = unsafe { libc::fork() };
    let fork_error = (pid < 0).then(io::Error::last_os_error);
    if pid == 0 {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&run));
        let status = if result.is_ok() { 0 } else { 101 };
        // Catching the panic keeps its original hook diagnostics and prevents
        // unwinding into the inherited libtest stack. Flush diagnostics before
        // _exit; flushing failures also make this driver fail.
        for result in [std::io::stdout().flush(), std::io::stderr().flush()] {
            if let Err(error) = result {
                eprintln!("function-test driver could not flush diagnostics: {error}");
                unsafe { libc::_exit(101) };
            }
        }
        unsafe { libc::_exit(status) };
    }
    std::io::set_output_capture(output_capture);
    if let Some(error) = fork_error {
        panic!("cannot fork function-test driver: {error}");
    }

    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        if waited == pid {
            break;
        }
        let error = io::Error::last_os_error();
        if waited < 0 && error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        panic!("cannot wait for function-test driver {pid}: {error}");
    }
    let status = ExitStatus::from_raw(status);
    assert_eq!(
        status,
        ExitStatus::Exited(0),
        "isolated function-test driver failed; see its original diagnostics above",
    );
}

// The fixed return type is the real Output and complete logs, never a stand-in
// GlobalState. Keep the deferred result inseparable from its checked child exit,
// even if serialization succeeds before child cleanup fails.
fn in_function_pid_namespace<F, D>(run: F) -> Result<(Output, TracerLogs), RunError>
where
    F: FnMut() -> ((Output, TracerLogs), D),
{
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    Container::new()
        .unshare(Namespace::PID)
        .map_uid(uid, uid)
        .map_gid(gid, gid)
        .mount(Mount::proc())
        .run_with_deferred_drop(run)?
        .finalize()
}

fn isolated_function_repetition<F, O>(
    f: &F,
    config: &Config,
    oracle: &O,
) -> Result<(Output, TracerLogs), RunError>
where
    F: Fn() + Send + Sync,
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    in_function_pid_namespace(|| {
        // The Container clone callback is an extern-C boundary. Keep a Rust
        // panic's diagnostics, then exit unsuccessfully instead of unwinding
        // through that boundary or publishing a substitute successful value.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (output, state, logs) =
                test_fn_with_logs::<Detcore, _>(f, config.clone(), true).unwrap();
            println!("({} log lines captured.)", logs.len());
            oracle(&output, state);
            (output, logs)
        }));
        match result {
            Ok(result) => (result, ()),
            Err(_panic) => unsafe { libc::_exit(101) },
        }
    })
}

/// Runs a command multiple times and checks to see if the output was
/// deterministic between runs.
pub fn det_test_cmd<O>(program: &str, args: &[&str], oracle: O)
where
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    det_test_cmd_with_config(program, args, oracle, Default::default())
}

/// Like `det_test_cmd`, but allows passing in a non-default configuration.
pub fn det_test_cmd_with_config<O>(program: &str, args: &[&str], oracle: O, config: Config)
where
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    LazyLock::force(&GLOBAL_TEST_SUBSCRIBER);
    let mut dts = DetTestState::default();
    for _ in 1..(TEST_REPS - 1) {
        let (output, state) =
            test_cmd_with_config::<Detcore>(program, args, config.clone()).unwrap();
        oracle(&output, state);
        println!("Oracle passed. Comparing against prior run, if any.");
        check_output(&output, Vec::new(), &mut dts);
    }

    let (output, state) = test_cmd_with_config::<Detcore>(program, args, config).unwrap();
    oracle(&output, state);
    println!("Oracle passed. Comparing against prior run, if any.");
    check_output(&output, Vec::new(), &mut dts);
}

/// Checks the output of a run to ensure there is no difference from the last
/// run.
fn check_output(output: &Output, logs: Vec<String>, dts: &mut DetTestState) {
    dts.test_run_num += 1;

    match dts.last_exit {
        None => {}
        Some(x) => assert_eq!(
            x, output.status,
            "\n  Consecutive runs of test had different exit status: {:?} {:?}",
            x, output.status
        ),
    }
    dts.last_exit = Some(output.status);

    let stdout_str = String::from_utf8(output.stdout.clone()).unwrap_or_else(|_| {
        panic!(
            "Test produced stdout that is not valid utf8: {:?}",
            output.stdout
        );
    });
    let stderr_str = String::from_utf8(output.stderr.clone()).unwrap_or_else(|_| {
        panic!(
            "Test produced stderr that is not valid utf8: {:?}",
            output.stderr
        );
    });

    // TODO: use some kind of diffing framework to concisely print only the
    // diffs, as well as some quantitative info about the extent of the diff.
    match &dts.last_stdout {
        None => {}
        Some(x) => assert_eq!(
            x, &stdout_str,
            "\n  Consecutive runs of test had different stdout, run1:\n{:?}\n  Run2:\n{:?}",
            x, &stdout_str
        ),
    }
    dts.last_stdout = Some(stdout_str);

    match &dts.last_stderr {
        None => {}
        Some(x) => assert_eq!(
            x, &stderr_str,
            "\n  Consecutive runs of test had different stderr, run1:\n{:?}\n  Run2:\n{:?}",
            x, &stderr_str
        ),
    }
    dts.last_stderr = Some(stderr_str);

    let filtered: Vec<String> = logs
        .iter()
        .filter(|l| l.contains("COMMIT turn"))
        .map(|s| {
            let vec: Vec<&str> = s.split("COMMIT").collect();
            if let [_pref, suffix] = vec[..] {
                suffix.to_string()
            } else {
                panic!("Unexpected form to COMMIT log line: {}", s);
            }
        })
        .collect();

    match &dts.last_log {
        None => {}
        Some(x) => {
            if x.len() != filtered.len() {
                eprintln!(
                    "Differing number of commit lines! ({} vs {})",
                    x.len(),
                    filtered.len()
                )
            }
            for ix in 0..x.len().min(filtered.len()) {
                let str_a = detcore::logdiff::strip_log_entry(&x[ix]);
                let str_b = detcore::logdiff::strip_log_entry(&filtered[ix]);
                assert_eq!(
                    str_a, str_b,
                    "\n  Consecutive runs of test had different COMMITs #{}, run1:\n{:?}\n  Run2:\n{:?}",
                    ix, str_a, str_b,
                )
            }
            if x.len() != filtered.len() {
                panic!(
                    "All present lines matched, but different number of COMMIT lines across two runs."
                )
            }
        }
    }
    dts.last_log = Some(filtered);
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;

    use reverie::ExitStatus;

    use super::BufWriter;
    use super::HERMETIC_TEST_WORKDIR;
    use super::ISOLATED_WORKDIR_ENV;
    use super::install_global_test_subscriber;
    use super::requested_test_workdir;
    use super::test_fn_with_config;

    fn oracle_receipts() -> (
        std::os::unix::net::UnixDatagram,
        std::os::unix::net::UnixDatagram,
    ) {
        let (reader, writer) = std::os::unix::net::UnixDatagram::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        (reader, writer)
    }

    fn receive_oracle_receipt(reader: &std::os::unix::net::UnixDatagram, expected: &[u8]) {
        let mut received = [0u8; 128];
        let size = reader.recv(&mut received).expect("oracle was not reached");
        std::assert_eq!(&received[..size], expected);
    }

    #[test]
    fn function_driver_inherits_completed_subscriber_initialization() {
        super::run_function_test_driver(|| {
            // Inspect without forcing: child-side initialization or a force
            // after the fork cannot repair the child's inherited Once state.
            assert!(
                std::sync::LazyLock::get(&super::GLOBAL_TEST_SUBSCRIBER).is_some(),
                "driver inherited an uninitialized or in-progress subscriber",
            );
        });
        assert!(
            std::sync::LazyLock::get(&super::GLOBAL_TEST_SUBSCRIBER).is_some(),
            "the original caller must complete subscriber initialization",
        );
    }

    #[test]
    fn isolated_function_repetitions_compare_real_pid_and_procfs_output() {
        let (receipts, oracle) = oracle_receipts();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        super::det_test_fn_with_config_repetitions(
            2,
            true,
            || {
                let pid = unsafe { libc::getpid() };
                let tid = unsafe { libc::syscall(libc::SYS_gettid) };
                std::assert_eq!(
                    std::fs::read_link("/proc/self").unwrap(),
                    std::path::PathBuf::from(pid.to_string()),
                );
                let status =
                    std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/status")).unwrap();
                assert!(status.lines().any(|line| line == format!("Pid:\t{tid}")));
                println!("pid={pid} tid={tid}");
                eprintln!("real guest stderr");
            },
            detcore::Config {
                max_timeslice: None,
                ..Default::default()
            },
            |output, state| {
                // These calls run in the untraced oracle, so they observe the
                // actual container credentials rather than Detcore's persona.
                std::assert_eq!(unsafe { libc::geteuid() }, uid);
                std::assert_eq!(unsafe { libc::getegid() }, gid);
                assert!(output.stdout.starts_with(b"pid="));
                std::assert_eq!(output.stderr, b"real guest stderr\n");
                super::expect_success(output, state);
                oracle.send(b"real successful guest and state").unwrap();
            },
        );
        for _ in 0..2 {
            receive_oracle_receipt(&receipts, b"real successful guest and state");
        }
        receipts.set_nonblocking(true).unwrap();
        std::assert_eq!(
            receipts.recv(&mut [0; 128]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "exactly two physical repetitions must reach the oracle",
        );
    }

    #[test]
    fn isolated_function_repetitions_propagate_nonzero_guest_exit() {
        let (receipts, oracle) = oracle_receipts();
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::det_test_fn_with_config_repetitions(
                2,
                true,
                || unsafe { libc::_exit(23) },
                detcore::Config {
                    max_timeslice: None,
                    ..Default::default()
                },
                |output, state| {
                    std::assert_eq!(output.status, ExitStatus::Exited(23));
                    oracle.send(b"real guest exited 23").unwrap();
                    super::expect_success(output, state);
                },
            );
        }));
        assert!(failure.is_err(), "nonzero guest must fail the calling test");
        // An unrelated setup/namespace/tracer failure cannot satisfy this test.
        receive_oracle_receipt(&receipts, b"real guest exited 23");
    }

    #[test]
    fn isolated_function_repetitions_propagate_reached_oracle_panic() {
        let (receipts, oracle) = oracle_receipts();
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            super::det_test_fn_with_config_repetitions(
                2,
                true,
                || {},
                detcore::Config {
                    max_timeslice: None,
                    ..Default::default()
                },
                |output, state| {
                    super::expect_success(output, state);
                    oracle
                        .send(b"successful guest before oracle panic")
                        .unwrap();
                    panic!("deliberate reached oracle failure");
                },
            );
        }));
        assert!(failure.is_err(), "oracle panic must fail the calling test");
        receive_oracle_receipt(&receipts, b"successful guest before oracle panic");
    }

    #[test]
    fn isolated_function_repetition_rejects_failure_after_publication() {
        struct ExitAfterPublication;
        impl Drop for ExitAfterPublication {
            fn drop(&mut self) {
                unsafe { libc::_exit(29) };
            }
        }
        super::run_function_test_driver(|| {
            let result = super::in_function_pid_namespace(|| {
                (
                    (
                        reverie::process::Output {
                            status: ExitStatus::Exited(0),
                            stdout: b"provisional output".to_vec(),
                            stderr: Vec::new(),
                        },
                        vec!["complete provisional log".to_owned()],
                    ),
                    ExitAfterPublication,
                )
            });
            assert!(
                matches!(
                    result,
                    Err(reverie::process::RunError::ExitStatus(ExitStatus::Exited(
                        29
                    )))
                ),
                "must reject the reached post-publication cleanup failure: {result:?}",
            );
        });
    }

    #[test]
    fn isolated_workdir_request_is_exact_and_fail_closed() {
        std::assert_eq!(requested_test_workdir(None).unwrap(), None);
        std::assert_eq!(
            requested_test_workdir(Some(OsStr::new("/test"))).unwrap(),
            Some(Path::new("/test"))
        );
        assert!(
            requested_test_workdir(Some(OsStr::new("/tmp")))
                .unwrap_err()
                .contains("HERMIT_E2E_EMPTY_WORKDIR must be /test")
        );
    }

    #[test]
    fn isolated_workdir_reaches_the_in_process_guest_when_requested() {
        if std::env::var_os(ISOLATED_WORKDIR_ENV).is_none() {
            return;
        }
        let parent_cwd = std::env::current_dir().unwrap();
        // The same relative name must be available to every physical run.
        for _ in 0..2 {
            let (output, ()) = test_fn_with_config::<(), _>(
                || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open("in-process-same-name")
                        .unwrap();
                    println!("{}", std::env::current_dir().unwrap().display());
                },
                (),
                true,
            )
            .unwrap();
            std::assert_eq!(output.status, ExitStatus::Exited(0));
            std::assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                HERMETIC_TEST_WORKDIR
            );
            std::assert_eq!(std::env::current_dir().unwrap(), parent_cwd);
        }
        super::det_test_fn_with_config_repetitions(
            2,
            true,
            || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open("detcore-same-name")
                    .unwrap();
            },
            detcore::Config {
                max_timeslice: None,
                ..Default::default()
            },
            super::expect_success,
        );
    }

    #[test]
    fn global_test_filter_applies_on_spawned_threads() {
        let writer = BufWriter::new();
        assert!(install_global_test_subscriber(
            "reverie_ptrace::timer=off,detcore_testutils=trace",
            writer.clone(),
        ));

        std::thread::spawn(|| {
            tracing::trace!(
                target: "reverie_ptrace::timer",
                "filtered timer instruction"
            );
            tracing::trace!(target: "detcore_testutils", "visible control event");
        })
        .join()
        .unwrap();

        let logs = writer.get_strings();
        assert!(
            logs.iter()
                .any(|line| line.contains("visible control event"))
        );
        assert!(
            !logs
                .iter()
                .any(|line| line.contains("filtered timer instruction"))
        );
    }
}
