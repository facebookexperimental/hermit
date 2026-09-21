use std::path::Path;
use std::process::Command;
use std::process::Output;

fn run(binary: &str, arguments: &[&str]) -> Output {
    run_from(binary, arguments, None)
}

fn run_from(binary: &str, arguments: &[&str], current_dir: Option<&Path>) -> Output {
    let mut command = Command::new(binary);
    command.args(arguments);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    command.output().expect("failed to execute manifest CLI")
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
    let git_probe = Command::new("git")
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
    let output = Command::new(env!("CARGO_BIN_EXE_test-harness"))
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
