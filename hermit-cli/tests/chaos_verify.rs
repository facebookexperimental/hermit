/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use detcore::logdiff;
use detcore::logdiff::DetLogFilter;
use detcore::logdiff::LogDiffOpts;
use detcore::preemptions::read_trace;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<Workloads> = OnceLock::new();
static CAS_SEQUENCE: OnceLock<PathBuf> = OnceLock::new();

struct Workloads {
    build_root: PathBuf,
    hello_chaos: PathBuf,
    wait_on_child: PathBuf,
}

struct CommandResult {
    rendered: String,
    output: Output,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn run_command(mut command: Command) -> CommandResult {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    CommandResult { rendered, output }
}

fn command_failure(result: &CommandResult) -> String {
    format!(
        "{}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        result.rendered,
        result.output.status,
        String::from_utf8_lossy(&result.output.stdout),
        String::from_utf8_lossy(&result.output.stderr),
    )
}

fn assert_success(result: &CommandResult, label: &str) {
    assert!(
        result.output.status.success(),
        "{label} failed: {}",
        command_failure(result)
    );
}

fn compile_rust(source: &Path, output: &Path) {
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    let result = run_command(command);
    assert_success(&result, "Rust workload compilation");
}

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(source)
        .arg("-o")
        .arg(output);
    let result = run_command(command);
    assert_success(&result, "C workload compilation");
}

fn workloads() -> &'static Workloads {
    WORKLOADS.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-batch-b-workloads");
        fs::create_dir_all(&build_root).expect("failed to create workload build directory");

        let hello_chaos = build_root.join("hello-chaos");
        compile_rust(&repository.join("tests/chaos/hello_chaos.rs"), &hello_chaos);

        let wait_on_child = build_root.join("wait-on-child");
        compile_c(&repository.join("tests/c/wait_on_child.c"), &wait_on_child);

        Workloads {
            build_root,
            hello_chaos,
            wait_on_child,
        }
    })
}

fn cas_sequence() -> &'static Path {
    CAS_SEQUENCE
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let output = workloads().build_root.join("cas-sequence");
            compile_rust(&repository.join("tests/chaos/cas_sequence.rs"), &output);
            output
        })
        .as_path()
}

fn hermit_command(workload: &Path, global_args: &[String], run_args: &[String]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(global_args)
        .arg("run")
        .args(["--base-env=minimal", "--no-virtualize-cpuid"])
        .args(run_args)
        .arg(format!("--bind={}", workloads().build_root.display()))
        .arg(workload);
    command
}

#[test]
fn chaos_seeds_expose_race_and_verify_reproducibly() {
    let _guard = hermit_run_lock();
    let workload = &workloads().hello_chaos;
    let mut passing_seed = None;
    let mut failing_seed = None;

    for seed in 0..32 {
        let result = run_command(hermit_command(
            workload,
            &[],
            &[
                "--chaos".to_owned(),
                format!("--sched-seed={seed}"),
                "--preemption-timeout=disabled".to_owned(),
            ],
        ));
        let stdout = String::from_utf8_lossy(&result.output.stdout);

        if stdout.contains("Parent went first") {
            assert!(
                !result.output.status.success(),
                "the race workload reported its failing order but exited successfully: {}",
                command_failure(&result)
            );
            failing_seed.get_or_insert(seed);
        } else if stdout.contains("Child went first") {
            assert_success(&result, "chaos race success outcome");
            passing_seed.get_or_insert(seed);
        } else {
            panic!(
                "chaos seed {seed} produced neither expected race outcome: {}",
                command_failure(&result)
            );
        }

        if passing_seed.is_some() && failing_seed.is_some() {
            break;
        }
    }

    let passing_seed = passing_seed.expect("32 chaos seeds should expose a passing race order");
    let failing_seed = failing_seed.expect("32 chaos seeds should expose a failing race order");

    for (seed, expected_success) in [(passing_seed, true), (failing_seed, false)] {
        let result = run_command(hermit_command(
            workload,
            &[],
            &[
                "--verify".to_owned(),
                "--verify-allow=both".to_owned(),
                "--chaos".to_owned(),
                format!("--sched-seed={seed}"),
                "--preemption-timeout=disabled".to_owned(),
            ],
        ));
        let stderr = String::from_utf8_lossy(&result.output.stderr);

        assert_eq!(
            result.output.status.success(),
            expected_success,
            "verify should preserve the guest status for seed {seed}: {}",
            command_failure(&result)
        );
        assert!(
            stderr.contains("Success: deterministic."),
            "verify did not confirm reproducibility for seed {seed}: {}",
            command_failure(&result)
        );
    }
}

#[test]
fn schedule_replay_preserves_schedule_and_syscall_trace() {
    let _guard = hermit_run_lock();
    let workloads = workloads();
    let trace_dir = tempfile::Builder::new()
        .prefix("hermit-batch-b-trace-")
        .tempdir_in(&workloads.build_root)
        .expect("failed to create trace directory");
    let recorded_log = trace_dir.path().join("recorded.log");
    let replayed_log = trace_dir.path().join("replayed.log");
    let recorded_schedule_path = trace_dir.path().join("recorded.schedule");
    let replayed_schedule_path = trace_dir.path().join("replayed.schedule");

    let recorded = run_command(hermit_command(
        &workloads.wait_on_child,
        &[
            "--log=trace".to_owned(),
            format!("--log-file={}", recorded_log.display()),
        ],
        &[
            "--chaos".to_owned(),
            "--sched-seed=17".to_owned(),
            "--preemption-timeout=disabled".to_owned(),
            format!(
                "--record-preemptions-to={}",
                recorded_schedule_path.display()
            ),
        ],
    ));
    assert_success(&recorded, "chaos schedule recording");

    let replayed = run_command(hermit_command(
        &workloads.wait_on_child,
        &[
            "--log=trace".to_owned(),
            format!("--log-file={}", replayed_log.display()),
        ],
        &[
            "--preemption-timeout=disabled".to_owned(),
            format!(
                "--replay-schedule-from={}",
                recorded_schedule_path.display()
            ),
            format!(
                "--record-preemptions-to={}",
                replayed_schedule_path.display()
            ),
            "--die-on-desync".to_owned(),
            "--replay-exhausted-panic".to_owned(),
        ],
    ));
    assert_success(&replayed, "strict schedule replay");
    assert_eq!(recorded.output.stdout, replayed.output.stdout);

    let recorded_schedule = read_trace(&recorded_schedule_path);
    let replayed_schedule = read_trace(&replayed_schedule_path);
    assert!(
        recorded_schedule.len() > 10,
        "recorded schedule should contain meaningful trace data"
    );
    assert_eq!(recorded_schedule.len(), replayed_schedule.len());
    assert!(
        recorded_schedule
            .iter()
            .map(|event| event.dettid)
            .collect::<BTreeSet<_>>()
            .len()
            >= 2,
        "fork/wait schedule should contain at least two deterministic tids"
    );
    for (recorded_event, replayed_event) in recorded_schedule.iter().zip(&replayed_schedule) {
        assert_eq!(recorded_event.dettid, replayed_event.dettid);
        assert_eq!(recorded_event.op, replayed_event.op);
        assert_eq!(recorded_event.count, replayed_event.count);
    }

    let traces_differ = logdiff::log_diff(
        &recorded_log,
        &replayed_log,
        &LogDiffOpts {
            strip_lines: true,
            ignore_lines: vec![
                "CHAOSRAND".to_owned(),
                "advance global time for scheduler turn".to_owned(),
                "inbound syscall: exit_group".to_owned(),
            ],
            syscall_history: 5,
            no_color: true,
            skip_commit: true,
            include_detlogs: vec![DetLogFilter::Syscall, DetLogFilter::SyscallResult],
            ..Default::default()
        },
    );
    assert!(
        !traces_differ,
        "recorded and replayed syscall traces differ"
    );
}

#[test]
#[ignore = "requires accessible PMU hardware performance counters"]
fn pmu_chaos_records_branch_preemptions() {
    let _guard = hermit_run_lock();
    assert!(
        reverie_ptrace::is_perf_supported(),
        "ignored PMU test requires perf_event access"
    );
    let trace_dir = tempfile::Builder::new()
        .prefix("hermit-batch-b-pmu-")
        .tempdir_in(&workloads().build_root)
        .expect("failed to create PMU trace directory");
    let schedule_path = trace_dir.path().join("pmu.schedule");
    let result = run_command(hermit_command(
        cas_sequence(),
        &[],
        &[
            "--chaos".to_owned(),
            "--sched-seed=0".to_owned(),
            "--preemption-timeout=120000".to_owned(),
            format!("--record-preemptions-to={}", schedule_path.display()),
        ],
    ));
    let stdout = String::from_utf8_lossy(&result.output.stdout);
    assert!(
        stdout.contains("Antagonistic schedule reached")
            || stdout.contains("Did not find antagonistic schedule"),
        "PMU chaos workload did not complete normally: {}",
        command_failure(&result)
    );

    let schedule = read_trace(&schedule_path);
    assert!(
        schedule
            .iter()
            .any(|event| format!("{:?}", event.op) == "Branch"),
        "PMU chaos schedule contained no branch preemption events"
    );
}
