/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod common;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use common::hermit_binary;
use common::nondeterminism::NondeterminismCase;

const DETERMINISM_RUNS: usize = 5;
const REPEATABLE_EPOCH: &str = "2000-12-31T23:59:59.123456789Z";

static HERMIT_CLOCK_LOCK: Mutex<()> = Mutex::new(());
static CLOCK_GUEST: OnceLock<PathBuf> = OnceLock::new();
static REPLAY_EPOCH_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_binary::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn hermit_clock_lock() -> MutexGuard<'static, ()> {
    HERMIT_CLOCK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clock_guest() -> &'static Path {
    CLOCK_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-determinism");
            fs::create_dir_all(&build_root)
                .expect("failed to create clock determinism build directory");
            let binary = build_root.join("clock_determinism");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-D_GNU_SOURCE",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/clock_determinism.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "clock determinism guest compilation");
            binary
        })
        .as_path()
}

/// Two spinning threads plus eight absolute CLOCK_REALTIME samples; see
/// `tests/c/replay_epoch_probe.c`.
fn replay_epoch_guest() -> &'static Path {
    REPLAY_EPOCH_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-determinism");
            fs::create_dir_all(&build_root)
                .expect("failed to create clock determinism build directory");
            let binary = build_root.join("replay_epoch_probe");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/replay_epoch_probe.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "replay epoch guest compilation");
            binary
        })
        .as_path()
}

/// One run of the replay-epoch guest. Only `epoch_env` may supply
/// `HERMIT_EPOCH`, so an omitted epoch really is omitted.
fn run_replay_epoch_guest(options: &[String], epoch_env: Option<&str>) -> Output {
    let mut command = Command::new(hermit_binary::hermit_binary());
    command.env_remove("HERMIT_EPOCH");
    if let Some(epoch) = epoch_env {
        command.env("HERMIT_EPOCH", epoch);
    }
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    command.args(options).arg("--").arg(replay_epoch_guest());
    hermit_binary::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"))
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The `(epoch, source)` pair of the run's `virtual-time epoch=` notice.
fn reported_epoch(output: &Output) -> (String, String) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stderr
        .lines()
        .find_map(|line| line.strip_prefix("hermit: virtual-time epoch="))
        .unwrap_or_else(|| panic!("no virtual-time epoch notice in stderr:\n{stderr}"));
    let (epoch, rest) = line
        .split_once(" source=")
        .expect("notice names its source");
    let (source, _) = rest
        .split_once(';')
        .expect("notice ends its source with ';'");
    (epoch.to_owned(), source.to_owned())
}

/// The probe's clock trajectory: all eight samples and the completion marker.
fn assert_full_trajectory(output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let samples = stdout
        .lines()
        .filter(|line| line.starts_with("sample "))
        .count();
    assert_eq!(samples, 8, "stdout:\n{stdout}");
    assert!(
        stdout.ends_with("replay-epoch-probe-ok\n"),
        "stdout:\n{stdout}"
    );
}

fn schedule_path(name: &str) -> PathBuf {
    let directory = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-determinism");
    fs::create_dir_all(&directory).expect("failed to create schedule directory");
    let path = directory.join(name);
    let _ = fs::remove_file(&path);
    path
}

fn run_clock_matrix(iteration: usize) -> Vec<u8> {
    let mut command = Command::new(hermit_binary::hermit_binary());
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    command.arg(format!("--epoch={REPEATABLE_EPOCH}")).arg("--");
    command.arg(clock_guest());
    let output = command_output(
        command,
        &format!("clock determinism matrix, iteration {}", iteration + 1),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "CLOCK_REALTIME ",
        "CLOCK_MONOTONIC ",
        "CLOCK_PROCESS_CPUTIME_ID ",
        "CLOCK_THREAD_CPUTIME_ID ",
        "CLOCK_BOOTTIME ",
        "gettimeofday consistent ",
        "clock matrix success\n",
    ] {
        assert!(
            stdout.contains(expected),
            "clock matrix iteration {} omitted {expected:?}\nstdout:\n{stdout}\nstderr:\n{}",
            iteration + 1,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output.stdout
}

fn run_date_at_epoch(epoch: Option<&str>) -> Output {
    let mut command = Command::new(hermit_binary::hermit_binary());
    // Keep the omitted-input case independent of the caller's valid override.
    // Environment precedence is covered separately in isolated parser children.
    command.env_remove("HERMIT_EPOCH");
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args(["--", "/bin/date", "+%s.%N"]);
    command_output(command, "virtual epoch date probe")
}

#[test]
fn default_virtual_epoch_tracks_invocation_start_and_is_reported() {
    let _guard = hermit_clock_lock();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let output = run_date_at_epoch(None);
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let observed = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .unwrap();
    assert!(
        observed >= before && observed <= after + 1.0,
        "default virtual epoch {observed} was not captured near host now [{before}, {after}]"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("virtual-time epoch="), "{stderr}");
    assert!(stderr.contains("source=host-now"), "{stderr}");
    assert!(stderr.contains("reproduce with --epoch="), "{stderr}");
}

#[test]
fn explicit_virtual_epoch_reproduces_identical_observed_time() {
    let _guard = hermit_clock_lock();
    let first = run_date_at_epoch(Some(REPEATABLE_EPOCH));
    let second = run_date_at_epoch(Some(REPEATABLE_EPOCH));
    assert_eq!(first.stdout, second.stdout);
    let rendered = String::from_utf8_lossy(&first.stdout);
    let (seconds, nanos) = rendered.trim().split_once('.').unwrap();
    let observed = seconds.parse::<u64>().unwrap() * 1_000_000_000 + nanos.parse::<u64>().unwrap();
    let epoch = 978_307_199_123_456_789;
    assert!(
        (epoch..epoch + 1_000_000_000).contains(&observed),
        "explicit epoch did not seed the expected virtual-time trajectory: {observed}"
    );
    for output in [first, second] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("source=explicit"), "{stderr}");
        assert!(
            stderr.contains("2000-12-31T23:59:59.123456789+00:00"),
            "{stderr}"
        );
    }
}

#[test]
fn clock_apis_are_deterministic_across_five_runs() {
    let _guard = hermit_clock_lock();
    let baseline = run_clock_matrix(0);

    for iteration in 1..DETERMINISM_RUNS {
        assert_eq!(
            run_clock_matrix(iteration),
            baseline,
            "clock matrix changed output on iteration {}",
            iteration + 1,
        );
    }
}

// NONDET_SOURCE: timestamp
#[test]
fn strict_mode_eliminates_native_clock_nondeterminism() {
    let _guard = hermit_clock_lock();
    let case =
        NondeterminismCase::new("timestamp", Path::new("/bin/date"), &["+%s%N"]).with_retries(5);

    case.assert_nondeterministic_without_hermit();
    case.assert_nondeterministic_with_noop_verify();
    case.assert_deterministic_with_strict();
}

const REPEATABLE_EPOCH_RFC3339: &str = "2000-12-31T23:59:59.123456789+00:00";

/// <https://github.com/rrnewton/hermit/issues/3411>: a preemption record holds
/// absolute virtual times measured from the recording's epoch, so a replay
/// whose epoch was omitted must start its clock there instead of at a fresh
/// host-clock sample. The replayed run then prints the same eight absolute
/// clock samples, for both replay flags. (With `--max-timeslice=disabled` the
/// samples advance by per-syscall charges; the PMU-driven chaos case that
/// replays real preemption points is `run_chaos_preemption_replay_reuses_the_recorded_epoch`
/// in cli.rs.)
#[test]
fn replay_with_an_omitted_epoch_starts_from_the_recorded_epoch() {
    let _guard = hermit_clock_lock();
    for flag in ["--replay-preemptions-from", "--replay-schedule-from"] {
        let schedule = schedule_path("recorded-at-explicit-epoch.json");
        let recorded = run_replay_epoch_guest(
            &[
                format!("--epoch={REPEATABLE_EPOCH}"),
                format!("--record-preemptions-to={}", schedule.display()),
            ],
            None,
        );
        assert_success(&recorded, "recording");
        assert_full_trajectory(&recorded);
        assert_eq!(
            reported_epoch(&recorded),
            (REPEATABLE_EPOCH_RFC3339.to_owned(), "explicit".to_owned())
        );

        let replayed = run_replay_epoch_guest(&[format!("{flag}={}", schedule.display())], None);
        assert_success(&replayed, flag);
        assert_eq!(
            reported_epoch(&replayed),
            (REPEATABLE_EPOCH_RFC3339.to_owned(), "recording".to_owned()),
            "{flag}"
        );
        assert_eq!(
            String::from_utf8_lossy(&replayed.stdout),
            String::from_utf8_lossy(&recorded.stdout),
            "{flag} did not reproduce the recorded clock trajectory"
        );
    }
}

/// The hermit-verify shape of <https://github.com/rrnewton/hermit/issues/3411>:
/// both runs omit the epoch, so the recording samples the host clock and the
/// replay, a separate process started later, must reuse that sample.
#[test]
fn replay_of_a_host_clock_recording_reuses_its_epoch() {
    let _guard = hermit_clock_lock();
    let schedule = schedule_path("recorded-at-host-now.json");
    let recorded = run_replay_epoch_guest(
        &[format!("--record-preemptions-to={}", schedule.display())],
        None,
    );
    assert_success(&recorded, "recording");
    let (recorded_epoch, source) = reported_epoch(&recorded);
    assert_eq!(source, "host-now");

    let replayed = run_replay_epoch_guest(
        &[format!("--replay-preemptions-from={}", schedule.display())],
        None,
    );
    assert_success(&replayed, "replay");
    assert_eq!(
        reported_epoch(&replayed),
        (recorded_epoch, "recording".to_owned())
    );
    assert_full_trajectory(&replayed);
    assert_eq!(replayed.stdout, recorded.stdout);
}

/// <https://github.com/rrnewton/hermit/issues/3413>: an explicit epoch that
/// contradicts the recording used to panic mid-replay with "Cannot set end of
/// timeslice". It is now refused before the guest starts, naming both epochs,
/// whether it came from `--epoch` or `HERMIT_EPOCH`, and with or without
/// virtual time. An explicit epoch equal to the recorded one is accepted.
#[test]
fn replay_refuses_an_explicit_epoch_that_contradicts_the_recording() {
    let _guard = hermit_clock_lock();
    let schedule = schedule_path("recorded-for-refusal.json");
    let recorded = run_replay_epoch_guest(
        &[
            format!("--epoch={REPEATABLE_EPOCH}"),
            format!("--record-preemptions-to={}", schedule.display()),
        ],
        None,
    );
    assert_success(&recorded, "recording");
    let replay = format!("--replay-preemptions-from={}", schedule.display());

    for (options, epoch_env) in [
        (
            vec!["--epoch=2026-01-01T00:00:00Z".to_owned(), replay.clone()],
            None,
        ),
        (vec![replay.clone()], Some("2026-01-01T00:00:00Z")),
        // Detcore's logical clock, which the recorded timeslice ends are
        // measured on, starts at the epoch even without virtual time.
        (
            vec![
                "--no-virtualize-time".to_owned(),
                "--no-virtualize-metadata".to_owned(),
                "--epoch=2026-01-01T00:00:00Z".to_owned(),
                replay.clone(),
            ],
            None,
        ),
    ] {
        let refused = run_replay_epoch_guest(&options, epoch_env);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        // EXIT-CLASS: hermit
        assert_eq!(refused.status.code(), Some(122), "{options:?}: {stderr}");
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{stderr}"
        );
        assert!(
            stderr.contains("the explicit virtual-time epoch 2026-01-01T00:00:00+00:00"),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("pass --epoch={REPEATABLE_EPOCH_RFC3339}")),
            "{stderr}"
        );
        assert!(!stderr.contains("Cannot set end of timeslice"), "{stderr}");
        assert!(
            refused.stdout.is_empty(),
            "the guest ran despite the refusal: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
    }

    let agreeing = run_replay_epoch_guest(&[format!("--epoch={REPEATABLE_EPOCH}"), replay], None);
    assert_success(&agreeing, "replay with the recorded epoch");
    assert_eq!(
        reported_epoch(&agreeing),
        (REPEATABLE_EPOCH_RFC3339.to_owned(), "explicit".to_owned())
    );
    assert_eq!(agreeing.stdout, recorded.stdout);
}

/// With `--no-virtualize-time` the guest reads the host clock and there is no
/// epoch notice, but detcore's logical clock -- which recorded timeslice ends
/// are measured on -- still starts at the epoch. So an omitted epoch is adopted
/// from the recording there too, which the replay's own recording shows. The
/// guest is `/bin/true` because strict mode refuses a host clock read without
/// virtual time.
#[test]
fn replay_without_virtual_time_adopts_the_recorded_epoch() {
    let _guard = hermit_clock_lock();
    let schedule = schedule_path("true-recorded-at-explicit-epoch.json");
    let rerecorded = schedule_path("true-rerecorded-without-virtual-time.json");
    let run = |options: &[String]| {
        let mut command = Command::new(hermit_binary::hermit_binary());
        command.env_remove("HERMIT_EPOCH").args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
        ]);
        command.args(options).args(["--", "/bin/true"]);
        command_output(command, &format!("{options:?}"))
    };
    run(&[
        format!("--epoch={REPEATABLE_EPOCH}"),
        format!("--record-preemptions-to={}", schedule.display()),
    ]);
    run(&[
        "--no-virtualize-time".to_owned(),
        "--no-virtualize-metadata".to_owned(),
        format!("--replay-preemptions-from={}", schedule.display()),
        format!("--record-preemptions-to={}", rerecorded.display()),
    ]);
    let rerecord: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&rerecorded).unwrap()).unwrap();
    assert_eq!(
        rerecord["epoch"], "2000-12-31T23:59:59.123456789Z",
        "a --no-virtualize-time replay did not start from the recorded epoch: {rerecord}"
    );
}
