/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::process::Command;
use std::process::Output;

fn run(binary: &str, arguments: &[&str]) -> Output {
    run_from(binary, arguments, None)
}

/// Git exports these to hooks and `git rebase --exec` steps, and they override
/// the working directory. An inherited `GIT_DIR` would put the deliberately
/// non-repository directory below inside a repository
/// (https://github.com/rrnewton/hermit/issues/3362).
const REPOSITORY_LOCATION_VARIABLES: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

fn located_by_directory(program: &str) -> Command {
    let mut command = Command::new(program);
    for name in REPOSITORY_LOCATION_VARIABLES {
        command.env_remove(name);
    }
    command
}

fn run_from(binary: &str, arguments: &[&str], current_dir: Option<&Path>) -> Output {
    let mut command = located_by_directory(binary);
    command.args(arguments);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    // A test that copies an executable and runs it can see ETXTBSY: while the
    // copy is open for writing, a sibling test's fork briefly holds that
    // descriptor until its child execs. The window closes within milliseconds.
    for _ in 0..100 {
        match command.output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            output => return output.expect("failed to execute manifest CLI"),
        }
    }
    panic!("{binary} stayed busy (ETXTBSY) for a second");
}

fn assert_help(binary: &str, name: &str, current_dir: &Path) {
    assert_command_help(binary, &[], &format!("Usage: {name}"), current_dir);
}

fn assert_command_help(binary: &str, command: &[&str], usage: &str, current_dir: &Path) {
    let invoke = |flag| {
        let mut arguments = command.to_vec();
        arguments.push(flag);
        run_from(binary, &arguments, Some(current_dir))
    };
    let short = invoke("-h");
    let long = invoke("--help");
    for output in [&short, &long] {
        assert!(
            output.status.success(),
            "{usage} help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with(usage), "{stdout}");
        assert!(output.stderr.is_empty(), "help wrote stderr: {output:?}");
    }
    assert_eq!(short.stdout, long.stdout, "-h and --help must agree");
}

fn non_repository_dir(label: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "hermit-manifest-cli-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).expect("create non-repository working directory");
    std::fs::write(directory.join(".git"), "deliberately not a Git directory\n")
        .expect("create an explicit non-repository boundary");
    let git_probe = located_by_directory("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&directory)
        .output()
        .expect("run non-repository control");
    assert!(
        !git_probe.status.success(),
        "negative control is inside a Git repository: {git_probe:?}"
    );
    directory
}

#[test]
fn every_manifest_cli_has_conventional_help() {
    let non_repo = non_repository_dir("root-help");
    for (name, binary) in [
        ("test-harness", env!("CARGO_BIN_EXE_test-harness")),
        (
            "hermit-manifest-plan",
            env!("CARGO_BIN_EXE_hermit-manifest-plan"),
        ),
        (
            "strict-green-authority",
            env!("CARGO_BIN_EXE_strict-green-authority"),
        ),
        (
            "generate-test-footprints",
            env!("CARGO_BIN_EXE_generate-test-footprints"),
        ),
        ("manifest-metadata", env!("CARGO_BIN_EXE_manifest-metadata")),
        (
            "generate-parity-cells",
            env!("CARGO_BIN_EXE_generate-parity-cells"),
        ),
    ] {
        assert_help(binary, name, &non_repo);
    }
    std::fs::remove_dir_all(&non_repo).expect("remove non-repository working directory");
}

#[test]
fn every_test_harness_subcommand_has_conventional_help() {
    // This is the complete semantic command list printed by test-harness. Clap's
    // generated `help` dispatchers in sibling binaries are alternate help syntax,
    // not product subcommands, and therefore are not part of this census.
    const COMMANDS: &[&str] = &[
        "validate",
        "plan",
        "expected-plan",
        "audit-gaps",
        "audit-inventory",
        "audit-test-binary-registration",
        "audit-test-footprints",
        "audit-ci",
        "build",
        "audit-compile",
        "run",
        "parity",
        "selftest",
    ];

    let non_repo = non_repository_dir("subcommand-help");
    let harness = env!("CARGO_BIN_EXE_test-harness");
    let root_help = run_from(harness, &["--help"], Some(&non_repo));
    assert!(root_help.status.success(), "{root_help:?}");
    let stdout = String::from_utf8(root_help.stdout).expect("root help must be UTF-8");
    let advertised = stdout
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(|line| {
            line.split_whitespace()
                .next()
                .expect("command line has a name")
        })
        .collect::<Vec<_>>();
    assert_eq!(advertised, COMMANDS, "subcommand census is stale");

    for command in COMMANDS {
        assert_command_help(
            harness,
            &[*command],
            &format!("Usage: test-harness {command}"),
            &non_repo,
        );
    }
    std::fs::remove_dir_all(&non_repo).expect("remove non-repository working directory");
}

#[test]
fn test_harness_help_names_every_public_environment_control() {
    let output = run(env!("CARGO_BIN_EXE_test-harness"), &["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help must be UTF-8");
    for variable in [
        "E2E_RESULT_ROOT",
        "E2E_BUILD_ROOT",
        "E2E_RUN_ID",
        "E2E_RUN_INDEX",
        "E2E_MACHINE_SHORTNAME",
        "E2E_KERNEL_VERSION",
        "HERMIT_BIN",
        "HERMIT_E2E_EMPTY_WORKDIR",
        "E2E_KEEP_VERIFY_LOGS",
        "E2E_PARITY_SELECT",
        "E2E_PARITY_POST_PASS",
        "HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER",
        "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER",
    ] {
        assert!(stdout.contains(variable), "help omitted {variable}");
    }
    for internal in ["DAGRUN_TEST_COUNTS_PATH", "HERMIT_E2E_SCHEDULED_JOBS"] {
        assert!(
            !stdout.contains(internal),
            "internal runner plumbing leaked into public help: {internal}"
        );
    }
}

#[test]
fn execution_subcommand_help_names_every_environment_read() {
    const RUN_CONTEXT: &[&str] = &[
        "E2E_RESULT_ROOT",
        "E2E_BUILD_ROOT",
        "E2E_RUN_ID",
        "E2E_RUN_INDEX",
        "E2E_MACHINE_SHORTNAME",
        "E2E_KERNEL_VERSION",
        "HERMIT_BIN",
        "HERMIT_E2E_EMPTY_WORKDIR",
        "E2E_KEEP_VERIFY_LOGS",
        "HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER",
        "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER",
        "HOME",
        "RUSTUP_HOME",
        "CARGO_HOME",
    ];
    let harness = env!("CARGO_BIN_EXE_test-harness");
    for command in ["build", "audit-compile", "run"] {
        let output = run(harness, &[command, "--help"]);
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("help must be UTF-8");
        for variable in RUN_CONTEXT {
            assert!(
                stdout.contains(&format!("  {variable}=")),
                "{command} help omitted {variable}"
            );
        }
        assert_eq!(
            stdout.contains("DAGRUN_TEST_COUNTS_PATH"),
            command == "run",
            "DAGRUN_TEST_COUNTS_PATH belongs only to the run protocol"
        );
        for parity in ["E2E_PARITY_SELECT", "E2E_PARITY_POST_PASS"] {
            assert_eq!(
                stdout.contains(&format!("  {parity}=")),
                command == "run",
                "only run reads {parity}, for its parity post-pass"
            );
        }
    }

    for command in [
        "validate",
        "plan",
        "expected-plan",
        "audit-gaps",
        "audit-inventory",
        "audit-test-binary-registration",
        "audit-test-footprints",
        "audit-ci",
        "selftest",
    ] {
        let output = run(harness, &[command, "--help"]);
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("help must be UTF-8");
        assert!(
            !stdout.contains("Environment:"),
            "{command} does not read execution environment controls"
        );
    }
}

#[test]
fn subcommand_help_precedes_environment_validation_and_output_creation() {
    let non_repo = non_repository_dir("side-effect-free-help");
    let result_root = non_repo.join("must-not-exist");
    let counts = non_repo.join("counts-must-not-exist.json");
    let output = located_by_directory(env!("CARGO_BIN_EXE_test-harness"))
        .args(["run", "-h"])
        .current_dir(&non_repo)
        .env("E2E_RESULT_ROOT", &result_root)
        .env("E2E_RUN_INDEX", "not-a-number")
        .env("DAGRUN_TEST_COUNTS_PATH", &counts)
        .output()
        .expect("run test-harness help");
    assert!(output.status.success(), "{output:?}");
    assert!(!result_root.exists(), "help created the result root");
    assert!(!counts.exists(), "help wrote the dagrun protocol output");
    std::fs::remove_dir_all(&non_repo).expect("remove non-repository working directory");
}

#[test]
fn help_does_not_turn_missing_or_unknown_arguments_into_success() {
    let missing_command = run(env!("CARGO_BIN_EXE_test-harness"), &[]);
    assert_eq!(missing_command.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&missing_command.stderr);
    assert!(stderr.contains("missing command"), "{stderr}");
    assert!(stderr.contains("test-harness --help"), "{stderr}");

    let unknown_command = run(env!("CARGO_BIN_EXE_test-harness"), &["definitely-unknown"]);
    assert_eq!(unknown_command.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown_command.stderr).contains("unknown command"),
        "{unknown_command:?}"
    );

    let unknown_option = run(
        env!("CARGO_BIN_EXE_test-harness"),
        &["validate", "--definitely-unknown"],
    );
    assert_eq!(unknown_option.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown_option.stderr).contains("unknown option"),
        "{unknown_option:?}"
    );

    let missing_authority_args = run(env!("CARGO_BIN_EXE_strict-green-authority"), &[]);
    assert_eq!(missing_authority_args.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing_authority_args.stderr).contains("missing --claims"),
        "{missing_authority_args:?}"
    );

    let unknown_plan_option = run(
        env!("CARGO_BIN_EXE_hermit-manifest-plan"),
        &["--definitely-unknown"],
    );
    assert_eq!(unknown_plan_option.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&unknown_plan_option.stderr).contains("unknown argument"),
        "{unknown_plan_option:?}"
    );

    for args in [
        vec!["--definitely-unknown"],
        vec!["--help", "--definitely-unknown"],
        vec!["unexpected-positional"],
    ] {
        let output = run(env!("CARGO_BIN_EXE_manifest-metadata"), &args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        assert!(
            output.stdout.is_empty(),
            "invalid invocation exported metadata"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
            "{args:?}: {output:?}"
        );
    }
}

/// The source-snapshot flags redirect a caller who uses them where they do not
/// apply, and --repo-root reaches expected-plan too, from a Git-less snapshot.
#[test]
fn test_harness_source_snapshot_flags_redirect_misuse() {
    let harness = env!("CARGO_BIN_EXE_test-harness");
    let sha = "03bbb83581fad247251df6363f50e61e24c2957e";
    for (arguments, expected) in [
        (vec!["plan", "--source-sha", sha], "pass --repo-root DIR"),
        (
            vec!["run", "--repo-root", ".", "--source-sha", "03bbb835"],
            "--source-sha must be a full 40-digit",
        ),
        (
            vec!["plan", "--tpx-json", "/nonexistent/tpx.jsonl"],
            "--tpx-json is accepted by run only",
        ),
        (
            vec!["expected-plan", "--format", "json"],
            "expected-plan accepts only --repo-root DIR and --source-sha SHA",
        ),
        (
            vec!["plan", "--repo-root", "/nonexistent-hermit-root"],
            "name an existing Hermit source tree",
        ),
    ] {
        let output = run(harness, &arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{arguments:?}: {stderr}");
    }
    let root = env!("CARGO_MANIFEST_DIR").to_string() + "/../..";
    let default = run(harness, &["expected-plan"]);
    let rooted = run(harness, &["expected-plan", "--repo-root", &root]);
    assert!(
        default.status.success() && rooted.status.success(),
        "{default:?} {rooted:?}"
    );
    assert_eq!(default.stdout, rooted.stdout);

    // A `git archive` of HEAD has no .git: without --source-sha the planner's
    // inventory check redirects to it; with it, the plan is printed.
    let snapshot = non_repository_dir("source-snapshot-plan");
    let archived = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "git -C '{root}' archive HEAD | tar x -C '{}'",
            snapshot.display()
        ))
        .status()
        .expect("run git archive");
    assert!(archived.success());
    let head = std::process::Command::new("git")
        .args(["-C", &root, "rev-parse", "HEAD"])
        .output()
        .expect("run git rev-parse");
    let head = String::from_utf8(head.stdout).unwrap().trim().to_string();
    let snapshot_root = snapshot.to_str().unwrap();
    let refused = run(harness, &["expected-plan", "--repo-root", snapshot_root]);
    let planned = run(
        harness,
        &[
            "expected-plan",
            "--repo-root",
            snapshot_root,
            "--source-sha",
            &head,
        ],
    );
    std::fs::remove_dir_all(&snapshot).expect("remove snapshot");
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("pass --source-snapshot"));
    assert!(planned.status.success(), "{planned:?}");
    assert!(String::from_utf8_lossy(&planned.stdout).contains("\"cells\""));
}

/// `run --parity-reference ptrace` used to add a ptrace reference run whose
/// log comparison could overwrite a candidate's outcome. The flag was removed
/// in https://github.com/rrnewton/hermit/issues/3301. It is refused by name,
/// before any selection, execution or output, and the refusal says what
/// replaced it.
#[test]
fn test_harness_refuses_the_removed_parity_reference_flag() {
    const REFUSAL: &str = "test-harness: --parity-reference was removed: a ptrace reference \
         run no longer decides a cell's outcome \
         (https://github.com/rrnewton/hermit/issues/3301). Drop the flag; each selected \
         verify cell runs its own backend's strict verification.\n";
    let harness = env!("CARGO_BIN_EXE_test-harness");
    let directory = non_repository_dir("removed-parity-reference");
    let results = directory.join("results.jsonl");
    let results_arg = results.to_string_lossy().into_owned();
    for arguments in [
        vec!["run", "--parity-reference", "ptrace"],
        vec!["run", "--parity-reference"],
        vec![
            "run",
            "--results",
            results_arg.as_str(),
            "--category",
            "c-programs",
            "--parity-reference",
            "ptrace",
        ],
        vec!["plan", "--parity-reference", "ptrace"],
    ] {
        let output = run_from(harness, &arguments, Some(&directory));
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            REFUSAL,
            "{arguments:?}"
        );
        assert!(!results.exists(), "{arguments:?} created {results:?}");
    }
    for help in [vec!["--help"], vec!["run", "--help"]] {
        let output = run_from(harness, &help, Some(&directory));
        assert!(output.status.success(), "{help:?}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("parity-reference"), "{help:?}: {stdout}");
    }
    std::fs::remove_dir_all(&directory).expect("remove non-repository working directory");
}

/// `selftest` runs exactly one named tool self-test. A missing, extra, or
/// unknown name is refused before any tool runs, so a typo in a DAG command
/// cannot turn into a vacuous pass.
#[test]
fn test_harness_selftest_refuses_anything_but_one_known_name() {
    const NAMES: &str =
        "scorecard, scorecard_commands, pressure_test, validate_rs, manifest_cli, dbt_budget";
    let harness = env!("CARGO_BIN_EXE_test-harness");
    let directory = non_repository_dir("selftest-refusals");
    for (arguments, refusal) in [
        (
            vec!["selftest"],
            format!("test-harness: selftest takes exactly one name, one of: {NAMES}\n"),
        ),
        (
            vec!["selftest", "scorecard", "manifest_cli"],
            format!("test-harness: selftest takes exactly one name, one of: {NAMES}\n"),
        ),
        (
            vec!["selftest", "scorecards"],
            format!("test-harness: unknown self-test scorecards; expected one of: {NAMES}\n"),
        ),
    ] {
        let output = run_from(harness, &arguments, Some(&directory));
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            refusal,
            "{arguments:?}"
        );
    }
    std::fs::remove_dir_all(&directory).expect("remove non-repository working directory");
}

/// selftest.scorecard runs the `hermit-manifest-plan` binary built beside
/// test-harness instead of building one with Cargo inside its CPU cap. When
/// that binary is missing, or is not a file, the self-test is refused before
/// the scorecard starts, rather than falling back to a Cargo build.
#[test]
fn test_harness_selftest_scorecard_refuses_a_missing_helper() {
    const REFUSAL: &str = "test-harness: self-test scorecard needs the hermit-manifest-plan \
         binary built beside test-harness; build both with \
         `cargo build -p hermit-manifest-plan --bins`\n";
    let directory = non_repository_dir("selftest-missing-helper");
    // A copy, not a symlink: the helper is looked up beside the resolved
    // executable, and a symlink resolves to the build directory.
    let harness = directory.join("test-harness");
    let built = env!("CARGO_BIN_EXE_test-harness");
    std::fs::hard_link(built, &harness)
        .or_else(|_| std::fs::copy(built, &harness).map(|_| ()))
        .expect("place test-harness without its helper");
    let harness = harness.to_str().expect("UTF-8 temporary path");
    for helper_is_a_directory in [false, true] {
        if helper_is_a_directory {
            std::fs::create_dir(directory.join("hermit-manifest-plan"))
                .expect("create a directory in the helper's place");
        }
        let output = run_from(harness, &["selftest", "scorecard"], Some(&directory));
        assert_eq!(
            output.status.code(),
            Some(2),
            "helper is a directory: {helper_is_a_directory}: {output:?}"
        );
        assert!(output.stdout.is_empty(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stderr), REFUSAL);
    }
    std::fs::remove_dir_all(&directory).expect("remove non-repository working directory");
}

/// `--probe-disabled` runs ONE disabled cell, so a broad selection with it is
/// refused before anything runs. A mixed-selection harness test used to cover
/// the broad case; it was removed with `--parity-reference` in
/// https://github.com/rrnewton/hermit/issues/3301, and this restores it at the
/// command line with the exact message. The last two cases pass the
/// exact-filter check and are refused by the next one, so the filters are what
/// the first refusal keys on.
#[test]
fn test_harness_refuses_a_broad_probe_disabled_selection() {
    let harness = env!("CARGO_BIN_EXE_test-harness");
    let directory = non_repository_dir("broad-probe-disabled");
    let results = directory.join("results.jsonl");
    let results_arg = results.to_string_lossy().into_owned();
    const EXACT: &str =
        "test-harness: --probe-disabled requires exact --test, --mode, and --backend filters\n";
    for (arguments, expected) in [
        (vec!["run", "--probe-disabled"], EXACT),
        (
            vec![
                "run",
                "--results",
                results_arg.as_str(),
                "--category",
                "c-programs",
                "--probe-disabled",
            ],
            EXACT,
        ),
        (
            vec![
                "run",
                "--probe-disabled",
                "--mode",
                "verify",
                "--backend",
                "kvm",
            ],
            EXACT,
        ),
        (
            vec![
                "run",
                "--probe-disabled",
                "--test",
                "fixture/probe",
                "--backend",
                "kvm",
            ],
            EXACT,
        ),
        (
            vec![
                "run",
                "--probe-disabled",
                "--test",
                "fixture/probe",
                "--mode",
                "verify",
            ],
            EXACT,
        ),
        (
            vec![
                "plan",
                "--probe-disabled",
                "--test",
                "fixture/probe",
                "--mode",
                "verify",
                "--backend",
                "kvm",
            ],
            "test-harness: --probe-disabled is accepted by run only\n",
        ),
        (
            vec![
                "run",
                "--results",
                results_arg.as_str(),
                "--probe-disabled",
                "--test",
                "fixture/probe",
                "--mode",
                "verify",
                "--backend",
                "kvm",
                "--ci-only",
            ],
            "test-harness: --probe-disabled is mutually exclusive with --include-manual and \
             --ci-only\n",
        ),
    ] {
        let output = run_from(harness, &arguments, Some(&directory));
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            expected,
            "{arguments:?}"
        );
        assert!(!results.exists(), "{arguments:?} created {results:?}");
    }
    std::fs::remove_dir_all(&directory).expect("remove non-repository working directory");
}

/// `--check --write` used to write the snapshot while `--write --check` was
/// refused. Both orders, and a repeated mode flag, are now refused before the
/// repository is located, so nothing is read or written. The run happens at
/// the repository root, where a lone `--check` succeeds, so the refusals are
/// not an artifact of the working directory.
#[test]
fn generate_parity_cells_refuses_conflicting_or_repeated_modes() {
    let binary = env!("CARGO_BIN_EXE_generate-parity-cells");
    // The refusal repeats the usage text exactly as `--help` prints it.
    let help = run(binary, &["--help"]);
    assert!(help.status.success(), "{help:?}");
    let usage = String::from_utf8(help.stdout).expect("usage is UTF-8");
    assert!(
        usage.starts_with("Usage: generate-parity-cells [--check | --write]\n"),
        "{usage}"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root");
    let snapshot = root.join("ci/compat-envelope/parity-cells.json");
    let before = std::fs::read(&snapshot).expect("read the committed snapshot");
    let modified_before = std::fs::metadata(&snapshot)
        .and_then(|metadata| metadata.modified())
        .expect("snapshot mtime");
    let refusals: [(&[&str], &str); 6] = [
        (
            &["--check", "--write"],
            "--check and --write are mutually exclusive",
        ),
        (
            &["--write", "--check"],
            "--check and --write are mutually exclusive",
        ),
        (&["--write", "--write"], "--write may be given only once"),
        (&["--check", "--check"], "--check may be given only once"),
        (&["--bogus"], "unrecognized argument \"--bogus\""),
        (&["--write", "--bogus"], "unrecognized argument \"--bogus\""),
    ];
    for (arguments, message) in refusals {
        let output = run_from(binary, arguments, Some(&root));
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("generate-parity-cells: {message}\n{usage}"),
            "{arguments:?}"
        );
    }
    assert_eq!(
        std::fs::read(&snapshot).expect("reread the committed snapshot"),
        before
    );
    assert_eq!(
        std::fs::metadata(&snapshot)
            .and_then(|metadata| metadata.modified())
            .expect("snapshot mtime"),
        modified_before,
        "a refused invocation rewrote {snapshot:?}"
    );

    let check = run_from(binary, &["--check"], Some(&root));
    assert!(check.status.success(), "{check:?}");
    assert!(
        String::from_utf8_lossy(&check.stdout)
            .starts_with("ci/compat-envelope/parity-cells.json is canonical and fresh\n"),
        "{check:?}"
    );
}

#[test]
fn manifest_plan_no_arguments_remains_the_default_text_plan() {
    let output = run(env!("CARGO_BIN_EXE_hermit-manifest-plan"), &[]);
    assert!(
        output.status.success(),
        "default plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "default text plan was empty");
    assert!(!stdout.starts_with("Usage:"), "no arguments became help");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manifest(s)"),
        "default plan omitted its validation summary: {output:?}"
    );
}
