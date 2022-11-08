/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Testing utilities.

#![feature(trace_macros)]
use std::io;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use detcore::Config;
use detcore::Detcore;
use detcore::SchedHeuristic;
use lazy_static::lazy_static;
use pretty_assertions::assert_eq;
use reverie::process::Output;
use reverie::Error;
use reverie::ExitStatus;
use reverie::GlobalTool;
use reverie::Tool;
use reverie_ptrace::spawn_fn_with_config;
use reverie_ptrace::testing::print_tracee_output;
use reverie_ptrace::testing::test_cmd_with_config;
use tracing_subscriber::fmt::MakeWriter;

/// How many runs for each test when confirming determinism.
static TEST_REPS: u64 = 3;

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

lazy_static! {
  static ref DEFAULT_CFG: Config = Default::default();

  /// Standardized test config: all options off.
  /// (This is the bottom element of a lattice containing exponentially many possibly Configs.)
  pub static ref BOTTOM_CFG: Config = Config {
    virtualize_cpuid: false,
    virtualize_time: false,
    virtualize_metadata: false,
    sequentialize_threads: false,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch.clone(),
    deterministic_io: false,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    sched_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    preemption_timeout: NonZeroU64::new(5000000),
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
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
  };

  /// Standardized test config: common options on.
  /// (This is drawn from the middle of the lattice of possible Configs.)
  pub static ref MIDDLE_CFG: Config = Config {
    virtualize_cpuid: true,
    virtualize_time: true,  // stat* could depends on this
    virtualize_metadata: true,
    sequentialize_threads: false,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch.clone(),
    deterministic_io: true,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    sched_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    preemption_timeout: NonZeroU64::new(5000000),
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
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
  };

  /// Standardized test config: all options on.
  /// (This is the top element of a lattice containing exponentially many possibly Configs.)
  pub static ref TOP_CFG: Config = Config {
    virtualize_cpuid: true,
    virtualize_time: true,
    virtualize_metadata: true,
    sequentialize_threads: true,
    imprecise_timers: false,
    chaos: false,
    clock_multiplier: DEFAULT_CFG.clock_multiplier,
    epoch: DEFAULT_CFG.epoch.clone(),
    deterministic_io: true,
    has_uts_namespace: false,
    panic_on_unsupported_syscalls: false,
    replay_data: None,
    kill_daemons: false,
    seed: DEFAULT_CFG.seed,
    sched_seed: None,
    gdbserver: false,
    gdbserver_port: 1234,
    preemption_timeout: NonZeroU64::new(5000000),
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
    sysinfo_uptime_offset: 60,
    memory: 1024 * 1024 * 1024, //1 GiB
    interrupt_at: vec![],
  };
}

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
/// Arguments are `basic_det_test(function, predicate, modes...)`, wWhere:
///   - `function` is the procedure under test.
///   - `predicate` is a function that accepts a Config and returns true if the test should
///      run deterministically in that configuration.
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
    T: Fn(&Config),
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
    F: FnOnce(),
{
    let trace_level = std::env::var("DETCORE_TEST_RUST_LOG").unwrap_or_else(|_| {
        // This is a compromise. We don't want to slow things down too much, but it's nice
        // if we print some logs for failures on Sandcastle.
        "detcore=info".into()
    });
    let bufwriter = BufWriter::new();
    let collector = tracing_subscriber::fmt()
        .with_env_filter(trace_level)
        .with_writer(bufwriter.clone())
        .finish();

    // Wrap function with an allocator reset for deterministic in-guest
    // allocations. This only has an effect if the global allocator has been set
    // to test_allocator.
    let f = move || {
        test_allocator::GLOBAL
            // Try to skip to an arbitrary fixed offset that's likely to be far
            // past all the memory we've allocated so far.
            .skip_to_offset_if_init(test_allocator::GLOBAL_MAX_OFFSET / 2)
            .unwrap();
        f()
    };

    // Here we have to keep the collector tightly scoped to this test, because
    // we want to recapture the output of this test and no other.
    let (out, state) = tracing::subscriber::with_default(collector, || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap();
        rt.block_on(async move {
            let tracee = spawn_fn_with_config::<T, _>(f, config, capture_output).await?;
            tracee.wait_with_output().await
        })
    })?;
    Ok((out, state, bufwriter.get_strings()))
}

/// Runs a function multiple times and checks to see if the output was
/// deterministic between runs. Expect successful exit code.
pub fn det_test_fn<F>(f: F)
where
    F: Fn(),
{
    det_test_fn_with_config(true, f, Default::default(), expect_success)
}

/// Like `det_test_fn`, but allows passing in a non-default configuration.  Takes a
/// boolean flag indicating whether to expect a strictly deterministic behavior given the
/// current test and current config.
pub fn det_test_fn_with_config<F, O>(isdet: bool, f: F, config: Config, oracle: O)
where
    F: Fn(),
    O: Fn(&Output, <Detcore as Tool>::GlobalState),
{
    if isdet {
        println!("Expecting determinism:");
    } else {
        println!("Not expecting determinism, but still performing multiple test runs:");
    }
    let mut dts = DetTestState::default();
    for ix in 1..(TEST_REPS - 1) {
        println!("Test Run {}:", ix);
        let (output, state, logs) =
            test_fn_with_logs::<Detcore, _>(&f, config.clone(), true).unwrap();
        println!("({} log lines captured.)", logs.len());
        oracle(&output, state);
        println!("Oracle passed.");
        if isdet {
            println!("Comparing against prior run, if any.");
            check_output(&output, logs, &mut dts);
        }
    }
    println!("Test Run {}:", TEST_REPS - 1);
    let (output, state, logs) = test_fn_with_logs::<Detcore, _>(f, config, true).unwrap();
    println!("({} log lines captured.)", logs.len());
    oracle(&output, state);
    println!("Oracle passed. Full stderr:");
    println!("{}", String::from_utf8_lossy(&output.stderr));
    println!("Full stdout:");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    if isdet {
        println!("Comparing against prior run, if any.");
        check_output(&output, logs, &mut dts);
    }
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
                panic!("Unexpected form to COMMIT log line: {}", &s);
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
