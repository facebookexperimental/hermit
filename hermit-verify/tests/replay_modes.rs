/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Runs the C guest corpus of Hermit's internal Buck matrix through
//! `hermit-verify trace-replay` and `hermit-verify chaos-replay`.
//!
//! Internally, `build_test` in fbsource's `hermetic_infra/hermit/tests/helpers.bzl`
//! gives every `tests/c/*.c` guest a `chaosreplay`, a `tracereplay` and a
//! `tracereplay_chaos` target. Each records one run's schedule and its
//! preemptions and then replays them, so it covers the schedule recording and
//! replay that race exposure and localization depend on. The flags below are
//! those targets' own. The guest list is the internal `tests/c` glob, less
//! `fp_reduction_nondeterminism.c` (excluded there too) and `dbi_mmap_exec.c`
//! (not in this repository), with the internal exemptions: `vforkExec` has no
//! trace replay, and `just_spin` has no chaotic trace replay. A guest whose
//! `main` takes a scenario name runs once per scenario that stands alone; the
//! exec target `pending-exec-check` and the deliberate deadlock
//! `blocking-sigsuspend-no-signal` of `signal_determinism` do not.
//!
//! Every one of those C guests and scenarios, and all 25 `tests/rust` guests,
//! also runs the internal `hermit_run_strict__` and `hermit_run_chaos__`
//! targets: `hermit-verify run`, two `hermit run`s compared, with
//! `--preemption-timeout=80000000` for strict and `--chaos
//! --preemption-timeout=1000000` for chaos, and the internal HERMIT_MODE in
//! the guest's environment.
//!
//! The internal `tests/rust` guests get the same three replay modes, with that
//! matrix's exemptions: thread_random and poll_spin have no chaos replay;
//! bind_connect_race, clock_total_order, exit_group, futex_and_print,
//! futex_wake_some, poll_spin and thread_random have no trace replay; and the
//! four print_*nanosleep*_race guests have no chaotic trace replay. They are
//! this workspace's `rustbin_<name>` binaries (package
//! hermetic_infra_hermit_tests), found beside `hermit-verify`.
//!
//! The six `tests/shell` scripts (the internal `tests/shell/*.sh` glob) run
//! in the same five modes, with that matrix's exemptions: py_hello and
//! devrand have no chaos replay, and py_hello and par_work have no chaotic
//! trace replay. The internal targets' raw and relaxed `hermit run` targets
//! are not ported, nor is `non_strict/curl.sh`, which needs the network.
//!
//! The Hermit binary is `HERMIT_BIN`, or the `hermit` built beside this
//! package's `hermit-verify`. Both need the PMU and user and mount namespaces,
//! as every Hermit integration test does.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Output;

#[path = "../../hermit-cli/tests/common/skid_overshoot.rs"]
mod skid_overshoot;

/// Attempts at one case when its Hermit runs are refused for PMU skid
/// overshoot, as hermit-cli's tests allow (`hermit_test::hermit_output`).
const SKID_ATTEMPTS: u32 = 5;

/// Wall bound for one hermit-verify run, which runs Hermit twice.
const VERIFY_TIMEOUT: &str = "240s";

#[derive(Clone, Copy)]
enum Mode {
    ChaosReplay,
    TraceReplay,
    TraceReplayChaos,
    Strict,
    Chaos,
}

impl Mode {
    /// The internal target's hermit-verify arguments for this mode.
    fn args(self) -> &'static [&'static str] {
        match self {
            Mode::ChaosReplay => &["chaos-replay", "--isolate-workdir", "--hermit-arg=--chaos"],
            Mode::TraceReplay => &["trace-replay", "--strip-times", "--isolate-workdir"],
            Mode::TraceReplayChaos => &[
                "trace-replay",
                "--strip-times",
                "--chaos",
                "--isolate-workdir",
            ],
            // The internal `hermit_run_strict__` and `hermit_run_chaos__`
            // targets: two `hermit run`s, compared. Their HERMIT_MODE reaches
            // the guest, and some guests read it: mem_race asserts preemptions
            // under strict, sched_yield skips itself under chaos.
            Mode::Strict => &[
                "run",
                "--isolate-workdir",
                "--hermit-arg=--env=HERMIT_MODE=strict",
                "--hermit-arg=--preemption-timeout=80000000",
            ],
            Mode::Chaos => &[
                "run",
                "--isolate-workdir",
                "--hermit-arg=--chaos",
                "--hermit-arg=--env=HERMIT_MODE=chaos",
                "--hermit-arg=--preemption-timeout=1000000",
            ],
        }
    }
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-verify should be inside the repository")
}

fn hermit_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("HERMIT_BIN").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let sibling = Path::new(env!("CARGO_BIN_EXE_hermit-verify")).with_file_name("hermit");
    assert!(
        sibling.is_file(),
        "no Hermit binary: set HERMIT_BIN, or build hermit beside {} (cargo build -p hermit)",
        env!("CARGO_BIN_EXE_hermit-verify")
    );
    sibling
}

fn render(output: &Output) -> String {
    fn tail(bytes: &[u8]) -> String {
        const LIMIT: usize = 16 * 1024;
        let start = bytes.len().saturating_sub(LIMIT);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
    format!(
        "status: {}\nstdout (tail):\n{}\nstderr (tail):\n{}",
        output.status,
        tail(&output.stdout),
        tail(&output.stderr)
    )
}

/// Compiles `tests/c/<guest>.c` into a directory of this test process's own.
fn compile_guest(guest: &str, directory: &Path) -> PathBuf {
    let source = repository().join("tests/c").join(format!("{guest}.c"));
    let binary = directory.join(guest);
    let output = Command::new("cc")
        .args(["-O2", "-g", "-pthread", "-D_GNU_SOURCE", "-I"])
        .arg(repository().join("tests/c"))
        .arg("-o")
        .arg(&binary)
        .arg(&source)
        .arg("-lm")
        .output()
        .unwrap_or_else(|error| panic!("failed to start cc for {}: {error}", source.display()));
    assert!(
        output.status.success(),
        "failed to compile {}:\n{}",
        source.display(),
        render(&output)
    );
    binary
}

/// The workspace's `rustbin_<guest>`, built beside `hermit-verify`.
fn rust_guest(guest: &str) -> PathBuf {
    let binary =
        Path::new(env!("CARGO_BIN_EXE_hermit-verify")).with_file_name(format!("rustbin_{guest}"));
    assert!(
        binary.is_file(),
        "{} is missing; build it with cargo build -p hermetic_infra_hermit_tests",
        binary.display()
    );
    binary
}

fn run_replay_case(guest: &str, scenario: Option<&str>, mode: Mode) {
    let directory = tempfile::Builder::new()
        .prefix(&format!("replay-{guest}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest build directory");
    let binary = compile_guest(guest, directory.path());
    run_replay(&binary, scenario, mode);
}

fn run_rust_replay_case(guest: &str, mode: Mode) {
    run_replay(&rust_guest(guest), None, mode);
}

/// Runs `tests/shell/<script>`, which Hermit executes through its `#!` line.
///
/// The scripts call `hexdump`, `taskset` and `python3` by name. Under
/// `--base-env=empty` bash falls back to its default `/usr/bin:/bin`, which
/// holds them on an FHS host such as the internal targets' but not in the
/// pinned root, where they are Nix store paths. So the guest gets this test's
/// own PATH, the same in both of hermit-verify's runs.
fn run_shell_replay_case(script: &str, mode: Mode) {
    let path = std::env::var("PATH").expect("the shell cases need a PATH");
    run_replay_with(
        &repository().join("tests/shell").join(script),
        None,
        mode,
        &[format!("--hermit-arg=--env=PATH={path}")],
    );
}

fn run_replay(binary: &Path, scenario: Option<&str>, mode: Mode) {
    run_replay_with(binary, scenario, mode, &[]);
}

fn run_replay_with(binary: &Path, scenario: Option<&str>, mode: Mode, hermit_args: &[String]) {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", VERIFY_TIMEOUT])
        .arg(env!("CARGO_BIN_EXE_hermit-verify"))
        .arg("--hermit-bin")
        .arg(hermit_binary())
        .arg("--verbose")
        .args(mode.args())
        .arg("--hermit-arg=--base-env=empty")
        .args(hermit_args)
        .arg("--")
        .arg(binary)
        .args(scenario)
        // hermit-verify runs Hermit at --log=trace, where Reverie's timer
        // logs a register dump for every instruction it single-steps to reach
        // a preemption point. These modes compare DETLOG records, stdout,
        // stderr, exit statuses and schedules, never those lines, and they
        // dominated the cost: chaos_replay_rust_futex_and_print logged 219,878
        // of them (133 MB) per run for 3 preemptions, and took 44-53 s on a
        // debug build against nextest's 57 s limit; 16 s without them.
        .env("RUST_LOG", "reverie_ptrace::timer=debug");
    let rendered = format!("{command:?}");
    // hermit-verify exits with its own status, not its Hermit run's, so the
    // typed classifier judges the run hermit-verify reports as failed: its own
    // exit status and its own stderr. Only such a refusal is retried; the
    // retry is recorded, and credited a CPU budget, like hermit_output's.
    let name = std::thread::current()
        .name()
        .unwrap_or("replay case")
        .to_owned();
    let (_, _, output) = skid_overshoot::retry_typed_skid_refusal(&name, SKID_ATTEMPTS, || {
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
        let judged = match refused_run_stderr(&output) {
            Some(stderr) => Output {
                status: ExitStatus::from_raw(detcore_model::HERMIT_POLICY_REFUSAL_EXIT << 8),
                stdout: Vec::new(),
                stderr: stderr.into_bytes(),
            },
            None => output.clone(),
        };
        (judged, output)
    });
    assert!(
        output.status.success(),
        "{rendered} did not replay its recording\n{}",
        render(&output)
    );
}

macro_rules! shell_replay_case {
    (#[ignore = $reason:literal] $name:ident, $script:literal, $mode:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            run_shell_replay_case($script, $mode);
        }
    };
    ($name:ident, $script:literal, $mode:expr) => {
        #[test]
        fn $name() {
            run_shell_replay_case($script, $mode);
        }
    };
}

/// The stderr of the Hermit run a failed hermit-verify reports, when exactly
/// one of its runs failed and that run exited with Hermit's policy-refusal
/// status. hermit-verify prints each run's `Executed command with status: N`
/// and, for a failed run, its stderr between `:: ------------------Stderr`
/// and `:: ----------------End Stderr` lines.
fn refused_run_stderr(output: &Output) -> Option<String> {
    if output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let failed: Vec<(usize, i32)> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let status = line.trim().strip_prefix("Executed command with status: ")?;
            Some((index, status.trim().parse::<i32>().ok()?))
        })
        .filter(|(_, status)| *status != 0)
        .collect();
    let [(index, status)] = failed[..] else {
        return None;
    };
    if status != detcore_model::HERMIT_POLICY_REFUSAL_EXIT {
        return None;
    }
    let start = (index..lines.len())
        .find(|&i| lines[i].trim() == ":: ------------------Stderr----------------------")?;
    let end = (start..lines.len())
        .find(|&i| lines[i].trim() == ":: ----------------End Stderr--------------------")?;
    Some(lines[start + 1..end].join("\n"))
}

#[test]
fn a_refused_run_is_found_only_when_it_is_the_one_failure() {
    let report = |runs: &[(i32, &str)]| -> Output {
        let mut stdout = String::from("Preparing temporary environment to run experiments\n");
        for (number, (status, stderr)) in runs.iter().enumerate() {
            stdout.push_str(&format!(
                ":: Run #{}...\n   hermit run -- guest\nExecuted command with status: {status}\n",
                number + 1
            ));
            if *status != 0 {
                stdout.push_str(&format!(
                    "Failing run because of non-zero exit code.\n\
                     :: ------------------Stdout----------------------\nout\n\
                     :: ----------------End Stdout--------------------\n\
                     :: ------------------Stderr----------------------\n{stderr}\n\
                     :: ----------------End Stderr--------------------\n"
                ));
            }
        }
        Output {
            status: ExitStatus::from_raw(1 << 8),
            stdout: stdout.into_bytes(),
            stderr: b"Error: Nonzero exit not allowed.\n".to_vec(),
        }
    };
    // The refusal from the 3703be04 validation, with Hermit's own lines.
    let refusal = "HERMIT_SKID_OVERSHOOT rcb_actual=240935 rcb_target=240682 skid_margin=1000 overshoot=253\n\
                   HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2\n\
                   Error: Hermit refused the run: a fail-closed policy stopped it before completion.";
    let found = refused_run_stderr(&report(&[(122, refusal)])).expect("the refused run");
    assert_eq!(
        skid_overshoot::skid_overshoot_refusal(Some(122), &found),
        Some("HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=2")
    );
    assert!(refused_run_stderr(&report(&[(0, ""), (122, refusal)])).is_some());
    // Another failing status, two failures, or no failed run is not one.
    assert!(refused_run_stderr(&report(&[(125, refusal)])).is_none());
    assert!(refused_run_stderr(&report(&[(122, refusal), (122, refusal)])).is_none());
    assert!(refused_run_stderr(&report(&[(0, ""), (0, "")])).is_none());
    // A refusal for another cause is found, and the classifier rejects it.
    let other = "HERMIT_POLICY_REFUSAL class=policy-refusal cause=unsupported-syscall count=1";
    let found = refused_run_stderr(&report(&[(122, other)])).unwrap();
    assert_eq!(
        skid_overshoot::skid_overshoot_refusal(Some(122), &found),
        None
    );
    // hermit-verify's own success is never a refusal.
    let mut passed = report(&[(122, refusal)]);
    passed.status = ExitStatus::from_raw(0);
    assert!(refused_run_stderr(&passed).is_none());
}

macro_rules! rust_replay_case {
    (#[ignore = $reason:literal] $name:ident, $guest:literal, $mode:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            run_rust_replay_case($guest, $mode);
        }
    };
    ($name:ident, $guest:literal, $mode:expr) => {
        #[test]
        fn $name() {
            run_rust_replay_case($guest, $mode);
        }
    };
}

macro_rules! replay_case {
    (#[ignore = $reason:literal] $name:ident, $guest:literal, $scenario:expr, $mode:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            run_replay_case($guest, $scenario, $mode);
        }
    };
    ($name:ident, $guest:literal, $scenario:expr, $mode:expr) => {
        #[test]
        fn $name() {
            run_replay_case($guest, $scenario, $mode);
        }
    };
}

replay_case!(
    chaos_replay_arch_prctl_determinism,
    "arch_prctl_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_arch_prctl_determinism,
    "arch_prctl_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_arch_prctl_determinism,
    "arch_prctl_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_clock_determinism,
    "clock_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_clock_determinism,
    "clock_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_clock_determinism,
    "clock_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_clone, "clone", None, Mode::ChaosReplay);
replay_case!(trace_replay_clone, "clone", None, Mode::TraceReplay);
replay_case!(
    trace_replay_chaos_clone,
    "clone",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_epoll_determinism_multi,
    "epoll_determinism",
    Some("multi"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_edge,
    "epoll_determinism",
    Some("edge"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_oneshot,
    "epoll_determinism",
    Some("oneshot"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_mixed,
    "epoll_determinism",
    Some("mixed"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_nested,
    "epoll_determinism",
    Some("nested"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_control_fds,
    "epoll_determinism",
    Some("control-fds"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_epoll_determinism_dupfd,
    "epoll_determinism",
    Some("dupfd"),
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_epoll_determinism_multi,
    "epoll_determinism",
    Some("multi"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_edge,
    "epoll_determinism",
    Some("edge"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_oneshot,
    "epoll_determinism",
    Some("oneshot"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_mixed,
    "epoll_determinism",
    Some("mixed"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_nested,
    "epoll_determinism",
    Some("nested"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_control_fds,
    "epoll_determinism",
    Some("control-fds"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_epoll_determinism_dupfd,
    "epoll_determinism",
    Some("dupfd"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_epoll_determinism_multi,
    "epoll_determinism",
    Some("multi"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_edge,
    "epoll_determinism",
    Some("edge"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_oneshot,
    "epoll_determinism",
    Some("oneshot"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_mixed,
    "epoll_determinism",
    Some("mixed"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_nested,
    "epoll_determinism",
    Some("nested"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_control_fds,
    "epoll_determinism",
    Some("control-fds"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_epoll_determinism_dupfd,
    "epoll_determinism",
    Some("dupfd"),
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_get_cpu, "getCpu", None, Mode::ChaosReplay);
replay_case!(trace_replay_get_cpu, "getCpu", None, Mode::TraceReplay);
replay_case!(
    trace_replay_chaos_get_cpu,
    "getCpu",
    None,
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_getpid, "getpid", None, Mode::ChaosReplay);
replay_case!(trace_replay_getpid, "getpid", None, Mode::TraceReplay);
replay_case!(
    trace_replay_chaos_getpid,
    "getpid",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_getsockopt_null,
    "getsockopt_null",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_getsockopt_null,
    "getsockopt_null",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_getsockopt_null,
    "getsockopt_null",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_hello_alarm,
    "hello_alarm",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_hello_alarm,
    "hello_alarm",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_hello_alarm,
    "hello_alarm",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_hello_signals,
    "hello_signals",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_hello_signals,
    "hello_signals",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_hello_signals,
    "hello_signals",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_io_uring_fallback,
    "io_uring_fallback",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_io_uring_fallback,
    "io_uring_fallback",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_io_uring_fallback,
    "io_uring_fallback",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_ioctl_fioclex,
    "ioctl_fioclex",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_ioctl_fioclex,
    "ioctl_fioclex",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_ioctl_fioclex,
    "ioctl_fioclex",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_ioctl_siocethtool,
    "ioctl_siocethtool",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_ioctl_siocethtool,
    "ioctl_siocethtool",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_ioctl_siocethtool,
    "ioctl_siocethtool",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_ipc_determinism_pipe_order,
    "ipc_determinism",
    Some("pipe-order"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_ipc_determinism_pipe_capacity,
    "ipc_determinism",
    Some("pipe-capacity"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_ipc_determinism_socketpair,
    "ipc_determinism",
    Some("socketpair"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_ipc_determinism_eventfd,
    "ipc_determinism",
    Some("eventfd"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_ipc_determinism_epoll,
    "ipc_determinism",
    Some("epoll"),
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_ipc_determinism_pipe_order,
    "ipc_determinism",
    Some("pipe-order"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_ipc_determinism_pipe_capacity,
    "ipc_determinism",
    Some("pipe-capacity"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_ipc_determinism_socketpair,
    "ipc_determinism",
    Some("socketpair"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_ipc_determinism_eventfd,
    "ipc_determinism",
    Some("eventfd"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_ipc_determinism_epoll,
    "ipc_determinism",
    Some("epoll"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_ipc_determinism_pipe_order,
    "ipc_determinism",
    Some("pipe-order"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_ipc_determinism_pipe_capacity,
    "ipc_determinism",
    Some("pipe-capacity"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_ipc_determinism_socketpair,
    "ipc_determinism",
    Some("socketpair"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_ipc_determinism_eventfd,
    "ipc_determinism",
    Some("eventfd"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_ipc_determinism_epoll,
    "ipc_determinism",
    Some("epoll"),
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_just_spin, "just_spin", None, Mode::ChaosReplay);
replay_case!(trace_replay_just_spin, "just_spin", None, Mode::TraceReplay);
replay_case!(
    chaos_replay_madvise_determinism,
    "madvise_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_madvise_determinism,
    "madvise_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_madvise_determinism,
    "madvise_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_memory_press,
    "memoryPress",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_memory_press,
    "memoryPress",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_memory_press,
    "memoryPress",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_mmap_determinism_multiple,
    "mmap_determinism",
    Some("multiple"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_mmap_determinism_fixed,
    "mmap_determinism",
    Some("fixed"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_mmap_determinism_heap,
    "mmap_determinism",
    Some("heap"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_mmap_determinism_shared,
    "mmap_determinism",
    Some("shared"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_mmap_determinism_reuse,
    "mmap_determinism",
    Some("reuse"),
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_mmap_determinism_multiple,
    "mmap_determinism",
    Some("multiple"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_mmap_determinism_fixed,
    "mmap_determinism",
    Some("fixed"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_mmap_determinism_heap,
    "mmap_determinism",
    Some("heap"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_mmap_determinism_shared,
    "mmap_determinism",
    Some("shared"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_mmap_determinism_reuse,
    "mmap_determinism",
    Some("reuse"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_mmap_determinism_multiple,
    "mmap_determinism",
    Some("multiple"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_mmap_determinism_fixed,
    "mmap_determinism",
    Some("fixed"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_mmap_determinism_heap,
    "mmap_determinism",
    Some("heap"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_mmap_determinism_shared,
    "mmap_determinism",
    Some("shared"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_mmap_determinism_reuse,
    "mmap_determinism",
    Some("reuse"),
    Mode::TraceReplayChaos
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_nanosleep_par,
    "nanosleep-par",
    None,
    Mode::ChaosReplay
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_nanosleep_par,
    "nanosleep-par",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_nanosleep_par,
    "nanosleep-par",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_ppoll_readv,
    "ppoll_readv",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_ppoll_readv,
    "ppoll_readv",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_ppoll_readv,
    "ppoll_readv",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_ppoll_simulation,
    "ppoll_simulation",
    None,
    Mode::ChaosReplay
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_ppoll_simulation,
    "ppoll_simulation",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_ppoll_simulation,
    "ppoll_simulation",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_print_memaddrs,
    "print_memaddrs",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_print_memaddrs,
    "print_memaddrs",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_print_memaddrs,
    "print_memaddrs",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_printf_with_threads,
    "printf_with_threads",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_printf_with_threads,
    "printf_with_threads",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_printf_with_threads,
    "printf_with_threads",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_random_sources,
    "random_sources",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_random_sources,
    "random_sources",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_random_sources,
    "random_sources",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_rcx_canonicalization,
    "rcx_canonicalization",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_rcx_canonicalization,
    "rcx_canonicalization",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_rcx_canonicalization,
    "rcx_canonicalization",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_record_replay_fd_close,
    "record_replay_fd_close",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_record_replay_fd_close,
    "record_replay_fd_close",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_record_replay_fd_close,
    "record_replay_fd_close",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_recvmsg_scm_rights_mmap,
    "recvmsg_scm_rights_mmap",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_recvmsg_scm_rights_mmap,
    "recvmsg_scm_rights_mmap",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_recvmsg_scm_rights_mmap,
    "recvmsg_scm_rights_mmap",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_resource_determinism,
    "resource_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    #[ignore = "replay diverged twice in the pinned validate root (the guest makes a different number of syscalls, around times and dup), never on a developer host; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_resource_determinism,
    "resource_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    #[ignore = "replay diverged twice in the pinned validate root (the guest makes a different number of syscalls, around times and dup), never on a developer host; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_chaos_resource_determinism,
    "resource_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sched_yield_progress,
    "sched_yield_progress",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sched_yield_progress,
    "sched_yield_progress",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sched_yield_progress,
    "sched_yield_progress",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_setitimer_determinism,
    "setitimer_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_setitimer_determinism,
    "setitimer_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_setitimer_determinism,
    "setitimer_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_signal_determinism_itimer_delivery,
    "signal_determinism",
    Some("itimer-delivery"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_itimer_exit,
    "signal_determinism",
    Some("itimer-exit"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_blocking_sigsuspend,
    "signal_determinism",
    Some("blocking-sigsuspend"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_pending_sigsuspend,
    "signal_determinism",
    Some("pending-sigsuspend"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_sigsuspend_invalid_arguments,
    "signal_determinism",
    Some("sigsuspend-invalid-arguments"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_masks_fork_clone,
    "signal_determinism",
    Some("masks-fork-clone"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_blocking_read_interrupted,
    "signal_determinism",
    Some("blocking-read-interrupted"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_blocking_read_restarted,
    "signal_determinism",
    Some("blocking-read-restarted"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_poll_sa_restart,
    "signal_determinism",
    Some("poll-sa-restart"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_epoll_wait_sa_restart,
    "signal_determinism",
    Some("epoll-wait-sa-restart"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_sigtimedwait_sa_restart,
    "signal_determinism",
    Some("sigtimedwait-sa-restart"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_handler_reentrance,
    "signal_determinism",
    Some("handler-reentrance"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_altstack_preservation,
    "signal_determinism",
    Some("altstack-preservation"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_signal_determinism_pending_exec,
    "signal_determinism",
    Some("pending-exec"),
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_signal_determinism_itimer_delivery,
    "signal_determinism",
    Some("itimer-delivery"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_itimer_exit,
    "signal_determinism",
    Some("itimer-exit"),
    Mode::TraceReplay
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_signal_determinism_blocking_sigsuspend,
    "signal_determinism",
    Some("blocking-sigsuspend"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_pending_sigsuspend,
    "signal_determinism",
    Some("pending-sigsuspend"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_sigsuspend_invalid_arguments,
    "signal_determinism",
    Some("sigsuspend-invalid-arguments"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_masks_fork_clone,
    "signal_determinism",
    Some("masks-fork-clone"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_blocking_read_interrupted,
    "signal_determinism",
    Some("blocking-read-interrupted"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_blocking_read_restarted,
    "signal_determinism",
    Some("blocking-read-restarted"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_poll_sa_restart,
    "signal_determinism",
    Some("poll-sa-restart"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_epoll_wait_sa_restart,
    "signal_determinism",
    Some("epoll-wait-sa-restart"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_sigtimedwait_sa_restart,
    "signal_determinism",
    Some("sigtimedwait-sa-restart"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_handler_reentrance,
    "signal_determinism",
    Some("handler-reentrance"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_altstack_preservation,
    "signal_determinism",
    Some("altstack-preservation"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_signal_determinism_pending_exec,
    "signal_determinism",
    Some("pending-exec"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_signal_determinism_itimer_delivery,
    "signal_determinism",
    Some("itimer-delivery"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_itimer_exit,
    "signal_determinism",
    Some("itimer-exit"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_blocking_sigsuspend,
    "signal_determinism",
    Some("blocking-sigsuspend"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_pending_sigsuspend,
    "signal_determinism",
    Some("pending-sigsuspend"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_sigsuspend_invalid_arguments,
    "signal_determinism",
    Some("sigsuspend-invalid-arguments"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_masks_fork_clone,
    "signal_determinism",
    Some("masks-fork-clone"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_blocking_read_interrupted,
    "signal_determinism",
    Some("blocking-read-interrupted"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_blocking_read_restarted,
    "signal_determinism",
    Some("blocking-read-restarted"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_poll_sa_restart,
    "signal_determinism",
    Some("poll-sa-restart"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_epoll_wait_sa_restart,
    "signal_determinism",
    Some("epoll-wait-sa-restart"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_sigtimedwait_sa_restart,
    "signal_determinism",
    Some("sigtimedwait-sa-restart"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_handler_reentrance,
    "signal_determinism",
    Some("handler-reentrance"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_altstack_preservation,
    "signal_determinism",
    Some("altstack-preservation"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_signal_determinism_pending_exec,
    "signal_determinism",
    Some("pending-exec"),
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sigpipe_siginfo,
    "sigpipe_siginfo",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sigpipe_siginfo,
    "sigpipe_siginfo",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sigpipe_siginfo,
    "sigpipe_siginfo",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sigtimedwait_no_timeout,
    "sigtimedwait-no-timeout",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sigtimedwait_no_timeout,
    "sigtimedwait-no-timeout",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sigtimedwait_no_timeout,
    "sigtimedwait-no-timeout",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sigtimedwait_timeout_0s,
    "sigtimedwait-timeout-0s",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sigtimedwait_timeout_0s,
    "sigtimedwait-timeout-0s",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sigtimedwait_timeout_0s,
    "sigtimedwait-timeout-0s",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sigtimedwait_timeout_1s,
    "sigtimedwait-timeout-1s",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sigtimedwait_timeout_1s,
    "sigtimedwait-timeout-1s",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sigtimedwait_timeout_1s,
    "sigtimedwait-timeout-1s",
    None,
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_sysinfo, "sysinfo", None, Mode::ChaosReplay);
replay_case!(trace_replay_sysinfo, "sysinfo", None, Mode::TraceReplay);
replay_case!(
    trace_replay_chaos_sysinfo,
    "sysinfo",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_sysinfo_uptime,
    "sysinfo_uptime",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_sysinfo_uptime,
    "sysinfo_uptime",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_sysinfo_uptime,
    "sysinfo_uptime",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_thread_exhaustion,
    "threadExhaustion",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_thread_exhaustion,
    "threadExhaustion",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_thread_exhaustion,
    "threadExhaustion",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_thread_sync_determinism_barrier,
    "thread_sync_determinism",
    Some("barrier"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_thread_sync_determinism_condvar,
    "thread_sync_determinism",
    Some("condvar"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_thread_sync_determinism_rwlock,
    "thread_sync_determinism",
    Some("rwlock"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_thread_sync_determinism_semaphore,
    "thread_sync_determinism",
    Some("semaphore"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_thread_sync_determinism_cancellation,
    "thread_sync_determinism",
    Some("cancellation"),
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_thread_sync_determinism_tls_fork,
    "thread_sync_determinism",
    Some("tls-fork"),
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_barrier,
    "thread_sync_determinism",
    Some("barrier"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_condvar,
    "thread_sync_determinism",
    Some("condvar"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_rwlock,
    "thread_sync_determinism",
    Some("rwlock"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_semaphore,
    "thread_sync_determinism",
    Some("semaphore"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_cancellation,
    "thread_sync_determinism",
    Some("cancellation"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_thread_sync_determinism_tls_fork,
    "thread_sync_determinism",
    Some("tls-fork"),
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_barrier,
    "thread_sync_determinism",
    Some("barrier"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_condvar,
    "thread_sync_determinism",
    Some("condvar"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_rwlock,
    "thread_sync_determinism",
    Some("rwlock"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_semaphore,
    "thread_sync_determinism",
    Some("semaphore"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_cancellation,
    "thread_sync_determinism",
    Some("cancellation"),
    Mode::TraceReplayChaos
);
replay_case!(
    trace_replay_chaos_thread_sync_determinism_tls_fork,
    "thread_sync_determinism",
    Some("tls-fork"),
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_timer_create_determinism,
    "timer_create_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_timer_create_determinism,
    "timer_create_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_timer_create_determinism,
    "timer_create_determinism",
    None,
    Mode::TraceReplayChaos
);
replay_case!(chaos_replay_uname, "uname", None, Mode::ChaosReplay);
replay_case!(trace_replay_uname, "uname", None, Mode::TraceReplay);
replay_case!(
    trace_replay_chaos_uname,
    "uname",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    chaos_replay_vfork_exec,
    "vforkExec",
    None,
    Mode::ChaosReplay
);
replay_case!(
    chaos_replay_wait_on_child,
    "wait_on_child",
    None,
    Mode::ChaosReplay
);
replay_case!(
    trace_replay_wait_on_child,
    "wait_on_child",
    None,
    Mode::TraceReplay
);
replay_case!(
    trace_replay_chaos_wait_on_child,
    "wait_on_child",
    None,
    Mode::TraceReplayChaos
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_writev_determinism,
    "writev_determinism",
    None,
    Mode::ChaosReplay
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_writev_determinism,
    "writev_determinism",
    None,
    Mode::TraceReplay
);
replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_chaos_writev_determinism,
    "writev_determinism",
    None,
    Mode::TraceReplayChaos
);

// The internal tests/rust guests.
rust_replay_case!(
    chaos_replay_rust_bind_connect_race,
    "bind_connect_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    chaos_replay_rust_clock_gettime,
    "clock_gettime",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_clock_gettime,
    "clock_gettime",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_clock_gettime,
    "clock_gettime",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_clock_total_order,
    "clock_total_order",
    Mode::ChaosReplay
);
rust_replay_case!(
    chaos_replay_rust_exit_group,
    "exit_group",
    Mode::ChaosReplay
);
rust_replay_case!(
    chaos_replay_rust_futex_and_print,
    "futex_and_print",
    Mode::ChaosReplay
);
rust_replay_case!(
    chaos_replay_rust_futex_timeout,
    "futex_timeout",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_futex_timeout,
    "futex_timeout",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_futex_timeout,
    "futex_timeout",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_futex_wait_child,
    "futex_wait_child",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_futex_wait_child,
    "futex_wait_child",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_futex_wait_child,
    "futex_wait_child",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_futex_wake_some,
    "futex_wake_some",
    Mode::ChaosReplay
);
rust_replay_case!(chaos_replay_rust_heap_ptrs, "heap_ptrs", Mode::ChaosReplay);
rust_replay_case!(trace_replay_rust_heap_ptrs, "heap_ptrs", Mode::TraceReplay);
rust_replay_case!(
    trace_replay_chaos_rust_heap_ptrs,
    "heap_ptrs",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_interrogate_tty,
    "interrogate_tty",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_interrogate_tty,
    "interrogate_tty",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_interrogate_tty,
    "interrogate_tty",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    #[ignore = "a mem_race recording runs about 150 s under the PMU timer (7 s without it), far past the 57 s per-test budget"]
    chaos_replay_rust_mem_race,
    "mem_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_mem_race,
    "mem_race",
    Mode::TraceReplay
);
rust_replay_case!(
    #[ignore = "a mem_race recording runs about 150 s under the PMU timer (7 s without it), far past the 57 s per-test budget"]
    trace_replay_chaos_rust_mem_race,
    "mem_race",
    Mode::TraceReplayChaos
);
rust_replay_case!(chaos_replay_rust_nanosleep, "nanosleep", Mode::ChaosReplay);
rust_replay_case!(trace_replay_rust_nanosleep, "nanosleep", Mode::TraceReplay);
rust_replay_case!(
    trace_replay_chaos_rust_nanosleep,
    "nanosleep",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_network_hello_world,
    "network_hello_world",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_network_hello_world,
    "network_hello_world",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_network_hello_world,
    "network_hello_world",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_pipe_basics,
    "pipe_basics",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_pipe_basics,
    "pipe_basics",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_pipe_basics,
    "pipe_basics",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_rust_poll,
    "poll",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_poll,
    "poll",
    Mode::TraceReplay
);
rust_replay_case!(trace_replay_chaos_rust_poll, "poll", Mode::TraceReplayChaos);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_rust_print_clock_nanosleep_monotonic_abs_race,
    "print_clock_nanosleep_monotonic_abs_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_print_clock_nanosleep_monotonic_abs_race,
    "print_clock_nanosleep_monotonic_abs_race",
    Mode::TraceReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_rust_print_clock_nanosleep_monotonic_race,
    "print_clock_nanosleep_monotonic_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_print_clock_nanosleep_monotonic_race,
    "print_clock_nanosleep_monotonic_race",
    Mode::TraceReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_rust_print_clock_nanosleep_realtime_abs_race,
    "print_clock_nanosleep_realtime_abs_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_print_clock_nanosleep_realtime_abs_race,
    "print_clock_nanosleep_realtime_abs_race",
    Mode::TraceReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    chaos_replay_rust_print_nanosleep_race,
    "print_nanosleep_race",
    Mode::ChaosReplay
);
rust_replay_case!(
    #[ignore = "replayed schedule diverges from its recording at a scheduler-turn time advance; https://github.com/rrnewton/hermit/issues/4399"]
    trace_replay_rust_print_nanosleep_race,
    "print_nanosleep_race",
    Mode::TraceReplay
);
rust_replay_case!(chaos_replay_rust_rdtsc, "rdtsc", Mode::ChaosReplay);
rust_replay_case!(trace_replay_rust_rdtsc, "rdtsc", Mode::TraceReplay);
rust_replay_case!(
    trace_replay_chaos_rust_rdtsc,
    "rdtsc",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_sched_yield,
    "sched_yield",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_sched_yield,
    "sched_yield",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_sched_yield,
    "sched_yield",
    Mode::TraceReplayChaos
);
rust_replay_case!(
    chaos_replay_rust_socketpair,
    "socketpair",
    Mode::ChaosReplay
);
rust_replay_case!(
    trace_replay_rust_socketpair,
    "socketpair",
    Mode::TraceReplay
);
rust_replay_case!(
    trace_replay_chaos_rust_socketpair,
    "socketpair",
    Mode::TraceReplayChaos
);
rust_replay_case!(chaos_replay_rust_stack_ptr, "stack_ptr", Mode::ChaosReplay);
rust_replay_case!(trace_replay_rust_stack_ptr, "stack_ptr", Mode::TraceReplay);
rust_replay_case!(
    trace_replay_chaos_rust_stack_ptr,
    "stack_ptr",
    Mode::TraceReplayChaos
);

// The internal targets' strict and chaos `hermit run` pairs, for every C
// guest and scenario above and all 25 tests/rust guests.
replay_case!(
    verify_run_strict_arch_prctl_determinism,
    "arch_prctl_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_arch_prctl_determinism,
    "arch_prctl_determinism",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_clock_determinism,
    "clock_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_clock_determinism,
    "clock_determinism",
    None,
    Mode::Chaos
);
replay_case!(verify_run_strict_clone, "clone", None, Mode::Strict);
replay_case!(verify_run_chaos_clone, "clone", None, Mode::Chaos);
replay_case!(
    verify_run_strict_epoll_determinism_control_fds,
    "epoll_determinism",
    Some("control-fds"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_control_fds,
    "epoll_determinism",
    Some("control-fds"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_dupfd,
    "epoll_determinism",
    Some("dupfd"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_dupfd,
    "epoll_determinism",
    Some("dupfd"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_edge,
    "epoll_determinism",
    Some("edge"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_edge,
    "epoll_determinism",
    Some("edge"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_mixed,
    "epoll_determinism",
    Some("mixed"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_mixed,
    "epoll_determinism",
    Some("mixed"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_multi,
    "epoll_determinism",
    Some("multi"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_multi,
    "epoll_determinism",
    Some("multi"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_nested,
    "epoll_determinism",
    Some("nested"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_nested,
    "epoll_determinism",
    Some("nested"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_epoll_determinism_oneshot,
    "epoll_determinism",
    Some("oneshot"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_epoll_determinism_oneshot,
    "epoll_determinism",
    Some("oneshot"),
    Mode::Chaos
);
replay_case!(verify_run_strict_get_cpu, "getCpu", None, Mode::Strict);
replay_case!(verify_run_chaos_get_cpu, "getCpu", None, Mode::Chaos);
replay_case!(verify_run_strict_getpid, "getpid", None, Mode::Strict);
replay_case!(verify_run_chaos_getpid, "getpid", None, Mode::Chaos);
replay_case!(
    verify_run_strict_getsockopt_null,
    "getsockopt_null",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_getsockopt_null,
    "getsockopt_null",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_hello_alarm,
    "hello_alarm",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_hello_alarm,
    "hello_alarm",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_hello_signals,
    "hello_signals",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_hello_signals,
    "hello_signals",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_io_uring_fallback,
    "io_uring_fallback",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_io_uring_fallback,
    "io_uring_fallback",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ioctl_fioclex,
    "ioctl_fioclex",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ioctl_fioclex,
    "ioctl_fioclex",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ioctl_siocethtool,
    "ioctl_siocethtool",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ioctl_siocethtool,
    "ioctl_siocethtool",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ipc_determinism_epoll,
    "ipc_determinism",
    Some("epoll"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ipc_determinism_epoll,
    "ipc_determinism",
    Some("epoll"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ipc_determinism_eventfd,
    "ipc_determinism",
    Some("eventfd"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ipc_determinism_eventfd,
    "ipc_determinism",
    Some("eventfd"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ipc_determinism_pipe_capacity,
    "ipc_determinism",
    Some("pipe-capacity"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ipc_determinism_pipe_capacity,
    "ipc_determinism",
    Some("pipe-capacity"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ipc_determinism_pipe_order,
    "ipc_determinism",
    Some("pipe-order"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ipc_determinism_pipe_order,
    "ipc_determinism",
    Some("pipe-order"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ipc_determinism_socketpair,
    "ipc_determinism",
    Some("socketpair"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ipc_determinism_socketpair,
    "ipc_determinism",
    Some("socketpair"),
    Mode::Chaos
);
replay_case!(verify_run_strict_just_spin, "just_spin", None, Mode::Strict);
replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_just_spin,
    "just_spin",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_madvise_determinism,
    "madvise_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_madvise_determinism,
    "madvise_determinism",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_memory_press,
    "memoryPress",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_memory_press,
    "memoryPress",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_mmap_determinism_fixed,
    "mmap_determinism",
    Some("fixed"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_mmap_determinism_fixed,
    "mmap_determinism",
    Some("fixed"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_mmap_determinism_heap,
    "mmap_determinism",
    Some("heap"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_mmap_determinism_heap,
    "mmap_determinism",
    Some("heap"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_mmap_determinism_multiple,
    "mmap_determinism",
    Some("multiple"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_mmap_determinism_multiple,
    "mmap_determinism",
    Some("multiple"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_mmap_determinism_reuse,
    "mmap_determinism",
    Some("reuse"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_mmap_determinism_reuse,
    "mmap_determinism",
    Some("reuse"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_mmap_determinism_shared,
    "mmap_determinism",
    Some("shared"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_mmap_determinism_shared,
    "mmap_determinism",
    Some("shared"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_nanosleep_par,
    "nanosleep-par",
    None,
    Mode::Strict
);
replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_nanosleep_par,
    "nanosleep-par",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ppoll_readv,
    "ppoll_readv",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ppoll_readv,
    "ppoll_readv",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_ppoll_simulation,
    "ppoll_simulation",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_ppoll_simulation,
    "ppoll_simulation",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_print_memaddrs,
    "print_memaddrs",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_print_memaddrs,
    "print_memaddrs",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_printf_with_threads,
    "printf_with_threads",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_printf_with_threads,
    "printf_with_threads",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_random_sources,
    "random_sources",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_random_sources,
    "random_sources",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_rcx_canonicalization,
    "rcx_canonicalization",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_rcx_canonicalization,
    "rcx_canonicalization",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_record_replay_fd_close,
    "record_replay_fd_close",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_record_replay_fd_close,
    "record_replay_fd_close",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_recvmsg_scm_rights_mmap,
    "recvmsg_scm_rights_mmap",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_recvmsg_scm_rights_mmap,
    "recvmsg_scm_rights_mmap",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_resource_determinism,
    "resource_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_resource_determinism,
    "resource_determinism",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_sched_yield_progress,
    "sched_yield_progress",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_sched_yield_progress,
    "sched_yield_progress",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_setitimer_determinism,
    "setitimer_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_setitimer_determinism,
    "setitimer_determinism",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_altstack_preservation,
    "signal_determinism",
    Some("altstack-preservation"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_altstack_preservation,
    "signal_determinism",
    Some("altstack-preservation"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_blocking_read_interrupted,
    "signal_determinism",
    Some("blocking-read-interrupted"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_blocking_read_interrupted,
    "signal_determinism",
    Some("blocking-read-interrupted"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_blocking_read_restarted,
    "signal_determinism",
    Some("blocking-read-restarted"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_blocking_read_restarted,
    "signal_determinism",
    Some("blocking-read-restarted"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_blocking_sigsuspend,
    "signal_determinism",
    Some("blocking-sigsuspend"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_blocking_sigsuspend,
    "signal_determinism",
    Some("blocking-sigsuspend"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_epoll_wait_sa_restart,
    "signal_determinism",
    Some("epoll-wait-sa-restart"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_epoll_wait_sa_restart,
    "signal_determinism",
    Some("epoll-wait-sa-restart"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_handler_reentrance,
    "signal_determinism",
    Some("handler-reentrance"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_handler_reentrance,
    "signal_determinism",
    Some("handler-reentrance"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_itimer_delivery,
    "signal_determinism",
    Some("itimer-delivery"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_itimer_delivery,
    "signal_determinism",
    Some("itimer-delivery"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_itimer_exit,
    "signal_determinism",
    Some("itimer-exit"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_itimer_exit,
    "signal_determinism",
    Some("itimer-exit"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_masks_fork_clone,
    "signal_determinism",
    Some("masks-fork-clone"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_masks_fork_clone,
    "signal_determinism",
    Some("masks-fork-clone"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_pending_exec,
    "signal_determinism",
    Some("pending-exec"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_pending_exec,
    "signal_determinism",
    Some("pending-exec"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_pending_sigsuspend,
    "signal_determinism",
    Some("pending-sigsuspend"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_pending_sigsuspend,
    "signal_determinism",
    Some("pending-sigsuspend"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_poll_sa_restart,
    "signal_determinism",
    Some("poll-sa-restart"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_poll_sa_restart,
    "signal_determinism",
    Some("poll-sa-restart"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_sigsuspend_invalid_arguments,
    "signal_determinism",
    Some("sigsuspend-invalid-arguments"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_sigsuspend_invalid_arguments,
    "signal_determinism",
    Some("sigsuspend-invalid-arguments"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_signal_determinism_sigtimedwait_sa_restart,
    "signal_determinism",
    Some("sigtimedwait-sa-restart"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_signal_determinism_sigtimedwait_sa_restart,
    "signal_determinism",
    Some("sigtimedwait-sa-restart"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_sigpipe_siginfo,
    "sigpipe_siginfo",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_sigpipe_siginfo,
    "sigpipe_siginfo",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_sigtimedwait_no_timeout,
    "sigtimedwait-no-timeout",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_sigtimedwait_no_timeout,
    "sigtimedwait-no-timeout",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_sigtimedwait_timeout_0s,
    "sigtimedwait-timeout-0s",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_sigtimedwait_timeout_0s,
    "sigtimedwait-timeout-0s",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_sigtimedwait_timeout_1s,
    "sigtimedwait-timeout-1s",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_sigtimedwait_timeout_1s,
    "sigtimedwait-timeout-1s",
    None,
    Mode::Chaos
);
replay_case!(verify_run_strict_sysinfo, "sysinfo", None, Mode::Strict);
replay_case!(verify_run_chaos_sysinfo, "sysinfo", None, Mode::Chaos);
replay_case!(
    verify_run_strict_sysinfo_uptime,
    "sysinfo_uptime",
    None,
    Mode::Strict
);
replay_case!(
    #[ignore = "over budget: 21.0 CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_sysinfo_uptime,
    "sysinfo_uptime",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_exhaustion,
    "threadExhaustion",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_exhaustion,
    "threadExhaustion",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_barrier,
    "thread_sync_determinism",
    Some("barrier"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_barrier,
    "thread_sync_determinism",
    Some("barrier"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_cancellation,
    "thread_sync_determinism",
    Some("cancellation"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_cancellation,
    "thread_sync_determinism",
    Some("cancellation"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_condvar,
    "thread_sync_determinism",
    Some("condvar"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_condvar,
    "thread_sync_determinism",
    Some("condvar"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_rwlock,
    "thread_sync_determinism",
    Some("rwlock"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_rwlock,
    "thread_sync_determinism",
    Some("rwlock"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_semaphore,
    "thread_sync_determinism",
    Some("semaphore"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_semaphore,
    "thread_sync_determinism",
    Some("semaphore"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_thread_sync_determinism_tls_fork,
    "thread_sync_determinism",
    Some("tls-fork"),
    Mode::Strict
);
replay_case!(
    verify_run_chaos_thread_sync_determinism_tls_fork,
    "thread_sync_determinism",
    Some("tls-fork"),
    Mode::Chaos
);
replay_case!(
    verify_run_strict_timer_create_determinism,
    "timer_create_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_timer_create_determinism,
    "timer_create_determinism",
    None,
    Mode::Chaos
);
replay_case!(verify_run_strict_uname, "uname", None, Mode::Strict);
replay_case!(verify_run_chaos_uname, "uname", None, Mode::Chaos);
replay_case!(
    verify_run_strict_vfork_exec,
    "vforkExec",
    None,
    Mode::Strict
);
replay_case!(verify_run_chaos_vfork_exec, "vforkExec", None, Mode::Chaos);
replay_case!(
    verify_run_strict_wait_on_child,
    "wait_on_child",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_wait_on_child,
    "wait_on_child",
    None,
    Mode::Chaos
);
replay_case!(
    verify_run_strict_writev_determinism,
    "writev_determinism",
    None,
    Mode::Strict
);
replay_case!(
    verify_run_chaos_writev_determinism,
    "writev_determinism",
    None,
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_bind_connect_race,
    "bind_connect_race",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_bind_connect_race,
    "bind_connect_race",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_clock_gettime,
    "clock_gettime",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_clock_gettime,
    "clock_gettime",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_clock_total_order,
    "clock_total_order",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_clock_total_order,
    "clock_total_order",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_exit_group,
    "exit_group",
    Mode::Strict
);
rust_replay_case!(verify_run_chaos_rust_exit_group, "exit_group", Mode::Chaos);
rust_replay_case!(
    #[ignore = "over budget: 18.2 CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_strict_rust_futex_and_print,
    "futex_and_print",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_futex_and_print,
    "futex_and_print",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_futex_timeout,
    "futex_timeout",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_futex_timeout,
    "futex_timeout",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_futex_wait_child,
    "futex_wait_child",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_futex_wait_child,
    "futex_wait_child",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_futex_wake_some,
    "futex_wake_some",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_futex_wake_some,
    "futex_wake_some",
    Mode::Chaos
);
rust_replay_case!(verify_run_strict_rust_heap_ptrs, "heap_ptrs", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_heap_ptrs, "heap_ptrs", Mode::Chaos);
rust_replay_case!(
    verify_run_strict_rust_interrogate_tty,
    "interrogate_tty",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_interrogate_tty,
    "interrogate_tty",
    Mode::Chaos
);
rust_replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_strict_rust_mem_race,
    "mem_race",
    Mode::Strict
);
rust_replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_rust_mem_race,
    "mem_race",
    Mode::Chaos
);
rust_replay_case!(verify_run_strict_rust_nanosleep, "nanosleep", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_nanosleep, "nanosleep", Mode::Chaos);
rust_replay_case!(
    verify_run_strict_rust_network_hello_world,
    "network_hello_world",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_network_hello_world,
    "network_hello_world",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_pipe_basics,
    "pipe_basics",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_pipe_basics,
    "pipe_basics",
    Mode::Chaos
);
rust_replay_case!(verify_run_strict_rust_poll, "poll", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_poll, "poll", Mode::Chaos);
rust_replay_case!(verify_run_strict_rust_poll_spin, "poll_spin", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_poll_spin, "poll_spin", Mode::Chaos);
rust_replay_case!(
    verify_run_strict_rust_print_clock_nanosleep_monotonic_abs_race,
    "print_clock_nanosleep_monotonic_abs_race",
    Mode::Strict
);
rust_replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_rust_print_clock_nanosleep_monotonic_abs_race,
    "print_clock_nanosleep_monotonic_abs_race",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_print_clock_nanosleep_monotonic_race,
    "print_clock_nanosleep_monotonic_race",
    Mode::Strict
);
rust_replay_case!(
    #[ignore = "over budget: 24.1 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_rust_print_clock_nanosleep_monotonic_race,
    "print_clock_nanosleep_monotonic_race",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_print_clock_nanosleep_realtime_abs_race,
    "print_clock_nanosleep_realtime_abs_race",
    Mode::Strict
);
rust_replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_rust_print_clock_nanosleep_realtime_abs_race,
    "print_clock_nanosleep_realtime_abs_race",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_print_nanosleep_race,
    "print_nanosleep_race",
    Mode::Strict
);
rust_replay_case!(
    #[ignore = "over budget: 24.0 (killed at the cap) CPU-s against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_rust_print_nanosleep_race,
    "print_nanosleep_race",
    Mode::Chaos
);
rust_replay_case!(verify_run_strict_rust_rdtsc, "rdtsc", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_rdtsc, "rdtsc", Mode::Chaos);
rust_replay_case!(
    verify_run_strict_rust_sched_yield,
    "sched_yield",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_sched_yield,
    "sched_yield",
    Mode::Chaos
);
rust_replay_case!(
    verify_run_strict_rust_socketpair,
    "socketpair",
    Mode::Strict
);
rust_replay_case!(verify_run_chaos_rust_socketpair, "socketpair", Mode::Chaos);
rust_replay_case!(verify_run_strict_rust_stack_ptr, "stack_ptr", Mode::Strict);
rust_replay_case!(verify_run_chaos_rust_stack_ptr, "stack_ptr", Mode::Chaos);
rust_replay_case!(
    verify_run_strict_rust_thread_random,
    "thread_random",
    Mode::Strict
);
rust_replay_case!(
    verify_run_chaos_rust_thread_random,
    "thread_random",
    Mode::Chaos
);

// The internal tests/shell targets: strict, chaos, chaos replay, trace replay
// and chaotic trace replay, with their exemptions.
shell_replay_case!(verify_run_strict_shell_date, "date.sh", Mode::Strict);
shell_replay_case!(verify_run_chaos_shell_date, "date.sh", Mode::Chaos);
shell_replay_case!(trace_replay_shell_date, "date.sh", Mode::TraceReplay);
shell_replay_case!(chaos_replay_shell_date, "date.sh", Mode::ChaosReplay);
shell_replay_case!(
    trace_replay_chaos_shell_date,
    "date.sh",
    Mode::TraceReplayChaos
);
shell_replay_case!(verify_run_strict_shell_devrand, "devrand.sh", Mode::Strict);
shell_replay_case!(verify_run_chaos_shell_devrand, "devrand.sh", Mode::Chaos);
shell_replay_case!(trace_replay_shell_devrand, "devrand.sh", Mode::TraceReplay);
shell_replay_case!(
    trace_replay_chaos_shell_devrand,
    "devrand.sh",
    Mode::TraceReplayChaos
);
shell_replay_case!(
    verify_run_strict_shell_par_work,
    "par_work.sh",
    Mode::Strict
);
shell_replay_case!(
    #[ignore = "over budget: 24.0 CPU-s (killed at the cap) against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_shell_par_work,
    "par_work.sh",
    Mode::Chaos
);
shell_replay_case!(
    #[ignore = "https://github.com/rrnewton/hermit/issues/4472: the schedule replay panics in Detcore when a non-root process spawns"]
    trace_replay_shell_par_work,
    "par_work.sh",
    Mode::TraceReplay
);
shell_replay_case!(
    chaos_replay_shell_par_work,
    "par_work.sh",
    Mode::ChaosReplay
);
shell_replay_case!(
    verify_run_strict_shell_py_hello,
    "py_hello.sh",
    Mode::Strict
);
shell_replay_case!(
    #[ignore = "over budget: 24.0 CPU-s (killed at the cap) against the 22 CPU-s per-test cap in the pinned root, 2026-10-15"]
    verify_run_chaos_shell_py_hello,
    "py_hello.sh",
    Mode::Chaos
);
shell_replay_case!(
    trace_replay_shell_py_hello,
    "py_hello.sh",
    Mode::TraceReplay
);
shell_replay_case!(verify_run_strict_shell_race, "race.sh", Mode::Strict);
shell_replay_case!(verify_run_chaos_shell_race, "race.sh", Mode::Chaos);
shell_replay_case!(trace_replay_shell_race, "race.sh", Mode::TraceReplay);
shell_replay_case!(chaos_replay_shell_race, "race.sh", Mode::ChaosReplay);
shell_replay_case!(
    trace_replay_chaos_shell_race,
    "race.sh",
    Mode::TraceReplayChaos
);
shell_replay_case!(verify_run_strict_shell_taskset, "taskset.sh", Mode::Strict);
shell_replay_case!(verify_run_chaos_shell_taskset, "taskset.sh", Mode::Chaos);
shell_replay_case!(trace_replay_shell_taskset, "taskset.sh", Mode::TraceReplay);
shell_replay_case!(chaos_replay_shell_taskset, "taskset.sh", Mode::ChaosReplay);
shell_replay_case!(
    trace_replay_chaos_shell_taskset,
    "taskset.sh",
    Mode::TraceReplayChaos
);
