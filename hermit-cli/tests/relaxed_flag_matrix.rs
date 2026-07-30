/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Occasional end-to-end coverage for meaningful relaxed-mode flag combinations.
//!
//! The valid cross-product contains 60 configurations: relaxed mode varies
//! thread sequentialization and deterministic I/O, strict mode keeps both on,
//! and both modes vary the three valid time/metadata states, CPUID
//! virtualization, and verification. `--strace-only` (with and without
//! verification) and `--namespace-only` add three explicit passthrough
//! endpoints. Every configuration runs an exec/exit program, a fixed-output
//! program, and a threaded observation program that exercises clocks,
//! metadata, CPUID, and randomness.

use std::env;
use std::fs;
use std::fs::File;
use std::io::BufWriter;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;
use std::time::Instant;

const TIMEOUT_SECONDS: &str = "30s";
const KILL_AFTER: &str = "5s";
const DETERMINISM_MARKER: &str = "Success: deterministic. Determinism verified.";
const NONDETERMINISM_MARKER: &str = "Failure: nondeterministic.";

static OBSERVATION_GUEST: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug)]
struct Configuration {
    name: String,
    args: Vec<&'static str>,
    verify: bool,
    strict_without_virtual_time: bool,
}

#[derive(Clone, Copy)]
struct Program<'a> {
    name: &'static str,
    path: &'a Path,
    args: &'static [&'static str],
    marker: Option<&'static str>,
}

#[derive(Clone, Copy)]
enum TimeMetadata {
    Both,
    TimeOnly,
    Neither,
}

impl TimeMetadata {
    fn name(self) -> &'static str {
        match self {
            Self::Both => "time-on_metadata-on",
            Self::TimeOnly => "time-on_metadata-off",
            Self::Neither => "time-off_metadata-off",
        }
    }

    fn append_args(self, args: &mut Vec<&'static str>) {
        match self {
            Self::Both => {}
            Self::TimeOnly => args.push("--no-virtualize-metadata"),
            Self::Neither => {
                args.push("--no-virtualize-time");
                args.push("--no-virtualize-metadata");
            }
        }
    }
}

fn matrix_configurations() -> Vec<Configuration> {
    let mut configurations = Vec::new();
    let time_metadata_states = [
        TimeMetadata::Both,
        TimeMetadata::TimeOnly,
        TimeMetadata::Neither,
    ];

    for strict in [false, true] {
        let sequentialization_states: &[bool] = if strict { &[true] } else { &[true, false] };
        let deterministic_io_states: &[bool] = if strict { &[true] } else { &[true, false] };
        for &sequentialize in sequentialization_states {
            for &deterministic_io in deterministic_io_states {
                for &time_metadata in &time_metadata_states {
                    for virtualize_cpuid in [true, false] {
                        for verify in [false, true] {
                            let mut args = vec![
                                "run",
                                "--backend=ptrace",
                                "--base-env=minimal",
                                "--max-timeslice=disabled",
                            ];
                            if strict {
                                args.push("--strict");
                            }
                            if !sequentialize {
                                args.push("--no-sequentialize-threads");
                            }
                            if !deterministic_io {
                                args.push("--no-deterministic-io");
                            }
                            time_metadata.append_args(&mut args);
                            if !virtualize_cpuid {
                                args.push("--no-virtualize-cpuid");
                            }
                            if verify {
                                args.push("--verify");
                            }

                            configurations.push(Configuration {
                                name: format!(
                                    "{}_seq-{}_io-{}_{}_cpuid-{}_verify-{}",
                                    if strict { "strict" } else { "relaxed" },
                                    on_off(sequentialize),
                                    on_off(deterministic_io),
                                    time_metadata.name(),
                                    on_off(virtualize_cpuid),
                                    on_off(verify),
                                ),
                                args,
                                verify,
                                strict_without_virtual_time: strict
                                    && matches!(time_metadata, TimeMetadata::Neither),
                            });
                        }
                    }
                }
            }
        }
    }

    configurations.extend([
        Configuration {
            name: "strace-only_verify-off".to_owned(),
            args: vec!["run", "--strace-only"],
            verify: false,
            strict_without_virtual_time: false,
        },
        Configuration {
            name: "strace-only_verify-on".to_owned(),
            args: vec!["run", "--strace-only", "--verify"],
            verify: true,
            strict_without_virtual_time: false,
        },
        Configuration {
            name: "namespace-only_verify-off".to_owned(),
            args: vec!["run", "--namespace-only"],
            verify: false,
            strict_without_virtual_time: false,
        },
    ]);
    configurations
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn observation_guest() -> &'static Path {
    OBSERVATION_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("relaxed-flag-matrix");
            fs::create_dir_all(&build_root)
                .expect("failed to create relaxed flag matrix build directory");
            let output = build_root.join("observation-guest");
            let compilation = Command::new("cc")
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/relaxed_flag_matrix.c"))
                .arg("-o")
                .arg(&output)
                .output()
                .expect("failed to start relaxed flag matrix guest compilation");
            assert_success(&compilation, "observation guest compilation");
            output
        })
        .as_path()
}

fn report_path() -> PathBuf {
    match env::var_os("HERMIT_FLAG_MATRIX_REPORT").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository")
            .join(path),
        None => Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("relaxed-flag-matrix")
            .join("results.tsv"),
    }
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\n{}",
        output.status,
        bounded_output(output),
    );
}

fn bounded_output(output: &Output) -> String {
    fn bounded(bytes: &[u8]) -> String {
        const LIMIT: usize = 4096;
        let start = bytes.len().saturating_sub(LIMIT);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
    format!(
        "stdout tail:\n{}\nstderr tail:\n{}",
        bounded(&output.stdout),
        bounded(&output.stderr),
    )
}

fn run_case(configuration: &Configuration, program: Program<'_>) -> (String, u128) {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", KILL_AFTER, TIMEOUT_SECONDS])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(&configuration.args)
        .arg("--")
        .arg(program.path)
        .args(program.args)
        .env_remove("HERMIT_FAIL_CLOSED");
    let rendered = format!("{command:?}");
    let started = Instant::now();
    let output = command.output().unwrap_or_else(|error| {
        panic!(
            "failed to start configuration {} / {}: {rendered}: {error}",
            configuration.name, program.name,
        )
    });
    let elapsed_ms = started.elapsed().as_millis();
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    for crash_marker in [
        "panicked at",
        "fatal runtime error",
        "Segmentation fault",
        "stack overflow",
    ] {
        assert!(
            !combined.contains(crash_marker),
            "configuration {} / {} emitted crash marker {crash_marker:?}\ncommand: {rendered}\n{}",
            configuration.name,
            program.name,
            bounded_output(&output),
        );
    }
    assert_ne!(
        output.status.code(),
        Some(124),
        "configuration {} / {} timed out\ncommand: {rendered}\n{}",
        configuration.name,
        program.name,
        bounded_output(&output),
    );

    // Verify mode captures both guest outputs in its private run logs, so only
    // direct runs expose the guest marker on this process's stdout.
    if !configuration.verify
        && output.status.success()
        && let Some(marker) = program.marker
    {
        assert!(
            combined.contains(marker),
            "configuration {} / {} omitted guest marker {marker:?}\ncommand: {rendered}\n{}",
            configuration.name,
            program.name,
            bounded_output(&output),
        );
    }

    // https://github.com/rrnewton/hermit/issues/1176: strict mode currently
    // turns the opted-out clock syscall into an opaque container exit. Keep
    // the waiver pinned to the exact configuration, workload, and signature.
    let known_issue_1176 = configuration.strict_without_virtual_time
        && program.name == "threaded-observation"
        && !output.status.success()
        && combined.contains("Sandbox container exited unexpectedly")
        && (configuration.verify || combined.contains("inbound syscall: clock_gettime"));

    let outcome = if known_issue_1176 {
        "expected-failure-1176"
    } else if output.status.success() {
        if configuration.verify {
            assert!(
                combined.contains(DETERMINISM_MARKER),
                "configuration {} / {} exited successfully without the verification marker\ncommand: {rendered}\n{}",
                configuration.name,
                program.name,
                bounded_output(&output),
            );
            "deterministic"
        } else {
            "completed"
        }
    } else {
        assert!(
            configuration.verify && combined.contains(NONDETERMINISM_MARKER),
            "configuration {} / {} failed for a reason other than a sane verification result ({})\ncommand: {rendered}\n{}",
            configuration.name,
            program.name,
            output.status,
            bounded_output(&output),
        );
        "nondeterministic"
    };
    (outcome.to_owned(), elapsed_ms)
}

#[test]
fn matrix_shape_covers_valid_and_passthrough_states() {
    let configurations = matrix_configurations();
    assert_eq!(configurations.len(), 63);
    assert_eq!(
        configurations
            .iter()
            .filter(|configuration| configuration.name.starts_with("strict_"))
            .count(),
        12,
    );
    assert_eq!(
        configurations
            .iter()
            .filter(|configuration| configuration.name.starts_with("relaxed_"))
            .count(),
        48,
    );
}

#[test]
fn invalid_matrix_states_are_rejected_without_panicking() {
    let cases: &[(&[&str], &[&str])] = &[
        (
            &["--strict", "--no-sequentialize-threads"],
            &["--strict", "--no-sequentialize-threads"],
        ),
        (
            &["--strict", "--no-deterministic-io"],
            &["--strict", "--no-deterministic-io"],
        ),
        (
            &["--no-virtualize-time"],
            &["also requires --no-virtualize-metadata"],
        ),
        (
            &["--namespace-only", "--verify"],
            &["--namespace-only", "--verify"],
        ),
    ];

    for &(args, expected_diagnostics) in cases {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", KILL_AFTER, "5s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .arg("run")
            .args(args)
            .args(["--", "/bin/true"]);
        let output = command.output().unwrap_or_else(|error| {
            panic!("failed to start invalid-state probe {args:?}: {error}")
        });
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );

        assert!(
            !output.status.success() && output.status.code() != Some(124),
            "invalid state {args:?} was accepted or timed out\n{}",
            bounded_output(&output),
        );
        assert!(
            !combined.contains("panicked at"),
            "invalid state {args:?} panicked\n{}",
            bounded_output(&output),
        );
        for &expected in expected_diagnostics {
            assert!(
                combined.contains(expected),
                "invalid state {args:?} omitted diagnostic {expected:?}\n{}",
                bounded_output(&output),
            );
        }
    }
}

#[test]
#[ignore = "occasional e2e: exhaustive ptrace flag matrix requires mount namespaces"]
fn meaningful_flag_combinations_run_without_crashing() {
    let configurations = matrix_configurations();
    let true_path = Path::new("/bin/true");
    let echo_path = Path::new("/bin/echo");
    assert!(true_path.is_file(), "required program /bin/true is missing");
    assert!(echo_path.is_file(), "required program /bin/echo is missing");
    let guest = observation_guest();
    let programs = [
        Program {
            name: "exec-exit",
            path: true_path,
            args: &[],
            marker: None,
        },
        Program {
            name: "fixed-stdio",
            path: echo_path,
            args: &["flag-matrix-echo"],
            marker: Some("flag-matrix-echo"),
        },
        Program {
            name: "threaded-observation",
            path: guest,
            args: &[],
            marker: Some("flag-matrix-probe"),
        },
    ];

    let path = report_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create flag matrix report directory");
    }
    let mut report = BufWriter::new(File::create(&path).expect("failed to create matrix report"));
    writeln!(
        report,
        "configuration\tprogram\tverify\toutcome\telapsed_ms"
    )
    .expect("failed to write matrix report header");

    let mut deterministic = 0;
    let mut nondeterministic = 0;
    let mut completed = 0;
    let mut expected_failure_1176 = 0;
    for configuration in &configurations {
        for program in programs {
            let (outcome, elapsed_ms) = run_case(configuration, program);
            match outcome.as_str() {
                "deterministic" => deterministic += 1,
                "nondeterministic" => nondeterministic += 1,
                "completed" => completed += 1,
                "expected-failure-1176" => expected_failure_1176 += 1,
                unexpected => panic!("unexpected matrix outcome {unexpected}"),
            }
            writeln!(
                report,
                "{}\t{}\t{}\t{}\t{}",
                configuration.name, program.name, configuration.verify, outcome, elapsed_ms,
            )
            .expect("failed to write matrix result");
            report.flush().expect("failed to flush matrix report");
        }
    }

    let total = deterministic + nondeterministic + completed;
    assert_eq!(expected_failure_1176, 4, "update issue #1176 expectations");
    assert_eq!(
        total + expected_failure_1176,
        configurations.len() * programs.len()
    );
    println!(
        "flag matrix: {} cases classified (completed={completed}, deterministic={deterministic}, explicit-nondeterminism={nondeterministic}, expected-failure-1176={expected_failure_1176}); report={}",
        total + expected_failure_1176,
        path.display(),
    );
}
