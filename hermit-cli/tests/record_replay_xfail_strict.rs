/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const DIVERGENCE_MARKER: &str = "Replay diverged from recording";
const CASE_TIMEOUT: Duration = Duration::from_secs(120);
const RECORD_TIMEOUT: &str = "90";

#[derive(Clone, Copy, Debug)]
enum Expectation {
    Pass,
    Divergence(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Xfail,
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Command {
        program: &'static str,
        args: &'static [&'static str],
    },
    Functional(&'static str),
}

#[derive(Clone, Copy, Debug)]
struct Case {
    label: &'static str,
    expectation: Expectation,
    workload: Workload,
}

struct RunResult {
    record: PhaseResult,
    replay: Option<PhaseResult>,
    evidence: Result<RecordingEvidence, String>,
}

struct PhaseResult {
    output: Output,
    timed_out: bool,
}

#[derive(Debug)]
struct RecordingEvidence {
    metadata_bytes: u64,
    event_files: usize,
    event_bytes: u64,
}

struct Observation<'a> {
    record_success: bool,
    evidence: Option<&'a RecordingEvidence>,
    replay_success: bool,
    replay_exit_code: Option<i32>,
    record_stdout: &'a [u8],
    replay_stdout: &'a [u8],
    replay_output: &'a str,
}

const CASES: &[Case] = &[
    // Positive controls prove the harness still treats ordinary, unlisted rows
    // as must-pass cases. They are not waivers and cannot become XFAIL.
    Case {
        label: "echo",
        expectation: Expectation::Pass,
        workload: Workload::Command {
            program: "/bin/echo",
            args: &["hermit-rr-xfail-control"],
        },
    },
    Case {
        label: "true",
        expectation: Expectation::Pass,
        workload: Workload::Command {
            program: "/usr/bin/true",
            args: &[],
        },
    },
    Case {
        label: "g++",
        expectation: Expectation::Divergence(
            "C++ front-end header/.gch path resolution desynchronizes readlink and newfstatat",
        ),
        workload: Workload::Functional("g++"),
    },
    Case {
        label: "ar",
        expectation: Expectation::Divergence(
            "archive workload teardown reorders execveat rm -rf against the recorded stream",
        ),
        workload: Workload::Functional("ar"),
    },
    Case {
        label: "strip",
        expectation: Expectation::Divergence("replay event stream desynchronizes"),
        workload: Workload::Functional("strip"),
    },
    Case {
        label: "gprof",
        expectation: Expectation::Divergence("replay event stream desynchronizes"),
        workload: Workload::Functional("gprof"),
    },
    Case {
        label: "gcov",
        expectation: Expectation::Divergence("replay event stream desynchronizes"),
        workload: Workload::Functional("gcov"),
    },
];

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .to_path_buf()
}

fn hermit_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hermit"))
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn bounded_output(output: &Output) -> String {
    const LIMIT: usize = 16 * 1024;
    let combined = combined_output(output);
    let bytes = combined.as_bytes();
    let start = bytes.len().saturating_sub(LIMIT);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn classify(
    label: &str,
    expectation: Expectation,
    observation: Observation<'_>,
) -> Result<Verdict, String> {
    if !observation.record_success {
        return Err(format!(
            "NO-RESULT: {label} did not produce a successful recording"
        ));
    }

    let Some(evidence) = observation.evidence else {
        return Err(format!(
            "NO-RESULT: {label} produced no dereferenceable recording artifact"
        ));
    };
    if evidence.metadata_bytes == 0 || evidence.event_files == 0 || evidence.event_bytes == 0 {
        return Err(format!(
            "NO-RESULT: {label} recording evidence is empty: {evidence:?}"
        ));
    }

    if observation.replay_success {
        if observation.record_stdout != observation.replay_stdout {
            return Err(format!(
                "{label} replay exited successfully but stdout differs from the recording"
            ));
        }
        return match expectation {
            Expectation::Pass => Ok(Verdict::Pass),
            Expectation::Divergence(reason) => Err(format!(
                "XPASS: {label} now records and replays successfully; remove its expected-divergence entry ({reason})"
            )),
        };
    }

    if matches!(observation.replay_exit_code, Some(124 | 137)) {
        return Err(format!(
            "NO-RESULT: {label} replay timed out or required a forced kill (exit {:?})",
            observation.replay_exit_code
        ));
    }

    match expectation {
        Expectation::Pass => Err(format!(
            "{label} is an unlisted must-pass row but replay failed (exit {:?})",
            observation.replay_exit_code
        )),
        Expectation::Divergence(_) if observation.replay_output.contains(DIVERGENCE_MARKER) => {
            Ok(Verdict::Xfail)
        }
        Expectation::Divergence(reason) => Err(format!(
            "{label} failed with an unrecognized shape (exit {:?}); expected only a replay event-stream divergence ({reason})",
            observation.replay_exit_code
        )),
    }
}

fn prepare_fixtures(repository: &Path, fixture_root: &Path) {
    let script = repository.join("tests/compat/prepare_real_compat_fixtures.sh");
    let output = Command::new(&script)
        .arg(fixture_root)
        .output()
        .unwrap_or_else(|error| panic!("failed to start {}: {error}", script.display()));
    assert!(
        output.status.success(),
        "fixture preparation failed with {}:\n{}",
        output.status,
        bounded_output(&output),
    );
}

fn kill_process_group(pid: u32, label: &str) {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            panic!("failed to kill {label} process group {pid}: {error}");
        }
    }
}

fn output_contains(path: &Path, marker: &str) -> bool {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).contains(marker))
        .unwrap_or(false)
}

fn inspect_recording(data_dir: &Path) -> Result<RecordingEvidence, String> {
    let last_path = data_dir.join("last");
    let last = fs::read_to_string(&last_path)
        .map_err(|error| format!("failed to read {}: {error}", last_path.display()))?;
    let recording_id = last.trim();
    if recording_id.is_empty() || recording_id.contains('/') {
        return Err(format!(
            "{} contains an invalid recording id {recording_id:?}",
            last_path.display()
        ));
    }

    let recording_dir = data_dir.join(recording_id);
    let metadata_path = recording_dir.join("metadata.json");
    let metadata_bytes = fs::metadata(&metadata_path)
        .map_err(|error| format!("failed to stat {}: {error}", metadata_path.display()))?
        .len();
    if metadata_bytes == 0 {
        return Err(format!("{} is empty", metadata_path.display()));
    }

    let thread_dir = recording_dir.join("thread");
    let entries = fs::read_dir(&thread_dir)
        .map_err(|error| format!("failed to read {}: {error}", thread_dir.display()))?;
    let mut event_files = 0;
    let mut event_bytes = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read an entry in {}: {error}",
                thread_dir.display()
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.parse::<u32>().is_err() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("failed to stat {}: {error}", entry.path().display()))?;
        if metadata.is_file() && metadata.len() != 0 {
            event_files += 1;
            event_bytes += metadata.len();
        }
    }

    if event_files == 0 || event_bytes == 0 {
        return Err(format!(
            "{} contains no nonempty event streams",
            thread_dir.display()
        ));
    }
    Ok(RecordingEvidence {
        metadata_bytes,
        event_files,
        event_bytes,
    })
}

fn run_phase(
    mut command: Command,
    stdout_path: &Path,
    stderr_path: &Path,
    label: &str,
    stop_after_divergence: bool,
) -> PhaseResult {
    command
        .process_group(0)
        .stdout(Stdio::from(
            File::create(stdout_path).expect("failed to create phase stdout file"),
        ))
        .stderr(Stdio::from(
            File::create(stderr_path).expect("failed to create phase stderr file"),
        ));
    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None)
                if stop_after_divergence
                    && (output_contains(stdout_path, DIVERGENCE_MARKER)
                        || output_contains(stderr_path, DIVERGENCE_MARKER)) =>
            {
                kill_process_group(child.id(), label);
                break false;
            }
            Ok(None) if started.elapsed() >= CASE_TIMEOUT => {
                kill_process_group(child.id(), label);
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                kill_process_group(child.id(), label);
                let _ = child.wait();
                panic!("failed to poll {label}: {rendered}: {error}");
            }
        }
    };

    kill_process_group(child.id(), label);
    let status = child
        .wait()
        .unwrap_or_else(|error| panic!("failed to collect {label}: {rendered}: {error}"));
    let output = Output {
        status,
        stdout: fs::read(stdout_path).expect("failed to read phase stdout"),
        stderr: fs::read(stderr_path).expect("failed to read phase stderr"),
    };
    PhaseResult { output, timed_out }
}

fn run_case(
    repository: &Path,
    hermit: &Path,
    fixture_root: &Path,
    recording_root: &Path,
    case: Case,
) -> RunResult {
    let data_dir = recording_root.join(case.label);
    let mut record_command = Command::new(hermit);
    record_command
        .args(["--log=info", "record", "start"])
        .arg(format!("--record-timeout={RECORD_TIMEOUT}"))
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--");

    match case.workload {
        Workload::Command { program, args } => {
            record_command.arg(program).args(args);
        }
        Workload::Functional(label) => {
            record_command
                .arg("/usr/bin/env")
                .arg(OsStr::new(&format!(
                    "REAL_COMPAT_FIXTURES={}",
                    fixture_root.display()
                )))
                .arg("/bin/bash")
                .arg(repository.join("tests/compat/real_compat_workload.sh"))
                .arg(label);
        }
    }

    let record = run_phase(
        record_command,
        &recording_root.join(format!("{}.record.stdout", case.label)),
        &recording_root.join(format!("{}.record.stderr", case.label)),
        &format!("{} record", case.label),
        false,
    );
    let evidence = inspect_recording(&data_dir);
    if record.timed_out || !record.output.status.success() || evidence.is_err() {
        return RunResult {
            record,
            replay: None,
            evidence,
        };
    }

    let mut replay_command = Command::new(hermit);
    replay_command
        .args(["--log=info", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.display()));
    let replay = run_phase(
        replay_command,
        &recording_root.join(format!("{}.replay.stdout", case.label)),
        &recording_root.join(format!("{}.replay.stderr", case.label)),
        &format!("{} replay", case.label),
        matches!(case.expectation, Expectation::Divergence(_)),
    );
    RunResult {
        record,
        replay: Some(replay),
        evidence,
    }
}

fn classify_run(case: Case, result: &RunResult) -> Result<Verdict, String> {
    let replay = result.replay.as_ref();
    let replay_stdout = replay.map_or(&[][..], |phase| phase.output.stdout.as_slice());
    let replay_output = replay
        .map(|phase| combined_output(&phase.output))
        .unwrap_or_default();
    classify(
        case.label,
        case.expectation,
        Observation {
            record_success: !result.record.timed_out && result.record.output.status.success(),
            evidence: result.evidence.as_ref().ok(),
            replay_success: replay
                .is_some_and(|phase| !phase.timed_out && phase.output.status.success()),
            replay_exit_code: replay.and_then(|phase| {
                if phase.timed_out {
                    Some(124)
                } else {
                    phase.output.status.code()
                }
            }),
            record_stdout: &result.record.output.stdout,
            replay_stdout,
            replay_output: &replay_output,
        },
    )
}

fn synthetic_evidence() -> RecordingEvidence {
    RecordingEvidence {
        metadata_bytes: 64,
        event_files: 1,
        event_bytes: 128,
    }
}

#[test]
fn classifier_tolerates_only_the_expected_divergence_shape() {
    let evidence = synthetic_evidence();
    assert_eq!(
        classify(
            "g++",
            Expectation::Divergence("known"),
            Observation {
                record_success: true,
                evidence: Some(&evidence),
                replay_success: false,
                replay_exit_code: Some(101),
                record_stdout: b"",
                replay_stdout: b"",
                replay_output: "Replay diverged from recording /tmp/example",
            },
        ),
        Ok(Verdict::Xfail)
    );
    assert!(
        classify(
            "g++",
            Expectation::Divergence("known"),
            Observation {
                record_success: true,
                evidence: Some(&evidence),
                replay_success: false,
                replay_exit_code: Some(101),
                record_stdout: b"",
                replay_stdout: b"",
                replay_output: "fixture setup panicked",
            },
        )
        .is_err()
    );
}

#[test]
fn classifier_rejects_an_unexpected_pass() {
    let evidence = synthetic_evidence();
    let error = classify(
        "g++",
        Expectation::Divergence("known"),
        Observation {
            record_success: true,
            evidence: Some(&evidence),
            replay_success: true,
            replay_exit_code: Some(0),
            record_stdout: b"same\n",
            replay_stdout: b"same\n",
            replay_output: "",
        },
    )
    .expect_err("a fixed expected-divergence row must fail xfail-strict");
    assert!(error.contains("XPASS"), "unexpected diagnostic: {error}");
}

#[test]
fn classifier_leaves_three_unlisted_passes_unaffected() {
    let evidence = synthetic_evidence();
    for label in ["echo", "true", "pwd"] {
        assert_eq!(
            classify(
                label,
                Expectation::Pass,
                Observation {
                    record_success: true,
                    evidence: Some(&evidence),
                    replay_success: true,
                    replay_exit_code: Some(0),
                    record_stdout: b"same\n",
                    replay_stdout: b"same\n",
                    replay_output: "",
                },
            ),
            Ok(Verdict::Pass),
            "unlisted row {label} changed classification"
        );
    }
}

#[test]
fn classifier_rejects_markers_without_recording_evidence() {
    let error = classify(
        "forged",
        Expectation::Pass,
        Observation {
            record_success: true,
            evidence: None,
            replay_success: true,
            replay_exit_code: Some(0),
            record_stdout: b"Success: replay matched recording.\n",
            replay_stdout: b"Success: replay matched recording.\n",
            replay_output: "Replay diverged from recording",
        },
    )
    .expect_err("markers without a recording artifact must be a no-result");
    assert!(
        error.contains("NO-RESULT"),
        "unexpected diagnostic: {error}"
    );
}

#[test]
fn fabricated_markers_without_recording_are_no_result() {
    let cargo_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(cargo_tmp).expect("failed to create Cargo test temp directory");
    let run_root = tempfile::Builder::new()
        .prefix("record-replay-xfail-forged-")
        .tempdir_in(cargo_tmp)
        .expect("failed to create forged-marker temp directory");
    let fake_hermit = run_root.path().join("fake-hermit");
    fs::write(
        &fake_hermit,
        "#!/bin/sh\nprintf '%s\\n' 'Success: replay matched recording.'\nexit 0\n",
    )
    .expect("failed to write forged Hermit fixture");
    fs::set_permissions(&fake_hermit, fs::Permissions::from_mode(0o755))
        .expect("failed to make forged Hermit fixture executable");
    let recording_root = run_root.path().join("recordings");
    fs::create_dir(&recording_root).expect("failed to create forged recording root");
    let case = Case {
        label: "forged",
        expectation: Expectation::Pass,
        workload: Workload::Command {
            program: "/usr/bin/true",
            args: &[],
        },
    };
    let result = run_case(
        &repository(),
        &fake_hermit,
        run_root.path(),
        &recording_root,
        case,
    );
    let error = classify_run(case, &result)
        .expect_err("a marker-only fixture must not satisfy the record/replay ratchet");
    assert!(
        error.contains("NO-RESULT"),
        "unexpected diagnostic: {error}"
    );
}

#[test]
fn genuine_pass_in_xfail_set_demands_removal() {
    let cargo_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(cargo_tmp).expect("failed to create Cargo test temp directory");
    let run_root = tempfile::Builder::new()
        .prefix("record-replay-xfail-xpass-")
        .tempdir_in(cargo_tmp)
        .expect("failed to create XPASS temp directory");
    let recording_root = run_root.path().join("recordings");
    fs::create_dir(&recording_root).expect("failed to create XPASS recording root");
    let case = Case {
        label: "xpass-control",
        expectation: Expectation::Divergence("deliberate end-to-end XPASS plant"),
        workload: Workload::Command {
            program: "/usr/bin/true",
            args: &[],
        },
    };
    let result = run_case(
        &repository(),
        &hermit_binary(),
        run_root.path(),
        &recording_root,
        case,
    );
    let error = classify_run(case, &result)
        .expect_err("a genuine pass in the xfail set must demand removal");
    assert!(error.contains("XPASS"), "unexpected diagnostic: {error}");
}

#[test]
fn record_replay_compatibility_is_xfail_strict() {
    let repository = repository();
    let hermit = hermit_binary();
    assert!(
        hermit.is_file(),
        "Hermit binary does not exist: {}",
        hermit.display()
    );

    let cargo_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(cargo_tmp).expect("failed to create Cargo test temp directory");
    let run_root = tempfile::Builder::new()
        .prefix("record-replay-xfail-strict-")
        .tempdir_in(cargo_tmp)
        .expect("failed to create record/replay xfail temp directory");
    let fixture_root = run_root.path().join("fixtures");
    let recording_root = run_root.path().join("recordings");
    fs::create_dir(&recording_root).expect("failed to create recording root");
    prepare_fixtures(&repository, &fixture_root);

    let mut passed = 0;
    let mut xfailed = 0;
    let mut failures = Vec::new();
    for &case in CASES {
        let result = run_case(&repository, &hermit, &fixture_root, &recording_root, case);
        match classify_run(case, &result) {
            Ok(Verdict::Pass) => {
                passed += 1;
                println!("PASS  {} evidence={:?}", case.label, result.evidence);
            }
            Ok(Verdict::Xfail) => {
                xfailed += 1;
                println!("XFAIL {} evidence={:?}", case.label, result.evidence);
            }
            Err(error) => {
                let replay = result.replay.as_ref().map(|phase| {
                    format!(
                        "replay status: {}\nreplay output tail:\n{}",
                        phase.output.status,
                        bounded_output(&phase.output)
                    )
                });
                failures.push(format!(
                    "{error}\nrecord status: {}\nrecord output tail:\n{}\nevidence: {:?}\n{}",
                    result.record.output.status,
                    bounded_output(&result.record.output),
                    result.evidence,
                    replay.as_deref().unwrap_or("replay not attempted"),
                ));
            }
        }
    }

    assert_eq!(passed, 2, "the two must-pass controls must remain unwaived");
    assert_eq!(
        xfailed, 5,
        "every documented replay divergence must still be exercised"
    );
    assert!(
        failures.is_empty(),
        "record/replay xfail-strict failures:\n\n{}",
        failures.join("\n\n")
    );
}
