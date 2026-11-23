/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Occasional end-to-end coverage for meaningful relaxed-mode flag combinations.
//!
//! The valid cross-product contains 56 configurations: relaxed mode varies
//! thread sequentialization and deterministic I/O and the three valid
//! time/metadata states; strict mode keeps both on and varies only the two
//! time/metadata states that keep virtual time, because `--strict` refuses
//! `--no-virtualize-time` (<https://github.com/rrnewton/hermit/issues/5461>);
//! both modes vary CPUID virtualization and verification. `--strace-only`
//! (with and without verification) and `--namespace-only` add three explicit
//! passthrough endpoints. Every configuration runs an exec/exit program, a
//! fixed-output program, and a threaded observation program that exercises
//! clocks, metadata, CPUID, and randomness.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

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

    for strict in [false, true] {
        let sequentialization_states: &[bool] = if strict { &[true] } else { &[true, false] };
        let deterministic_io_states: &[bool] = if strict { &[true] } else { &[true, false] };
        // `--strict` refuses `--no-virtualize-time`, so the strict states keep
        // virtual time; `invalid_matrix_states_are_rejected_without_panicking`
        // checks the refusal.
        let time_metadata_states: &[TimeMetadata] = if strict {
            &[TimeMetadata::Both, TimeMetadata::TimeOnly]
        } else {
            &[
                TimeMetadata::Both,
                TimeMetadata::TimeOnly,
                TimeMetadata::Neither,
            ]
        };
        for &sequentialize in sequentialization_states {
            for &deterministic_io in deterministic_io_states {
                for &time_metadata in time_metadata_states {
                    for virtualize_cpuid in [true, false] {
                        for verify in [false, true] {
                            let mut args = vec![
                                "--backend=ptrace",
                                "run",
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
                                // `--no-virtualize-cpuid` serves the host's CPUID, so the guest
                                // sees no profile's table. x86-64-v2 is named because it is the one
                                // profile admitted without CPUID virtualization: unnamed,
                                // `--cpu-profile=auto` picks x86-64-v3 for a guest that declares it
                                // (as CentOS 10 binaries and crt objects do), and v3 is refused
                                // without CPUID virtualization.
                                args.push("--no-virtualize-cpuid");
                                args.push("--cpu-profile=x86-64-v2");
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
        },
        Configuration {
            name: "strace-only_verify-on".to_owned(),
            args: vec!["run", "--strace-only", "--verify"],
            verify: true,
        },
        Configuration {
            name: "namespace-only_verify-off".to_owned(),
            args: vec!["run", "--namespace-only"],
            verify: false,
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
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("relaxed-flag-matrix-{}", std::process::id()));
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
            .join(format!("relaxed-flag-matrix-{}", std::process::id()))
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
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
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

    let outcome = if output.status.success() {
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
    assert_eq!(configurations.len(), 59);
    assert_eq!(
        configurations
            .iter()
            .filter(|configuration| configuration.name.starts_with("strict_"))
            .count(),
        8,
    );
    assert!(
        !configurations.iter().any(|configuration| {
            configuration.args.contains(&"--strict")
                && configuration.args.contains(&"--no-virtualize-time")
        }),
        "a valid configuration combines --strict with --no-virtualize-time"
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
    // Each state, the diagnostics its refusal must contain, and the exit status
    // where the test pins one.
    let cases: &[(&[&str], &[&str], Option<i32>)] = &[
        (
            &["--strict", "--no-sequentialize-threads"],
            &["--strict", "--no-sequentialize-threads"],
            None,
        ),
        (
            &["--strict", "--no-deterministic-io"],
            &["--strict", "--no-deterministic-io"],
            None,
        ),
        (
            &["--no-virtualize-time"],
            &["also requires --no-virtualize-metadata"],
            None,
        ),
        (
            &["--namespace-only", "--verify"],
            &["--namespace-only", "--verify"],
            None,
        ),
        // https://github.com/rrnewton/hermit/issues/5461: host time would reach
        // the guest unbound. Argument validation refuses it as a policy
        // refusal.
        (
            &[
                "--strict",
                "--no-virtualize-time",
                "--no-virtualize-metadata",
            ],
            &[
                "--strict refuses --no-virtualize-time:",
                "host time",
                "unbound",
                "HERMIT_POLICY_REFUSAL class=policy-refusal",
            ],
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        ),
    ];

    for &(args, expected_diagnostics, expected_exit) in cases {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", KILL_AFTER, "5s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .arg("run")
            .args(args)
            .args(["--", "/bin/true"]);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
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
        if let Some(expected_exit) = expected_exit {
            assert_eq!(
                output.status.code(),
                Some(expected_exit),
                "invalid state {args:?} exited with another status\n{}",
                bounded_output(&output),
            );
        }
        for &expected in expected_diagnostics {
            assert!(
                combined.contains(expected),
                "invalid state {args:?} omitted diagnostic {expected:?}\n{}",
                bounded_output(&output),
            );
        }
    }
}

/// `--strict` refuses `--debug-futex-mode=external` before the guest starts,
/// by the name `strict-external-futex-mode`: that mode admits a futex wake at
/// a host-timed instant. The refusal is a policy refusal (exit
/// `HERMIT_POLICY_REFUSAL_EXIT` and the `HERMIT_POLICY_REFUSAL
/// class=policy-refusal` line) with no guest output, from the command line and
/// from a `--config` file. `--allow-unsupported-syscalls` does not lift it:
/// beside `--strict` the opt-in is a usage error (exit 2), and the guest does
/// not run either. Without `--strict` the mode still runs the guest as a
/// debugging aid, and `--strict` still runs it in the precise and polling
/// modes.
#[test]
fn strict_refuses_the_external_futex_mode_before_the_guest_runs() {
    const MARKER: &str = "external-futex-mode-guest-ran";
    let directory = tempfile::tempdir().expect("failed to create a config directory");
    let strict_config = directory.path().join("strict.yaml");
    fs::write(
        &strict_config,
        "schema: hermit-run-config/v2\nrun:\n  strict: true\n  debug-futex-mode: external\n",
    )
    .expect("failed to write a run config");
    let strict_config = strict_config.to_str().expect("config path is UTF-8");
    let run = |args: &[&str]| -> Output {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", KILL_AFTER, "120s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .arg("run")
            .args(args)
            .args(["--", "/bin/echo", MARKER]);
        hermit_test::configure_guest_execution(&mut command);
        hermit_test::hermit_output(&format!("{command:?}"), &mut command)
    };

    let refused: [&[&str]; 2] = [
        &["--strict", "--debug-futex-mode=external"],
        &["--config", strict_config],
    ];
    for args in refused {
        let output = run(args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{args:?} was not refused as a policy refusal\n{}",
            bounded_output(&output),
        );
        assert!(
            stderr.contains(
                "--strict refuses --debug-futex-mode=external (strict-external-futex-mode):"
            ) && stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{args:?} was not refused by name\n{}",
            bounded_output(&output),
        );
        assert!(
            output.stdout.is_empty(),
            "{args:?}: the refused guest ran\n{}",
            bounded_output(&output),
        );
    }

    let args = [
        "--strict",
        "--debug-futex-mode=external",
        "--allow-unsupported-syscalls",
    ];
    let output = run(&args);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{args:?} was not refused as a usage error\n{}",
        bounded_output(&output),
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
        "{args:?} was not refused for the conflict\n{}",
        bounded_output(&output),
    );
    assert!(
        output.stdout.is_empty(),
        "{args:?}: the refused guest ran\n{}",
        bounded_output(&output),
    );

    let ran: [&[&str]; 3] = [
        &["--debug-futex-mode=external"],
        &["--strict", "--debug-futex-mode=precise"],
        &["--strict", "--debug-futex-mode=polling"],
    ];
    for args in ran {
        let output = run(args);
        assert!(
            output.status.success(),
            "{args:?} did not run the guest\n{}",
            bounded_output(&output),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{MARKER}\n"),
            "{args:?}\n{}",
            bounded_output(&output),
        );
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
    for configuration in &configurations {
        for program in programs {
            let (outcome, elapsed_ms) = run_case(configuration, program);
            match outcome.as_str() {
                "deterministic" => deterministic += 1,
                "nondeterministic" => nondeterministic += 1,
                "completed" => completed += 1,
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
    assert_eq!(total, configurations.len() * programs.len());
    println!(
        "flag matrix: {total} cases classified (completed={completed}, deterministic={deterministic}, explicit-nondeterminism={nondeterministic}); report={}",
        path.display(),
    );
}
