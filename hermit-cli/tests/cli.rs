/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
// The one definition of hermit's own failure status, imported rather than
// written out. Copying the number here is what let eight tests keep asserting
// `1` for months after the product moved to `125`.
use hermit::GUEST_PROGRAM_NOT_EXECUTABLE_EXIT;
use hermit::GUEST_PROGRAM_NOT_FOUND_EXIT;
use hermit::HERMIT_INTERNAL_FAILURE_EXIT;
use hermit::canonical_verdict::InfrastructureError;
use hermit::canonical_verdict::Verdict;
use hermit::canonical_verdict::VerificationReport;

static DBT_MMAP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_EXEC_FAILURE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_EXECVEAT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_PID_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_PRLIMIT_SELF_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_WAIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static KVM_EXACT_CHILD_WAITS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_UNSUPPORTED_SYSCALL_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_SELF_SIGQUEUE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_STDERR_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_LOG_ENV_GUEST: OnceLock<PathBuf> = OnceLock::new();
static LITEINST_INERT_RUNTIME: OnceLock<PathBuf> = OnceLock::new();
static EXEC_CLOCK_CONTINUITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_LSEEK_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_INODE_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FORK_CHILD_GETRANDOM_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";

const DBT_IO_BUFFER_MUTATOR_SOURCE: &str = r#"
#define _XOPEN_SOURCE 700
#include <fcntl.h>
#include <unistd.h>

int main(int argc, char **argv) {
  static const char replacement[16] = {
      66, 66, 66, 66, 66, 66, 66, 66,
      66, 66, 66, 66, 66, 66, 66, 66,
  };
  char observed[16];
  if (argc != 2) return 64;
  int fd = open(argv[1], O_RDWR | O_CLOEXEC);
  if (fd < 0) return 65;
  if (pread(fd, observed, sizeof(observed), 0) != (ssize_t)sizeof(observed)) return 66;
  if (pwrite(fd, replacement, sizeof(replacement), 0) != (ssize_t)sizeof(replacement)) return 67;
  if (fsync(fd) != 0 || close(fd) != 0) return 68;
  return 0;
}
"#;

// This lock only serializes independent child processes; a failed assertion carries no
// protected state invariant and must not poison unrelated tests.
fn hermit_run_guard() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn execution_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
    match requested {
        None => Ok(Vec::new()),
        Some(value) if value == OsStr::new(HERMETIC_TEST_WORKDIR) => Ok(vec![
            "--mount=type=tmpfs,target=/test".into(),
            "--workdir=/test".into(),
        ]),
        Some(value) => Err(format!(
            "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {value:?}"
        )),
    }
}

fn append_hermit_args_with_execution_root(
    command: &mut Command,
    args: &[&str],
    requested_workdir: Option<&OsStr>,
) -> Result<(), String> {
    let options = command
        .get_args()
        .chain(args.iter().map(OsStr::new))
        .take_while(|arg| *arg != OsStr::new("--"))
        .collect::<Vec<_>>();
    let uses_outer_mount = options.contains(&OsStr::new("--no-namespace"))
        || options.contains(&OsStr::new("--backend=dbt"))
        || options
            .windows(2)
            .any(|pair| pair == [OsStr::new("--backend"), OsStr::new("dbt")]);
    append_hermit_args_with_execution_root_mode(command, args, requested_workdir, !uses_outer_mount)
}

fn append_hermit_args_with_execution_root_mode(
    command: &mut Command,
    args: &[&str],
    requested_workdir: Option<&OsStr>,
    private_mount: bool,
) -> Result<(), String> {
    let mut execution_root = execution_root_args(requested_workdir)?;
    if execution_root.is_empty() {
        command.args(args);
        return Ok(());
    }
    // These are Hermit options, so they must precede the delimiter rather than
    // becoming arguments to the guest program.
    let Some(guest_separator) = args.iter().position(|arg| *arg == "--") else {
        // Argument-parsing and non-guest subcommand tests have no guest whose
        // working directory can be changed. Preserve their argv so they keep
        // exercising Hermit's parser. The requested value was validated above,
        // so an unsupported marker still fails before Hermit can run.
        command.args(args);
        return Ok(());
    };
    // Several tests append the guest only after adding a Path argument to the
    // command. Classify the complete Hermit prefix, not just that final slice.
    let command_args = command
        .get_args()
        .chain(args[..guest_separator].iter().map(OsStr::new))
        .collect::<Vec<_>>();
    let starts_guest = command_args.contains(&OsStr::new("run"))
        || command_args
            .iter()
            .position(|arg| *arg == OsStr::new("record"))
            .is_some_and(|record| {
                !command_args[record + 1..].iter().any(|arg| {
                    matches!(
                        arg.to_str(),
                        Some("list" | "ls" | "rm" | "remove" | "clean")
                    )
                })
            });
    if !starts_guest {
        command.args(args);
        return Ok(());
    }
    let has_explicit_base_env = command_args.iter().any(|arg| {
        *arg == OsStr::new("--base-env")
            || arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--base-env="))
    });
    if !has_explicit_base_env {
        execution_root.insert(0, "--base-env=minimal".into());
    }
    if !private_mount || command_args.contains(&OsStr::new("--mount=type=tmpfs,target=/test")) {
        execution_root.retain(|arg| arg != OsStr::new("--mount=type=tmpfs,target=/test"));
    }
    if command_args.contains(&OsStr::new("--workdir=/test"))
        || command_args
            .windows(2)
            .any(|pair| pair == [OsStr::new("--workdir"), OsStr::new("/test")])
    {
        execution_root.retain(|arg| arg != OsStr::new("--workdir=/test"));
    }
    command
        .args(&args[..guest_separator])
        .args(execution_root)
        .args(&args[guest_separator..]);
    Ok(())
}

fn append_hermit_args(command: &mut Command, args: &[&str]) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    append_hermit_args_with_execution_root(command, args, requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
}

fn append_hermit_args_using_outer_mount(command: &mut Command, args: &[&str]) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    append_hermit_args_with_execution_root_mode(command, args, requested.as_deref(), false)
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
}

fn hermit_command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, args);
    command
}

fn hermit(args: &[&str]) -> Output {
    hermit_command(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn hermit_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, args);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
    child
        .stdin
        .take()
        .expect("hermit stdin should be piped")
        .write_all(input)
        .expect("failed to write hermit stdin");
    child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to wait for hermit with {args:?}: {error}"))
}

fn dbt_stderr_guest() -> &'static Path {
    DBT_STDERR_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-stderr-nostdlib");
        fs::create_dir_all(&build_root).expect("failed to create DBT stderr guest directory");
        let guest = build_root.join("stderr_nostdlib");
        let output = Command::new("cc")
            .args([
                "-nostdlib",
                "-static",
                "-fno-pie",
                "-no-pie",
                "-Wall",
                "-Wextra",
                "-Werror",
            ])
            .arg(repository.join("hermit-cli/tests/fixtures/dbt/stderr_nostdlib.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT stderr guest");
        assert!(
            output.status.success(),
            "DBT stderr guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_log_env_guest() -> &'static Path {
    DBT_LOG_ENV_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-hermit-log-env");
        fs::create_dir_all(&build_root).expect("failed to create DBT log-env guest directory");
        let guest = build_root.join("hermit_log_env");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("hermit-cli/tests/fixtures/dbt/hermit_log_env.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT log-env guest");
        assert!(
            output.status.success(),
            "DBT log-env guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn read_terminal_dbt_verdict(path: &Path) -> serde_json::Value {
    let verdict: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("failed to read DBT verification verdict"))
            .expect("DBT verification verdict should be JSON");
    let matched = verdict["verdict"] == "matched";
    assert!(
        matched || verdict["verdict"] == "diverged",
        "verification did not reach a terminal comparison: {verdict}"
    );
    assert_eq!(
        verdict["verified"], matched,
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["bitwise_parity"], matched,
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["comparison"]["strictness"], "canonical",
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["comparison"]["log_scope"], "info",
        "unexpected verdict: {verdict}"
    );
    // Authenticated initialization records are checked separately by count;
    // every remaining comparable record uses the named DBT transport envelope.
    assert_eq!(
        verdict["comparison"]["record_envelope"], "dbt_evidence_transport_v1",
        "unexpected verdict: {verdict}"
    );
    assert_eq!(
        verdict["guest_exit_code"], 0,
        "guest rejected its environment: {verdict}"
    );
    for side in ["left", "right"] {
        assert!(
            verdict["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "empty {side} INFO population: {verdict}"
        );
    }
    verdict
}

fn write_matching_liteinst_revision(runtime: &Path) {
    let revision = PathBuf::from(format!("{}.revision", runtime.display()));
    fs::write(revision, format!("{}\n", env!("HERMIT_REVERIE_PIN")))
        .expect("failed to write LiteInst runtime revision fixture");
}

#[test]
fn liteinst_runtime_cache_requires_the_current_revision() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create LiteInst cache fixture directory");
    let runtime = directory.path().join("libreverie_liteinst.so");
    assert!(!liteinst_runtime::staged_runtime_matches_current_pin(
        &runtime
    ));
    fs::write(&runtime, b"fixture").expect("failed to write LiteInst cache fixture");
    assert!(!liteinst_runtime::staged_runtime_matches_current_pin(
        &runtime
    ));
    fs::write(
        format!("{}.revision", runtime.display()),
        "stale-revision\n",
    )
    .expect("failed to write stale LiteInst revision fixture");
    assert!(!liteinst_runtime::staged_runtime_matches_current_pin(
        &runtime
    ));
    write_matching_liteinst_revision(&runtime);
    assert!(liteinst_runtime::staged_runtime_matches_current_pin(
        &runtime
    ));
}

fn liteinst_inert_runtime() -> &'static Path {
    LITEINST_INERT_RUNTIME.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("liteinst-inert-runtime");
        fs::create_dir_all(&build_root).expect("failed to create inert runtime directory");
        let runtime = build_root.join("libreverie_liteinst_inert.so");
        let output = Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/liteinst_inert_runtime.c"))
            .arg("-o")
            .arg(&runtime)
            .output()
            .expect("failed to compile inert LiteInst runtime fixture");
        assert!(
            output.status.success(),
            "inert LiteInst fixture compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        write_matching_liteinst_revision(&runtime);
        runtime
    })
}

fn exec_clock_continuity_guest() -> &'static Path {
    EXEC_CLOCK_CONTINUITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("exec-clock-continuity");
        fs::create_dir_all(&build_root)
            .expect("failed to create exec-clock-continuity guest directory");
        let guest = build_root.join("exec_clock_continuity");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/exec_clock_continuity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile exec-clock-continuity guest");
        assert!(
            output.status.success(),
            "exec-clock-continuity guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn stdio_lseek_identity_guest() -> &'static Path {
    STDIO_LSEEK_IDENTITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stdio-lseek-identity");
        fs::create_dir_all(&build_root).expect("failed to create stdio-lseek build directory");
        let guest = build_root.join("stdio_lseek_identity");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/stdio_lseek_identity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile stdio-lseek fixture");
        assert!(
            output.status.success(),
            "stdio-lseek fixture compilation failed:
stdout:
{}
stderr:
{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn stdio_inode_identity_guest() -> &'static Path {
    STDIO_INODE_IDENTITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("stdio-inode-identity");
        fs::create_dir_all(&build_root).expect("failed to create stdio-inode build directory");
        let guest = build_root.join("stdio_inode_identity");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/fixtures/stdio_inode_identity.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile stdio-inode fixture");
        assert!(
            output.status.success(),
            "stdio-inode fixture compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-1052): Review no-namespace fork-child RNG coverage.
fn fork_child_getrandom_guest() -> &'static Path {
    FORK_CHILD_GETRANDOM_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fork-child-getrandom");
        fs::create_dir_all(&build_root)
            .expect("failed to create fork-child-getrandom guest directory");
        let guest = build_root.join("fork_child_getrandom");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/fork_child_getrandom.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile fork-child-getrandom guest");
        assert!(
            output.status.success(),
            "fork-child-getrandom guest compilation failed:
stdout:
{}
stderr:
{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_mmap_guest() -> &'static Path {
    DBT_MMAP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-mmap");
        fs::create_dir_all(&build_root).expect("failed to create DBT mmap guest directory");
        let guest = build_root.join("dbt_mmap_exec");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_mmap_exec.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT mmap guest");
        assert!(
            output.status.success(),
            "DBT mmap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_exec_failure_guest() -> &'static Path {
    DBT_EXEC_FAILURE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-exec-failure");
        fs::create_dir_all(&build_root).expect("failed to create DBT exec-failure guest directory");
        let guest = build_root.join("dbt_exec_failure");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_exec_failure.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT exec-failure guest");
        assert!(
            output.status.success(),
            "DBT exec-failure guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_execveat_guest() -> &'static Path {
    DBT_EXECVEAT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-execveat");
        fs::create_dir_all(&build_root).expect("failed to create DBT execveat guest directory");
        let guest = build_root.join("dbt_execveat_unsupported");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_execveat_unsupported.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT execveat guest");
        assert!(
            output.status.success(),
            "DBT execveat guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_wait_guest() -> &'static Path {
    DBT_WAIT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-wait");
        fs::create_dir_all(&build_root).expect("failed to create DBT wait guest directory");
        let guest = build_root.join("dbt_wait_lifecycle");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_wait_lifecycle.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT wait guest");
        assert!(
            output.status.success(),
            "DBT wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn kvm_exact_child_waits_guest() -> &'static Path {
    KVM_EXACT_CHILD_WAITS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-exact-child-waits");
        fs::create_dir_all(&build_root)
            .expect("failed to create KVM exact-child wait guest directory");
        let guest = build_root.join("kvm_exact_child_waits");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/kvm_exact_child_waits.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile KVM exact-child wait guest");
        assert!(
            output.status.success(),
            "KVM exact-child wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-723): Review the DBT PID fixture build.
fn dbt_pid_guest() -> &'static Path {
    DBT_PID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-pid");
        fs::create_dir_all(&build_root).expect("failed to create DBT PID guest directory");
        let guest = build_root.join("dbt_pid_virtualization");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_pid_virtualization.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT PID guest");
        assert!(
            output.status.success(),
            "DBT PID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1065): Review DBT self-prlimit fixture coverage.
fn dbt_prlimit_self_guest() -> &'static Path {
    DBT_PRLIMIT_SELF_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-prlimit-self");
        fs::create_dir_all(&build_root).expect("failed to create DBT self-prlimit guest directory");
        let guest = build_root.join("dbt_prlimit_self");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_prlimit_self.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT self-prlimit guest");
        assert!(
            output.status.success(),
            "DBT self-prlimit guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review the DBT unsupported-syscall fixture build.
fn dbt_unsupported_syscall_guest() -> &'static Path {
    DBT_UNSUPPORTED_SYSCALL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-unsupported-syscall");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBT unsupported-syscall guest directory");
        let guest = build_root.join("dbt_unsupported_syscall");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_unsupported_syscall.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT unsupported-syscall guest");
        assert!(
            output.status.success(),
            "DBT unsupported-syscall guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

// TODO-HUMAN-REVIEW(PR-1038): Review the DBT self-signal fixture build.
fn dbt_self_sigqueue_guest() -> &'static Path {
    DBT_SELF_SIGQUEUE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("dbt-self-sigqueue");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBT self-sigqueue guest directory");
        let guest = build_root.join("dbt_self_sigqueue");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/dbt_self_sigqueue.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT self-sigqueue guest");
        assert!(
            output.status.success(),
            "DBT self-sigqueue guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hermit_with_closed_stdin(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // SAFETY: pre_exec closes only the child descriptor immediately before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::close(libc::STDIN_FILENO) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn assert_success(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "hermit {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("hermit stdout should be UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("hermit stderr should be UTF-8")
}

#[test]
fn kvm_pinned_root_arguments_are_exact_and_fail_closed() {
    assert!(execution_root_args(None).unwrap().is_empty());
    assert_eq!(
        execution_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );
    let error = execution_root_args(Some(OsStr::new("/tmp"))).unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));

    let args = ["run", "--backend", "kvm", "--", "/bin/true"];
    let mut host_command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args_with_execution_root(&mut host_command, &args, None)
        .expect("an unset workdir request should preserve the host command");
    assert_eq!(
        host_command.get_args().collect::<Vec<_>>(),
        args.map(OsStr::new)
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args_with_execution_root(&mut command, &args, Some(OsStr::new("/test")))
        .expect("the documented workdir request should build a command");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("run"),
            OsStr::new("--backend"),
            OsStr::new("kvm"),
            OsStr::new("--base-env=minimal"),
            OsStr::new("--mount=type=tmpfs,target=/test"),
            OsStr::new("--workdir=/test"),
            OsStr::new("--"),
            OsStr::new("/bin/true"),
        ]
    );

    let mut refused = Command::new(env!("CARGO_BIN_EXE_hermit"));
    let error =
        append_hermit_args_with_execution_root(&mut refused, &args, Some(OsStr::new("/tmp")))
            .unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
    assert!(refused.get_args().next().is_none());

    for args in [
        ["run", "--backend=dbt", "--", "/bin/true"],
        ["run", "--no-namespace", "--", "/bin/true"],
    ] {
        let mut outer_mount_command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        append_hermit_args_with_execution_root(
            &mut outer_mount_command,
            &args,
            Some(OsStr::new("/test")),
        )
        .expect("DBT and --no-namespace should use the outer /test mount");
        assert_eq!(
            outer_mount_command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("run"),
                OsStr::new(args[1]),
                OsStr::new("--base-env=minimal"),
                OsStr::new("--workdir=/test"),
                OsStr::new("--"),
                OsStr::new("/bin/true"),
            ]
        );
    }

    let explicit_base_env_args = [
        "run",
        "--backend=kvm",
        "--base-env=empty",
        "--",
        "/usr/bin/env",
    ];
    let mut explicit_base_env = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args_with_execution_root(
        &mut explicit_base_env,
        &explicit_base_env_args,
        Some(OsStr::new("/test")),
    )
    .expect("an explicit base environment is the test subject and must survive");
    assert_eq!(
        explicit_base_env.get_args().collect::<Vec<_>>(),
        [
            OsStr::new("run"),
            OsStr::new("--backend=kvm"),
            OsStr::new("--base-env=empty"),
            OsStr::new("--mount=type=tmpfs,target=/test"),
            OsStr::new("--workdir=/test"),
            OsStr::new("--"),
            OsStr::new("/usr/bin/env"),
        ]
    );

    let mut parser_only = Command::new(env!("CARGO_BIN_EXE_hermit"));
    let parser_args = ["run", "--namespace-only", "--chaos", "/bin/true"];
    append_hermit_args_with_execution_root(
        &mut parser_only,
        &parser_args,
        Some(OsStr::new("/test")),
    )
    .expect("a command with no guest separator should retain its parser input");
    assert_eq!(
        parser_only.get_args().collect::<Vec<_>>(),
        parser_args.map(OsStr::new)
    );

    let strace_args = [
        "--backend",
        "sabre",
        "--log",
        "info",
        "strace",
        "--",
        "/bin/true",
    ];
    let mut strace = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args_with_execution_root(&mut strace, &strace_args, Some(OsStr::new("/test")))
        .expect("strace does not accept the run execution-root options");
    assert_eq!(
        strace.get_args().collect::<Vec<_>>(),
        strace_args.map(OsStr::new)
    );

    for args in [
        ["record", "--verify", "--", "/bin/true"],
        ["record", "start", "--", "/bin/true"],
    ] {
        let mut record = Command::new(env!("CARGO_BIN_EXE_hermit"));
        append_hermit_args_with_execution_root(&mut record, &args, Some(OsStr::new("/test")))
            .expect("a recording launch supports the execution-root options");
        assert!(
            record
                .get_args()
                .any(|arg| arg == OsStr::new("--workdir=/test"))
        );
    }
}

#[test]
fn pinned_root_arguments_cover_split_guest_commands() {
    // These are the actual construction shapes used by the capture-name,
    // io-buffer divergence, and skid-refusal tests: a path is appended before
    // the guest separator. No Hermit process is launched by this control.
    let cases: &[(&[&str], &[&str], bool)] = &[
        (
            &[
                "run",
                "--backend",
                "dbt",
                "--verify",
                "--keep-logs",
                "--verify-log-dir",
                "/fixture/verify-logs",
            ],
            &["--", "/bin/echo", "dbt-verify-log-naming"],
            false,
        ),
        (
            &[
                "--log",
                "info",
                "run",
                "--backend",
                "dbt",
                "--strict",
                "--verify",
                "--keep-logs",
                "--verify-log-dir",
                "/fixture/verify-logs",
            ],
            &["--"],
            false,
        ),
        (
            &[
                "run",
                "--strict",
                "--verify",
                "--verify-strict",
                "--verify-json",
                "/fixture/verification.json",
            ],
            &["--", "/bin/sh", "-c", "exit 37"],
            true,
        ),
    ];
    for &(prefix, suffix, private_mount) in cases {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args(prefix);
        append_hermit_args_with_execution_root(&mut command, suffix, Some(OsStr::new("/test")))
            .unwrap();
        let mut expected = prefix.to_vec();
        expected.push("--base-env=minimal");
        if private_mount {
            expected.push("--mount=type=tmpfs,target=/test");
        }
        expected.push("--workdir=/test");
        expected.extend_from_slice(suffix);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            expected.iter().map(OsStr::new).collect::<Vec<_>>(),
            "split command {prefix:?} + {suffix:?}"
        );
    }

    // Main's namespace-only isolation test already requests this mount and
    // workdir. Keep its exact single options and all original assertions.
    let args = [
        "run",
        "--namespace-only",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        "/bin/true",
    ];
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args_with_execution_root(&mut command, &args, Some(OsStr::new("/test"))).unwrap();
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [
            "run",
            "--namespace-only",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--base-env=minimal",
            "--",
            "/bin/true",
        ]
        .map(OsStr::new)
    );
}

fn strip_ansi_sgr(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find("\x1b[") {
        output.push_str(&remaining[..start]);
        let escape = &remaining[start + 2..];
        let Some(end) = escape.find('m') else {
            output.push_str(&remaining[start..]);
            return output;
        };
        remaining = &escape[end + 1..];
    }
    output.push_str(remaining);
    output
}

/// WHICH failure a call site means to assert. Named at the call site, so the
/// assertion states an intention instead of inheriting one.
///
/// ⚠️ ONE HELPER CANNOT PIN ONE CODE HERE, AND ASSUMING IT COULD IS THE ORIGINAL
/// DEFECT. `assert_failure_contains` hardcoded `Some(1)` for every caller. When
/// hermit#2558 introduced 125 for hermit's own failures, and the guest-program
/// faults later took 127/126, the sixteen call sites stopped meaning one thing —
/// but they still shared one assertion, so a fix that simply swapped in the
/// newest observed number would have been wrong for five of them and would have
/// gone green anyway.
#[derive(Clone, Copy)]
enum Refusal {
    /// Hermit itself refused and NO GUEST WAS LAUNCHED: a contradictory flag
    /// pair, an unwritable log path, a denied capability, a backend used outside
    /// its supported command. The caller's tooling or invocation is at fault.
    Hermit,
    /// The named guest program does not exist. The caller's COMMAND LINE is at
    /// fault, not hermit — a distinction `Hermit` cannot express.
    GuestNotFound,
    /// The guest program exists but cannot be executed as given: a directory, a
    /// non-executable mode, a missing shebang interpreter target.
    GuestNotExecutable,
}

impl Refusal {
    fn code(self) -> i32 {
        match self {
            Refusal::Hermit => HERMIT_INTERNAL_FAILURE_EXIT,
            Refusal::GuestNotFound => GUEST_PROGRAM_NOT_FOUND_EXIT,
            Refusal::GuestNotExecutable => GUEST_PROGRAM_NOT_EXECUTABLE_EXIT,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Refusal::Hermit => "a hermit-internal refusal (no guest launched)",
            Refusal::GuestNotFound => "a guest-program-not-found refusal",
            Refusal::GuestNotExecutable => "a guest-program-not-executable refusal",
        }
    }
}

/// Assert that hermit refused for the stated REASON, said why, and did not panic.
///
/// ⚠️ THE EXIT CODE HERE IS A CLAIM, NOT A FORMALITY, WHICH IS WHY THE CALLER
/// NAMES IT. The three codes answer different questions — is my tooling broken
/// (125), is my program path wrong (127), is it unrunnable (126) — and they are
/// one scheme, not three unrelated numbers: GNU `env`/`chroot`/`timeout` reserve
/// exactly this split, which is where hermit's 125 came from.
///
/// ⚠️ DO NOT REPLACE A `Refusal` WITH WHATEVER CODE THE TEST HAPPENS TO EMIT.
/// This assertion was `Some(1)` until hermit#2558 and was passing for the wrong
/// reason: `1` is also the commonest guest exit, so it accepted "hermit refused"
/// and "the guest ran and failed" alike. Substituting today's observed value
/// without deciding what the test MEANS reintroduces that, one number later.
fn assert_hermit_refusal_contains(output: &Output, refusal: Refusal, expected: &[&str]) {
    // ⚠️ FAILURE FIRST, AND SEPARATELY FROM WHICH FAILURE. The equality below is
    // only as good as the constant it reads: if a `Refusal` code ever became 0,
    // `assert_eq!(code, Some(0))` would stop demanding a failure and start
    // demanding a SUCCESS, and all sixteen call sites would invert and still
    // pass. This line cannot be satisfied by any success, whatever the constants
    // say, so the two assertions fail independently rather than together.
    assert!(
        !output.status.success(),
        "expected {} but the command SUCCEEDED: {output:?}",
        refusal.describe()
    );
    assert_eq!(
        output.status.code(),
        Some(refusal.code()),
        "expected {}, got: {output:?}",
        refusal.describe()
    );
    let stderr = stderr(output);
    for message in expected {
        assert!(
            stderr.contains(message),
            "missing {message:?} in:\n{stderr}"
        );
    }
    assert!(!stderr.contains("panicked"), "unexpected panic:\n{stderr}");
}

fn deny_syscall(command: &mut Command, syscall: libc::c_long) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec. The filter is an
    // allow-all policy except for the single syscall used by each capability-probe test.
    unsafe {
        command.pre_exec(move || {
            let mut filter = [
                libc::sock_filter {
                    code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                    jt: 0,
                    jf: 0,
                    k: 0, // offsetof(seccomp_data, nr)
                },
                libc::sock_filter {
                    code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                    jt: 0,
                    jf: 1,
                    k: syscall as u32,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | libc::EPERM as u32, // SECCOMP_RET_ERRNO
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff_0000, // SECCOMP_RET_ALLOW
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            ) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[test]
fn run_strict_flag_is_accepted_and_runs() {
    // Regression test for GH #12: `docs/Users.md` documents
    // `hermit run --strict ...`, and the CLI must accept that spelling and run
    // the guest to completion. Strict determinism is the default, so `--strict`
    // is a compatibility no-op over the defaults. `--max-timeslice=disabled`
    // and `--no-virtualize-cpuid` keep this runnable on hosts without accessible
    // PMU counters or CPUID faulting; neither weakens what `--strict` controls.
    let args = [
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn verify_verbose_requires_verify() {
    let args = ["run", "--verify-verbose", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("--verify-verbose"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--verify"), "unexpected error:\n{stderr}");
    assert!(stderr.contains("required"), "unexpected error:\n{stderr}");
}

#[test]
fn run_rejects_unknown_backends_during_argument_parsing() {
    let args = ["run", "--backend", "unknown", "--", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
    for backend in ["ptrace", "dbt", "kvm"] {
        assert!(
            stderr.contains(backend),
            "missing {backend:?} in:\n{stderr}"
        );
    }
}

/// True when this binary cannot exercise the DBT backend, having said so.
///
/// ⚠️ KEYED ON THE COMPILE-TIME FEATURE, NEVER ON THE RUN'S OUTCOME. Skipping
/// because a `--backend dbt` invocation failed would be the opposite defect and
/// strictly worse than the reds it removes: it would convert every genuine DBT
/// regression into silence. `cfg!(feature = "dbt")` is a fact about how this
/// test binary was compiled, decided before any guest runs, so a broken but
/// PRESENT backend still fails exactly as it did before.
///
/// `default = []` in hermit-cli/Cargo.toml, so a plain `cargo test` excludes
/// DBT and these 18 tests failed on EVERY default build -- not merely on hosts
/// lacking DynamoRIO. Validate is unaffected: it builds
/// `--features third-party-backends`, which includes `dbt`.
///
/// Setting `HERMIT_REQUIRE_DBT` turns the skip back into a failure, so a CI job
/// that intends to cover DBT cannot silently stop covering it. That mirrors
/// `sabre_examples.rs`, which panics when an explicitly configured artifact is
/// missing and only skips when the default path is absent.
fn dbt_unavailable(test: &str) -> bool {
    if cfg!(feature = "dbt") {
        return false;
    }
    assert!(
        std::env::var_os("HERMIT_REQUIRE_DBT").is_none(),
        "HERMIT_REQUIRE_DBT is set, but this test binary was built WITHOUT the \
         `dbt` feature, so {test} cannot exercise the backend it claims to cover. \
         Rebuild with --features dbt (or third-party-backends), or unset \
         HERMIT_REQUIRE_DBT to allow skipping."
    );
    eprintln!(
        "skipping {test}: built without the `dbt` feature (hermit-cli default = []); \
         an absent backend is not a product failure. Build with --features dbt or \
         --features third-party-backends to exercise it."
    );
    true
}

#[test]
fn run_dbt_executes_integrated_backend() {
    if dbt_unavailable("run_dbt_executes_integrated_backend") {
        return;
    }
    let args = ["run", "--backend", "dbt", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn run_dbt_uses_the_requested_guest_environment() {
    if dbt_unavailable("run_dbt_uses_the_requested_guest_environment") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--base-env=empty",
        "--env=DBT_GUEST_ONLY=present",
        "--",
        "/usr/bin/env",
    ];
    let output = hermit_command(&args)
        .env("DBT_HOST_ONLY", "must-not-leak")
        .output()
        .expect("failed to run DBT environment regression");

    assert_success(&output, &args);
    let stdout = stdout(&output);
    assert!(
        stdout.lines().any(|line| line == "DBT_GUEST_ONLY=present"),
        "DBT guest environment omitted the requested value:\n{stdout}",
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.starts_with("DBT_HOST_ONLY=")),
        "DBT guest inherited a host-only value:\n{stdout}",
    );
}

// TODO(#2791): Remove the portable test.cli skip when DBT env-shebang defect #2805 is fixed.
#[test]
fn run_dbt_verifies_simple_env_shebang() {
    if dbt_unavailable("run_dbt_verifies_simple_env_shebang") {
        return;
    }
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBT env-shebang test directory");
    let script = directory.path().join("env-echo");
    fs::write(&script, b"#!/usr/bin/env echo\n")
        .expect("failed to write DBT env-shebang test script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
        .expect("failed to mark DBT env-shebang test script executable");
    let program = script
        .to_str()
        .expect("DBT env-shebang test path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];

    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), format!("{}\n", script.display()));
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
#[ignore = "requires the pinned-root isolation validation node and its /test marker"]
fn run_dbt_verifies_fresh_physical_workdirs() {
    if !cfg!(feature = "dbt") {
        panic!("the isolation control requires the DBT feature");
    }
    assert_eq!(std::env::var_os(ISOLATED_WORKDIR_ENV), Some("/test".into()));
    let parent_cwd = std::env::current_dir().unwrap();
    let parent_namespace = fs::read_link("/proc/self/ns/mnt").unwrap();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap()
        .keep();
    println!("DBT physical-workdir evidence: {}", directory.display());
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("test-resources/exclusive_workdir.c");
    let guest = directory.join("exclusive_workdir");
    let compile = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap();
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    assert!(!Path::new("/test/physical-run-exclusive").exists());
    let verify = |attempt: usize| {
        let logs = directory.join(format!("verify-{attempt}"));
        fs::create_dir(&logs).unwrap();
        let verdict = directory.join(format!("verify-{attempt}.json"));
        let mut command = hermit_command(&[
            "--log",
            "info",
            "run",
            "--backend",
            "dbt",
            "--strict",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--verify-json",
            verdict.to_str().unwrap(),
            "--",
            guest.to_str().unwrap(),
        ]);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "verify {attempt}: {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output), "empty per-run workdir verified\n");
        let report = read_terminal_dbt_verdict(&verdict);
        assert_eq!(report["verdict"], "matched", "{report}");
        assert_eq!(report["comparison"]["compare_io_buffers"], true, "{report}");
        assert_eq!(report["comparison"]["compare_logs"], true, "{report}");
        assert_eq!(
            report["compared_log_messages"]["left"], report["compared_log_messages"]["right"],
            "{report}"
        );
        for side in ["run1_log_", "run2_log_"] {
            let captures = fs::read_dir(&logs)
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(side))
                .collect::<Vec<_>>();
            assert_eq!(captures.len(), 1);
            assert!(captures[0].metadata().unwrap().len() > 0);
        }
    };
    // Each command performs the original strict two-run comparison. Reusing
    // either a physical-run directory or a sibling's directory makes O_EXCL
    // fail without changing the comparator or the guest workload.
    verify(0);
    verify(1);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| verify(2));
        let second = scope.spawn(|| verify(3));
        first.join().unwrap();
        second.join().unwrap();
    });
    assert_eq!(std::env::current_dir().unwrap(), parent_cwd);
    assert_eq!(
        fs::read_link("/proc/self/ns/mnt").unwrap(),
        parent_namespace
    );
    assert!(!Path::new("/test/physical-run-exclusive").exists());
}

/// DBT's retained verify captures must carry the names THE HARNESS SCANS FOR.
///
/// This is not a style assertion. `ci/compat-envelope/pressure-test.rs` and
/// `ci/manifest-plan/src/runner.rs` both locate the pair with
/// `name.starts_with("run1_log_")` / `("run2_log_")`, and a terminal verify
/// result whose directory does not yield exactly one of each is recorded
/// `infrastructure-error` regardless of the verdict it actually reached.
///
/// DBT used to pass "dbt-run1"/"dbt-run2" to `temp_log_files_in`, producing
/// `dbt-run1_log_*`, which does not start with `run1_log_`. Measured 2026-08-27
/// with the backend as the only variable: DBT wrote both captures, 47,507 bytes
/// each, and the harness predicate matched 0 of the 2 it requires; the same
/// command without `--backend dbt` matched 2 of 2. Every dbt verify cell was
/// filed as an infrastructure failure with its logs present but unnamed --
/// 174 of 174 records under that condition were dbt, and two of them were
/// concealing a real divergence.
///
/// Nothing else in the tree ever referenced the `dbt-` spelling, so it was
/// undefended: the mismatch could return by editing one string with no test
/// failing. This asserts BOTH directions -- the scanned names are present, and
/// the old spelling is absent -- so a revert is visible.
#[test]
fn dbt_verify_retains_captures_under_the_names_the_harness_scans_for() {
    if dbt_unavailable("dbt_verify_retains_captures_under_the_names_the_harness_scans_for") {
        return;
    }
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBT verify-log naming test directory");
    let log_dir = root.path().join("verify-logs");
    fs::create_dir(&log_dir).expect("failed to create DBT verification log directory");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--backend",
            "dbt",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
        ])
        .arg(&log_dir);
    append_hermit_args_using_outer_mount(
        &mut command,
        &["--", "/bin/echo", "dbt-verify-log-naming"],
    );
    let output = command.output().expect("failed to run DBT verification");

    // DELIBERATELY NOT asserting the verdict. The subject here is the NAME the
    // captures are retained under, and `--keep-logs` retains them whether the
    // two runs matched or not. An earlier version of this test also required
    // success and was flaky within three runs: DBT verification of /bin/echo
    // diverged on one of them ("Log differences found between run 1 and run 2"),
    // which failed the test for a reason that has nothing to do with the naming
    // it exists to pin. Coupling a property to an unrelated verdict is how a
    // test starts getting re-run until it passes.
    let stderr_text = strip_ansi_sgr(&stderr(&output));
    assert!(
        stderr_text.contains("Verification logs retained") || output.status.success(),
        "DBT verification neither succeeded nor reported retained logs, so this test cannot \
         observe the capture names at all:\n{stderr_text}"
    );

    let names = |prefix: &str| -> Vec<String> {
        fs::read_dir(&log_dir)
            .expect("failed to read the retained verify-log directory")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name.starts_with(prefix))
            .collect()
    };

    // The harness predicate, applied verbatim: exactly one of each, both nonempty.
    for prefix in ["run1_log_", "run2_log_"] {
        let matched = names(prefix);
        assert_eq!(
            matched.len(),
            1,
            "DBT verification must retain exactly one {prefix} capture for the harness to find; \
             directory held {:?}",
            fs::read_dir(&log_dir)
                .map(|entries| entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>())
                .unwrap_or_default()
        );
        let size = fs::metadata(log_dir.join(&matched[0]))
            .expect("failed to stat a retained capture")
            .len();
        assert!(size > 0, "retained capture {} is empty", matched[0]);
    }

    // And the spelling that hid them must not come back.
    for stale in ["dbt-run1_log_", "dbt-run2_log_"] {
        assert!(
            names(stale).is_empty(),
            "DBT retained a {stale} capture; the harness scans for run1_log_/run2_log_ and will \
             record this cell as an infrastructure failure even when it reached a verdict"
        );
    }
}

#[test]
fn dbt_verify_without_json_rejects_io_buffer_content_divergence() {
    if dbt_unavailable("dbt_verify_without_json_rejects_io_buffer_content_divergence") {
        return;
    }
    let _guard = hermit_run_guard();
    let build_root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBT io-buffer test directory");
    let source = build_root.path().join("io_buffer_mutator.c");
    let guest = build_root.path().join("io_buffer_mutator");
    fs::write(&source, DBT_IO_BUFFER_MUTATOR_SOURCE)
        .expect("failed to write DBT io-buffer mutator source");
    let compile = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile DBT io-buffer mutator");
    assert!(
        compile.status.success(),
        "failed to compile DBT io-buffer mutator:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let state = Path::new("/tmp").join(format!(
        "hermit-dbt-iobuf-verification-{}",
        std::process::id()
    ));
    fs::write(&state, b"AAAAAAAAAAAAAAAA").expect("failed to seed DBT io-buffer state");
    let log_dir = build_root.path().join("verify-logs");
    fs::create_dir(&log_dir).expect("failed to create DBT verification log directory");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--log",
            "info",
            "run",
            "--backend",
            "dbt",
            "--strict",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
        ])
        .arg(&log_dir);
    append_hermit_args_using_outer_mount(&mut command, &["--"]);
    let output = command
        .arg(&guest)
        .arg(&state)
        .output()
        .expect("failed to run DBT io-buffer verification");
    let _ = fs::remove_file(&state);
    let stderr = strip_ansi_sgr(&stderr(&output));

    assert!(
        !output.status.success(),
        "DBT verification without --verify-json accepted an A-to-B syscall output-buffer \
         mutation:\n{stderr}"
    );
    assert!(
        !stderr.contains("Success: deterministic. Determinism verified."),
        "DBT announced deterministic success after an output-buffer divergence:\n{stderr}"
    );
    assert!(
        stderr.contains("Failure: nondeterministic."),
        "DBT rejected the mutation for a reason other than the two-run comparison:\n{stderr}"
    );
    for marker in [
        "[iobuf]",
        "pread64",
        "= Ok(16)",
        "991204fba2b6216d476282d375ab88d20e6108d109aecded97ef424ddd114706",
        "900dfeb7f1b5e344209e2abce56c333dafe606fb3bf59f68ab2b0e2ef8a0662b",
    ] {
        assert!(
            stderr.contains(marker),
            "DBT's no-JSON verification failure did not name {marker:?}; it must fail on the \
             syscall output-buffer evidence itself:\n{stderr}"
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review ptrace verification warning delivery.
// After pidfd_send_signal/pidfd_getfd were determinized, restart_syscall is the
// lone remaining Unsupported syscall. The ptrace seccomp filter must honor the
// Detcore subscription so ordinary execution fails closed before the guest can
// publish its success marker. The explicit compatibility opt-out remains noisy
// and preserves the native -EINTR result.
#[test]
fn run_ptrace_fails_closed_by_default_on_unsupported_syscall() {
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("unsupported-syscall guest path should be UTF-8");

    let supported_args = ["run", "--", "/bin/echo", "ptrace-supported-ok"];
    let supported = hermit(&supported_args);
    assert_success(&supported, &supported_args);
    assert_eq!(stdout(&supported), "ptrace-supported-ok\n");

    let default_args = ["run", "--", program];
    let default = hermit(&default_args);
    assert!(
        !default.status.success(),
        "default ptrace unexpectedly allowed restart_syscall:\n{}",
        stderr(&default)
    );
    assert!(
        stderr(&default).contains("unsupported syscall: restart_syscall"),
        "default ptrace failure omitted restart_syscall:\n{}",
        stderr(&default)
    );
    assert_eq!(
        stdout(&default),
        "",
        "unsupported guest published its success marker"
    );

    let compatibility_args = [
        "run",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
    ];
    let compatibility = hermit(&compatibility_args);
    assert_success(&compatibility, &compatibility_args);
    assert_eq!(stdout(&compatibility), "dbt-unsupported-ok\n");
    let compatibility_stderr = stderr(&compatibility);
    let warning = "used but not yet supported";
    assert_eq!(
        compatibility_stderr.matches(warning).count(),
        1,
        "ptrace compatibility run omitted or duplicated the unsupported warning:\n\
         {compatibility_stderr}"
    );
    assert_eq!(
        compatibility_stderr
            .matches("a successful exit does not establish complete deterministic execution")
            .count(),
        1,
        "ptrace compatibility run omitted or duplicated its determinism warning:\n\
         {compatibility_stderr}"
    );
}
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review DBT normal aggregation and strict failure coverage.
// TODO(#2791): Remove the portable test.cli skip when DBT aggregation defect #2804 is fixed.
#[test]
fn run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls() {
    if dbt_unavailable(
        "run_dbt_fails_closed_by_default_and_opt_out_aggregates_unsupported_syscalls",
    ) {
        return;
    }
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("DBT unsupported-syscall guest path should be UTF-8");

    // Positive bracket: fail-closed changes only unsupported behavior. A supported
    // guest still succeeds through the same default DBT front door.
    let supported_args = [
        "run",
        "--backend",
        "dbt",
        "--",
        "/bin/echo",
        "dbt-supported-ok",
    ];
    let supported = hermit(&supported_args);
    assert_success(&supported, &supported_args);
    assert_eq!(stdout(&supported), "dbt-supported-ok\n");

    // Negative bracket: the real unsupported restart_syscall must fail and name
    // itself before the guest can publish its former success marker.
    let default_args = ["run", "--backend", "dbt", "--", program];
    let default = hermit(&default_args);
    assert!(
        !default.status.success(),
        "default DBT unexpectedly allowed an unsupported syscall:\n{}",
        stderr(&default)
    );
    assert!(
        stderr(&default).contains("unsupported syscall: restart_syscall"),
        "default DBT failure omitted unsupported syscall:\n{}",
        stderr(&default)
    );
    assert_eq!(
        stdout(&default),
        "",
        "unsupported guest published its success marker"
    );

    let normal_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
    ];
    let normal = hermit(&normal_args);
    assert_success(&normal, &normal_args);
    assert_eq!(stdout(&normal), "dbt-unsupported-ok\n");
    let normal_stderr = stderr(&normal);
    let opt_out_warning = "a successful exit does not establish complete deterministic execution";
    assert_eq!(
        normal_stderr.matches(opt_out_warning).count(),
        1,
        "compatibility opt-out warning missing or duplicated:\n{normal_stderr}"
    );
    let warning = "syscalls restart_syscall used but not yet supported";
    assert_eq!(
        normal_stderr.matches(warning).count(),
        1,
        "expected one aggregate warning:\n{normal_stderr}"
    );

    let tamper_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--",
        program,
        "report-tamper",
    ];
    let tamper = hermit(&tamper_args);
    assert_success(&tamper, &tamper_args);
    assert_eq!(stdout(&tamper), "dbt-unsupported-report-tamper-ok\n");
    assert_eq!(
        stderr(&tamper).matches(warning).count(),
        1,
        "report tampering suppressed the aggregate warning:\n{}",
        stderr(&tamper)
    );

    let fork_tamper_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--",
        program,
        "fork-report-tamper",
    ];
    let fork_tamper = hermit(&fork_tamper_args);
    assert_success(&fork_tamper, &fork_tamper_args);
    assert_eq!(
        stdout(&fork_tamper),
        "dbt-unsupported-fork-report-tamper-ok\n"
    );
    assert_eq!(
        stderr(&fork_tamper).matches(warning).count(),
        1,
        "fork-child report tampering suppressed the aggregate warning:\n{}",
        stderr(&fork_tamper)
    );

    let strict_args = ["run", "--backend", "dbt", "--strict", "--", program];
    let strict = hermit(&strict_args);
    assert!(
        !strict.status.success(),
        "strict DBT unexpectedly succeeded:\n{}",
        stderr(&strict)
    );
    assert!(
        stderr(&strict).contains("unsupported syscall: restart_syscall"),
        "strict DBT failure omitted unsupported syscall:\n{}",
        stderr(&strict)
    );
    let normal_fork_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
        "fork",
    ];
    let normal_fork = hermit(&normal_fork_args);
    assert_success(&normal_fork, &normal_fork_args);
    assert_eq!(stdout(&normal_fork), "dbt-unsupported-fork-ok\n");
    assert_eq!(
        stderr(&normal_fork).matches(warning).count(),
        1,
        "fork-child warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork)
    );

    let normal_fork_exec_args = [
        "run",
        "--backend",
        "dbt",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        program,
        "fork-exec",
    ];
    let normal_fork_exec = hermit(&normal_fork_exec_args);
    assert_success(&normal_fork_exec, &normal_fork_exec_args);
    assert_eq!(
        stdout(&normal_fork_exec),
        "dbt-unsupported-exec-ok\ndbt-unsupported-fork-exec-parent-ok\n"
    );
    assert_eq!(
        stderr(&normal_fork_exec).matches(warning).count(),
        1,
        "fork-exec warning was not aggregated exactly once:\n{}",
        stderr(&normal_fork_exec)
    );

    for mode in ["fork", "fork-exec", "fork-setsid-exec", "exec-empty"] {
        let args = ["run", "--backend", "dbt", "--strict", "--", program, mode];
        let output = hermit(&args);
        assert!(
            !output.status.success(),
            "strict DBT {mode} unexpectedly succeeded:\n{}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("unsupported syscall"),
            "strict DBT {mode} omitted unsupported-syscall diagnostic:\n{}",
            stderr(&output)
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-644): Review strict DBT teardown with a blocked stdin source.
#[test]
fn run_dbt_strict_returns_with_blocked_stdin_source() {
    if dbt_unavailable("run_dbt_strict_returns_with_blocked_stdin_source") {
        return;
    }
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("DBT unsupported-syscall guest path should be UTF-8");
    let mut source = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to start blocked DBT stdin source");
    let args = ["run", "--backend", "dbt", "--strict", "--", program];
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "2s", "10s"])
        .arg(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, &args);
    let output = command
        .stdin(source.stdout.take().expect("sleep stdout was not piped"))
        .output()
        .expect("failed to run strict DBT blocked-input regression");
    let _ = source.kill();
    let _ = source.wait();
    assert_ne!(output.status.code(), Some(124), "strict DBT hung on stdin");
    assert!(
        !output.status.success(),
        "strict DBT unexpectedly succeeded"
    );
    assert!(stderr(&output).contains("unsupported syscall"));
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-736): Review the real LiteInst Detcore CLI assertion.
#[test]
fn run_liteinst_verifies_detcore_backend() {
    liteinst_runtime::ensure_liteinst_runtime();
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--verify",
        "--",
        "/bin/echo",
        "liteinst-cli-ok",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "liteinst-cli-ok\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains(
            "liteinst host hybrid] activation verified (traps=1, hooks=31); Detcore Tool active in ptrace host"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("Success: deterministic. Determinism verified."),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "LiteInst host hybrid (reverie-liteinst patch runtime + ptrace Detcore Tool)"
        ),
        "{stderr}"
    );
}

/// Backend statistics are a HARNESS record and must stay out of the INFO parity
/// envelope, while remaining available to anyone who asks for DEBUG.
///
/// ⚠️ WHY THE LEVEL IS THE POINT, not a formatting preference.
/// `ComparedLogScope::Info` is `BitwiseInfoV1` — "every INFO message, exactly" —
/// so an INFO record is compared between two runs as if it were guest behaviour.
/// This record is hermit describing its own harness, and both of its fields say
/// which harness: `backend=<NAME>`, and `stats=` carrying that backend's own
/// instrumentation. Measured before the change, it was the ONE record of 303 in
/// a real ptrace INFO stream that named a backend — so two backends running an
/// identical guest could not agree under that envelope however correct they
/// were. Putting it back at INFO restores a divergence by construction.
#[test]
fn backend_stats_are_debug_gated_and_absent_from_the_info_envelope() {
    let default_args = ["run", "--strict", "--", "/bin/true"];
    let default_output = hermit(&default_args);
    assert_success(&default_output, &default_args);
    assert!(!stderr(&default_output).contains("backend run complete"));

    // THE REGRESSION THIS GUARDS: not merely that the record is absent, but that
    // the INFO envelope names no backend at all. A future record reintroducing a
    // backend name at INFO fails here even if it is spelled differently.
    let info_args = ["--log", "info", "run", "--strict", "--", "/bin/true"];
    let info_output = hermit(&info_args);
    assert_success(&info_output, &info_args);
    let info_stderr = stderr(&info_output);
    assert!(
        !info_stderr.contains("backend run complete"),
        "the backend-stats record must not be in the INFO parity envelope:\n{info_stderr}"
    );
    let naming_a_backend: Vec<&str> = info_stderr
        .lines()
        .filter(|line| line.contains(" INFO "))
        .filter(|line| {
            [
                "backend=ptrace",
                "backend=dbt",
                "backend=sabre",
                "backend=liteinst",
                "backend=kvm",
            ]
            .iter()
            .any(|needle| line.contains(needle))
        })
        .collect();
    assert!(
        naming_a_backend.is_empty(),
        "no INFO record may name the backend -- it cannot agree across backends by \
         construction, so it caps cross-backend parity:\n{naming_a_backend:#?}"
    );

    // POSITIVE, so this cannot pass by the record having been deleted: it is
    // still emitted, with both fields, one level down.
    let debug_args = ["--log", "debug", "run", "--strict", "--", "/bin/true"];
    let debug_output = hermit(&debug_args);
    assert_success(&debug_output, &debug_args);
    assert!(
        stderr(&debug_output).contains("backend run complete backend=ptrace stats=metrics=none"),
        "{}",
        stderr(&debug_output)
    );
}

#[test]
fn inherited_container_output_does_not_expose_capture_offset() {
    let _guard = HERMIT_RUN_LOCK.lock().unwrap();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create stdio-lseek test directory");
    let combined_log_path = directory.path().join("hermit-and-guest.log");
    let report_path = directory.path().join("guest-report");
    let guest_file_path = directory.path().join("guest-output");
    let combined_log = fs::File::create(&combined_log_path)
        .expect("failed to create combined Hermit/guest output log");
    let args = ["--log", "info", "run", "--strict", "--"];
    let status = hermit_command(&args)
        .arg(stdio_lseek_identity_guest())
        .arg(&report_path)
        .arg(&guest_file_path)
        .stdout(Stdio::from(
            combined_log
                .try_clone()
                .expect("failed to clone combined output log"),
        ))
        .stderr(Stdio::from(combined_log))
        .status()
        .expect("failed to run stdio-lseek identity fixture");
    let combined = fs::read_to_string(&combined_log_path)
        .expect("failed to read combined Hermit/guest output log");
    let report = fs::read_to_string(&report_path).expect("failed to read guest seek report");

    assert!(status.success(), "Hermit failed:\n{combined}");
    assert!(
        report.contains("inherited-stdout offset=-1 errno=29"),
        "inherited stdout exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("inherited-stderr offset=-1 errno=29"),
        "inherited stderr exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("stdout-alias offset=-1 errno=29"),
        "a dup of inherited stdout exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("stderr-alias offset=-1 errno=29"),
        "a dup of inherited stderr exposed the outer capture offset:\n{report}"
    );
    assert!(
        report.contains("guest-file-stdout offset=0 errno=0"),
        "guest-installed stdout lost ordinary file seek semantics:\n{report}"
    );
    assert!(
        report.contains("guest-file-stderr offset=0 errno=0"),
        "guest-installed stderr lost ordinary file seek semantics:\n{report}"
    );
}

#[test]
fn run_liteinst_rejects_a_non_runtime_override_before_activation_claim() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create false LiteInst runtime directory");
    let runtime = directory.path().join("not-a-liteinst-runtime");
    fs::copy("/bin/true", &runtime).expect("failed to copy false LiteInst runtime fixture");
    write_matching_liteinst_revision(&runtime);
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env("HERMIT_LITEINST_RUNTIME", &runtime)
        .output()
        .expect("failed to run Hermit with a false LiteInst runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("missing required export"), "{stderr}");
    assert!(!stderr.contains("activation verified"), "{stderr}");
    assert!(!stderr.contains("Success: deterministic"), "{stderr}");
}

#[test]
fn run_liteinst_rejects_an_inert_dso_before_activation_claim() {
    let args = [
        "run",
        "--backend",
        "liteinst",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env("HERMIT_LITEINST_RUNTIME", liteinst_inert_runtime())
        .output()
        .expect("failed to run Hermit with an inert LiteInst runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("does not register reverie_liteinst_initialize as a preload constructor"),
        "{stderr}"
    );
    assert!(!stderr.contains("activation verified"), "{stderr}");
    assert!(!stderr.contains("Success: deterministic"), "{stderr}");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#679): validate the dedicated DBT diagnostic channel.
#[test]
fn run_dbt_keeps_diagnostics_out_of_guest_stderr() {
    if dbt_unavailable("run_dbt_keeps_diagnostics_out_of_guest_stderr") {
        return;
    }
    let program = dbt_stderr_guest()
        .to_str()
        .expect("DBT stderr guest path should be UTF-8");
    let script = r#"set -euo pipefail; output=$("$1" 2>&1); test "$output" = guest-stderr; printf 'isolated=%s\n' "$output""#;
    let args = [
        "--log",
        "INFO",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/bash",
        "-c",
        script,
        "dbt-stderr-fixture",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "isolated=guest-stderr\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("INFO detcore") && stderr.contains("DETLOG [syscall]"),
        "DBT child diagnostics were not emitted:\n{stderr}"
    );
    assert!(
        !stderr.contains("guest-stderr"),
        "guest fd 2 leaked into controller diagnostics:\n{stderr}"
    );

    // Reported verification transports controller evidence out of band and must
    // not overwrite the guest's own HERMIT_LOG value.  The canonical comparator
    // can currently diverge on run-specific DBT process IDs, so this test owns
    // the environment/evidence contract, not that separate determinism defect.
    for (guest_value, expected) in [
        (None, "<unset>"),
        (Some("guest-sentinel"), "guest-sentinel"),
    ] {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create DBT log-env verification directory");
        let logs = directory.path().join("logs");
        fs::create_dir(&logs).expect("failed to create DBT log-env verification log directory");
        let verdict = directory.path().join("verdict.json");
        let expected_arg = format!("expect={expected}");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .args([
                "--log",
                "INFO",
                "run",
                "--backend",
                "dbt",
                "--strict",
                "--verify",
                "--verify-strict",
                "--keep-logs",
                "--verify-log-dir",
            ])
            .arg(&logs)
            .arg("--verify-json")
            .arg(&verdict)
            .arg("--")
            .arg(dbt_log_env_guest())
            .arg(&expected_arg)
            .env_remove("HERMIT_LOG");
        if let Some(value) = guest_value {
            command.env("HERMIT_LOG", value);
        }
        let output = command.output().expect("failed to run DBT log-env case");
        let report = read_terminal_dbt_verdict(&verdict);
        assert_eq!(
            output.status.success(),
            report["verified"] == true,
            "process status disagrees with terminal verdict: {report}"
        );
        let retained_logs = fs::read_dir(&logs)
            .expect("failed to read retained DBT log-env verification logs")
            .map(|entry| entry.expect("failed to read retained log entry").path())
            .collect::<Vec<_>>();
        assert_eq!(retained_logs.len(), 2, "unexpected logs: {retained_logs:?}");
        for log in retained_logs {
            let contents = fs::read_to_string(&log).expect("failed to read retained DBT log");
            assert!(contents.contains("INFO detcore"), "empty INFO log: {log:?}");
            assert!(
                !contents.contains("hermit_log="),
                "guest stdout leaked into DBT diagnostics: {log:?}"
            );
        }
    }
}

#[test]
fn run_dbt_forwards_detcore_info_logs() {
    if dbt_unavailable("run_dbt_forwards_detcore_info_logs") {
        return;
    }
    let args = [
        "--log",
        "INFO",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("INFO detcore") && stderr.contains("DETLOG [syscall]"),
        "DBT did not forward the Detcore INFO syscall stream:\n{stderr}",
    );
}

#[test]
fn run_dbt_uses_the_normalized_backend_config() {
    if dbt_unavailable("run_dbt_uses_the_normalized_backend_config") {
        return;
    }
    let args = [
        "--log",
        "DEBUG",
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(
        stderr.contains("detcore-dbt: using CLI-provided Detcore Config"),
        "DBT did not consume the CLI-provided config:\n{stderr}",
    );
    assert!(
        stderr.contains("backend_requires_thread_directed_process_signals: true"),
        "DBT did not receive its required process-signal translation capability:\n{stderr}",
    );
    assert!(
        !stderr.contains("backend_requires_thread_directed_process_signals: false"),
        "DBT received an unnormalized process-signal capability:\n{stderr}",
    );
}

// TODO-HUMAN-REVIEW(PR-1038): Review DBT queued self-signal verification.
// TODO(#2791): Remove the portable test.cli skip when DBT queued-signal defect #1818 is fixed.
#[test]
fn run_dbt_verifies_queued_self_signals() {
    if dbt_unavailable("run_dbt_verifies_queued_self_signals") {
        return;
    }
    let program = dbt_self_sigqueue_guest()
        .to_str()
        .expect("DBT self-sigqueue guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-self-sigqueue-ok\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#543): validate the explicit application-mmap DBT regression.
#[test]
fn run_dbt_verifies_application_mmap() {
    if dbt_unavailable("run_dbt_verifies_application_mmap") {
        return;
    }
    let program = dbt_mmap_guest()
        .to_str()
        .expect("DBT mmap guest path should be UTF-8");
    let args = ["run", "--backend", "dbt", "--verify", "--", program];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-mmap-exec-ok\n");
    assert!(
        stderr(&output).contains(":: DBT path confirmed: DynamoRIO client reported tool=Detcore"),
        "DBT confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbt_verifies_process_wait_lifecycle() {
    if dbt_unavailable("run_dbt_verifies_process_wait_lifecycle") {
        return;
    }
    let program = dbt_wait_guest()
        .to_str()
        .expect("DBT wait guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "wait4=7 waitid=9 sigchld=observed reaped=2 cpu=zero\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_exact_child_waits_have_stable_scheduler_turns() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM exact-child waits: /dev/kvm is unavailable");
        return;
    }

    let _guard = hermit_run_guard();
    let program = kvm_exact_child_waits_guest()
        .to_str()
        .expect("exact-child wait guest path should be UTF-8");
    let args = [
        "--log=info",
        "run",
        "--backend=kvm",
        "--strict",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--",
        program,
    ];
    for iteration in 0..4 {
        let output = hermit(&args);

        assert_success(&output, &args);
        assert!(
            stdout(&output)
                == "wait4=7 waitid=9 wait4-any=11 waitid-any=13 \
                live-wnohang=empty child-ready-won\n",
            "iteration {iteration} did not exercise the KVM child-wait contract: {:?}",
            stdout(&output)
        );
        let log = stderr(&output);
        assert!(
            log.contains("hermit::kvm: launching guest through reverie-kvm"),
            "iteration {iteration} did not use the KVM backend:\n{log}"
        );
        let sigchld_deliveries = log
            .lines()
            .filter(|line| line.contains("Alarm fired, delivering signal SIGCHLD"))
            .count();
        assert!(
            sigchld_deliveries >= 4,
            "iteration {iteration} did not race each ready child against SIGCHLD:\n{log}"
        );
        let exact_wait_turns = log
            .lines()
            .filter(|line| {
                line.contains("resources {WaitChild") && line.contains("selector: Exact")
            })
            .count();
        let any_wait_turns = log
            .lines()
            .filter(|line| line.contains("resources {WaitChild") && line.contains("selector: Any"))
            .count();
        assert_eq!(
            (exact_wait_turns, any_wait_turns),
            (2, 2),
            "iteration {iteration} changed the scheduler child-wait turn population:\n{log}"
        );
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-723): Review DBT PID virtualization L2 coverage.
#[test]
fn run_dbt_virtualizes_process_identities() {
    if dbt_unavailable("run_dbt_virtualizes_process_identities") {
        return;
    }
    let program = dbt_pid_guest()
        .to_str()
        .expect("DBT PID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        concat!(
            "root pid=3 ppid=1 tid=3\n",
            "grandchild pid=5 ppid=4 tid=5\n",
            "child pid=4 ppid=3 tid=4\n",
            "child grandchild=5 waited=5 exit=5\n",
            "root child=4 waited=4 exit=6\n",
            "exec-child pid=6 ppid=3 tid=6\n",
            "exec-proc stat=6/3 status=6/3 tracer=1\n",
            "root exec=6 waited=6 exit=8\n",
            "waitid-child pid=7 ppid=3 tid=7\n",
            "root waitid=7 reported=7 exit=9\n",
            "root vfork=8 waited=8 exit=0 pid=3 tid=3\n",
            "vfork-exec-child pid=9 ppid=3 tid=9\n",
            "root vfork-exec=9 waited=9 exit=10 pid=3 tid=3\n",
        )
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1065): Review DBT self-prlimit L2 coverage.
// TODO(#2791): Remove the portable test.cli skip when DBT self-prlimit defect #2806 is fixed.
#[test]
fn run_dbt_verifies_self_prlimit() {
    if dbt_unavailable("run_dbt_verifies_self_prlimit") {
        return;
    }
    let program = dbt_prlimit_self_guest()
        .to_str()
        .expect("DBT self-prlimit guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "dbt-prlimit-self-ok\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// TODO(#2791): Remove the portable test.cli skip when DBT process-group defect #605 is fixed.
#[test]
fn run_dbt_verifies_shell_process_lifecycle() {
    if dbt_unavailable("run_dbt_verifies_shell_process_lifecycle") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--verify",
        "--",
        "/bin/sh",
        "-c",
        "/bin/echo hello; :",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#598): Confirm this captures the host-inherited O_NONBLOCK regression.
// TODO-HUMAN-REVIEW(#689): Confirm the split-write case protects partial-read semantics.
#[test]
fn run_dbt_verifies_pipe_backpressure() {
    if dbt_unavailable("run_dbt_verifies_pipe_backpressure") {
        return;
    }
    let args = [
        "run",
        "--backend",
        "dbt",
        "--verify",
        "--",
        "/bin/bash",
        "-c",
        r#"{ printf "%4096s" x; for _ in {1..100000}; do :; done; printf "%1371s" y; } | wc -c"#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "5467\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_dbt_recovers_after_failed_exec() {
    if dbt_unavailable("run_dbt_recovers_after_failed_exec") {
        return;
    }
    let program = dbt_exec_failure_guest()
        .to_str()
        .expect("DBT exec-failure guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "recovered after failed exec\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}
#[test]
fn run_dbt_rejects_unfollowed_execveat() {
    if dbt_unavailable("run_dbt_rejects_unfollowed_execveat") {
        return;
    }
    let program = dbt_execveat_guest()
        .to_str()
        .expect("DBT execveat guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "dbt",
        "--strict",
        "--verify",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "execveat unsupported in root and fork child\n"
    );
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "DBT determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_executes_dynamic_guest() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
    assert!(
        !stderr(&output).contains("Hermit cannot use ptrace"),
        "kvm must not fall through to the ptrace backend:\n{}",
        stderr(&output),
    );
}

/// ⚠️ A GUEST THAT MUTATES hermit's OWN stderr MUST NOT MAKE `--verify` REPORT
/// NONDETERMINISM. Both verification runs inherit hermit's fd 2, so a guest that
/// sets a status flag on it leaves run 2 starting from state run 1 created --
/// and the comparison then blames the guest for the harness.
///
/// MEASURED 2026-08-26 on `awk 'BEGIN { print 42 }'`, which sets O_APPEND on
/// stderr only when it is not already set:
///     run 1  fcntl(2, F_GETFL) = 32769  -> fcntl(2, F_SETFL, 33793)
///     run 2  fcntl(2, F_GETFL) = 33793  -> no SETFL at all
/// One extra syscall in run 1 shifted every later record by one and reported
/// TWENTY mismatches for TWO real divergences -- the extra `F_SETFL` record AND
/// the differing `F_GETFL` RESULT (`Ok(32769)` against `Ok(33793)`). ⚠️ THIS
/// SAID "ONE" UNTIL `agent(codex-rev-2668)` AND `agent(hermit-dbg)` CAUGHT IT,
/// and `run.rs` already said two: a reader who checked only the inbound records
/// would have found them clean and concluded the harness was blameless.
/// `2>>file` instead of `2>file` -- the same run with the bit already set --
/// was deterministic, which is the whole demonstration.
///
/// ⚠️ THE `run_kvm_` PREFIX IS LOAD-BEARING, NOT DECORATION. `ci/dag/validate.json`
/// selects this suite with `-E 'test(/^run_kvm_/)'`, and that is the only lane with
/// /dev/kvm. Without the prefix the test is selected on PORTABLE instead, where there
/// is no /dev/kvm, so its own guard returns early and it reports a silent pass -- the
/// twenty-third instance of the hazard that node's own description warns about. The
/// name is what schedules it.
///
/// ⚠️ THIS USES awk TOO, AND THE COMMENT HERE ONCE CLAIMED OTHERWISE. It said
/// "THIS DRIVES THE BIT DIRECTLY RATHER THAN THROUGH awk", which was false at the
/// moment it was written: the guest below is `/usr/bin/awk`. I had drafted a
/// purpose-built guest, measured that it does NOT reproduce (see the note on the
/// `args` array), swapped back to awk, and left the sentence describing the guest
/// I had abandoned. `agent(hermit-dbg)` caught it.
///
/// ⚠️ SO WHAT DOES THIS ADD OVER `run_kvm_awk_mincore_probe_terminates`, WHICH ALSO
/// RUNS awk? Only its ASSERTION. That cell asserts `stdout == "42\n"` and the
/// determinism line, so it fails for any reason at all and names none of them.
/// This one asserts exactly the determinism verdict and says in its failure
/// message which mechanism is suspected, so a future regression arrives with its
/// cause attached instead of as a bare mismatch. It is a NAMED cell for a known
/// mechanism, not an independent trigger for it -- and if awk ever stops setting
/// the flag conditionally, BOTH cells go quiet together. That residual risk is
/// real and is stated here rather than papered over with a claim of independence.
#[test]
fn run_kvm_verify_is_deterministic_when_the_guest_mutates_hermit_stderr_flags() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }

    // ⚠️ RUN IT TWICE IN THE SAME PROCESS. One invocation cannot see this
    // defect: the leak is hermit mutating its OWN stderr and handing the dirty
    // state to its SECOND verification run, so the property is about what the
    // two runs inherit, not about either run alone.
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--",
        // ⚠️ awk IS THE GUEST BECAUSE IT IS THE ONE THAT REPRODUCES, and I
        // checked the alternative rather than assuming it. `sh -c 'exec 2>>...'`
        // looks like the same mutation and is NOT: it REOPENS the file on
        // descriptor 2, giving a fresh open file description, so hermit's own
        // description is untouched and the case does not arise (measured: zero
        // F_SETFL, rc=0, deterministic with the fix REMOVED). Only a guest that
        // calls fcntl(F_SETFL) on the INHERITED description exercises this.
        "/usr/bin/awk",
        "BEGIN { print 42 }",
    ];
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "2s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, &args);
    let output = command
        .output()
        .expect("failed to run the stderr-flag determinism regression");

    assert_ne!(
        output.status.code(),
        Some(124),
        "the stderr-flag determinism probe hung"
    );
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "a guest that mutated hermit's stderr flags was reported as nondeterministic; \
         the two verification runs did not start from identical fd state\nstderr:\n{}",
        stderr(&output),
    );
}

/// ⚠️ THE SAME LEAK ON **STDOUT**, AND IT IS HERE BECAUSE THE FIRST VERSION OF
/// THIS PULL REQUEST RESTORED fd 2 ALONE AND I CALLED THAT "SCOPE".
///
/// It was not scope, it was the limit of the guest I tested with. `awk` mutates
/// stderr, so stderr is what I fixed. `agent(codex-rev-2668)` reproduced the
/// identical mechanism on fd 1 at the head I had asked to land, and
/// `agent(hermit-dbg)` -- who had written the fd 0/fd 1 gap down in his own
/// review as "fine as scope" -- reproduced it, withdrew his approval and
/// stripped the label. Both were right. Measured by me at that head, on the
/// binary as it then stood:
///
/// ```text
/// mutating STDOUT, kvm --strict --verify   rc=125  Failure: nondeterministic
/// mutating STDOUT, ptrace (control)        rc=0    Success: deterministic
/// mutating STDERR, kvm (fd 2 fix works)    rc=0    Success: deterministic
/// mutating STDIN,  kvm                     rc=0    Success: deterministic
/// touching no flags, kvm (neg. control)    rc=0    Success: deterministic
/// run 1: fcntl(1, F_GETFL) = Ok(32769)     run 2: fcntl(1, F_GETFL) = Ok(33793)
/// ```
///
/// The last three rows are what make it a finding rather than a coincidence: the
/// shipped fd 2 fix genuinely works, stdin does not reproduce, and a guest that
/// touches no flags at all is green. It is the flag mutation on the inherited
/// description that decides the verdict, and the descriptor number is incidental.
///
/// ⚠️ THIS CELL DRIVES THE BIT DIRECTLY AND OWES NOTHING TO awk -- which is what
/// the stderr cell above cannot say, and `agent(codex-rev-2668)` was right that
/// the stderr cell is a NAMED cell rather than an independent trigger. This guest
/// is four lines of `perl -e` that call `fcntl(F_SETFL)` on fd 1 unconditionally
/// of any tool's internals, so if `awk` ever stops setting the flag both awk
/// cells go quiet together and THIS one still fails. That is the independence
/// the other cell explicitly disclaims.
///
/// ⚠️ `run_kvm_` PREFIX IS LOAD-BEARING. `ci/dag/validate.json` selects this
/// suite with `-E 'test(/^run_kvm_/)'` and that is the only lane with /dev/kvm.
/// Without the prefix this is selected on PORTABLE, its guard returns early, and
/// it reports a silent pass -- the exact hazard that node's description warns of.
#[test]
fn run_kvm_verify_is_deterministic_when_the_guest_mutates_hermit_stdout_flags() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    // Conditional on purpose: setting the flag only when it is not already set
    // is what makes run 1 and run 2 issue DIFFERENT syscall sequences. An
    // unconditional `F_SETFL` fires in both runs and the sequences match, so it
    // would not reproduce -- the same trap the stderr cell records for
    // `sh -c 'exec 2>>...'`, which reopens the descriptor instead of mutating
    // the inherited description.
    let program = concat!(
        "use Fcntl; ",
        "my $f = fcntl(STDOUT, F_GETFL, 0); ",
        "fcntl(STDOUT, F_SETFL, $f | O_APPEND) unless $f & O_APPEND; ",
        "print \"42\\n\";",
    );
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--",
        "/usr/bin/perl",
        "-e",
        program,
    ];
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "2s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, &args);
    let output = command
        .output()
        .expect("failed to run the stdout-flag determinism regression");

    assert_ne!(
        output.status.code(),
        Some(124),
        "the stdout-flag determinism probe hung"
    );
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "a guest that mutated hermit's STDOUT flags was reported as nondeterministic; \
         the two verification runs did not start from identical fd state, so the \
         restore does not cover every inherited descriptor\nstderr:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_awk_mincore_probe_terminates() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/awk").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--",
        "/usr/bin/awk",
        "BEGIN { print 42 }",
    ];
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "2s", "20s"])
        .arg(env!("CARGO_BIN_EXE_hermit"));
    append_hermit_args(&mut command, &args);
    let output = command
        .output()
        .expect("failed to run the KVM awk mincore regression");

    assert_ne!(
        output.status.code(),
        Some(124),
        "KVM awk mincore probe hung"
    );
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "42\n");
    assert!(
        stderr(&output).contains(":: Success: deterministic. Determinism verified."),
        "KVM determinism confirmation missing:\n{}",
        stderr(&output),
    );
}

#[test]
fn run_kvm_resolves_bare_program_from_guest_path() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "echo",
        "from-kvm-path",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "from-kvm-path\n");
}

#[test]
fn run_kvm_setpriv_capability_wrapper_is_deterministic() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/usr/bin/setpriv").exists()
        || !Path::new("/bin/date").exists()
    {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/setpriv",
        "--bounding-set=-sys_time",
        "/bin/date",
        "-u",
        "+%s",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "1767225600\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

/// The two sanitizer variables Hermit forces into *every* guest, on *every*
/// backend.
///
/// `hermit-cli/src/bin/hermit/run.rs` sets both to `detect_leaks=0` in one
/// backend-independent place. That is a deliberate cross-backend parity fix,
/// not a leak: the ptrace family forces them at spawn time inside
/// `reverie-ptrace`, while the out-of-process KVM backend has no such spawn
/// hook, so without this the guest would see two fewer variables under KVM than
/// under ptrace -- directly observable as a differing DETLOG `[env ...]` hash.
/// The unit test `guest_env_disables_sanitizer_leak_detection_on_every_backend`
/// pins that invariant where the command is constructed; this file observes it
/// end to end, in the guest's own `env` output.
///
/// So these two lines are *expected* output of `--base-env=empty`. Measured on
/// a dedicated KVM host at hermit `2b6005cf`: `--backend kvm` and `--backend
/// ptrace` both emit exactly this pair plus the explicit value, byte-identical, 5/5 runs
/// each.
const FORCED_GUEST_ENV: [&str; 2] = ["ASAN_OPTIONS=detect_leaks=0", "LSAN_OPTIONS=detect_leaks=0"];

/// Describes how a guest's `env` output differs from *exactly*
/// [`FORCED_GUEST_ENV`] plus `explicit`; `None` means it matched exactly.
///
/// This is an EXCLUSIVE set comparison on purpose. A `contains` check would
/// pass when a host variable leaks in *and* pass when the explicit variable is
/// dropped, so it could not tell a working `--base-env=empty` from a broken
/// one. This test has already been non-discriminating once -- it silently
/// skipped and reported success -- and a `contains` check would be the same
/// defect wearing a different costume.
fn guest_env_difference(stdout: &str, explicit: &[&str]) -> Option<String> {
    let mut expected: Vec<&str> = FORCED_GUEST_ENV
        .iter()
        .copied()
        .chain(explicit.iter().copied())
        .collect();
    expected.sort_unstable();
    let mut actual: Vec<&str> = stdout.lines().filter(|line| !line.is_empty()).collect();
    actual.sort_unstable();
    if actual == expected {
        return None;
    }
    let missing: Vec<&str> = expected
        .iter()
        .filter(|value| !actual.contains(value))
        .copied()
        .collect();
    let unexpected: Vec<&str> = actual
        .iter()
        .filter(|value| !expected.contains(value))
        .copied()
        .collect();
    Some(format!(
        "guest environment mismatch: missing {missing:?}, unexpected {unexpected:?} \
         (observed {actual:?})"
    ))
}

// The four controls below deliberately do NOT invoke Hermit and are NOT named
// `run_kvm_*`. They therefore run on every host, including one without
// `/dev/kvm`, and survive the `--skip run_kvm_` filter that the portable DAG
// applies to `test.cli`. A control that only runs where the thing it controls
// runs is not a control.

#[test]
fn guest_env_difference_accepts_the_forced_pair_plus_the_explicit_value() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nKVM_M3C=passed\nLSAN_OPTIONS=detect_leaks=0\n";
    assert_eq!(guest_env_difference(observed, &["KVM_M3C=passed"]), None);
}

/// The exact string this test asserted before it was corrected -- which is also
/// the *pre-parity-fix* KVM behaviour, where the guest saw the explicit value
/// and not the two sanitizer variables. The corrected expectation must reject
/// it, or it would still pass against the divergence the product already fixed.
#[test]
fn guest_env_difference_rejects_the_pre_parity_fix_output() {
    let difference = guest_env_difference("KVM_M3C=passed\n", &["KVM_M3C=passed"])
        .expect("the pre-parity-fix output must not satisfy the corrected expectation");
    assert!(
        difference.contains("ASAN_OPTIONS=detect_leaks=0"),
        "{difference}"
    );
    assert!(
        difference.contains("LSAN_OPTIONS=detect_leaks=0"),
        "{difference}"
    );
}

#[test]
fn guest_env_difference_rejects_a_leaked_host_variable() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nKVM_HOST_ONLY=must-not-leak\n\
                    KVM_M3C=passed\nLSAN_OPTIONS=detect_leaks=0\n";
    let difference = guest_env_difference(observed, &["KVM_M3C=passed"])
        .expect("a leaked host variable must fail the expectation");
    assert!(
        difference.contains("KVM_HOST_ONLY=must-not-leak"),
        "{difference}"
    );
}

#[test]
fn guest_env_difference_rejects_a_dropped_explicit_variable() {
    let observed = "ASAN_OPTIONS=detect_leaks=0\nLSAN_OPTIONS=detect_leaks=0\n";
    let difference = guest_env_difference(observed, &["KVM_M3C=passed"])
        .expect("a dropped explicit variable must fail the expectation");
    assert!(difference.contains("KVM_M3C=passed"), "{difference}");
}

#[test]
fn run_kvm_propagates_explicit_environment() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=empty",
        "--env=KVM_M3C=passed",
        "--",
        "/usr/bin/env",
    ];
    // Plant a host-only value, as `run_dbt_uses_the_requested_guest_environment`
    // does: `--base-env=empty` only means something if there was something to
    // exclude. The exclusive comparison below fails on this value specifically
    // and on any other unexpected one.
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.env("KVM_HOST_ONLY", "must-not-leak");
    append_hermit_args(&mut command, &args);
    let output = command
        .output()
        .expect("failed to run the KVM guest-environment regression");

    assert_success(&output, &args);
    let stdout = stdout(&output);
    assert_eq!(
        guest_env_difference(&stdout, &["KVM_M3C=passed"]),
        None,
        "guest environment was not exactly the forced sanitizer pair plus the \
         explicit value:\n{stdout}",
    );
}

#[test]
fn run_kvm_bash_process_substitution_is_deterministic() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/bin/bash").exists()
        || !Path::new("/usr/bin/paste").exists()
        || !Path::new("/usr/bin/diff").exists()
    {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/bash",
        "-c",
        r#"set -euo pipefail; /usr/bin/paste -d: <(printf "alpha\nbeta\n") <(printf "1\n2\n") | /usr/bin/diff -u <(printf "alpha:1\nbeta:2\n") -; printf "paste-ok\n""#,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "paste-ok\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_cpuid_policy_is_deterministic() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM CPUID regression requires cc, gcc, or clang on PATH");
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-cpuid");
    fs::create_dir_all(&build_root).expect("failed to create KVM CPUID guest directory");
    let binary = build_root.join("cpuid_probe");
    let compile = Command::new(compiler)
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/backend-parity/fixtures/cpuid_probe.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to compile KVM CPUID guest");
    assert!(
        compile.status.success(),
        "KVM CPUID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let program = binary.to_str().expect("CPUID guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "CPUID-SUCCESS vendor=GenuineIntel signature=00000663\n"
    );
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_respects_workdir_for_relative_paths() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM cwd fixture");
    fs::write(temp.path().join("message.txt"), b"from-kvm-cwd\n")
        .expect("failed to write KVM cwd fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let output = if std::env::var_os(ISOLATED_WORKDIR_ENV).is_some() {
        // Keep the host fixture explicit while the subject remains relative
        // path resolution from the required /test working directory.
        let fixture_mount = format!(
            "--mount=type=bind,source={},target=/tmp/input",
            temp.path().display()
        );
        let args = [
            "run",
            "--backend",
            "kvm",
            "--strict",
            "--verify",
            "--tmp=/tmp",
            fixture_mount.as_str(),
            "--",
            "/bin/sh",
            "-c",
            "test \"$PWD\" = /test && cat ../tmp/input/message.txt",
        ];
        hermit(&args)
    } else {
        let args = [
            "run",
            "--backend",
            "kvm",
            "--strict",
            "--verify",
            "--tmp=/tmp",
            "--workdir",
            workdir,
            "--",
            "/bin/cat",
            "message.txt",
        ];
        hermit(&args)
    };

    assert!(
        output.status.success(),
        "KVM relative-workdir check failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(stdout(&output), "from-kvm-cwd\n");
}

#[test]
fn run_kvm_lists_host_directory_metadata() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let temp = tempfile::tempdir().expect("failed to create KVM directory fixture");
    fs::write(temp.path().join("alpha.txt"), b"alpha\n")
        .expect("failed to write KVM directory fixture");
    fs::create_dir(temp.path().join("subdir")).expect("failed to create KVM subdirectory");
    std::os::unix::fs::symlink("alpha.txt", temp.path().join("alpha-link"))
        .expect("failed to create KVM symlink fixture");
    let workdir = temp
        .path()
        .to_str()
        .expect("temporary path should be UTF-8");
    let output = if std::env::var_os(ISOLATED_WORKDIR_ENV).is_some() {
        // The fixture is still host-created metadata; only its guest-visible
        // location changes so the guest itself can start in /test.
        let fixture_mount = format!(
            "--mount=type=bind,source={},target=/tmp/input",
            temp.path().display()
        );
        let args = [
            "run",
            "--backend",
            "kvm",
            "--verify",
            "--base-env=minimal",
            "--tmp=/tmp",
            fixture_mount.as_str(),
            "--",
            "/bin/ls",
            "-ln",
            "../tmp/input",
        ];
        hermit(&args)
    } else {
        let args = [
            "run",
            "--backend",
            "kvm",
            "--verify",
            "--base-env=minimal",
            "--tmp=/tmp",
            "--workdir",
            workdir,
            "--",
            "/bin/ls",
            "-ln",
            ".",
        ];
        hermit(&args)
    };

    assert!(
        output.status.success(),
        "KVM directory-metadata check failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let listing = stdout(&output);
    let alpha = listing
        .lines()
        .find(|line| line.ends_with(" alpha.txt") && !line.contains(" -> "))
        .unwrap_or_else(|| panic!("missing file in:\n{listing}"));
    let alpha_fields: Vec<_> = alpha.split_whitespace().collect();
    assert!(alpha_fields[0].starts_with("-rw"), "bad file mode: {alpha}");
    assert_eq!(alpha_fields[4], "6", "bad file size: {alpha}");
    let subdir = listing
        .lines()
        .find(|line| line.ends_with(" subdir"))
        .unwrap_or_else(|| panic!("missing directory in:\n{listing}"));
    assert!(subdir.starts_with("d"), "bad directory type: {subdir}");
    let link = listing
        .lines()
        .find(|line| line.ends_with(" alpha-link -> alpha.txt"))
        .unwrap_or_else(|| panic!("missing symlink in:\n{listing}"));
    assert!(link.starts_with("l"), "bad symlink type: {link}");
}

#[test]
fn run_kvm_reads_host_file() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let expected = fs::read_to_string("/etc/hostname").expect("failed to read host hostname");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/cat",
        "/etc/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), expected);
}

#[test]
fn run_kvm_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hello\n");
}

#[test]
fn run_kvm_f_getfl_and_reads_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; defined($line) && $line eq "hello\n" or die "stdin mismatch\n"; print "fcntl-stdin-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-stdin-ok\n");
}

#[test]
fn run_kvm_verify_f_getfl_and_replays_standard_input() {
    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/bin/perl").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/perl",
        "-MFcntl=F_GETFL",
        "-e",
        r#"defined(fcntl(STDIN, F_GETFL, 0)) or die "fcntl failed: $!\n"; my $line = <STDIN>; defined($line) && $line eq "hello\n" or die "stdin mismatch\n"; print "fcntl-verify-ok\n";"#,
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "fcntl-verify-ok\n");
    assert!(stderr(&output).contains(":: Success: deterministic. Determinism verified."));
}

#[test]
fn run_kvm_verify_replays_standard_input() {
    for backend in ["ptrace", "kvm"] {
        if backend == "kvm" && !Path::new("/dev/kvm").exists() {
            continue;
        }

        let args = [
            "run",
            "--backend",
            backend,
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
            "/bin/cat",
        ];
        let output = hermit_with_stdin(&args, b"visible-in-both-runs\n");

        assert_success(&output, &args);
        assert_eq!(
            stdout(&output),
            "visible-in-both-runs\n",
            "backend={backend}"
        );
        assert!(
            stderr(&output).contains(":: Success: deterministic. Determinism verified."),
            "backend={backend}"
        );
    }
}

#[test]
fn run_kvm_preserves_closed_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/bin/cat",
    ];
    let output = hermit_with_closed_stdin(&args);

    // ⚠️ THE GUEST RAN, SO THIS IS THE GUEST'S OWN STATUS AND NOT A `Refusal`.
    // `/bin/cat` starts, finds stdin closed, prints its own diagnostic
    // ("/bin/cat: -: Bad file descriptor") and exits 1 of its own accord. That
    // the closed descriptor is PRESERVED INTO THE GUEST is the whole subject of
    // this test, so a hermit-internal code here would assert the opposite of
    // what the test exists to check.
    //
    // This assertion was briefly changed to `HERMIT_INTERNAL_FAILURE_EXIT` on
    // the theory that no guest was launched. It was wrong, and the reason it
    // looked right is worth keeping: from a checkout under /tmp the case fails
    // earlier, with "failed to resolve KVM guest working directory", and that
    // unrelated failure hides the real exit status. Verify this one from a
    // checkout OUTSIDE /tmp or the evidence is not about this test.
    //
    // So this is a deliberate bare literal, like the guest arm of
    // `a_guest_side_fault_is_not_reported_as_a_hermit_internal_failure`: it must
    // NOT track a hermit constant, because it is not hermit's number.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected the guest's own exit status to survive a closed stdin: {output:?}"
    );
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output)
            .to_ascii_lowercase()
            .contains("bad file descriptor")
    );

    // Keep the original loader/closed-stdin assertion above. These additional
    // controls start with stdin open, then exercise object identity after the
    // program's entry point, independently of that historical loader failure.
    // The fixture's complete-stat-routes mode retains the separate followed
    // proc-fd stat control; this test explicitly selects descriptor reuse.
    let guest = stdio_inode_identity_guest();
    let mode = "descriptor-reuse";
    let expected = format!("stdio-inode-{mode}-ok\n");
    let native_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create native stdio-inode workdir");
    let native = Command::new(guest)
        .arg(mode)
        .current_dir(native_dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("failed to run native stdio-inode control");
    assert!(native.status.success(), "native {mode} control: {native:?}");
    assert_eq!(stdout(&native), expected);
    assert_eq!(stderr(&native), "");
    for backend in ["ptrace", "kvm"] {
        let args = [
            "run",
            "--backend",
            backend,
            "--strict",
            "--base-env=minimal",
            "--",
        ];
        let output = hermit_command(&args)
            .arg(guest)
            .arg(mode)
            .current_dir(native_dir.path())
            .stdin(Stdio::null())
            .output()
            .expect("failed to run stdio-inode identity fixture");
        assert_success(&output, &args);
        assert_eq!(stdout(&output), expected, "{backend} {mode}");
        assert_eq!(stderr(&output), "", "{backend} {mode}");
    }
}

#[test]
fn run_kvm_verify_does_not_write_to_standard_input() {
    if !Path::new("/usr/bin/perl").exists() {
        return;
    }

    for backend in ["ptrace", "kvm"] {
        if backend == "kvm" && !Path::new("/dev/kvm").exists() {
            continue;
        }

        let temp = tempfile::tempdir().expect("failed to create stdin fixture");
        let path = temp.path().join("stdin");
        fs::write(&path, b"original-data").expect("failed to write stdin fixture");
        let stdin = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("failed to open stdin fixture");
        let args = [
            "run",
            "--backend",
            backend,
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--",
            "/usr/bin/perl",
            "-MPOSIX",
            "-e",
            "POSIX::write(0, \"leak\", 4); exit 0",
        ];
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        append_hermit_args(&mut command, &args);
        let output = command
            .stdin(Stdio::from(stdin))
            .output()
            .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));

        assert_success(&output, &args);
        assert_eq!(
            fs::read(path).unwrap(),
            b"original-data",
            "backend={backend}"
        );
    }
}

#[test]
fn run_kvm_counts_standard_input() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--base-env=minimal",
        "--",
        "/usr/bin/wc",
    ];
    let output = hermit_with_stdin(&args, b"hello\n");

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output).split_whitespace().collect::<Vec<_>>(),
        ["1", "1", "6"]
    );
}

#[test]
fn run_kvm_reports_hostname() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/bin/hostname",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "hermetic-container.local\n");
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm the host C compiler is acceptable for this KVM smoke guest.
#[test]
fn run_kvm_pipe_pipe2_and_getgroups_round_trip() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("KVM syscall regression requires cc, gcc, or clang on PATH");

    // The pinned root hides its own /tmp from the Hermit guest. Stage an
    // executable guest beside the test binary when that path is requested.
    let temp = if std::env::var_os(ISOLATED_WORKDIR_ENV).is_some() {
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
    } else {
        tempfile::tempdir()
    }
    .expect("failed to create pipe guest directory");
    let source = temp.path().join("pipe_roundtrip.c");
    let binary = temp.path().join("pipe_roundtrip");
    fs::write(
        &source,
        br#"#define _GNU_SOURCE
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int roundtrip(int flags) {
    int fds[2];
    char buffer[3] = {0};
    int result = flags < 0 ? pipe(fds) : pipe2(fds, flags);
    if (result != 0) return 1;
    if (write(fds[1], "ok", 2) != 2) return 2;
    if (read(fds[0], buffer, 2) != 2) return 3;
    if (close(fds[0]) != 0 || close(fds[1]) != 0) return 4;
    return strcmp(buffer, "ok") != 0;
}

int main(void) {
    gid_t groups[1] = {0};
    if (roundtrip(-1) || roundtrip(O_CLOEXEC | O_NONBLOCK)) return 1;
    if (getgroups(0, NULL) != 1) return 5;
    if (getgroups(1, groups) != 1 || groups[0] != 65534) return 6;
    puts("kvm-syscalls-ok");
    return 0;
}
"#,
    )
    .expect("failed to write pipe guest");
    let compile = Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("failed to invoke C compiler");
    assert!(
        compile.status.success(),
        "failed to compile pipe guest: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let program = binary.to_str().expect("pipe guest path should be UTF-8");
    let args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "kvm-syscalls-ok\n");
}

#[test]
fn run_kvm_random_device_lseek_matches_linux() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }
    let _guard = hermit_run_guard();
    let compiler = ["cc", "gcc", "clang"]
        .into_iter()
        .find(|program| {
            Command::new(program)
                .args(["-x", "c", "-fsyntax-only", "-"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
        .expect("random-device lseek regression requires cc, gcc, or clang on PATH");

    // The pinned root hides its own /tmp from the Hermit guest. Stage an
    // executable guest beside the test binary when that path is requested.
    let temp = if std::env::var_os(ISOLATED_WORKDIR_ENV).is_some() {
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
    } else {
        tempfile::tempdir()
    }
    .expect("failed to create random-device lseek guest directory");
    let source = temp.path().join("random_device_lseek.c");
    let binary = temp.path().join("random_device_lseek");
    fs::write(
        &source,
        br#"#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int expect_lseek(int fd, off_t offset, int whence, off_t expected,
                        int expected_errno, const char *label) {
    errno = 0;
    off_t actual = syscall(SYS_lseek, fd, offset, whence);
    int actual_errno = errno;
    if (actual == expected && actual_errno == expected_errno) return 0;
    fprintf(stderr,
            "%s: expected %lld errno %d, got %lld errno %d\n",
            label, (long long)expected, expected_errno,
            (long long)actual, actual_errno);
    return 1;
}

int main(int argc, char **argv) {
    int opath = argc == 2 && strcmp(argv[1], "--opath") == 0;
    int fd = open("/dev/urandom", opath ? O_PATH : O_RDONLY);
    if (fd < 0) {
        perror("open /dev/urandom");
        return 1;
    }

    int failed = 0;
    if (opath) {
        failed |= expect_lseek(fd, 0, SEEK_SET, -1, EBADF, "O_PATH SEEK_SET");
        failed |= expect_lseek(fd, -4, SEEK_CUR, -1, EBADF, "O_PATH SEEK_CUR");
        failed |= expect_lseek(fd, 0, SEEK_END, -1, EBADF, "O_PATH SEEK_END");
        failed |= expect_lseek(fd, 0, SEEK_DATA, -1, EBADF, "O_PATH SEEK_DATA");
        failed |= expect_lseek(fd, 0, SEEK_HOLE, -1, EBADF, "O_PATH SEEK_HOLE");
        failed |= expect_lseek(fd, 0, 99, -1, EBADF, "O_PATH invalid whence");
        if (close(fd) != 0) {
            perror("close O_PATH /dev/urandom");
            return 2;
        }
        if (failed) return 3;
        puts("random-device-lseek-opath-ok");
        return 0;
    }

    unsigned char bytes[8];
    if (read(fd, bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        perror("read /dev/urandom before lseek");
        return 4;
    }
    failed |= expect_lseek(fd, -4, SEEK_CUR, 0, 0, "SEEK_CUR");
    failed |= expect_lseek(fd, 123, SEEK_SET, 0, 0, "SEEK_SET");
    failed |= expect_lseek(fd, 0, SEEK_END, 0, 0, "SEEK_END");
    failed |= expect_lseek(fd, 0, SEEK_DATA, 0, 0, "SEEK_DATA");
    failed |= expect_lseek(fd, 0, SEEK_HOLE, 0, 0, "SEEK_HOLE");
    failed |= expect_lseek(fd, 0, 99, -1, EINVAL, "invalid whence");
    if (read(fd, bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        perror("read /dev/urandom after lseek");
        return 5;
    }
    if (close(fd) != 0) {
        perror("close /dev/urandom");
        return 6;
    }
    if (failed) return 7;
    puts("random-device-lseek-ok");
    return 0;
}
"#,
    )
    .expect("failed to write random-device lseek guest");
    let compile = Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&binary)
        .arg(&source)
        .output()
        .expect("failed to invoke C compiler");
    assert!(
        compile.status.success(),
        "failed to compile random-device lseek guest:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let program = binary
        .to_str()
        .expect("random-device lseek guest path should be UTF-8");
    let kvm_args = [
        "--log=info",
        "run",
        "--backend=kvm",
        "--strict",
        "--verify",
        "--verify-strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
    ];
    let kvm_output = hermit(&kvm_args);
    assert_success(&kvm_output, &kvm_args);
    assert_eq!(stdout(&kvm_output), "random-device-lseek-ok\n");
    assert!(
        stderr(&kvm_output).contains(":: Success: deterministic. Determinism verified."),
        "KVM determinism confirmation missing:\n{}",
        stderr(&kvm_output),
    );

    // Reverie KVM currently rejects O_PATH at openat. Exercise the Linux
    // lseek error precedence through the ptrace backend until KVM accepts the
    // descriptor itself.
    let ptrace_args = [
        "--log=info",
        "run",
        "--backend=ptrace",
        "--strict",
        "--verify",
        "--verify-strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--tmp=/tmp",
        "--base-env=minimal",
        "--",
        program,
        "--opath",
    ];
    let ptrace_output = hermit(&ptrace_args);
    assert_success(&ptrace_output, &ptrace_args);
    assert_eq!(stdout(&ptrace_output), "random-device-lseek-opath-ok\n");
    assert!(
        stderr(&ptrace_output).contains("Success: deterministic. Determinism verified."),
        "ptrace determinism confirmation missing:\n{}",
        stderr(&ptrace_output),
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#544): Confirm 65534 remains the fixed container overflow group.
#[test]
fn run_kvm_reports_fixed_supplementary_groups() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let kvm_args = [
        "run",
        "--backend",
        "kvm",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "id",
        "-G",
    ];
    let kvm_output = hermit(&kvm_args);
    assert_success(&kvm_output, &kvm_args);
    assert_eq!(
        stdout(&kvm_output),
        "0 65534\n",
        "KVM must report its root-plus-overflow-group credential persona"
    );
}

#[test]
fn namespace_only_rejects_every_explicit_backend() {
    for backend in ["ptrace", "dbt", "kvm"] {
        let args = [
            "run",
            "--backend",
            backend,
            "--namespace-only",
            "--",
            "/bin/true",
        ];
        let output = hermit(&args);
        assert_eq!(output.status.code(), Some(2));
        let message = stderr(&output);
        assert!(
            message.contains("--backend"),
            "unexpected error:\n{message}"
        );
        assert!(
            message.contains("--namespace-only"),
            "unexpected error:\n{message}"
        );
    }
}

#[test]
fn namespace_only_applies_minimal_and_explicit_environment() {
    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--namespace-only",
        "--base-env=minimal",
        "--env=NAMESPACE_ONLY_EXPLICIT=present",
        "--",
        "/bin/sh",
        "-c",
        "printf 'home=%s\\nhostname=%s\\npath=%s\\nexplicit=%s\\nhost=%s\\nasan=%s\\nlsan=%s\\n' \
         \"${HOME-UNSET}\" \"${HOSTNAME-UNSET}\" \"${PATH-UNSET}\" \
         \"${NAMESPACE_ONLY_EXPLICIT-UNSET}\" \"${NAMESPACE_ONLY_HOST_ONLY-UNSET}\" \
         \"${ASAN_OPTIONS-UNSET}\" \"${LSAN_OPTIONS-UNSET}\"",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("NAMESPACE_ONLY_HOST_ONLY", "must-not-leak")
        .env("ASAN_OPTIONS", "namespace-only-host-asan")
        .env("LSAN_OPTIONS", "namespace-only-host-lsan")
        .args(args)
        .output()
        .expect("failed to run namespace-only minimal-environment regression");

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "home=/root\nhostname=hermetic-container.local\n\
         path=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
         explicit=present\nhost=UNSET\nasan=UNSET\nlsan=UNSET\n"
    );
    assert_eq!(stderr(&output), "");
}

#[test]
fn namespace_only_preserves_default_host_environment() {
    let _guard = hermit_run_guard();
    let unrelated_secret = "namespace-only-secret-must-not-appear";
    let args = [
        "run",
        "--namespace-only",
        "--env=NAMESPACE_ONLY_EXPLICIT=present",
        "--",
        "/bin/sh",
        "-c",
        "printf 'host=%s\\nexplicit=%s\\nasan=%s\\nlsan=%s\\n' \
         \"$NAMESPACE_ONLY_HOST_ONLY\" \"$NAMESPACE_ONLY_EXPLICIT\" \
         \"$ASAN_OPTIONS\" \"$LSAN_OPTIONS\"",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("NAMESPACE_ONLY_HOST_ONLY", "preserved")
        .env("ASAN_OPTIONS", "namespace-only-host-asan")
        .env("LSAN_OPTIONS", "namespace-only-host-lsan")
        .env("NAMESPACE_ONLY_UNRELATED_SECRET", unrelated_secret)
        .args(args)
        .output()
        .expect("failed to run namespace-only host-environment regression");

    assert_success(&output, &args);
    let guest_stdout = stdout(&output);
    let guest_stderr = stderr(&output);
    assert_eq!(
        guest_stdout,
        "host=preserved\nexplicit=present\nasan=namespace-only-host-asan\n\
         lsan=namespace-only-host-lsan\n"
    );
    assert_eq!(guest_stderr, "");
    assert!(!guest_stdout.contains(unrelated_secret));
    assert!(!guest_stderr.contains(unrelated_secret));
}

#[test]
fn namespace_only_applies_a_fresh_private_tmpfs_workdir() {
    let _guard = hermit_run_guard();
    let marker_source = tempfile::Builder::new()
        .prefix("hermit_namespace_only_private_")
        .tempdir_in("/tmp")
        .expect("failed to allocate a unique namespace-only marker");
    let marker = marker_source
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("temporary marker name should be UTF-8");
    let host_marker = Path::new("/test").join(marker);
    assert!(
        !host_marker.exists(),
        "host marker already exists: {host_marker:?}"
    );

    let script = format!(
        "test ! -e /test/{marker} || exit 91; /bin/pwd -P; \
         /usr/bin/stat -f -c %T .; : > /test/{marker}"
    );
    let args = [
        "run",
        "--namespace-only",
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ];

    for run in 1..=2 {
        let output = hermit(&args);
        assert_success(&output, &args);
        assert_eq!(stdout(&output), "/test\ntmpfs\n", "run {run}");
        assert_eq!(stderr(&output), "", "run {run}");
        assert!(
            !host_marker.exists(),
            "run {run} leaked its private tmpfs marker to {host_marker:?}"
        );
    }
}

#[test]
fn namespace_only_propagates_guest_exit_status() {
    let _guard = hermit_run_guard();
    let args = ["run", "--namespace-only", "--", "/bin/sh", "-c", "exit 37"];
    let output = hermit(&args);

    assert_eq!(
        output.status.code(),
        Some(37),
        "unexpected output: {output:?}"
    );
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");
}

#[test]
fn namespace_only_reports_invalid_process_settings() {
    let _guard = hermit_run_guard();

    let missing_workdir = "/definitely/missing/hermit-namespace-only-workdir";
    assert!(!Path::new(missing_workdir).exists());
    let unrelated_secret = "namespace-only-failure-secret-must-not-appear";
    let workdir_args = [
        "run",
        "--namespace-only",
        "--workdir",
        missing_workdir,
        "--",
        "/bin/true",
    ];
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("NAMESPACE_ONLY_UNRELATED_SECRET", unrelated_secret)
        .args(workdir_args)
        .output()
        .expect("failed to run namespace-only bad-workdir regression");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["chdir failed", "ENOENT", "No such file or directory"],
    );
    assert!(!stdout(&output).contains(unrelated_secret));
    assert!(!stderr(&output).contains(unrelated_secret));

    let missing_mount = "/definitely/missing/hermit-namespace-only-mount";
    assert!(!Path::new(missing_mount).exists());
    let mount = format!("--mount=type=bind,source={missing_mount},target=/test/input");
    let output = hermit(&["run", "--namespace-only", &mount, "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["--mount source", missing_mount, "does not exist"],
    );

    let missing_env = format!("HERMIT_NAMESPACE_ONLY_MISSING_ENV_{}", std::process::id());
    assert!(std::env::var_os(&missing_env).is_none());
    let env = format!("--env={missing_env}");
    let output = hermit(&["run", "--namespace-only", &env, "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &[&missing_env, "not set in the host environment"],
    );
}

#[test]
fn backend_accepted_in_global_position() {
    if dbt_unavailable("backend_accepted_in_global_position") {
        return;
    }
    // The global-position `--backend` (before the subcommand) must be threaded
    // through to `run` and reach the integrated DBT backend.
    let dbt_args = ["--backend", "dbt", "run", "--", "/bin/true"];
    let dbt = hermit(&dbt_args);

    assert_success(&dbt, &dbt_args);

    if Path::new("/dev/kvm").exists() {
        let args = ["--backend", "kvm", "run", "--", "/bin/true"];
        let kvm = hermit(&args);
        assert_success(&kvm, &args);
        assert!(
            !stderr(&kvm).contains("Hermit cannot use ptrace"),
            "global-position kvm should reach its dispatch:\n{}",
            stderr(&kvm),
        );
    }
}

#[test]
fn sabre_backend_validation_honors_command_scope() {
    let non_run = hermit(&["--backend", "sabre", "record", "list"]);
    assert_hermit_refusal_contains(
        &non_run,
        Refusal::Hermit,
        &["SaBRe backend", "only through", "strace"],
    );

    let local_override = hermit(&[
        "--backend",
        "sabre",
        "run",
        "--backend",
        "ptrace",
        "--",
        "/definitely/missing/sabre-backend-override-test",
    ]);
    assert_hermit_refusal_contains(
        &local_override,
        Refusal::GuestNotFound,
        &["does not exist or is not accessible"],
    );
    assert!(!stderr(&local_override).contains("SaBRe backend"));

    let log = hermit(&[
        "--backend",
        "sabre",
        "--log",
        "info",
        "strace",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &log,
        Refusal::Hermit,
        &["does not support --log or --log-file"],
    );
}

#[test]
fn sabre_rpc_socket_is_hidden_from_proc_environ() {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let loader = target_dir.join("sabre/sabre");
    let plugin = executable_dir.join("libdetcore_sabre.so");
    if !loader.is_file() || !plugin.is_file() {
        return;
    }

    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--backend",
        "sabre",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--",
        "/usr/bin/cat",
        "/proc/self/environ",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    let guest_environment = stdout(&output);
    assert!(
        !guest_environment.contains("REVERIE_SABRE_HERMIT_RPC_SOCKET"),
        "private coordinator setting leaked through procfs: {guest_environment:?}"
    );
    assert!(
        stderr(&output).contains("Determinism verified"),
        "strict repeat verification did not complete:\n{}",
        stderr(&output)
    );
}

#[test]
fn sabre_rpc_socket_ignores_host_tmpdir_hidden_by_container_tmp() {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let loader = std::env::var_os("HERMIT_SABRE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = std::env::var_os("HERMIT_INSTALL_DIR")
        .map(PathBuf::from)
        .map(|install| install.join("rsrcs/libdetcore_sabre.so"))
        .unwrap_or_else(|| executable_dir.join("libdetcore_sabre.so"));
    if !loader.is_file() || !plugin.is_file() {
        // ⚠️ SAY SO. A bare `return` here reports `ok` in 0.00s having executed
        // nothing, and the reader concludes the namespace fix is verified when
        // the test never ran. `sabre_examples.rs` already prints its skip for
        // exactly this reason; matching that rather than inventing a form.
        eprintln!(
            "skipping SaBRe RPC TMPDIR check: artifacts are unavailable: loader={}, plugin={}",
            loader.display(),
            plugin.display()
        );
        return;
    }

    let host_tmpdir = tempfile::Builder::new()
        .prefix("sabre-host-tmpdir-")
        .tempdir_in("/tmp")
        .expect("failed to create nested host TMPDIR");
    let verify_report =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join("sabre-nested-host-tmpdir-verify.json");
    let _ = fs::remove_file(&verify_report);

    let _guard = hermit_run_guard();
    let args = [
        "--log=info",
        "run",
        "--backend",
        "sabre",
        "--strict",
        "--verify",
        "--verify-strict",
        "--verify-json",
        verify_report.to_str().unwrap(),
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env("TMPDIR", host_tmpdir.path())
        .env("HERMIT_SABRE_BINARY", &loader)
        .output()
        .expect("failed to run SaBRe nested-TMPDIR regression");
    assert_success(&output, &args);

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&verify_report).expect("SaBRe nested-TMPDIR verification report was not written"),
    )
    .expect("SaBRe nested-TMPDIR verification report was not valid JSON");
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["compare_logs"] == true
            && report["comparison"]["log_scope"] == "info",
        "SaBRe nested-TMPDIR run did not produce a canonical matched report:\n{report}"
    );
}

#[test]
fn global_position_rejects_unknown_backends() {
    let args = ["--backend", "unknown", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_eq!(output.status.code(), Some(2));
    let stderr = stderr(&output);
    assert!(
        stderr.contains("invalid value 'unknown'"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn namespace_only_rejects_global_position_backend() {
    let args = [
        "--backend",
        "ptrace",
        "run",
        "--namespace-only",
        "--",
        "/bin/true",
    ];
    let output = hermit(&args);
    let message = stderr(&output);
    assert!(
        message.contains("--backend"),
        "unexpected error:\n{message}"
    );
    assert!(
        message.contains("--namespace-only"),
        "unexpected error:\n{message}"
    );
}

#[test]
fn incompatible_run_modes_fail_during_argument_parsing() {
    let args = ["run", "--namespace-only", "--chaos", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("--namespace-only"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--chaos"), "unexpected error:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn no_namespace_rejects_container_only_options() {
    let cases = [
        "--namespace-only",
        "--analyze-networking",
        "--mount=type=bind,source=/tmp,target=/tmp",
        "--bind=/tmp",
        "--network=local",
        "--network=host",
        "--tmp=/tmp/custom",
        "--replay-schedule-from=/tmp/schedule.json",
        "--replay-preemptions-from=/tmp/preemptions.json",
    ];

    for incompatible in cases {
        let args = ["run", "--no-namespace", incompatible, "/bin/true"];
        let output = hermit(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "hermit {args:?} unexpectedly ran"
        );

        let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
        assert!(
            stderr.contains("--no-namespace"),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains(incompatible.split_once("=").map_or(incompatible, |x| x.0)),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains("cannot be used with"),
            "unexpected error:\n{stderr}"
        );
    }
}

#[test]
fn no_namespace_runs_without_container_setup() {
    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--no-namespace",
        "--max-timeslice=disabled",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    assert_eq!(stdout(&output), "hello\n");
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("WARNING: --no-namespace"),
        "unexpected stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("less deterministic"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn no_namespace_preserves_affinity_for_run_and_verify() {
    let _guard = hermit_run_guard();

    let run_args = [
        "run",
        "--no-namespace",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/usr/bin/nproc",
    ];
    let output = hermit(&run_args);
    assert_success(&output, &run_args);
    assert_eq!(stdout(&output), "1\n");

    let verify_args = [
        "run",
        "--no-namespace",
        "--verify",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        "/usr/bin/nproc",
    ];
    let output = hermit(&verify_args);
    assert_success(&output, &verify_args);
    assert_eq!(stdout(&output), "1\n");
    assert!(
        stderr(&output).contains("Determinism verified"),
        "no-namespace verify did not complete:\n{}",
        stderr(&output),
    );
}

#[test]
fn no_namespace_fork_children_have_deterministic_distinct_rng_streams() {
    let _guard = hermit_run_guard();
    let guest = fork_child_getrandom_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--no-namespace",
        "--verify",
        "--pin-threads",
        "--max-timeslice=disabled",
        "--",
        guest,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("Determinism verified"),
        "missing verification success marker:\n{stderr}"
    );
}

#[test]
fn record_list_json_reports_an_empty_inventory() {
    let data_dir = tempfile::tempdir().expect("failed to create recording data directory");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(data_dir.path())
        .output()
        .expect("failed to run hermit record list");
    assert!(
        output.status.success(),
        "hermit record list failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("record list should emit JSON");
    assert_eq!(value, serde_json::json!([]));
}

#[test]
fn record_list_rejects_a_non_directory_inventory() {
    let parent = tempfile::tempdir().expect("failed to create recording data parent");
    let data_file = parent.path().join("not-a-directory");
    fs::write(&data_file, b"not a recording inventory")
        .expect("failed to create non-directory data path");

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(&data_file)
        .output()
        .expect("failed to run hermit record list");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["Failed to read recording inventory", "not-a-directory"],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "clean", "--data-dir"])
        .arg(&data_file)
        .output()
        .expect("failed to run hermit record clean");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["Failed to read recording inventory", "not-a-directory"],
    );
    assert_eq!(
        fs::read(&data_file).expect("record clean removed the data path"),
        b"not a recording inventory"
    );
}

#[test]
fn run_rejects_invalid_programs_with_actionable_errors() {
    let output = hermit(&["run", "--", "/definitely/missing/hermit-program"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotFound,
        &["does not exist or is not accessible", "Check the path"],
    );

    let output = hermit(&["run", "--", "definitely-missing-hermit-program"]);
    assert_hermit_refusal_contains(
        &output,
        // ⚠️ GuestNotFound, THE SAME AS THE ABSOLUTE-PATH ARM ABOVE, AND THAT IS
        // THE POINT OF THIS ASSERTION. An earlier version of this comment
        // recorded the opposite as a filed inconsistency: a bare name that would
        // not resolve on the guest PATH exited 125 with `class=cli-error` while
        // `/nope/x` exited 127. Same condition, and the only difference was how
        // the caller SPELLED it -- a property of the command line, not of the
        // failure.
        //
        // ⚠️ AND THE SPLIT WAS BACKWARDS WITH RESPECT TO ITS OWN CONVENTION. The
        // scheme is borrowed from GNU `env`/`chroot`/`timeout`, where 127 is
        // PRIMARILY the PATH-lookup failure -- "command not found" is the shell
        // failing to resolve a bare name. The branch getting the non-PATH code
        // was the one the code was written for.
        Refusal::GuestNotFound,
        &["Could not resolve program", "guest PATH"],
    );

    let temp = tempfile::tempdir().expect("failed to create program fixture directory");
    let non_executable = temp.path().join("non-executable");
    fs::write(&non_executable, "#!/bin/sh\nexit 0\n").expect("failed to write program fixture");

    let output = hermit_command(&["run", "--tmp=/tmp", "--"])
        .arg(&non_executable)
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotExecutable,
        &["is not executable", "chmod +x"],
    );

    let output = hermit_command(&["run", "--tmp=/tmp", "--"])
        .arg(temp.path())
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::GuestNotExecutable,
        &["is a directory", "executable file"],
    );

    let bad_shebang = temp.path().join("bad-shebang");
    fs::write(&bad_shebang, "#!/definitely/missing/interpreter\n").expect("failed to write script");
    let mut permissions = fs::metadata(&bad_shebang)
        .expect("failed to stat script")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&bad_shebang, permissions).expect("failed to make script executable");

    let output = hermit_command(&["run", "--tmp=/tmp", "--"])
        .arg(&bad_shebang)
        .output()
        .expect("failed to run hermit");
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["uses shebang interpreter", "does not exist", "#! line"],
    );
}

#[test]
fn run_rejects_invalid_configuration_without_panicking() {
    let output = hermit(&["run", "--no-virtualize-time", "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["also requires --no-virtualize-metadata", "timestamps"],
    );

    let output = hermit(&["run", "--sched-sticky-random-param=-0.1", "--", "/bin/true"]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["must be between 0 and 1", "received -0.1"],
    );
}

#[test]
fn run_rejects_a_missing_bind_source_before_mounting() {
    let output = hermit(&[
        "run",
        "--bind=/definitely/missing/hermit-test:/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["--bind source", "does not exist", "correct"],
    );

    let output = hermit(&[
        "run",
        "--mount=type=bind,source=/definitely/missing/hermit-test,target=/tmp/input",
        "--",
        "/bin/true",
    ]);
    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &["--mount source", "does not exist", "correct"],
    );
}

#[test]
fn run_reports_denied_ptrace_and_seccomp_capabilities() {
    for (syscall, expected) in [
        (
            libc::SYS_ptrace,
            ["cannot use ptrace", "PTRACE_TRACEME", "--namespace-only"],
        ),
        (
            libc::SYS_seccomp,
            [
                "cannot install",
                "SECCOMP_SET_MODE_FILTER",
                "--namespace-only",
            ],
        ),
    ] {
        let mut command = hermit_command(&[
            "run",
            "--max-timeslice=disabled",
            "--no-virtualize-cpuid",
            "--",
            "/bin/true",
        ]);
        deny_syscall(&mut command, syscall);
        let output = command.output().expect("failed to run restricted hermit");
        assert_hermit_refusal_contains(&output, Refusal::Hermit, &expected);
    }
}

/// The container's virtual clock must keep advancing across `execve`.
///
/// `execve` replaces the process image, not the container, so a guest must
/// never see time restart at the configured epoch after an exec. This is the
/// property hermit#705 was ultimately about: showing the guest a *plausible*
/// epoch is not clock virtualization if the clock rewinds at every image
/// boundary. The guest itself samples the whole trajectory (repeated reads
/// before and after the exec) rather than a single value, because first-sample
/// agreement on a tidy origin is the classic false green here.
///
/// ptrace is the golden reference and keeps one container-wide `GlobalTime`
/// out-of-process, so this is a regression guard for that reference.
#[test]
fn run_ptrace_virtual_clock_advances_across_execve() {
    let program = exec_clock_continuity_guest()
        .to_str()
        .expect("exec-clock-continuity guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "exec-clock-continuity-ok\n");
}

/// The same exec-boundary clock trajectory must also be reproducible, so the
/// continuity above cannot be bought with a nondeterministic clock.
#[test]
fn run_ptrace_virtual_clock_across_execve_is_deterministic() {
    let program = exec_clock_continuity_guest()
        .to_str()
        .expect("exec-clock-continuity guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        program,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains("deterministic"),
        "expected a determinism verdict from --verify:\n{}",
        stderr(&output),
    );
}

/// `--log-file` must resolve on the HOST, exactly like a shell redirect.
///
/// The container mounts a fresh writable /tmp over its root, and tracing is
/// initialized inside the container. Opening the log there resolved `/tmp/x.log`
/// against the GUEST's tmpfs, where the create SUCCEEDED and the file then died with
/// the namespace: exit 0, no log, no warning. A user who asked for a log got success
/// and nothing, which is indistinguishable from a log that was legitimately empty.
/// Measured 2026-08-20; a debugging session was lost to it.
///
/// /tmp is the case that matters because it is both the natural place to put a
/// scratch log and the one directory the container replaces.
#[test]
fn log_file_under_tmp_lands_on_the_host() {
    let directory = tempfile::Builder::new()
        .prefix("hermit_log_file_host_ns_")
        .tempdir_in("/tmp")
        .unwrap();
    let log = directory.path().join("guest.log");

    let output = hermit(&[
        "--log=info",
        "--log-file",
        log.to_str().unwrap(),
        "run",
        "--",
        "/bin/true",
    ]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "unexpected status: {output:?}"
    );
    assert!(
        log.exists(),
        "--log-file under /tmp produced no host file: {output:?}"
    );
    let size = std::fs::metadata(&log).unwrap().len();
    // Non-empty, not merely created: an empty file is the symptom this fixes.
    assert!(
        size > 0,
        "--log-file under /tmp produced an empty host file"
    );
}

/// A log destination that cannot be opened must say so and fail, never exit 0
/// having written nothing. This half needs no policy ruling: silent success is
/// never the right answer to "write my diagnostics here".
#[test]
fn log_file_that_cannot_be_opened_is_refused_by_path() {
    let output = hermit(&[
        "--log=info",
        "--log-file",
        "/nonexistent-root-dir-for-hermit-test/guest.log",
        "run",
        "--",
        "/bin/true",
    ]);

    assert_hermit_refusal_contains(
        &output,
        Refusal::Hermit,
        &[
            "cannot open --log-file",
            "/nonexistent-root-dir-for-hermit-test/guest.log",
        ],
    );
}
#[test]
fn hermit_dap_forwards_remote_settings_to_gdb() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .args(["--gdb", "/bin/echo"])
        .output()
        .expect("failed to run hermit-dap");

    assert!(
        output.status.success(),
        "hermit-dap failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "--quiet --nx --init-eval-command=set debuginfod enabled off \
         --init-eval-command=set sysroot / --interpreter=dap\n"
    );
}

#[test]
fn hermit_dap_reports_a_missing_gdb_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .arg("--gdb")
        .output()
        .expect("failed to run hermit-dap");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--gdb requires a path"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn hermit_dap_help_describes_managed_replay() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .arg("--help")
        .output()
        .expect("failed to run hermit-dap");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--replay ID"), "stdout:\n{stdout}");
    assert!(stdout.contains("stepBack"), "stdout:\n{stdout}");
    assert!(stdout.contains("reverseContinue"), "stdout:\n{stdout}");
}

#[test]
fn hermit_dap_rejects_replay_options_without_replay() {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit-dap"))
        .args(["--data-dir", "/tmp/recordings"])
        .output()
        .expect("failed to run hermit-dap");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--data-dir requires --replay"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A recorded PMU skid overshoot and a guest that exited 1 must be
/// distinguishable from `$?`
/// ALONE, with no stderr parsing.
///
/// ⚠️ THIS IS THE WHOLE POINT AND IT IS EASY TO SATISFY BY ACCIDENT. Asserting
/// only that the refusal arm is 122 would still pass if hermit returned 122 for
/// everything, so both arms are asserted together and the test is named for the
/// DIFFERENCE rather than for either value.
///
/// Measured before the fix: both arms returned 1, with the `HERMIT_TASK_PANIC`
/// marker present on stderr in the panic arm. The information existed and only
/// `$?` could not carry it -- every harness and gate on this project decides
/// pass/fail from exactly that value.
///
/// The overshoot is induced with Reverie's own fault injector rather than a mock,
/// so this exercises the real timer, Tool callback, structural counter,
/// container boundary, receipt writer, and policy-refusal exit.
#[test]
fn skid_overshoot_and_guest_failure_have_different_exit_codes() {
    let _guard = hermit_run_guard();

    // A guest with enough retired conditional branches to reach the timer path;
    // a trivial guest exits before the injected zero skid margin can bite.
    let busy = "awk 'BEGIN{s=0;for(i=0;i<300000;i++)s+=i;print s}'";
    let receipt_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create skid-overshoot receipt directory");
    let receipt_path = receipt_dir.path().join("verification.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&receipt_path);
    append_hermit_args(&mut command, &["--", "/bin/sh", "-c", busy]);
    let overshot = command
        .env("REVERIE_SKID_MARGIN_OVERRIDE", "0")
        .output()
        .expect("failed to run hermit under the skid injector");

    let stderr = String::from_utf8_lossy(&overshot.stderr);
    assert!(
        stderr.contains("HERMIT_SKID_OVERSHOOT"),
        "the skid injector did not induce an overshoot; this test is measuring nothing:\n{stderr}"
    );
    assert!(
        !stderr.contains("HERMIT_TASK_PANIC") && !stderr.contains("panicked at"),
        "a recorded skid overshoot must reach the Tool rather than panic:\n{stderr}"
    );

    let report = VerificationReport::from_current_json_value(
        serde_json::from_slice(&fs::read(&receipt_path).expect("read skid receipt"))
            .expect("parse skid receipt JSON"),
    )
    .expect("read current skid receipt");
    assert_eq!(report.verdict, Verdict::InfrastructureError);
    assert!(!report.verified);
    assert!(!report.bitwise_parity);
    assert!(
        report.comparison.is_some(),
        "both completed runs must be retained"
    );
    assert!(matches!(
        report.infrastructure_error,
        Some(InfrastructureError::SkidOvershoot { count }) if count > 0
    ));

    let guest_failed = hermit(&["run", "--", "/bin/sh", "-c", "exit 1"]);

    let overshoot_code = overshot.status.code();
    let guest_code = guest_failed.status.code();
    assert_ne!(
        overshoot_code, guest_code,
        "an infrastructure refusal and a guest exiting 1 are indistinguishable from $? alone \
         (both {overshoot_code:?}); every gate reading the exit code cannot tell a machine \
         fault from a product failure"
    );
    // Deliberately a literal `1`: this is the GUEST's own chosen status passing
    // through, not hermit's reserved code, so it must NOT track
    // `HERMIT_INTERNAL_FAILURE_EXIT`. If that constant ever became 1 this
    // assertion pair should start failing, and substituting the constant here
    // would hide exactly that.
    assert_eq!(
        guest_code,
        Some(1),
        "the guest's own exit status must pass through unchanged"
    );
    assert_eq!(
        overshoot_code,
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "an understood infrastructure failure should use the policy-refusal status"
    );
}

/// A container child that exits with a status IT DID NOT CHOOSE must be
/// distinguishable from an ordinary CLI error, and neither may be confused with
/// the guest's own exit.
///
/// ⚠️ MEASURED BEFORE THE FIX, at main `b92c2227fc`: all three arms returned 1
/// and emitted NO classification at all, so `$?` and stderr agreed on nothing.
/// The information was never missing — reverie hands hermit a typed
/// `RunError::ExitStatus`, and `with_container` discarded it with
/// `.context(..)?`. This test fails if that discard comes back.
///
/// It deliberately asserts on the TYPED classification rather than on an exit
/// code: every value in `0..=255` is a legal guest status, so no exit code can
/// separate these classes without colliding with some guest.
#[test]
fn container_child_exit_is_distinguishable_from_an_ordinary_cli_error() {
    // (a) The container child dies of a fault `catch_unwind` cannot intercept,
    // so reverie reports a real typed status rather than a reported error.
    let child_exit = hermit_command(&["run", "--strict", "--", "/bin/true"])
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .output()
        .expect("failed to run the fault-injected container child");
    let child_exit_stderr = String::from_utf8_lossy(&child_exit.stderr).into_owned();

    // (c) An ordinary CLI failure: hermit cannot open the requested log file.
    let cli_error = hermit_command(&[
        "--log-file",
        "/nonexistent-directory-for-hermit-cli-test/log",
        "run",
        "--strict",
        "--",
        "/bin/true",
    ])
    .output()
    .expect("failed to run the unwritable-log-path case");
    let cli_error_stderr = String::from_utf8_lossy(&cli_error.stderr).into_owned();

    assert!(
        child_exit_stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a container child that exited with an unchosen status must be classified as \
         such\nstderr:\n{child_exit_stderr}"
    );
    assert!(
        cli_error_stderr.contains("HERMIT_INTERNAL_FAILURE class=cli-error"),
        "an ordinary CLI error must be classified as such\nstderr:\n{cli_error_stderr}"
    );
    // The whole point: the two must not read the same.
    assert!(
        !cli_error_stderr.contains("class=container-child-exit"),
        "an ordinary CLI error must NOT be reported as a container child exit\nstderr:\n\
         {cli_error_stderr}"
    );

    // The typed status must survive, not just the class. This is what
    // `.context(..)?` used to destroy.
    assert!(
        child_exit_stderr.contains("status=Signaled(SIGSEGV"),
        "the child's typed status must survive to the classification\nstderr:\n\
         {child_exit_stderr}"
    );

    // (a2) A container-child panic that IS caught still reports the tracer
    // breaking, not the CLI refusing. This is the third flattening: the panic
    // crosses a process boundary through `SerializableError`, which carries
    // only strings, so without the `kind` discriminant it arrives
    // indistinguishable from an ordinary reported error.
    let child_panic = hermit_command(&["run", "--strict", "--", "/bin/true"])
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "panic")
        .output()
        .expect("failed to run the panic-injected container child");
    let child_panic_stderr = String::from_utf8_lossy(&child_panic.stderr).into_owned();
    assert!(
        child_panic_stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-panic"),
        "a CAUGHT container-child panic must be classified as a panic, not as a CLI          error
stderr:
{child_panic_stderr}"
    );
    assert!(
        !child_panic_stderr.contains("class=cli-error"),
        "a caught container-child panic must NOT read as an ordinary CLI          error
stderr:
{child_panic_stderr}"
    );

    // (b) The guest's own exit is NOT an internal failure and must carry no
    // marker at all — "hermit's exit IS the guest's exit" stays intact.
    let guest_exit = hermit_command(&["run", "--strict", "--", "/bin/false"])
        .output()
        .expect("failed to run the guest-exit case");
    assert_eq!(
        guest_exit.status.code(),
        Some(1),
        "a guest exiting 1 must still surface as 1"
    );
    assert!(
        !String::from_utf8_lossy(&guest_exit.stderr).contains("HERMIT_INTERNAL_FAILURE"),
        "the guest's own exit must not be classified as a hermit-internal failure"
    );
}

/// A guest must not be able to escape the deterministic pipe-capacity pin, and
/// must not be able to read the host's ceiling.
///
/// ⚠️ THIS IS THE WIRING TEST, AND IT EXISTS BECAUSE THE UNIT TESTS DO NOT COVER
/// IT. `pipe_capacity_request` is unit-tested in `detcore`, but deleting the
/// `F_SETPIPE_SZ` arm from `handle_fcntl` entirely leaves every one of those
/// unit tests green while restoring the escape in full — measured. Only an
/// end-to-end run catches that, so this asserts through a real guest.
///
/// ⚠️ AND IT IS A DETERMINISM TEST, NOT A POLICY PREFERENCE. Before the fix the
/// request was forwarded to the host, so the guest-visible answer was decided by
/// `/proc/sys/fs/pipe-max-size`: on this host (ceiling 1048576) the guest got
/// success and a 1 MiB pipe; on a host with the common hardened 65536 the same
/// guest got EPERM and kept 8192. Same binary, same `--strict`, different answer
/// per host.
#[test]
fn a_guest_cannot_escape_the_deterministic_pipe_capacity_pin() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pipe-capacity-pin");
    fs::create_dir_all(&build_root).expect("failed to create the pipe-capacity build root");
    let source = build_root.join("pipecap.c");
    fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
int main(void) {
    int fd[2];
    if (pipe(fd)) return 1;
    printf("initial=%d\n", fcntl(fd[0], F_GETPIPE_SZ));
    printf("grow=%d\n", fcntl(fd[0], F_SETPIPE_SZ, 1 << 20));
    printf("after=%d\n", fcntl(fd[0], F_GETPIPE_SZ));
    char buf[64] = {0};
    FILE *f = fopen("/proc/sys/fs/pipe-max-size", "r");
    if (f && fgets(buf, sizeof buf, f)) printf("ceiling=%s", buf);
    if (f) fclose(f);
    return 0;
}
"#,
    )
    .expect("failed to write the pipe-capacity guest");
    let guest = build_root.join("pipecap");
    let built = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&guest)
        .arg(&source)
        .output()
        .expect("failed to invoke cc for the pipe-capacity guest");
    assert!(
        built.status.success(),
        "failed to build the pipe-capacity guest:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    let output = hermit_command(&["run", "--strict", "--"])
        .arg(&guest)
        .output()
        .expect("failed to run the pipe-capacity guest under hermit");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();

    assert!(
        stdout.contains("initial=8192"),
        "the creation-time pin must still apply\nstdout:\n{stdout}"
    );
    // -1 is EPERM: exactly what Linux returns when the request exceeds the
    // ceiling, except the ceiling is now one Detcore owns.
    assert!(
        stdout.contains("grow=-1"),
        "a guest must not be able to grow a pipe past the pinned capacity\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("after=8192"),
        "the pinned capacity must survive the guest's own F_SETPIPE_SZ\nstdout:\n{stdout}"
    );
    assert!(
        stdout.contains("ceiling=8192"),
        "the guest must read the enforced ceiling, not the host's\nstdout:\n{stdout}"
    );
}

/// A guest-side fact must not be reported as a hermit-internal failure — and a
/// genuine hermit-internal failure must keep saying so.
///
/// ⚠️ BOTH ARMS ARE PINNED DELIBERATELY. A change that returned 127 for
/// everything would be WORSE than the behaviour it replaces: today the two are
/// equally wrong, and that change would make the common case (a typo in a guest
/// path) silently claim the rarer one. A test asserting only the new code would
/// pass on exactly that broken change, so the 125 arm has to be load-bearing --
/// which means it has to reach `failure_exit_code`, and as first written it did
/// not. See the comment on that arm below for the measurement.
///
/// Measured before the fix, at main `b97a4bc3a4`: `/no/such/program` and an
/// unwritable `--log-file` both returned 125 with the same class.
#[test]
fn a_guest_side_fault_is_not_reported_as_a_hermit_internal_failure() {
    let missing = hermit_command(&[
        "run",
        "--strict",
        "--",
        "/no/such/program-for-hermit-cli-test",
    ])
    .output()
    .expect("failed to run the missing-program case");
    let missing_stderr = String::from_utf8_lossy(&missing.stderr).into_owned();
    assert_eq!(
        missing.status.code(),
        Some(127),
        "a missing program is command-not-found, the GNU convention 125 came from\nstderr:\n{missing_stderr}"
    );
    assert!(
        missing_stderr.contains("class=guest-program-not-found"),
        "the class must name a guest-side fault\nstderr:\n{missing_stderr}"
    );

    // Present but not executable: 126, distinct from both 127 and 125.
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("guest-fault-not-executable");
    fs::create_dir_all(&build_root).expect("failed to create the not-executable build root");
    let unexecutable = build_root.join("not-executable");
    fs::write(&unexecutable, b"\x7fELF not really\n").expect("failed to write the file");
    fs::set_permissions(&unexecutable, fs::Permissions::from_mode(0o644))
        .expect("failed to drop the execute bit");
    let denied = hermit_command(&["run", "--strict", "--"])
        .arg(&unexecutable)
        .output()
        .expect("failed to run the not-executable case");
    let denied_stderr = String::from_utf8_lossy(&denied.stderr).into_owned();
    assert_eq!(
        denied.status.code(),
        Some(126),
        "found-but-not-executable is 126, not 127\nstderr:\n{denied_stderr}"
    );

    // ⚠️ THE ARM THAT STOPS THIS BECOMING A BLANKET 127, AND IT HAS TO REACH THE
    // MAPPING TO BE THAT ARM. An injected container-child fault is a genuine
    // hermit-internal failure that travels the ordinary route: `main` -> `Err` ->
    // `failure_exit_code`, which is the function a blanket 127 would live in.
    //
    // ⚠️ IT WAS WRITTEN WITH AN UNWRITABLE `--log-file` AND THAT DID NOT WORK.
    // `--log-file` fails in `open_log_file`, BEFORE the command is dispatched, and
    // that path raises the 125 constant directly without consulting
    // `failure_exit_code` at all. Measured on this branch: with `None => 127`
    // substituted for `None => HERMIT_INTERNAL_FAILURE_EXIT`, the test as
    // originally written still reported `1 passed` -- the arm named for the
    // mutation did not see it. Through the fault-injected route the same mutation
    // fails here.
    let internal = hermit_command(&["run", "--strict", "--", "/bin/true"])
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .output()
        .expect("failed to run the hermit-internal case");
    let internal_stderr = String::from_utf8_lossy(&internal.stderr).into_owned();
    assert_eq!(
        internal.status.code(),
        Some(125),
        "a hermit-internal failure must stay 125\nstderr:\n{internal_stderr}"
    );
    assert!(
        internal_stderr.contains("class=container-child-exit"),
        "this arm is only load-bearing if it goes through the mapping, which needs a \
         failure that reaches it\nstderr:\n{internal_stderr}"
    );
    assert!(
        !internal_stderr.contains("guest-program"),
        "a hermit-internal failure must not be classed as guest-side\nstderr:\n{internal_stderr}"
    );

    // The pre-dispatch path keeps its own answer, which is a SEPARATE fact: an
    // unwritable `--log-file` never reaches `failure_exit_code`, so this pins the
    // constant in `main` rather than the mapping. Kept, and no longer described as
    // the arm that guards the mapping.
    let pre_dispatch = hermit_command(&[
        "--log-file",
        "/nonexistent-directory-for-hermit-cli-test/log",
        "run",
        "--strict",
        "--",
        "/bin/true",
    ])
    .output()
    .expect("failed to run the unwritable-log-file case");
    let pre_dispatch_stderr = String::from_utf8_lossy(&pre_dispatch.stderr).into_owned();
    assert_eq!(
        pre_dispatch.status.code(),
        Some(125),
        "a failure before dispatch must still be 125\nstderr:\n{pre_dispatch_stderr}"
    );
    // ⚠️ THE NEGATIVE CLASS ASSERTION BELONGS ON THIS PATH TOO, NOT ONLY ON THE
    // injected-fault arm above. The exit code alone does not distinguish a
    // pre-dispatch failure from a guest-side one: both can be 125 while the
    // classification is wrong. This arm previously carried it, and it was lost
    // when the assertion migrated to the injected-segv invocation -- the text
    // survived branch-wide while its coverage of THIS path did not.
    assert!(
        !pre_dispatch_stderr.contains("guest-program"),
        "a pre-dispatch failure must not be classified guest-side\nstderr:\n{pre_dispatch_stderr}"
    );

    // And the guest's own exit is untouched by any of this.
    let guest = hermit_command(&["run", "--strict", "--", "/bin/false"])
        .output()
        .expect("failed to run the guest-exit case");
    assert_eq!(
        guest.status.code(),
        Some(1),
        "a guest exiting 1 still exits 1"
    );
}

/// The REPLAY-stage classification site of `record --verify-with-gdbex`, the
/// sixth and last of them.
///
/// ⚠️ THIS SITE WAS TWICE DECLARED UNTESTABLE BY THIS BRANCH AND IS NEITHER.
/// First the claim was that no fault could reach a replay stage at all; then,
/// after that was disproved, that reaching THIS one wedges the process. Both
/// were wrong, and `agent(codex-rev-2628)` supplied the missing step each time.
/// THE HANG THAT BLOCKED THIS TEST WAS THE PROBE'S: killing the replay gdbserver
/// while leaving GDB ALIVE leaves GDB holding the captured stderr pipe open, so
/// the reader never sees EOF. Making GDB `quit` after the kill returns promptly
/// -- rc=125 in about a second.
///
/// ⚠️ THAT IS NOT THE SAME AS SAYING HERMIT HAS NO BUG HERE, AND AN EARLIER
/// VERSION OF THIS COMMENT SAID EXACTLY THAT. `agent(hermit-dbgrev7)` measured
/// the two cases apart: killing the replay CONTAINER CHILD makes hermit exit in
/// 0.050s, but killing GDB makes hermit NEVER RETURN -- 188s, killed by hand.
/// `--verify-with-gdbex` waits forever when GDB dies before completing the
/// connection, and `--record-timeout` arms the recording only. That defect is
/// real, is filed, and is out of scope for a test-only change. Correcting the
/// CAUSE of a hang is not the same as retracting the hang.
///
/// ⚠️ SO `--verify-with-gdbex` IS ITSELF THE CONTROL HOOK. The CLI already
/// accepts GDB commands, GDB runs while the replay container is alive, and GDB
/// can run Python. No production change, no new injector, and no external
/// process supervision: the kill is issued from inside the run being tested and
/// the same `-ex` sequence then shuts GDB down.
#[test]
fn record_classifies_a_gdbserver_replay_stage_container_child_failure() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");
    let script = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kill-gdbserver-replay-peer.py");
    // Finds the OUTER `hermit record` by walking GDB's own ancestry, then kills
    // that process's non-ancestor children -- the replay container. Written from
    // inside the run rather than supervised from outside, so the test does not
    // have to guess a pid or race the fork.
    fs::write(
        &script,
        r#"import os, signal

def parent_of(pid):
    try:
        return int(open("/proc/%d/stat" % pid).read().rsplit(")", 1)[-1].split()[1])
    except (OSError, IndexError, ValueError):
        return None

def cmdline(pid):
    try:
        with open("/proc/%d/cmdline" % pid, "rb") as handle:
            return handle.read().replace(b"\0", b" ").decode("utf-8", "replace")
    except OSError:
        return ""

ancestors = []
current = os.getpid()
for _ in range(30):
    ancestors.append(current)
    current = parent_of(current)
    if current is None or current <= 1:
        break

outer = None
for candidate in ancestors:
    text = cmdline(candidate)
    if "hermit" in text and " record " in text:
        outer = candidate
        break

if outer is not None:
    known = set(ancestors)
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        if pid in known:
            continue
        if parent_of(pid) == outer:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass
"#,
    )
    .expect("failed to write the gdb kill script");

    let gdb_commands = format!("pi exec(open(\"{}\").read());quit", script.display());
    let mut child = hermit_command(&[
        "record",
        "--verify-with-gdbex",
        // `;` is the -ex delimiter: kill the replay container, then shut GDB
        // down. ⚠️ THE `quit` IS LOAD-BEARING -- without it GDB stays alive
        // holding this test's stderr pipe and the run never appears to end.
        gdb_commands.as_str(),
        "--",
        "/bin/true",
    ])
    .env("HERMIT_DATA_DIR", data_dir.path())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("failed to spawn the gdbserver verify");

    // ⚠️ A DEADLINE, SO A HANG IS A FAILURE RATHER THAN A STUCK SUITE. This
    // drives hermit into an error path on purpose and an earlier version of the
    // probe really did wedge; a test that can stall CI is not an acceptable
    // price for a covered call site.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut timed_out = false;
    loop {
        match child.try_wait().expect("failed to poll hermit") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    let output = child
        .wait_with_output()
        .expect("failed to collect the gdbserver verify output");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !timed_out,
        "hermit did not exit within 120s after its gdbserver replay container was killed\n\
         stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a gdbserver-replay container child killed by a signal it did not choose must be \
         classified as a container-child exit, not as a CLI error\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("status=Signaled(SIGKILL"),
        "the typed status must survive to the classification\nstderr:\n{stderr}"
    );
}

/// The REPLAY-stage classification site of `record --verify`, which the fault
/// injector cannot reach and which was therefore left unpinned.
///
/// ⚠️ THIS EXISTS BECAUSE "UNREACHABLE" WAS WRONG. The sibling test below covers
/// four of `record`'s six `run_guarded` sites, and its doc comment used to
/// assert that the remaining two -- the replay stages of `--verify` and
/// `--verify-with-gdbex` -- could not be reached without teaching
/// `inject_test_fault` to name a stage. `agent(codex-rev-2628)` disproved that
/// in review, and the idea is the one this file was missing: THE FAULT DOES NOT
/// HAVE TO COME FROM INSIDE THE CHILD. Waiting for `:: Replaying...` on stderr
/// and killing the container child from outside reaches the site, with no
/// change to production code and no new injector.
///
/// ⚠️ AND `SIGKILL` IS WHAT MAKES IT A CONTAINER-CHILD EXIT rather than a
/// reported error: it is a status the child did not choose and no handler can
/// intercept, so reverie hands hermit a typed `RunError::ExitStatus` -- exactly
/// the shape `.classified()` preserves and `.context(..)??` destroyed.
///
/// ⚠️ THE `--verify-with-gdbex` REPLAY SITE IS COVERED BY THE TEST ABOVE, AND AN
/// EARLIER VERSION OF THIS COMMENT BLAMED HERMIT FOR NOT COVERING IT. It said
/// hermit hangs when that replay container is killed, on the evidence of three
/// invocations that sat for 25 minutes and an in-suite probe that timed out at
/// 300s. The attribution was wrong twice over, and the second correction is the
/// one this comment used to get backwards:
///
///   1. Killing the container child does NOT hang hermit -- that path exits in
///      0.050s. What stalled the original probe was leaving GDB alive holding
///      the captured stderr pipe, so the reader never saw EOF. Issuing `quit`
///      after the kill returns in about a second, which is what the test above
///      does.
///
///   2. ⚠️ BUT HERMIT DOES HAVE A BUG HERE, AND THIS COMMENT USED TO SAY IT DOES
///      NOT. When GDB exits or dies WITHOUT completing its connection, the
///      container child blocks forever in an unbounded
///      `listener.accept().await` (`reverie-ptrace/src/gdbstub/server.rs`),
///      waiting for a client that is already gone. Nothing bounds it;
///      `--record-timeout` arms the recording only. Reproduced with no kill and
///      no signal, by putting a `gdb` on PATH that exits 0 without connecting.
///      Filed as `hermit_never_exits_when`.
///
/// ⚠️ THE TEST ABOVE CANNOT WITNESS THAT, AND MUST NOT BE READ AS EVIDENCE
/// AGAINST IT. It supplies `-ex quit`, so its GDB connects successfully and the
/// accept is satisfied long before the kill. It will keep passing after the hang
/// is fixed, which is correct for a coverage test and is exactly why the hang
/// needs its own.
#[test]
fn record_classifies_a_replay_stage_container_child_failure() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");

    // The direct children of a process, found by scanning `/proc/<pid>/stat`.
    //
    // ⚠️ NOT `/proc/<pid>/task/<tid>/children`, WHICH IS EMPTY HERE. That file is
    // the obvious answer and it is the wrong one: hermit's container child is put
    // in a NEW PID NAMESPACE, and the kernel omits such a child from the
    // `children` list of a reader in the parent namespace. Measured at the same
    // instant, with the child plainly alive: `children` across every task read
    // `[]` while a ppid scan found it. The first version of this test used
    // `children`, found nothing on five runs out of five, and reported "no
    // container child appeared" about a child that was there.
    fn children_of(pid: u32) -> Vec<u32> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let Some(candidate) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(format!("/proc/{candidate}/stat")) else {
                continue;
            };
            // `comm` is parenthesised and may itself contain spaces, so the
            // fields after it are taken from the LAST ')' rather than by
            // splitting the whole line. ppid is the second field after that.
            let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let (_state, parent) = (fields.next(), fields.next());
            if parent.and_then(|value| value.parse::<u32>().ok()) == Some(pid) {
                found.push(candidate);
            }
        }
        found
    }

    let mut child = hermit_command(&["record", "--verify", "--", "/bin/sleep", "5"])
        .env("HERMIT_DATA_DIR", data_dir.path())
        // ⚠️ THE GUEST ARGUMENT BUYS NO HEADROOM AND AN EARLIER COMMENT HERE
        // CLAIMED IT DID. Guest time is virtualized, so `sleep 1`, `sleep 5` and
        // `sleep 60` all give the same ~0.4s window and `/bin/true` gives 0.2s --
        // measured by `agent(hermit-dbgrev7)`. What makes the kill land is the
        // poll loop below, which waits for the replay container to appear rather
        // than assuming it is already there; the guest is incidental.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn the fault-injected verify");
    let pid = child.id();
    let stderr = child.stderr.take().expect("hermit stderr must be piped");

    // ⚠️ A DEADLINE, SO A HANG IS A FAILURE RATHER THAN A STUCK SUITE. This test
    // drives hermit into an error path on purpose, and hermit really does hang
    // on a neighbouring one: under `--verify-with-gdbex`, GDB dying before it
    // connects leaves hermit waiting forever. A test that can wedge CI is not an
    // acceptable price for a covered call site.
    let deadline = Instant::now() + Duration::from_secs(120);
    let captured = Arc::new(Mutex::new(String::new()));
    let reader_captured = Arc::clone(&captured);
    let reader = thread::spawn(move || {
        let mut killed = false;
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            {
                let mut buffer = reader_captured.lock().expect("stderr buffer poisoned");
                buffer.push_str(&line);
                buffer.push('\n');
            }
            if !killed && line.contains("Replaying...") {
                // The replay container is forked after the announcement, so poll
                // rather than assume it is already there.
                let until = Instant::now() + Duration::from_secs(10);
                while Instant::now() < until {
                    if let Some(target) = children_of(pid).first().copied() {
                        let _ = Command::new("kill")
                            .args(["-9", &target.to_string()])
                            .status();
                        killed = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        killed
    });

    let mut timed_out = false;
    loop {
        match child.try_wait().expect("failed to poll hermit") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                timed_out = true;
                break;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }
    let _ = child.wait();
    let killed = reader.join().unwrap_or(false);
    let stderr = captured.lock().expect("stderr buffer poisoned").clone();

    assert!(
        !timed_out,
        "hermit did not exit within 120s after its replay container child was killed; a \
         hanging error path is a defect, not a slow test\nstderr:\n{stderr}"
    );
    // ⚠️ REFUSES RATHER THAN PASSES ON AN UNFIRED PROBE. If the kill never
    // landed, this test proves nothing and must say so instead of going green.
    assert!(
        killed,
        "no container child was signalled after `Replaying...`, so this test never \
         exercised the replay stage it exists for\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit"),
        "a replay-stage container child killed by a signal it did not choose must be \
         classified as a container-child exit, not as a CLI error\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("status=Signaled(SIGKILL"),
        "the typed status must survive to the classification\nstderr:\n{stderr}"
    );
}

/// `hermit record` must classify a container-child failure the same way
/// `hermit run` does -- IN EVERY SPELLING THAT ENTERS A CONTAINER, not just the
/// bare one.
///
/// ⚠️ THIS IS THE CALL SITE THE FIX ORIGINALLY MISSED. `record` calls
/// `RunGuarded::run_guarded` directly at six sites in `record_start.rs` and
/// never goes through `with_container`, so the `.context(..)??` discard survived
/// there and BOTH container-child classes surfaced as `class=cli-error`. A wrong
/// machine-readable class is worse than none, because a gate believes it.
///
/// ⚠️ AND ONE SPELLING IS NOT SIX. The first version of this test drove only
/// `record -- prog`, which reaches exactly one of the six sites; adversarial
/// review pointed out that the other five could be reverted to the old
/// flattening with this test still green, so the "every spelling" claim in the
/// paragraph above was not the claim being tested. Four of the six are now
/// driven, one per entry point that can be reached with the guest's FIRST
/// container:
///
/// | spelling | site |
/// | --- | --- |
/// | `record -- prog` | `main`, no deadline |
/// | `record --record-timeout N -- prog` | `main`, deadline armed |
/// | `record --verify -- prog` | `record_verify`, record stage |
/// | `record --verify-with-gdbex ... -- prog` | `record_verify_debug`, record stage |
///
/// ⚠️ THE REMAINING TWO ARE THE REPLAY STAGES OF THOSE LAST TWO, AND THIS
/// INJECTOR CANNOT REACH THEM: `inject_test_fault` is a process-local
/// environment check with no notion of which stage it is in, so with the
/// variable set the RECORD stage faults first and the replay stage is never
/// entered.
///
/// ⚠️ THAT IS A LIMIT OF THIS INJECTOR AND NOT OF TESTING, AND AN EARLIER
/// VERSION OF THIS COMMENT SAID OTHERWISE. It claimed the two sites could not be
/// reached without a stage-aware injector, and `agent(codex-rev-2628)` disproved
/// it in review: the fault does not have to come from inside the child.
/// `record_classifies_a_replay_stage_container_child_failure` above pins `:535`
/// by killing the container child from outside after `:: Replaying...`, and
/// `record_classifies_a_gdbserver_replay_stage_container_child_failure` pins
/// `:660` by killing from inside GDB and then quitting it. All six sites are
/// covered; this test carries four of them.
#[test]
fn record_classifies_a_container_child_failure_the_same_way_run_does() {
    let data_dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a recording dir");
    let case = |fault: &str, extra: &[&str]| -> String {
        let mut args = vec!["record"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "/bin/true"]);
        let output = hermit_command(&args)
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", fault)
            .env("HERMIT_DATA_DIR", data_dir.path())
            .output()
            .unwrap_or_else(|error| panic!("failed to run {args:?}: {error}"));
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    // One entry point per row of the table above. Each is asserted for BOTH
    // classes, because the two travel different paths out of the child: an
    // unchosen exit status is observed by reverie, a caught panic is reported
    // through `SerializableError`'s discriminant.
    let spellings: [(&str, &[&str]); 4] = [
        ("bare", &[]),
        ("with a recording deadline", &["--record-timeout", "600"]),
        ("with --verify", &["--verify"]),
        ("with --verify-with-gdbex", &["--verify-with-gdbex", "quit"]),
    ];

    // ⚠️ EVERY SPELLING IS DRIVEN AND EVERY FAILURE IS COLLECTED, RATHER THAN
    // ASSERTED ONE AT A TIME. An `assert!` inside this loop aborts at the FIRST
    // failing spelling, so a run in which one call site regressed could never
    // show that the other three were unaffected -- and "each spelling reaches
    // one site and no other" is precisely the property this test is here to
    // carry. Collecting turns one revert into a one-line proof of the mapping:
    // exactly one spelling fails and the report names the three that did not.
    let mut failures: Vec<String> = Vec::new();
    for (name, extra) in spellings {
        let segv = case("segv", extra);
        if !segv.contains("HERMIT_INTERNAL_FAILURE class=container-child-exit") {
            failures.push(format!(
                "  [{name}] a record-path container child that exited with an unchosen \
                 status was not classified as such\n    stderr: {}",
                segv.replace('\n', " | ")
            ));
        }

        let panicked = case("panic", extra);
        if !panicked.contains("HERMIT_INTERNAL_FAILURE class=container-child-panic") {
            failures.push(format!(
                "  [{name}] a record-path CAUGHT container-child panic was not classified \
                 as a panic\n    stderr: {}",
                panicked.replace('\n', " | ")
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} record spelling/fault combinations misclassified a container-child \
         failure. The combinations NOT listed here passed in this same run, which is what \
         makes this a mapping rather than an aggregate:\n{}",
        failures.len(),
        spellings.len() * 2,
        failures.join("\n")
    );
}

/// Every `record` container site classifies a child fault, addressed BY NAME.
///
/// ⚠️ WHY A NAME AND NOT AN OCCURRENCE INDEX. `record` enters a container at six
/// `run_guarded` sites. With only `HERMIT_TEST_CONTAINER_CHILD_FAULT` set, the
/// FIRST child to run faults and every later stage is never entered, so two of the
/// six -- the replay stages of `--verify` and `--verify-with-gdbex` -- could not be
/// reached by any test and their classification was asserted rather than measured.
/// An occurrence index would aim at them but is positional: it retargets silently
/// the moment a site is added, removed or reordered, and the test keeps passing
/// while pointing somewhere else. A process-local counter does not work at all,
/// because each `run_guarded` forks a fresh child and the injector runs in the
/// CHILD, so a static counter resets every time. The label is identity, which is
/// the thing whose absence made these sites untestable.
#[test]
fn every_record_container_site_classifies_a_child_fault_by_name() {
    let data_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("failed to create a data dir");

    let case = |site: &str, fault: &str, extra: &[&str]| -> String {
        let mut args = vec!["record"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "/bin/true"]);
        let output = hermit_command(&args)
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", fault)
            .env("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE", site)
            .env("HERMIT_DATA_DIR", data_dir.path())
            .output()
            .unwrap_or_else(|error| panic!("failed to run {args:?} for site {site}: {error}"));
        String::from_utf8_lossy(&output.stderr).into_owned()
    };

    for (site, extra) in RECORD_FAULT_SITES {
        for (fault, class) in [
            ("segv", "container-child-exit"),
            ("panic", "container-child-panic"),
        ] {
            let stderr = case(site, fault, extra);
            assert!(
                stderr.contains(&format!("HERMIT_INTERNAL_FAILURE class={class}")),
                "site {site} under an injected {fault} must be classified as {class}, \
                 not folded into a CLI error\nstderr:\n{stderr}"
            );
        }
    }
}

/// The control WITHOUT WHICH THE TEST ABOVE PROVES NOTHING.
///
/// If the site filter were ignored -- the injector faulting on every container as it
/// does today -- every row above would still pass, because the first child would
/// fault and produce the expected class. Naming a site that does not exist must
/// therefore fault NOTHING: the recording completes and no internal-failure class is
/// printed. That is what distinguishes "the fault was aimed" from "the fault always
/// fires".
#[test]
fn a_fault_aimed_at_no_existing_site_fires_nowhere() {
    let data_dir =
        tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("failed to create a data dir");
    let output = hermit_command(&["record", "--", "/bin/true"])
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "segv")
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE", "no.such.site")
        .env("HERMIT_DATA_DIR", data_dir.path())
        .output()
        .expect("failed to run the unaimed fault injection");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("HERMIT_INTERNAL_FAILURE"),
        "a fault aimed at a site that does not exist must fire nowhere; firing anyway \
         means the site filter is not consulted and the by-name test above is vacuous\
         \nstderr:\n{stderr}"
    );
}

/// One row per `run_guarded_at` site in `record_start.rs`, with a spelling that
/// reaches it. Kept at module scope so
/// `no_container_site_is_unreachable_by_the_fault_injector` can hold the source
/// to it.
const RECORD_FAULT_SITES: [(&str, &[&str]); 6] = [
    ("record.main", &[]),
    ("record.main.deadline", &["--record-timeout", "600"]),
    ("record_verify.record", &["--verify"]),
    ("record_verify.replay", &["--verify"]),
    (
        "record_verify_debug.record",
        &["--verify-with-gdbex", "quit"],
    ),
    (
        "record_verify_debug.replay",
        &["--verify-with-gdbex", "quit"],
    ),
];

/// Sites that exist but are deliberately not driven by the `record` table above,
/// each with the reason. An entry here is a DECLARATION, not an exemption from
/// thought: it says a different test already reaches this site.
const FAULT_SITES_DRIVEN_ELSEWHERE: [(&str, &str); 1] = [(
    "with_container",
    "the `run` path; covered by the existing run-mode fault-injection tests",
)];

/// ⚠️ A CLASSIFICATION SITE CANNOT SILENTLY OPT OUT OF BEING ADDRESSABLE.
///
/// This is the invariant made STRUCTURAL rather than left as a convention. Two
/// sites -- the replay stages -- were untestable for as long as they existed, and
/// nothing said so; they were discovered by a human reading the code. The failure
/// mode is not that a test broke, it is that no test could ever have existed, and
/// silence is indistinguishable from coverage.
///
/// So: every `run_guarded_at("...")` label in the sources is extracted here and
/// must appear either in [`RECORD_FAULT_SITES`] or in
/// [`FAULT_SITES_DRIVEN_ELSEWHERE`] with a reason. A site added without a row
/// fails THIS test by name, at the moment it is added, rather than being
/// untestable by default and noticed years later.
#[test]
fn no_container_site_is_unreachable_by_the_fault_injector() {
    const SOURCES: [(&str, &str); 2] = [
        (
            "record_start.rs",
            include_str!("../src/bin/hermit/record_start.rs"),
        ),
        (
            "container.rs",
            include_str!("../src/bin/hermit/container.rs"),
        ),
    ];

    let mut declared: Vec<(String, String)> = Vec::new();
    for (file, text) in SOURCES {
        for (needle, close) in [("run_guarded_at(\"", '"'), ("inject_test_fault(\"", '"')] {
            let mut rest = text;
            while let Some(at) = rest.find(needle) {
                rest = &rest[at + needle.len()..];
                if let Some(end) = rest.find(close) {
                    declared.push((file.to_string(), rest[..end].to_string()));
                }
            }
        }
    }
    assert!(
        !declared.is_empty(),
        "extracted zero site labels from the sources; this test would pass \
         vacuously and prove nothing about coverage"
    );

    let covered: Vec<&str> = RECORD_FAULT_SITES
        .iter()
        .map(|(site, _)| *site)
        .chain(FAULT_SITES_DRIVEN_ELSEWHERE.iter().map(|(site, _)| *site))
        .collect();

    let mut unreachable: Vec<String> = declared
        .iter()
        .filter(|(_, site)| !covered.contains(&site.as_str()))
        .map(|(file, site)| format!("{file}: {site}"))
        .collect();
    unreachable.sort();
    unreachable.dedup();
    assert!(
        unreachable.is_empty(),
        "these container sites can be faulted but no test aims at them, so their \
         classification would be asserted rather than measured -- add a row to \
         RECORD_FAULT_SITES, or to FAULT_SITES_DRIVEN_ELSEWHERE with the test that \
         covers it:\n  {}",
        unreachable.join("\n  ")
    );

    // And the other direction: a row naming a site that no longer exists is a test
    // aimed at nothing, which passes while covering less than it claims.
    let existing: Vec<&str> = declared.iter().map(|(_, s)| s.as_str()).collect();
    let stale: Vec<&str> = covered
        .iter()
        .copied()
        .filter(|site| !existing.contains(site))
        .collect();
    assert!(
        stale.is_empty(),
        "these rows name a site that no longer exists in the sources, so they aim \
         at nothing: {stale:?}"
    );
}

/// A guest that exits with one of hermit's own reserved statuses must still be
/// reported as the guest, not as hermit.
///
/// ⚠️ THIS IS THE ASSERTION hermit#2659 SHIPPED WITHOUT. That PR introduced
/// `HERMIT_POLICY_REFUSAL_EXIT` (122) and its doc claimed, correctly, that an
/// ordinary guest exit does not reach the new match arm -- but it added zero test
/// functions, so the claim was carried by prose. A reviewer executed it by hand
/// once at one head; nothing re-executes it. If the refusal arm ever moved above
/// the `Ok(Ok(..))` path, a guest legitimately exiting 122 would be reported as a
/// policy refusal -- the exact inverse of the defect 122 was introduced to fix,
/// and silent, because the number the operator sees would be unchanged.
///
/// ⚠️ AND 130 IS NOW THE SAME SHAPE. `sigint_instakill` reports `128 + SIGINT`, so
/// 130 acquired a hermit meaning too and needs the identical guarantee.
///
/// The discriminator is the MARKER, never the code: every value in `0..=255` is a
/// legal guest status, so a bare code cannot separate these and a test asserting
/// only the code would pass while the defect was live.
#[test]
fn a_guest_exiting_a_reserved_status_is_not_reported_as_hermit() {
    // 122 and 130 are the reserved values under test; 0/1/7 are controls that
    // must behave identically, so a failure says "the reserved ones are special"
    // rather than "exit codes are broken generally".
    for code in [
        0,
        1,
        7,
        detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
        125,
        detcore_model::HERMIT_SIGINT_DEATH_EXIT,
    ] {
        let shell_command = format!("exit {code}");
        let run = hermit_command(&["run", "--", "/bin/sh", "-c", &shell_command])
            .output()
            .unwrap_or_else(|e| panic!("failed to run the guest exiting {code}: {e}"));
        let stderr = String::from_utf8_lossy(&run.stderr).into_owned();

        assert_eq!(
            run.status.code(),
            Some(code),
            "a guest exiting {code} must report {code}\nstderr:\n{stderr}"
        );
        // The marker is the exclusive channel. Its ABSENCE is what says the status
        // came from the guest.
        assert!(
            !stderr.contains("HERMIT_POLICY_REFUSAL"),
            "a guest exiting {code} must not be reported as a hermit policy \
             refusal\nstderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("HERMIT_SIGNAL_DEATH"),
            "a guest exiting {code} must not be reported as a signal death\n\
             stderr:\n{stderr}"
        );
        assert!(
            !stderr.contains("HERMIT_INTERNAL_FAILURE"),
            "a guest exiting {code} must not be reported as a hermit internal \
             failure\nstderr:\n{stderr}"
        );
    }
}

/// A container child killed by a signal reports `128 + signo` with the
/// signal-death marker, NOT a policy refusal and NOT an internal failure.
///
/// ⚠️ THIS IS THE END-TO-END HALF THAT TWO EARLIER ATTEMPTS FAILED TO MEASURE,
/// and both failures are recorded here so a third attempt does not repeat them.
/// `/bin/sleep 30` proves nothing: hermit VIRTUALIZES time, so the sleep retires
/// in ~40ms of wall clock (measured: 40ms under hermit vs 4004ms bare) and the
/// run completes before any signal arrives -- it reads `Some(0)` and looks like a
/// product defect reporting an interrupt as success. Signalling the hermit CLI
/// itself is also the wrong path: the outer process has default SIGINT
/// disposition, dies of it, and `status.code()` is `None` because the death is
/// `WIFSIGNALED` -- a real report, but not the one the container's own status
/// channel produces.
///
/// Signalling the CONTAINER CHILD is what exercises the status channel: the child
/// chooses `128 + signo` (it cannot produce a genuine signalled status, being a
/// namespace init) and the parent must recognise that as a signal death rather
/// than an unaccounted exit.
///
/// ⚠️ SCOPE, STATED SO THE COVERAGE IS NOT OVERCLAIMED. This exercises
/// `on_container_init_stop_signal` and the parent's `SignalDeath` arm. It does
/// NOT exercise `--sigint-instakill`, which governs a SIGINT delivered to the
/// GUEST inside the namespace. ⚠️ THAT IS NOW COVERED, by
/// `sigint_instakill_reports_a_signal_death_not_a_policy_refusal` below; this
/// sentence said otherwise for three heads after the cell existed, which
/// agent(hermit-005)'s codex lane caught. The two cells exercise DIFFERENT
/// producers and both are needed -- that is why this one is still here.
#[test]
fn a_signal_killed_container_reports_a_signal_death_not_a_refusal() {
    // ⚠️ THE GUEST MUST BLOCK ON REAL I/O, NOT ON A CLOCK -- see above. A read on
    // a pipe nobody writes to is not determinized, so it holds the run open in
    // wall-clock time.
    let mut child = hermit_command(&["run", "--", "/bin/cat"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Own process group, so `wait_bounded` can reach namespace descendants
        // with a negative pid rather than only the direct child.
        .process_group(0)
        .spawn()
        .expect("failed to spawn the container under test");
    let _stdin = child.stdin.take().expect("piped stdin");

    // Find the container child, polling rather than sleeping a fixed time: the
    // fork has to have happened before there is anything to signal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let container = loop {
        let out = Command::new("pgrep")
            .args(["-P", &child.id().to_string()])
            .output()
            .expect("pgrep failed");
        let first = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<i32>().ok());
        if let Some(pid) = first {
            break Some(pid);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let container = container.expect("the container child never appeared within 30s");

    // Give it a moment to install its stop-signal handler before signalling.
    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: `kill` on a pid in this process's own tree.
    assert_eq!(
        unsafe { libc::kill(container, libc::SIGINT) },
        0,
        "failed to deliver SIGINT to the container child"
    );

    // ⚠️ STDIN IS DROPPED FIRST so the guest can retire if the signal was missed,
    // and the wait is bounded so it cannot wedge if it still does not.
    drop(_stdin);
    let status = wait_bounded(&mut child, "a_signal_killed_container");
    let stderr = drain_bounded(child.stderr.take());

    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_SIGINT_DEATH_EXIT),
        "a signal-killed container must report 128 + SIGINT\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_SIGNAL_DEATH class=signal-death signal=2"),
        "it must be classified as a signal death\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("HERMIT_POLICY_REFUSAL"),
        "a signal death is not a policy refusal -- hermit refused nothing\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("HERMIT_INTERNAL_FAILURE"),
        "a signal death is not a hermit failure\nstderr:\n{stderr}"
    );
}

/// `--sigint-instakill` reports a SIGNAL DEATH, not a policy refusal.
///
/// ⚠️ THIS IS THE PRODUCER, AND IT IS A DIFFERENT CLAIM FROM THE CLASSIFIER.
/// `a_signal_killed_container_reports_a_signal_death_not_a_refusal` signals the
/// CONTAINER, which runs `on_container_init_stop_signal` and never touches
/// `unrecoverable_shutdown`. agent(hermit-007)'s codex lane proved that by
/// mutation: reverting the producer at `detcore/src/lib.rs` from
/// `HERMIT_SIGINT_DEATH_EXIT` to `HERMIT_POLICY_REFUSAL_EXIT` left the whole
/// suite green. This test is what closes that: the signal goes to the GUEST, so
/// detcore's `handle_signal_event` runs and `sigint_instakill` chooses the code.
///
/// ⚠️ THE GUEST IS FOUND BY WALKING `/proc`, NOT BY `pgrep -f`. A pattern search
/// matches the searching process's own command line -- three probes for this test
/// signalled themselves before this was written, one of them exiting 130 from its
/// own SIGINT and looking briefly like a product result.
#[test]
fn sigint_instakill_reports_a_signal_death_not_a_policy_refusal() {
    fn parent_of(pid: i32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
    fn children_of(pid: i32) -> Vec<i32> {
        std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
            .filter(|c| parent_of(*c) == Some(pid))
            .collect()
    }

    // Resolve cat through PATH so the pinned single-binary coreutils receives
    // argv[0] = "cat", the process name asserted below.
    let mut child = hermit_command(&["run", "--sigint-instakill", "--", "cat"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Own process group, so `wait_bounded` can reach namespace descendants
        // with a negative pid rather than only the direct child.
        .process_group(0)
        .spawn()
        .expect("failed to spawn the sigint-instakill run");
    let stdin = child.stdin.take().expect("piped stdin");

    // hermit -> container init -> guest. Poll: the two forks must both have
    // happened before there is a guest to signal.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let guest = loop {
        let found = children_of(child.id() as i32)
            .into_iter()
            .flat_map(children_of)
            .next();
        if let Some(pid) = found {
            break Some(pid);
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let guest = guest.expect("the guest never appeared under the container within 30s");

    // Verify WHICH process is being signalled rather than trusting the walk.
    let comm = std::fs::read_to_string(format!("/proc/{guest}/comm")).unwrap_or_default();
    assert_eq!(comm.trim(), "cat", "walked to the wrong process: {comm:?}");

    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: `kill` on a pid in this process's own tree, verified above.
    assert_eq!(
        unsafe { libc::kill(guest, libc::SIGINT) },
        0,
        "SIGINT to guest failed"
    );

    // ⚠️ CLOSE STDIN AFTER SIGNALLING, BEFORE WAITING. This one line is the
    // difference between measuring the fix and measuring nothing. With it held
    // open through the wait, this reported `code=None signal=Some(2)` -- the
    // CONTROL outcome, as though the flag were absent -- and I spent six
    // eliminations blaming the cargo harness for it. The guest is blocked in a
    // read on this pipe; the signal alone does not retire the run.
    drop(stdin);
    let status = wait_bounded(&mut child, "sigint_instakill_reports_a_signal_death");
    let stderr = drain_bounded(child.stderr.take());

    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_SIGINT_DEATH_EXIT),
        "sigint_instakill must report 128 + SIGINT, not a policy refusal\nstderr:\n{stderr}"
    );
    // ⚠️ THE MARKER, NOT ONLY THE CODE, AND THE PAIR IS THE POINT. Asserting
    // exit 130 alone leaves the marker free to disappear: every value in
    // `0..=255` is a legal guest status, which is exactly why this change
    // introduced a marker at all. agent(hermit-007)'s codex lane proved the gap
    // by mutation -- the producer mutation failed this test, but a MARKER-only
    // mutation left it green, so the machine-readable half was unpinned. That is
    // the same shape as the defect this PR exists to fix: an outcome
    // distinguishable only by a channel nobody checks.
    assert!(
        stderr.contains("HERMIT_SIGNAL_DEATH"),
        "a signal death must be machine-readable as one, not only exit 130\nstderr:\n{stderr}"
    );
    // ⚠️ AND THE OPERATOR-FACING HALF, WHICH IS A SEPARATE CHANNEL FROM THE MARKER.
    // agent(hermit-dbg) withdrew an approval over this and proved it with a
    // DIFFERENT mutation than the marker one: gutting `SignalDeath`'s `Display` to
    // `write!(f, "")` leaves the marker intact, so a test checking only
    // `HERMIT_SIGNAL_DEATH` stays green while the sentence the operator actually
    // reads disappears. Two halves, two mutations, two assertions -- the machine
    // channel and the human channel fail independently.
    assert!(
        stderr.contains("not a hermit failure and not a refusal"),
        "the operator must be told this was not a hermit failure; the marker alone is \
         machine-readable but says nothing to a person\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("HERMIT_POLICY_REFUSAL"),
        "an operator interrupt is not a policy refusal -- hermit refused nothing\n{stderr}"
    );
}

/// Wait for a child, but never forever.
///
/// ⚠️ AN UNBOUNDED `wait()` IN A SIGNAL TEST WEDGES THE RUNNER INSTEAD OF FAILING,
/// and this repository has already paid for that shape: hermit#2654's cell hung
/// the run to an external rc=124 with no test named and no failing status of its
/// own. Both callers below hold the guest open on a pipe read, so if the signal
/// is missed -- handler not yet installed, or the pid raced -- nothing ends the
/// run. A wedge costs the whole run's budget and reports nothing; a red names
/// itself in one line. Raised by agent(hermit-006) and agent(hermit-007).
///
/// The child is killed on expiry so the failure is a named red and leaves nothing
/// running.
fn wait_bounded(child: &mut std::process::Child, what: &str) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match child.try_wait().expect("failed to poll the child") {
            Some(status) => return status,
            None => {
                if std::time::Instant::now() >= deadline {
                    // ⚠️ THE GROUP, NOT THE CHILD. `child.kill()` reaps only the
                    // direct CLI process; a namespace descendant that outlived it
                    // keeps running AND keeps the inherited stderr write end open,
                    // so the drain below would then block forever on a pipe that
                    // never reaches EOF. agent(hermit-005)'s codex lane found this
                    // and named the reachable window: the few milliseconds before
                    // `PR_SET_PDEATHSIG` is armed (container.rs:299-310). Both
                    // callers spawn with `process_group(0)`, so the child leads its
                    // own group and a negative pid reaches every descendant.
                    // SAFETY: signalling a process group this test created.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{what}: the run did not finish within 60s of the signal; killed it \
                         so this is a named failure rather than a wedged runner"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

/// Read a child's stderr without the possibility of blocking forever.
///
/// ⚠️ `read_to_string` RETURNS ON EOF, NOT ON THE CHILD EXITING, and those are
/// different events. EOF needs every holder of the write end to close it, so one
/// surviving namespace descendant with inherited stderr blocks the read
/// indefinitely — after the child has been waited for and after the test believes
/// it is finished. Bounding the WAIT and leaving the DRAIN unbounded moves the
/// hang one line down rather than removing it. Found by agent(hermit-005)'s codex
/// lane.
fn drain_bounded(pipe: Option<std::process::ChildStderr>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = pipe.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    // The reader thread is detached on timeout: it is blocked on a pipe nobody
    // will close, and leaving it parked costs one thread for the rest of the
    // process rather than wedging the run.
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .unwrap_or_else(|_| {
            String::from("<stderr drain timed out: a descendant still holds the write end>")
        })
}

/// `hermit record` reports a fail-closed refusal as a REFUSAL, not as "hermit broke".
///
/// ⚠️ THE CELL agent(hermit-001)'s CODEX LANE ASKED FOR, AND IT DID NOT EXIST BECAUSE
/// THE FIX DID NOT WORK. Their finding was that nothing pinned the positive
/// direction. Writing this cell is what exposed why: at that head `hermit record`
/// still reported exit 125 `class=cli-error` against this exact guest. The
/// in-process unit test passed throughout, because it builds the error directly and
/// never crosses the boundary that broke it.
///
/// ⚠️ THE BOUNDARY IS `reverie::Error::Tool`, DECLARED `#[error(transparent)]`. That
/// forwards `source()` past the inner `anyhow::Error`'s own value, and
/// `UnsupportedSyscallError` IS that value, so a chain walk cannot see it. Detection
/// now reaches through the wrapper; see `is_policy_refusal`.
///
/// Run mode already worked and is covered by
/// `run_ptrace_fails_closed_by_default_on_unsupported_syscall`: it sets
/// `shutdown_on_unsupported_syscall`, so the STATUS carries the meaning. Record mode
/// sets `exit_on_unsupported_syscall` instead and returns a typed error, producing no
/// status of its own — which is why the two spellings of one policy disagreed.
#[test]
fn record_reports_an_unsupported_syscall_as_a_refusal_not_an_internal_failure() {
    let program = dbt_unsupported_syscall_guest()
        .to_str()
        .expect("unsupported-syscall guest path should be UTF-8");

    let args = ["record", "start", "--", program];
    let recorded = hermit(&args);
    let err = stderr(&recorded);

    assert_eq!(
        recorded.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "record mode must report a fail-closed refusal as a REFUSAL; run mode already \
         does, and the two spellings of one policy must not disagree\nstderr:\n{err}"
    );
    assert!(
        err.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "the class marker is the only exclusive channel -- every value in 0..=255 is a \
         legal guest status\nstderr:\n{err}"
    );
    // ⚠️ THE DIAGNOSTIC MUST SURVIVE. `PolicyRefusal`'s prose says the reason is
    // above; on this path the reason exists only in the error chain, so a fix that
    // classified correctly while dropping the cause would satisfy the two assertions
    // above and still leave the operator without the syscall's name.
    // ⚠️ MATCHED ON THE CHAIN LINE, NOT ON THE TEXT ANYWHERE. detcore ALSO logs
    // "unsupported syscall: restart_syscall() = ?" independently, so a plain
    // `contains` is satisfied by the log and says nothing about the error chain.
    // Measured: with the cause dropped, that assertion still passed. The chain
    // rendering has no parens and ends the line; detcore's log has "() = ?".
    let chain_line = err.lines().any(|l| {
        l.trim_end()
            .ends_with("unsupported syscall: restart_syscall")
    });
    assert!(
        chain_line,
        "the refused syscall must survive into the error CHAIN, not merely appear in \
         detcore's own log line\nstderr:\n{err}"
    );
    // CONTROL: it must NOT read as hermit breaking, which is what it did before.
    assert!(
        !err.contains("HERMIT_INTERNAL_FAILURE"),
        "a deliberate refusal must not be reported as an internal failure\nstderr:\n{err}"
    );
}

/// A guest can set `O_NONBLOCK` on the inherited fd 2, and hermit's own
/// diagnostics must survive it.
///
/// ⚠️ THIS CANNOT BE TESTED THROUGH A FILE. On a regular file `O_NONBLOCK` is a
/// no-op for writes, so a test that redirects stderr to a temp file passes
/// whether or not the defect is present. The back-pressure has to be real,
/// which means a pipe with a known amount of free space.
///
/// Measured before the fix, at this exact shape: 170 of 2240 bytes arrived, the
/// process exited 101 (a panic inside `eprintln!`) instead of 127, and the
/// delivered bytes were NOT a prefix of the true text -- 92 good bytes followed
/// by the tail of the panic message, which reads as a complete short error
/// rather than a severed one.
#[test]
fn a_stopped_stderr_reader_does_not_hang_hermit_on_its_way_out() {
    // ⚠️ THE COMPANION CASE TO THE TEST BELOW, AND THE ONE IT DOES NOT COVER.
    // That test's reader sleeps 600ms and then drains -- a SLOW reader, which
    // `RetryingStderr` is right to wait for. This is a STOPPED reader: the read
    // end stays open so `write` keeps returning EAGAIN rather than EPIPE, and
    // nothing ever drains.
    //
    // Without a ceiling the retry loop never ends, on the path that reports why
    // hermit is stopping. Measured 2026-08-26 on this exact setup:
    //
    //     eprintln! (before RetryingStderr)   exits rc=101 immediately
    //     RetryingStderr with no ceiling      still running after 25s
    //     RetryingStderr with the ceiling     exits rc=127 after 5.0s
    //
    // Waiting for a slow reader is the feature. Waiting for a stopped one is a
    // hang, and a supervisor that hangs while reporting an error is harder to
    // diagnose than one that dies reporting it badly.
    use std::os::unix::io::FromRawFd;
    const F_SETPIPE_SZ: i32 = 1031;
    const F_GETPIPE_SZ: i32 = 1032;

    let program = String::from("/nonexistent-guest-program-for-this-test");
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe { libc::fcntl(write_fd, F_SETPIPE_SZ, 4096) };
    let pipe_bytes = unsafe { libc::fcntl(write_fd, F_GETPIPE_SZ) };
    assert!(pipe_bytes > 196, "read pipe capacity after F_SETPIPE_SZ");
    let filler = vec![b'x'; pipe_bytes as usize - 196];
    assert_eq!(
        unsafe { libc::write(write_fd, filler.as_ptr().cast(), filler.len()) },
        filler.len() as isize,
        "prefill"
    );
    let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
    unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };

    let started = std::time::Instant::now();
    let mut child = hermit_command(&["run", "--", &program])
        .stdout(std::process::Stdio::null())
        .stderr(unsafe { std::process::Stdio::from_raw_fd(write_fd) })
        .spawn()
        .expect("spawn hermit");

    // ⚠️ THE READ END IS HELD AND NEVER READ. Holding it is what keeps the
    // writes returning EAGAIN; closing it would give EPIPE, which is a
    // different path and already terminates.
    let held = unsafe { std::fs::File::from_raw_fd(read_fd) };

    // ⚠️ BOUNDED WAIT, NOT `child.wait()`. A plain wait makes the FAILURE MODE OF
    // THIS TEST A HANG: with the ceiling removed the runner wedges instead of
    // going red, which is a stuck job somebody has to notice rather than a named
    // failure. Measured while writing this -- the mutation had to be killed by an
    // outer timeout. Poll, then kill and assert, so an unbounded loop is a RED.
    let deadline = std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(std::time::Duration::from_millis(50)),
        }
    };
    let elapsed = started.elapsed();
    drop(held);

    let status = status.unwrap_or_else(|| {
        panic!(
            "hermit had not exited after {elapsed:?} against a STOPPED stderr \
             reader; the diagnostic retry loop is unbounded and hangs on the \
             exit path (killed to keep this a red rather than a wedged runner)"
        )
    });
    // EXIT-CLASS: hermit
    assert_eq!(
        status.code(),
        Some(127),
        "giving up on an undeliverable diagnostic must not change the exit status"
    );

    // ⚠️ THIS IS THE ASSERTION THAT PINS THE BOUND, AND WITHOUT IT THE CELL WAS
    // GREEN FOR ANY DEADLINE UNDER 30s. `elapsed` used to be computed and then
    // used ONLY in the panic message above, so the sole timing check was the 30s
    // poll deadline: the production constant could drift from 5s to 29s and this
    // test would not notice. A bound nothing pins is a bound that moves.
    //
    // The ceiling is the deadline plus process startup and teardown, not the
    // deadline alone, so it is stated as the deadline plus a stated margin rather
    // than as a bare number. Measured on this fixture: 5.0s against a 5s deadline.
    let ceiling = detcore::util::STDERR_DIAGNOSTIC_DEADLINE * 2 + Duration::from_secs(5);
    assert!(
        elapsed < ceiling,
        "hermit took {elapsed:?} to give up on an undeliverable diagnostic, but the \
         deadline is {:?} and it is a TOTAL for the process, not a budget per write. \
         Exceeding it means either the deadline drifted or the clock went back to \
         being per-`write()`, which multiplies it by the number of diagnostic lines.",
        detcore::util::STDERR_DIAGNOSTIC_DEADLINE
    );
}

/// The diagnostic deadline must fit inside the bound that encloses it.
///
/// ⚠️ THIS IS THE CHECK THAT MAKES THE DERIVATION REAL RATHER THAN A COMMENT.
/// Diagnostics on the exit path run inside `RUN_TIMEOUT_UNWIND_GRACE`: the window
/// between a `--timeout` expiring and the SIGALRM fallback calling `_exit`.
/// Overrunning it does not just delay the report, it loses it, and additionally
/// emits `HERMIT_RUN_TIMEOUT_FALLBACK` -- the marker that is supposed to mean the
/// teardown wedged. So the failure mode of getting this wrong is a correct run
/// that reports a false internal defect.
///
/// Raising either side alone now fails HERE, by name, instead of silently
/// producing an inner bound that cannot fit inside its outer one. That is the
/// third instance of this shape found on 2026-08-26; see docs/TIMEOUT_LADDER.md.
#[test]
fn the_stderr_diagnostic_deadline_fits_inside_the_unwind_grace() {
    // Not imported from hermit-cli: `RUN_TIMEOUT_UNWIND_GRACE` is private to the
    // crate. Pinning the number here means a change there must also change this
    // line, which is the point -- the two are only related if something says so.
    let unwind_grace = Duration::from_secs(10);
    let per_process = detcore::util::STDERR_DIAGNOSTIC_DEADLINE;

    // ⚠️ THE MULTIPLIER IS GONE, AND ITS ABSENCE IS THE POINT. This used to read
    // `let writing_processes = 2;` -- "hermit plus the forked init" -- and the
    // assertion below then evaluated to 10000 <= 10000, passing EXACTLY on the
    // boundary. It is not two on every path: `hermit run --verify` runs the guest
    // twice and forks once per run, so it is the outer hermit plus TWO container
    // inits. At three the same assertion is 15000 <= 10000 and fails.
    //
    // The origin is now shared across the invocation (detcore's STDERR_ORIGIN_ADDR),
    // so every hermit process measures from the same start and the invocation spends
    // ONE deadline however many of them write. A count that is not in the arithmetic
    // cannot be the wrong count.
    let diagnostics = per_process;

    assert!(
        diagnostics < unwind_grace,
        "the diagnostic deadline ({diagnostics:?}) must be STRICTLY smaller than the \
         unwind grace ({unwind_grace:?}) that encloses it; an inner bound at or above \
         its outer bound can never fire and is dead configuration that reads as \
         protection"
    );
    assert!(
        diagnostics * 2 <= unwind_grace,
        "diagnostics for the whole invocation ({diagnostics:?}) must leave at least as \
         much of the unwind grace ({unwind_grace:?}) for the unwind itself as they take \
         for reporting; that half-and-half split is the stated derivation in \
         detcore/src/util.rs and this is what holds the two numbers together"
    );
}

#[test]
fn diagnostics_survive_a_nonblocking_stderr_under_back_pressure() {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    // A long, fully deterministic diagnostic whose length the caller controls:
    // hermit echoes the (absent) program path back in the error chain.
    let program = format!("/nonexistent-{}", "A".repeat(2000));

    // The truth to compare against, captured with an ordinary pipe.
    let control = hermit_command(&["run", "--", &program])
        .output()
        .expect("control run");
    let expected = control.stderr;
    // EXIT-CLASS: hermit
    assert_eq!(
        control.status.code(),
        Some(127),
        "control run should report guest-program-not-found"
    );
    assert!(
        expected.len() > 2000,
        "control diagnostic unexpectedly short ({} bytes); the probe needs a \
         message larger than the pipe's free space to prove anything",
        expected.len()
    );

    // A pipe with exactly 196 bytes free, and the write end made nonblocking
    // exactly as a guest's fcntl would leave it. Some kernels refuse to shrink
    // a pipe to 4096 bytes, so measure the capacity after requesting it rather
    // than treating F_SETPIPE_SZ as infallible.
    let mut fds = [0 as libc::c_int; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    const F_SETPIPE_SZ: libc::c_int = 1031;
    const F_GETPIPE_SZ: libc::c_int = 1032;
    unsafe { libc::fcntl(write_fd, F_SETPIPE_SZ, 4096) };
    let pipe_bytes = unsafe { libc::fcntl(write_fd, F_GETPIPE_SZ) };
    assert!(pipe_bytes > 196, "read pipe capacity after F_SETPIPE_SZ");
    let filler = vec![b'F'; pipe_bytes as usize - 196];
    let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
    unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    let mut filled = 0usize;
    while filled < filler.len() {
        let n = unsafe {
            libc::write(
                write_fd,
                filler[filled..].as_ptr().cast(),
                filler.len() - filled,
            )
        };
        if n <= 0 {
            break;
        }
        filled += n as usize;
    }

    let stderr_for_child = unsafe { std::process::Stdio::from_raw_fd(libc::dup(write_fd)) };
    let mut child = hermit_command(&["run", "--", &program])
        .stdout(std::process::Stdio::null())
        .stderr(stderr_for_child)
        .spawn()
        .expect("spawn hermit");
    unsafe { libc::close(write_fd) };

    // Hold the pipe full briefly, then drain. A fix that merely stopped
    // erroring would still have dropped the bytes during this window.
    let reader = thread::spawn(move || {
        thread::sleep(std::time::Duration::from_millis(600));
        let mut f = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut buf = Vec::new();
        let _ = f.read_to_end(&mut buf);
        buf
    });

    let status = child.wait().expect("hermit exited");
    let got = reader.join().expect("reader");
    let diagnostic = &got[filled.min(got.len())..];

    // EXIT-CLASS: hermit
    assert_eq!(
        status.code(),
        Some(127),
        "a guest-visible fd flag must not change hermit's exit status \
         (101 here means eprintln! panicked on EAGAIN)"
    );
    assert_eq!(
        diagnostic.len(),
        expected.len(),
        "diagnostic was truncated: {} of {} bytes survived a nonblocking stderr",
        diagnostic.len(),
        expected.len()
    );
    assert_eq!(
        diagnostic,
        expected.as_slice(),
        "diagnostic differed byte-for-byte from the control; not erroring is \
         not the same as delivering the message"
    );
}

/// A guest that never finishes and that IGNORES `SIGTERM`.
///
/// ⚠️ IGNORING `SIGTERM` IS LOAD-BEARING, NOT DECORATION, and the reasoning is
/// `container_init_deadline.rs`'s, measured there: an ordinary spinning guest is
/// an ordinary host process, so anything that signals the process group kills it
/// and `hermit` then exits because its guest finished -- which makes the test
/// pass whether or not the bound did anything. With `trap '' TERM` the guest
/// survives a group signal, so the run can only end if hermit's own `--timeout`
/// actually tears the container down.
const RUN_TIMEOUT_SPINNER: &[&str] = &["/bin/sh", "-c", "trap '' TERM; while : ; do : ; done"];

/// The bound under test. Deliberately small: `.config/nextest.toml` caps every
/// test process at 15 seconds, so a regression cell for a timeout has to fire,
/// tear down and be checked well inside that. Two seconds plus startup and the
/// drain check measured about 4s total.
const RUN_TIMEOUT_SECS: u64 = 2;

/// Session id of `pid`, or `None` if it is gone.
///
/// The comm field is parenthesised and may itself contain spaces and
/// parentheses, so it is skipped from the LAST `)`. `session` is the sixth
/// field overall, i.e. index 3 after `state`.
fn run_timeout_session_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(3)?.parse().ok()
}

/// Every live pid in `session`: the outer `hermit`, the container init, and the
/// guest. Scanning the SESSION rather than a pid is what lets the test see a
/// process that was reparented away from us.
fn run_timeout_pids_in_session(session: i32) -> Vec<i32> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| run_timeout_session_of(*pid) == Some(session))
        .collect()
}

/// Run `hermit run --timeout <secs> -- <argv>` as its own session leader.
///
/// Its own session so the test can account for every process the run creates,
/// including one that outlives its parent and is reparented to host PID 1 --
/// which is precisely the residue this bound exists to prevent and which a
/// parent-child check cannot see.
fn spawn_timed_run(secs: u64, argv: &[&str]) -> std::process::Child {
    let secs = secs.to_string();
    let mut command = hermit_command(&["run", "--timeout", &secs, "--"]);
    command
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe, allocates nothing, and runs in the
    // forked child before exec where this process is not yet a session leader.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .expect("failed to spawn hermit run --timeout")
}

/// PAST the bound: the run is ended, named, and genuinely unwound.
///
/// ⚠️ THE THIRD ASSERTION IS THE ONE THAT MATTERS AND IT IS THE ONE A NAIVE
/// TEST OMITS. `container_init_deadline.rs` records the reason: exit 124, empty
/// output and a supervisor that returns promptly all look IDENTICAL whether or
/// not the run was actually torn down. A cell that checked only the code and
/// the class string would pass over a hermit that printed the right words and
/// left the guest spinning -- the out-of-disk incident's exact shape. So this
/// asserts on process liveness directly.
#[test]
fn run_timeout_fires_by_name_and_unwinds_the_container() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let child = spawn_timed_run(RUN_TIMEOUT_SECS, RUN_TIMEOUT_SPINNER);
    let session = child.id() as i32;
    let started = Instant::now();
    let output = child
        .wait_with_output()
        .expect("failed to wait for hermit run --timeout");
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert_eq!(
        output.status.code(),
        Some(124),
        "a --timeout expiry must report 124, the established code for a deadline. \
         Got {:?}. stderr:\n{stderr}",
        output.status
    );

    assert!(
        stderr.contains("class=run-timeout"),
        "the bound must fail BY NAME -- an anonymous kill is most of why a timed-out \
         cell teaches nothing. Expected `class=run-timeout` in stderr:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("bound of {RUN_TIMEOUT_SECS} seconds")),
        "the report must state the bound that was exceeded, not merely that one was. \
         stderr:\n{stderr}"
    );

    // The unwind. Nothing above this line can distinguish a torn-down container
    // from an orphaned one.
    let drained = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if run_timeout_pids_in_session(session).is_empty() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    };
    let survivors: Vec<String> = run_timeout_pids_in_session(session)
        .into_iter()
        .map(|pid| {
            let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
            format!("{pid} ({})", comm.trim())
        })
        .collect();
    for pid in run_timeout_pids_in_session(session) {
        // SAFETY: `kill` takes a pid and a signal and touches no caller memory;
        // a stale pid can only fail with ESRCH. A failing test must not leak the
        // very processes it is about.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        drained,
        "hermit reported the timeout but left the run alive: {survivors:?}. \
         The bound is supposed to UNWIND the container hermit built, not just \
         report that it expired."
    );

    assert!(
        elapsed < Duration::from_secs(15),
        "the bound fired but took {elapsed:?}; a --timeout that overruns the \
         15s nextest per-test cap would be killed by that cap instead, and this \
         cell would stop testing hermit's own teardown"
    );
}

/// INSIDE the bound: a guest that finishes in time is untouched.
///
/// Without this direction the cell above is satisfied by a `--timeout` that
/// simply kills every run, which would be a strictly worse product than having
/// no flag at all.
#[test]
fn run_timeout_leaves_a_guest_that_finishes_in_time_alone() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let output = spawn_timed_run(30, &["/bin/echo", "finished-well-inside"])
        .wait_with_output()
        .expect("failed to wait for hermit run --timeout");

    assert!(
        output.status.success(),
        "a guest that finishes inside its bound must exit 0; got {:?}. stderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "finished-well-inside",
        "the guest's own output must be unaffected by an unexpired bound"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("class=run-timeout"),
        "an unexpired bound must say nothing at all"
    );
}

/// The SIGALRM fallback fires, is named, and reports the deadline code.
///
/// ⚠️ THIS CELL EXISTS BECAUSE THE FALLBACK COULD NOT BE MADE TO FIRE ANY OTHER
/// WAY, AND SHIPPING IT UNEXERCISED WAS REFUSED. Five guest shapes were tried
/// against the primary path on 2026-08-26 -- a userspace spinner, a blocking
/// read on a pipe with no writer, a guest that `SIGSTOP`s itself, an
/// eight-thread guest ignoring `SIGTERM`, and a multi-process guest ignoring
/// `SIGTERM` -- and every one unwound cleanly at exactly the bound. So the
/// condition is injected instead, through the shipped binary rather than a
/// `cfg(test)` build.
///
/// ⚠️ WHAT IT PROVES, AND WHAT IT DOES NOT. It proves the alarm arms with the
/// inherited mask handled, the handler runs, the marker reaches stderr, the
/// container is torn down, and the status survives the container boundary as
/// 124. It does NOT prove that any particular teardown hang is survivable: the
/// injected delay sits after the drop, not inside a wedged destructor, because
/// no wedging destructor is known. Do not read a pass here as coverage of one.
///
/// Forcing it also FOUND A DEFECT that review had not: the init exited 124,
/// `classify_container_result` had no arm for that status, and the run reported
/// exit 125 `class=container-child-exit` -- "hermit broke" -- for a bound doing
/// its job. That is why `HERMIT_DEADLINE_EXIT` and its arm exist.
#[test]
fn run_timeout_fallback_fires_when_the_unwind_does_not_finish() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut command = hermit_command(&[
        "run",
        "--timeout",
        "1",
        "--",
        RUN_TIMEOUT_SPINNER[0],
        RUN_TIMEOUT_SPINNER[1],
        RUN_TIMEOUT_SPINNER[2],
    ]);
    command
        .env("HERMIT_INTERNAL_RUN_TIMEOUT_STALL_UNWIND", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe, allocates nothing, and runs in the
    // forked child before exec where this process is not yet a session leader.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .expect("failed to spawn hermit run --timeout");
    let session = child.id() as i32;
    let output = child.wait_with_output().expect("failed to wait for hermit");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        stderr.contains("HERMIT_RUN_TIMEOUT_FALLBACK"),
        "the fallback must announce itself -- an unnamed hard kill here is \
         indistinguishable from the outer rungs. stderr:\n{stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(124),
        "the fallback must report the deadline code across the container \
         boundary. Exit 125 here is the regression this cell was written for: \
         the init exits 124 and `classify_container_result` loses it, turning a \
         working bound into `class=container-child-exit`. Got {:?}. stderr:\n{stderr}",
        output.status
    );
    assert!(
        !stderr.contains("class=container-child-exit"),
        "a hermit-chosen deadline status must not be reported as an unchosen \
         child death. stderr:\n{stderr}"
    );

    let drained = {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if run_timeout_pids_in_session(session).is_empty() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    };
    for pid in run_timeout_pids_in_session(session) {
        // SAFETY: `kill` takes a pid and a signal and touches no caller memory;
        // a stale pid can only fail with ESRCH.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    assert!(
        drained,
        "the fallback `_exit`s the namespace init, and the kernel must then reap \
         every remaining member. Survivors mean the hard fallback leaks what the \
         gentle path does not."
    );
}

/// `--timeout` refuses a backend where it was measured not to bound the run.
///
/// ⚠️ THE BACKENDS ARE NAMED INDIVIDUALLY, NOT LOOPED OVER A LIST, so adding a
/// backend does not silently inherit a guarantee nobody measured for it.
///
/// Measured 2026-08-26 with `--timeout 3` on a guest that never exits, two runs
/// each: ptrace and liteinst stopped at 3s reporting `class=run-timeout`; kvm
/// stopped only via the hard fallback at 13s; sabre ran 40s and dbt ran 20s and
/// neither produced any marker at all -- those elapsed times are the harness's
/// own deadline, not hermit's. All five run correctly WITHOUT the flag, so it is
/// the bound that fails, not the backend.
///
/// ⚠️ `dbt` IS THE LOAD-BEARING CASE. The `RunOpts::main` DBT arm returns
/// `run_dbt(..)` and never reaches `RunOpts::run`, so the first version of this
/// check -- placed in `run()` -- covered every backend EXCEPT the one furthest
/// from working, and dbt still accepted the flag and ran unbounded. Same shape
/// as the `--namespace-only` second launch path. Move the check back into
/// `run()` and this cell fails on dbt while the others still pass.
#[test]
fn run_timeout_refuses_backends_where_it_cannot_bound_the_run() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    for backend in ["sabre", "dbt", "kvm"] {
        let output = hermit_command(&[
            "run",
            "--backend",
            backend,
            "--timeout",
            "3",
            "--",
            "/bin/echo",
            "unreachable",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        // EXIT-CLASS: hermit
        assert_eq!(
            output.status.code(),
            Some(122),
            "`--timeout` on `{backend}` must REFUSE (122), not accept a bound it \
             cannot enforce. Got {:?}. stderr:\n{stderr}",
            output.status
        );
        assert!(
            stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "a refusal is hermit working, and must not be reported as a failure. \
             backend={backend} stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("not qualified"),
            "the refusal must say the bound is unqualified on this backend, and \
             point somewhere. backend={backend} stderr:\n{stderr}"
        );
    }
}

/// The diagnostic deadline is spent ONCE across every write, not restarted by each.
///
/// ⚠️ THIS CELL EXISTS BECAUSE THE CELL ABOVE CANNOT DO THIS, AND THAT WAS FOUND BY
/// ABLATION RATHER THAN BY READING. Reverting the clock in `detcore/src/util.rs`
/// to per-`write()` and re-running
/// `a_stopped_stderr_reader_does_not_hang_hermit_on_its_way_out` still PASSED, at
/// 5.06s. Its pipe is prefilled to 3900 of 4096, so the short first diagnostic line
/// fits in the 196 free bytes and EXACTLY ONE write ever blocks — and at N=1 a
/// per-write budget and a per-exit budget are the same number. That cell pins the
/// deadline's VALUE; nothing in it pins the SEMANTICS.
///
/// ⚠️ THE ONE-CHARACTER DIFFERENCE THAT MAKES THE PROPERTY VISIBLE: fill the pipe
/// COMPLETELY. With zero free bytes even the first line blocks, so several writes
/// block instead of one, and the two designs separate. Measured 2026-08-26, three
/// runs each, on this exact fixture:
///
/// ```text
///   per-exit  (shared clock)    2.52s  2.52s  2.52s
///   per-write (clock in write)  10.04s 10.05s 10.05s
/// ```
///
/// Deterministic, and a factor of four apart. The per-write column is the deadline
/// times the number of diagnostic lines; the per-exit column is the deadline, once,
/// no matter how many lines there are. That invariance is the whole guarantee, and
/// it is what this asserts.
///
/// Only ONE process reports here: a missing guest fails before the container init
/// is forked. The real-guest case costs twice this, which is why the derivation in
/// `detcore/src/util.rs` divides by two.
#[test]
fn the_stderr_deadline_is_spent_once_across_writes_not_restarted_by_each() {
    use std::os::unix::io::FromRawFd;
    const F_SETPIPE_SZ: i32 = 1031;
    const F_GETPIPE_SZ: i32 = 1032;

    let program = String::from("/nonexistent-guest-program-for-the-shared-deadline-cell");
    let mut fds = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe { libc::fcntl(write_fd, F_SETPIPE_SZ, 4096) };
    let pipe_bytes = unsafe { libc::fcntl(write_fd, F_GETPIPE_SZ) };
    assert!(pipe_bytes > 0, "read pipe capacity after F_SETPIPE_SZ");

    // ⚠️ COMPLETELY FULL, NOT NEARLY FULL. Leaving even a couple of hundred bytes
    // free lets the first line through and collapses this back into the N=1 case
    // the cell above already covers. Filled while still BLOCKING, before O_NONBLOCK
    // goes on, so the fill itself cannot short-write.
    let filler = vec![b'x'; pipe_bytes as usize];
    assert_eq!(
        unsafe { libc::write(write_fd, filler.as_ptr().cast(), filler.len()) },
        pipe_bytes as isize,
        "the pipe must start completely full or this cell silently tests the N=1 case"
    );
    let flags = unsafe { libc::fcntl(write_fd, libc::F_GETFL) };
    unsafe { libc::fcntl(write_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };

    let started = Instant::now();
    let mut child = hermit_command(&["run", "--", &program])
        .stdout(Stdio::null())
        .stderr(unsafe { Stdio::from_raw_fd(write_fd) })
        .spawn()
        .expect("spawn hermit");

    // The read end is held and never read: that is what keeps every write returning
    // EAGAIN rather than EPIPE.
    let held = unsafe { std::fs::File::from_raw_fd(read_fd) };

    // ⚠️ BOUNDED WAIT, AND THE BOUND IS BELOW THE NEXTEST PER-TEST CAP ON PURPOSE.
    // Under the per-write regression this child runs ~10s; polling to completion
    // would let `.config/nextest.toml`'s 15s cap terminate the cell instead, which
    // reports as a timeout attributed to nextest rather than as this assertion
    // failing by name. Give up at 8s, comfortably above the ~2.5s this takes when
    // correct and comfortably below both 10s and 15s.
    let poll_deadline = Duration::from_secs(8);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break Some(status),
            None if started.elapsed() >= poll_deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let elapsed = started.elapsed();
    drop(held);

    // One process reports on this path, so the whole exit gets one deadline.
    let ceiling = detcore::util::STDERR_DIAGNOSTIC_DEADLINE * 2;

    assert!(
        status.is_some(),
        "hermit had not exited after {elapsed:?} with every diagnostic write blocked. \
         The deadline is {:?} and is spent ONCE for the whole exit; a run that outlives \
         it by this much is spending it again per `write()`, which multiplies the exit \
         path by the number of diagnostic lines. Measured: 2.52s shared, 10.05s per-write.",
        detcore::util::STDERR_DIAGNOSTIC_DEADLINE
    );
    assert!(
        elapsed < ceiling,
        "hermit took {elapsed:?} to give up on undeliverable diagnostics, above the \
         {ceiling:?} ceiling. With several writes blocked the elapsed time must still be \
         about one {:?} deadline, because the clock is shared. Growing with the number of \
         lines is the per-write regression this cell exists to catch.",
        detcore::util::STDERR_DIAGNOSTIC_DEADLINE
    );
    // EXIT-CLASS: hermit
    assert_eq!(
        status.expect("checked above").code(),
        Some(127),
        "giving up on undeliverable diagnostics must not change the exit status"
    );
}
