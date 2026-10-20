/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/exec_posix_timers.rs"]
mod exec_posix_timers;

#[path = "common/fault_sites.rs"]
mod fault_sites;

#[path = "common/kvm_cancellation.rs"]
mod kvm_cancellation;

#[path = "common/kvm_itimer.rs"]
mod kvm_itimer;

#[path = "common/kvm_nonleader_exec.rs"]
mod kvm_nonleader_exec;

#[path = "common/kvm_orphan_reparenting.rs"]
mod kvm_orphan_reparenting;

#[path = "common/kvm_signal_retirement.rs"]
mod kvm_signal_retirement;

#[path = "common/kvm_synchronous_fault.rs"]
mod kvm_synchronous_fault;

// Its runtime-staging helpers are used only by the LiteInst run tests, which
// a build without the `liteinst` feature leaves out.
#[cfg_attr(not(feature = "liteinst"), allow(dead_code))]
#[path = "common/liteinst.rs"]
mod liteinst_runtime;

#[path = "common/readonly_proc.rs"]
mod readonly_proc;

#[path = "common/run2_log.rs"]
mod run2_log;

#[path = "common/nonleader_exec.rs"]
mod nonleader_exec;

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
static KVM_GETTIMEOFDAY_EFAULT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_UNSUPPORTED_SYSCALL_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_SELF_SIGQUEUE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_STDERR_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_LOG_ENV_GUEST: OnceLock<PathBuf> = OnceLock::new();
#[cfg(feature = "liteinst")]
static LITEINST_INERT_RUNTIME: OnceLock<PathBuf> = OnceLock::new();
static EXEC_CLOCK_CONTINUITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_LSEEK_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_INODE_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static REPLAY_EPOCH_GUEST: OnceLock<PathBuf> = OnceLock::new();
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
    // The DBT log carries every authenticated record, initialization records
    // included at their arrival positions, so the verdict names all_records_v1.
    assert_eq!(
        verdict["comparison"]["record_envelope"], "all_records_v1",
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

/// A regular build embeds no source revision; only `HERMIT_STAMP_GIT_SHA=1`
/// stamps one. Both `--version` and `version --json` must say which.
#[test]
fn version_names_a_revision_only_when_the_build_was_stamped() {
    let text = hermit(&["--version"]);
    assert!(text.status.success(), "hermit --version failed: {text:?}");
    let text = String::from_utf8(text.stdout).expect("--version is not UTF-8");
    let json = hermit(&["version", "--json"]);
    assert!(
        json.status.success(),
        "hermit version --json failed: {json:?}"
    );
    let info: detcore_model::build_info::BuildInfo =
        serde_json::from_slice(&json.stdout).expect("version --json is not BuildInfo");

    if option_env!("HERMIT_STAMP_GIT_SHA") == Some("1") {
        let sha = info.git_sha.strip_suffix("-dirty").unwrap_or(&info.git_sha);
        assert!(
            sha.len() == 12 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "a stamped build must embed a 12-hex revision, got {:?}",
            info.git_sha
        );
        assert!(
            text.trim_end().ends_with(&format!(", g{})", info.git_sha)),
            "--version must name the stamped revision: {text:?}"
        );
    } else {
        assert_eq!(
            info.git_sha, "unknown",
            "a regular build embedded a revision"
        );
        assert!(
            text.trim_end().ends_with(", source revision not embedded)"),
            "--version must disclose the missing source revision: {text:?}"
        );
        assert!(
            !text.contains(", g"),
            "--version names a revision: {text:?}"
        );
    }
}

#[cfg(feature = "liteinst")]
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

/// Two spinning threads plus eight absolute CLOCK_REALTIME samples; see
/// `tests/c/replay_epoch_probe.c`.
fn replay_epoch_guest() -> &'static Path {
    REPLAY_EPOCH_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("replay-epoch-probe");
        fs::create_dir_all(&build_root).expect("failed to create replay-epoch build directory");
        let guest = build_root.join("replay_epoch_probe");
        let output = Command::new("cc")
            .args([
                "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
            ])
            .arg(repository.join("tests/c/replay_epoch_probe.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the replay-epoch probe");
        assert!(
            output.status.success(),
            "replay-epoch probe compilation failed:\nstdout:\n{}\nstderr:\n{}",
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
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
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

fn kvm_gettimeofday_efault_guest() -> &'static Path {
    KVM_GETTIMEOFDAY_EFAULT_GUEST.get_or_init(|| {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_gettimeofday_efault.c");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-gettimeofday-efault");
        fs::create_dir_all(&build_root)
            .expect("failed to create KVM gettimeofday EFAULT guest directory");
        let guest = build_root.join("kvm_gettimeofday_efault");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile KVM gettimeofday EFAULT guest");
        assert!(
            output.status.success(),
            "KVM gettimeofday EFAULT guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
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

    let args = ["--backend", "kvm", "run", "--", "/bin/true"];
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
            OsStr::new("--backend"),
            OsStr::new("kvm"),
            OsStr::new("run"),
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
        ["--backend=dbt", "run", "--", "/bin/true"],
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
                OsStr::new(args[0]),
                OsStr::new(args[1]),
                OsStr::new("--base-env=minimal"),
                OsStr::new("--workdir=/test"),
                OsStr::new("--"),
                OsStr::new("/bin/true"),
            ]
        );
    }

    let explicit_base_env_args = [
        "--backend=kvm",
        "run",
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
            OsStr::new("--backend=kvm"),
            OsStr::new("run"),
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
                "--backend",
                "dbt",
                "run",
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
                "--backend",
                "dbt",
                "run",
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

fn readonly_proc_command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    // The inherited validation workdir is sufficient for these read-only guests.
    // A fresh /test tmpfs mount would also be denied by the flags-zero filter.
    append_hermit_args_using_outer_mount(&mut command, args);
    // SAFETY: The helper installs the filter using only async-signal-safe
    // syscalls, in the child before exec, without affecting the test process.
    unsafe {
        command.pre_exec(readonly_proc::deny_writable_mounts);
    }
    command
}

fn assert_readonly_proc_run(namespace_only: bool) {
    let _guard = hermit_run_guard();
    let mut args = if namespace_only {
        vec!["run", "--namespace-only"]
    } else {
        vec!["--backend=ptrace", "run"]
    };
    args.extend_from_slice(&[
        // Local networking would require another flags-zero mount for sysfs.
        "--network=host",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
    ]);
    args.extend_from_slice(&["--", "/bin/cat", "/proc/mounts", "/proc/self/status"]);
    let output = readonly_proc_command(&args)
        .output()
        .expect("start Hermit with writable mounts denied");
    assert_success(&output, &args);
    let stdout = stdout(&output);
    let (mounts, status) = stdout
        .split_once("\nName:")
        .expect("cat must return both proc mounts and process status");
    readonly_proc::assert_readonly_proc(mounts, status, if namespace_only { 1 } else { 3 });
    assert_eq!(
        stderr(&output)
            .matches(hermit::proc_mount::READONLY_WARNING.trim())
            .count(),
        1,
        "each launch must report the read-only proc mount exactly once: {output:?}"
    );
}

#[test]
fn namespace_only_uses_readonly_proc_after_permission_denial() {
    assert_readonly_proc_run(true);
}

#[test]
fn namespace_only_readonly_proc_warning_tolerates_closed_stderr() {
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;

    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--namespace-only",
        "--network=host",
        "--",
        "/bin/true",
    ];
    let mut descriptors = [-1; 2];
    // SAFETY: pipe2 initializes both descriptors on success; each resulting
    // descriptor is transferred into exactly one OwnedFd below.
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0,
        "create stderr pipe: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: these are the two distinct descriptors just returned by pipe2.
    let (reader, writer) = unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    };
    drop(reader);
    let mut command = readonly_proc_command(&args);
    command.stderr(Stdio::from(writer));
    // SAFETY: the callback changes only this child's signal state using
    // async-signal-safe calls. A broken diagnostic pipe must remain harmless
    // even when SIGPIPE is neither ignored nor blocked by the caller.
    unsafe {
        command.pre_exec(|| {
            let mut action = std::mem::zeroed::<libc::sigaction>();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            let mut signals = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, libc::SIGPIPE);
            if libc::sigaction(libc::SIGPIPE, &action, std::ptr::null_mut()) == -1
                || libc::sigprocmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut()) == -1
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command
        .output()
        .expect("start namespace-only guest with a broken stderr pipe");
    assert_success(&output, &args);
    assert!(
        output.stdout.is_empty(),
        "unexpected guest output: {output:?}"
    );
}

#[test]
fn namespace_only_readonly_proc_warning_tolerates_denied_statfs() {
    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--namespace-only",
        "--network=host",
        "--",
        "/bin/true",
    ];
    let mut command = hermit_command(&args);
    // Deny only the diagnostic probe; container mounts must still complete.
    deny_syscall(&mut command, libc::SYS_statfs);
    let output = command
        .output()
        .expect("start namespace-only guest with statfs denied");
    assert_success(&output, &args);
    assert!(
        output.stdout.is_empty(),
        "unexpected guest output: {output:?}"
    );
    let stderr = stderr(&output);
    assert_eq!(
        stderr
            .matches("hermit: warning: could not determine the /proc mount mode.")
            .count(),
        1,
        "a failed probe must report the unknown proc mode exactly once: {stderr}"
    );
    assert!(
        !stderr.contains(hermit::proc_mount::READONLY_WARNING.trim()),
        "a denied probe must not claim to know the proc mode: {stderr}"
    );
}

#[test]
fn run_uses_readonly_proc_after_permission_denial() {
    assert_readonly_proc_run(false);
}

#[test]
fn run_verify_uses_readonly_proc_after_permission_denial() {
    let _guard = hermit_run_guard();
    let args = [
        "run",
        "--verify",
        "--network=host",
        "--max-timeslice=disabled",
        "--no-virtualize-cpuid",
        "--",
        "/bin/true",
    ];
    let output = readonly_proc_command(&args).output().unwrap();
    assert_success(&output, &args);
    assert!(stderr(&output).contains("Success: deterministic. Determinism verified."));
    assert_eq!(
        stderr(&output)
            .matches(hermit::proc_mount::READONLY_WARNING.trim())
            .count(),
        2,
        "both verification launches must report their read-only proc once: {output:?}"
    );
}

#[test]
fn record_replay_uses_readonly_proc_after_permission_denial() {
    let _guard = hermit_run_guard();
    // Local networking would require another flags-zero mount for sysfs.
    let args = ["record", "--network=host", "--verify", "--", "/bin/true"];
    let output = readonly_proc_command(&args)
        .output()
        .expect("start record/replay with writable mounts denied");
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains("Success: replay matched recording."),
        "record must complete replay verification under the mount restriction: {output:?}"
    );
    let stderr = stderr(&output);
    let (recording, replay) = stderr
        .split_once(":: Replaying...")
        .expect("replay started");
    // These diagnostics inspect the real stopped tracee in each stage, rather
    // than replaying a recorded statfs result back to the guest.
    for (stage, diagnostic) in [("recording", recording), ("replay", replay)] {
        assert_eq!(
            diagnostic
                .matches(hermit::proc_mount::READONLY_WARNING.trim())
                .count(),
            1,
            "{stage} must observe a read-only proc mount exactly once: {stderr}"
        );
    }
}

#[test]
fn readonly_proc_metadata_detects_replay_mismatch_and_accepts_older_recordings() {
    let _guard = hermit_run_guard();
    let data = tempfile::tempdir().unwrap();
    let directory = data.path().to_str().unwrap();
    // Local networking would require another flags-zero mount for sysfs.
    let record_args = [
        "record",
        "--network=host",
        "--data-dir",
        directory,
        "--",
        "/bin/true",
    ];
    let recording = readonly_proc_command(&record_args).output().unwrap();
    assert_success(&recording, &record_args);
    let id = fs::read_to_string(data.path().join("last")).unwrap();

    // The completion hint must name the data directory the recording went to.
    // Without it the printed command looks in the default directory and finds
    // nothing. Run the printed command itself, not a hand-built equivalent.
    let record_stderr = strip_ansi_sgr(&stderr(&recording));
    let hint = format!(
        "hermit replay --autopilot --data-dir={} {}",
        shell_words::quote(directory),
        id.trim()
    );
    assert!(
        record_stderr.contains(&hint),
        "missing replay hint {hint:?}:\n{record_stderr}"
    );
    let hint_words = shell_words::split(&hint).unwrap();
    let hint_args: Vec<&str> = hint_words[1..].iter().map(String::as_str).collect();
    let hinted_replay = readonly_proc_command(&hint_args).output().unwrap();
    assert_success(&hinted_replay, &hint_args);

    let metadata_path = data.path().join(id.trim()).join("metadata.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
    assert_eq!(metadata["proc_readonly"], true, "{metadata}");

    // Model a recording from a writable-proc host, while replay's real mount
    // remains read-only. A mismatch is diagnostic, not an event-format change.
    metadata["proc_readonly"] = false.into();
    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let replay_args = ["replay", "--autopilot", "--data-dir", directory];
    let replay = readonly_proc_command(&replay_args).output().unwrap();
    assert_success(&replay, &replay_args);
    assert!(
        stderr(&replay)
            .contains("/proc mount mode differs: recording was read-write, replay is read-only"),
        "missing proc-mode mismatch warning: {replay:?}"
    );
    assert_eq!(stderr(&replay).matches("hermit: warning:").count(), 1);

    metadata.as_object_mut().unwrap().remove("proc_readonly");
    fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let legacy = readonly_proc_command(&replay_args).output().unwrap();
    assert_success(&legacy, &replay_args);
    assert!(stderr(&legacy).contains(hermit::proc_mount::READONLY_WARNING.trim()));
    assert!(!stderr(&legacy).contains("mount mode differs"));
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
fn run_rejects_subcommand_level_backend() {
    // `--backend` is a global option. The old compatibility spelling after the
    // subcommand must be refused as a usage error (exit 2) that names the
    // working global form, not silently accepted and not mis-suggested as
    // `--backend-engagement-json`. These invocations never reach a guest, so the
    // raw binary is used without execution-root arguments.
    let hermit_binary = env!("CARGO_BIN_EXE_hermit");
    for (args, corrected) in [
        (
            &["run", "--backend=ptrace", "--", "/bin/true"][..],
            "--backend=ptrace run -- /bin/true",
        ),
        (
            &["run", "--backend", "ptrace", "--", "/bin/true"][..],
            "--backend=ptrace run -- /bin/true",
        ),
        (
            &[
                "--log=info",
                "run",
                "--strict",
                "--backend",
                "dbt",
                "--",
                "/bin/true",
            ][..],
            "--backend=dbt --log=info run --strict -- /bin/true",
        ),
        (
            &["oci", "run", "--backend=kvm", "busybox", "/bin/true"][..],
            "--backend=kvm oci run busybox /bin/true",
        ),
    ] {
        let output = Command::new(hermit_binary)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be a usage error:\n{}",
            stderr(&output)
        );
        let message = stderr(&output);
        assert!(
            message.contains("unexpected argument '--backend"),
            "{args:?}: unexpected error:\n{message}"
        );
        assert!(
            message.contains("is a global option and must come before the subcommand"),
            "{args:?}: error does not explain the global position:\n{message}"
        );
        assert!(
            message.contains(&format!("{hermit_binary} {corrected}`")),
            "{args:?}: error does not give the corrected command {corrected:?}:\n{message}"
        );
        assert!(
            message.contains("Usage: hermit [OPTIONS] <COMMAND>"),
            "{args:?}: error does not show where global options go:\n{message}"
        );
        assert!(
            !message.contains("backend-engagement-json"),
            "{args:?}: error points at an unrelated flag:\n{message}"
        );
    }

    // The global form is the working path and still runs the guest.
    let args = ["--backend=ptrace", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn analyze_rejects_backend_in_its_run_arguments() {
    // `hermit analyze` builds its trials' `run` options from --run-arg and its
    // trailing run arguments. `run` has no `--backend`, so a backend there must
    // be refused with the working global spelling rather than clap's unrelated
    // `--backend-engagement-json` hint.
    let hermit_binary = env!("CARGO_BIN_EXE_hermit");
    for (args, corrected) in [
        (
            &["analyze", "--run-arg=--backend=kvm", "--", "/bin/true"][..],
            "hermit --backend=kvm analyze ...",
        ),
        (
            &[
                "analyze",
                "--",
                "--backend",
                "kvm",
                "/bin/true",
                "--backend=x",
            ][..],
            "hermit --backend=kvm analyze ...",
        ),
        (
            &["analyze", "--", "--backend", "--strict", "/bin/true"][..],
            "hermit --backend=<BACKEND> analyze ...",
        ),
    ] {
        let output = Command::new(hermit_binary)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
        let message = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?} must be a usage error:\n{message}"
        );
        assert!(
            message.contains("It is a global option") && message.contains(corrected),
            "{args:?}: error does not give the working path {corrected:?}:\n{message}"
        );
        assert!(
            !message.contains("backend-engagement-json"),
            "{args:?}: error points at an unrelated flag:\n{message}"
        );
    }
}

#[test]
fn run_rejects_unknown_backends_during_argument_parsing() {
    let args = ["--backend", "unknown", "run", "--", "/bin/true"];
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
    let args = ["--backend", "dbt", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
}

#[test]
fn run_dbt_uses_the_requested_guest_environment() {
    if dbt_unavailable("run_dbt_uses_the_requested_guest_environment") {
        return;
    }
    let args = [
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
            "--backend",
            "dbt",
            "run",
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
        // After a match `--keep-logs` retains only run 1's log, the golden
        // log; run 2's log, which matched it, is deleted.
        let captures = |side: &str| {
            fs::read_dir(&logs)
                .unwrap()
                .map(Result::unwrap)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with(side))
                .collect::<Vec<_>>()
        };
        let golden = captures("run1_log_");
        assert_eq!(golden.len(), 1);
        assert!(golden[0].metadata().unwrap().len() > 0);
        assert!(
            captures("run2_log_").is_empty(),
            "verify {attempt}: a matched verification must not retain run 2's log"
        );
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
/// This is not a style assertion. `ci/compat-envelope/pressure-test.rs` locates
/// the retained logs with `name.starts_with("run1_log_")` / `("run2_log_")`,
/// and the parity post-pass reads the `run1_log_` golden. A matched verify
/// result must yield exactly one nonempty `run1_log_` capture and no
/// `run2_log_` capture, because `--keep-logs` keeps only the golden log after a
/// match; a diverged result must yield exactly one of each. A terminal result
/// whose directory does not fit its verdict is recorded `infrastructure-error`
/// regardless of the verdict it actually reached.
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
/// the old spelling is absent -- so a revert is visible. Run 2's name is only
/// retained after a divergence, so
/// `dbt_verify_without_json_rejects_io_buffer_content_divergence` pins it on a
/// deterministic divergence.
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
    let verdict_path = root.path().join("verdict.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--backend",
            "dbt",
            "run",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
        ])
        .arg(&log_dir)
        .arg("--verify-json")
        .arg(&verdict_path);
    append_hermit_args_using_outer_mount(
        &mut command,
        &["--", "/bin/echo", "dbt-verify-log-naming"],
    );
    let output = command.output().expect("failed to run DBT verification");

    // DELIBERATELY NOT REQUIRING a particular verdict. The subject here is the
    // NAME the captures are retained under. An earlier version of this test
    // required success and was flaky within three runs: DBT verification of
    // /bin/echo diverged on one of them ("Log differences found between run 1
    // and run 2"), which failed the test for a reason that has nothing to do
    // with the naming it exists to pin. Coupling a property to an unrelated
    // verdict is how a test starts getting re-run until it passes. The verdict
    // only selects which captures `--keep-logs` must have retained.
    let stderr_text = strip_ansi_sgr(&stderr(&output));
    assert!(
        stderr_text.contains("Verification logs retained"),
        "DBT verification did not report retained logs, so this test cannot observe the \
         capture names at all:\n{stderr_text}"
    );
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&verdict_path).expect("DBT verification did not write its --verify-json report"),
    )
    .expect("DBT verification report should be JSON");
    let matched = report["verdict"] == "matched";
    assert_eq!(
        output.status.success(),
        matched,
        "process status disagrees with the recorded verdict: {report}"
    );

    let names = |prefix: &str| -> Vec<String> {
        fs::read_dir(&log_dir)
            .expect("failed to read the retained verify-log directory")
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name.starts_with(prefix))
            .collect()
    };
    let directory_listing = || {
        fs::read_dir(&log_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    // The harness predicate, applied verbatim: a match retains exactly one
    // nonempty golden capture and no second capture; anything else retains
    // exactly one nonempty capture of each side.
    let retained: &[&str] = if matched {
        &["run1_log_"]
    } else {
        &["run1_log_", "run2_log_"]
    };
    for prefix in retained {
        let found = names(prefix);
        assert_eq!(
            found.len(),
            1,
            "DBT verification ({}) must retain exactly one {prefix} capture for the harness to \
             find; directory held {:?}",
            report["verdict"],
            directory_listing()
        );
        let size = fs::metadata(log_dir.join(&found[0]))
            .expect("failed to stat a retained capture")
            .len();
        assert!(size > 0, "retained capture {} is empty", found[0]);
    }
    if matched {
        assert!(
            names("run2_log_").is_empty(),
            "a matched DBT verification must not retain run 2's log; directory held {:?}",
            directory_listing()
        );
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
            "--backend",
            "dbt",
            "run",
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

    // A divergence keeps both logs under `--keep-logs`, each under the name the
    // harness scans for. This deterministic divergence is what pins run 2's
    // name: after a match only run 1's golden log is retained.
    let captures = |prefix: &str| -> Vec<PathBuf> {
        fs::read_dir(&log_dir)
            .expect("failed to read the retained DBT verify-log directory")
            .map(|entry| entry.expect("failed to read a retained log entry").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect()
    };
    for prefix in ["run1_log_", "run2_log_"] {
        let found = captures(prefix);
        assert_eq!(
            found.len(),
            1,
            "a diverged DBT verification must retain exactly one {prefix} capture: {found:?}"
        );
        let size = fs::metadata(&found[0])
            .expect("failed to stat a retained DBT capture")
            .len();
        assert!(
            size > 0,
            "retained DBT capture {} is empty",
            found[0].display()
        );
    }
    for stale in ["dbt-run1_log_", "dbt-run2_log_"] {
        assert!(
            captures(stale).is_empty(),
            "DBT retained a {stale} capture instead of the name the harness scans for"
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

/// A working directory under the host `/tmp`, made with `tempdir_in("/tmp")`
/// rather than `tempfile::tempdir()`: the latter follows `TMPDIR`, which
/// validation points elsewhere, and then the test would not reach the case.
fn working_directory_under_host_tmp(purpose: &str) -> tempfile::TempDir {
    let directory = tempfile::Builder::new()
        .prefix(&format!("hermit-3260-{purpose}-"))
        .tempdir_in("/tmp")
        .expect("failed to create a working directory under the host /tmp");
    assert!(
        directory.path().starts_with("/tmp/"),
        "fixture working directory {} is not under the host /tmp, so this test \
         would not exercise https://github.com/rrnewton/hermit/issues/3260",
        directory.path().display()
    );
    directory
}

/// Names left in `directory` (and its `ignored/`) that a private run summary uses.
fn leftover_private_summaries(directory: &Path) -> Vec<PathBuf> {
    [directory.to_path_buf(), directory.join("ignored")]
        .iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flatten()
        .map(|entry| entry.expect("listing the working directory").path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(".hermit-"))
        })
        .collect()
}

/// https://github.com/rrnewton/hermit/issues/3260: `hermit run --verify` from
/// a working directory under the host `/tmp`, the way Buck/TPX and lit start
/// tests (221 of the fbsource import's Buck/TPX failures).
///
/// The private run-1 summary used to be handed to the container by its host
/// name, which the container hides behind its own private `/tmp`. Three shapes:
///
/// - a plain directory: the summary is created in the cwd itself, Detcore's
///   write fails with NotFound and panics as PID 1 of the container, so even
///   `/bin/true` exits 125 before run 2;
/// - a git work tree that ignores `ignored/`: the same, via `<tree>/ignored/`;
/// - `/tmp` itself: the parent exists in the private `/tmp`, so the write
///   "succeeds" into a directory deleted with the container and verification
///   silently loses run 1's statistics.
///
/// "No statistics warning" is asserted, not just success: it proves run 1's
/// summary actually reached the controller. It is what catches the third
/// shape, and a fix that merely swallowed the failed write.
#[test]
fn run_verify_from_a_working_directory_under_host_tmp() {
    for shape in ["a plain directory", "a git work tree", "/tmp itself"] {
        let fixture =
            (shape != "/tmp itself").then(|| working_directory_under_host_tmp("verify-cwd"));
        let directory = fixture
            .as_ref()
            .map_or(Path::new("/tmp"), |directory| directory.path());
        if shape == "a git work tree" {
            fs::create_dir(directory.join(".git")).expect("fixture work tree");
            fs::write(directory.join(".gitignore"), "/ignored/\n").expect("fixture ignore rule");
        }
        let args = ["run", "--verify", "--", "/bin/true"];
        let output = hermit_command(&args)
            .current_dir(directory)
            .output()
            .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
        let stderr = stderr(&output);
        assert!(
            !stderr.contains("panicked") && !stderr.contains("class=container-child-panic"),
            "hermit run --verify panicked from {shape} under the host /tmp ({}):\n{stderr}",
            directory.display()
        );
        assert_success(&output, &args);
        assert!(
            stderr.contains(":: Success: deterministic. Determinism verified."),
            "determinism confirmation missing from {shape} under the host /tmp:\n{stderr}"
        );
        assert!(
            !stderr.contains("verification runtime statistics unavailable"),
            "run 1's private summary did not reach the controller from {shape} under \
             the host /tmp:\n{stderr}"
        );
        // Not for /tmp itself: unrelated processes create names there.
        if fixture.is_some() {
            assert_eq!(
                leftover_private_summaries(directory),
                Vec::<PathBuf>::new(),
                "hermit run --verify left a private summary behind in {shape}"
            );
        }
    }
}

/// The same fault for a `--summary-json` the caller chose: an absolute path
/// under the host `/tmp` used to let the whole guest run and then panic at
/// teardown (exit 125, `class=container-child-panic`). It is refused before
/// the guest starts, and the refusal names the path. `--tmp=/tmp` exposes the
/// host `/tmp`, so the same path then works and the summary lands on the host.
#[test]
fn run_refuses_a_summary_json_hidden_by_the_private_tmp() {
    let directory = working_directory_under_host_tmp("summary-json");
    let summary = directory
        .path()
        .join("missing-in-guest")
        .join("summary.json");
    fs::create_dir(summary.parent().unwrap()).expect("fixture summary directory");
    let summary_arg = format!("--summary-json={}", summary.display());
    let args = [
        "run",
        summary_arg.as_str(),
        "--",
        "/bin/echo",
        "guest-ran-3260",
    ];
    let output = hermit(&args);
    let stderr = stderr(&output);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("class=container-child-panic"),
        "a --summary-json under the host /tmp panicked instead of being refused:\n{stderr}"
    );
    assert_eq!(
        output.status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "{stderr}"
    );
    assert!(stderr.contains("class=cli-error"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "--summary-json {} is not visible inside the run container",
            summary.display()
        )),
        "the refusal does not name the summary path:\n{stderr}"
    );
    assert_eq!(
        stdout(&output),
        "",
        "the guest ran before the refusal, so it was not refused up front"
    );
    assert!(!summary.exists(), "a refused run wrote a summary");

    // `--tmp=<dir>` other than /tmp makes the guest's /tmp that directory, so
    // the write would land in `<dir>/...` (here a missing subdirectory, which
    // panicked at teardown before): refused the same way.
    let other_tmp = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the --tmp directory");
    let other_tmp_arg = format!("--tmp={}", other_tmp.path().display());
    let other_args = [
        "run",
        other_tmp_arg.as_str(),
        summary_arg.as_str(),
        "--",
        "/bin/echo",
        "guest-ran-3260",
    ];
    let other = hermit(&other_args);
    let other_stderr = String::from_utf8_lossy(&other.stderr).into_owned();
    assert_eq!(
        other.status.code(),
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "{other_stderr}"
    );
    assert!(
        other_stderr.contains("is not visible inside the run container")
            && !other_stderr.contains("panicked"),
        "--tmp=<dir> with a /tmp summary was not refused up front:\n{other_stderr}"
    );
    assert_eq!(
        stdout(&other),
        "",
        "the guest ran before the --tmp=<dir> refusal"
    );

    // A mode that never writes the summary is not refused.
    let namespace_only_args = [
        "run",
        "--namespace-only",
        summary_arg.as_str(),
        "--",
        "/bin/echo",
        "guest-ran-3260",
    ];
    let namespace_only = hermit(&namespace_only_args);
    assert_success(&namespace_only, &namespace_only_args);
    assert_eq!(stdout(&namespace_only), "guest-ran-3260\n");

    // Control: the check discriminates rather than refusing every /tmp path.
    let exposed_args = [
        "run",
        "--tmp=/tmp",
        summary_arg.as_str(),
        "--",
        "/bin/echo",
        "guest-ran-3260",
    ];
    let exposed = hermit(&exposed_args);
    assert_success(&exposed, &exposed_args);
    assert_eq!(stdout(&exposed), "guest-ran-3260\n");
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&summary).expect("--tmp=/tmp summary was not written"))
            .expect("--tmp=/tmp summary is not JSON");
    assert!(written.is_object(), "{written}");
}

/// The sibling private summary from the same issue: a ptrace run asked for
/// `--backend-engagement-json` without `--summary-json` reads its scheduler
/// turns from a private summary resolved the same way.
#[test]
fn run_ptrace_backend_engagement_from_a_working_directory_under_host_tmp() {
    let directory = working_directory_under_host_tmp("engagement-cwd");
    let records = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the engagement record directory");
    let record = records.path().join("engagement.json");
    let record_arg = format!("--backend-engagement-json={}", record.display());
    let args = [
        "--backend=ptrace",
        "run",
        record_arg.as_str(),
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .current_dir(directory.path())
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"));
    let stderr = stderr(&output);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("class=container-child-panic"),
        "ptrace engagement run panicked from under the host /tmp:\n{stderr}"
    );
    assert_success(&output, &args);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&record).expect("the engagement record was not written"))
            .expect("the engagement record is not JSON");
    assert_eq!(report["engagement"]["backend"], "ptrace", "{report}");
    assert!(
        report["engagement"]["scheduler_turns"]
            .as_u64()
            .is_some_and(|turns| turns > 0),
        "the engagement record carries no scheduler turns: {report}"
    );
    assert_eq!(
        leftover_private_summaries(directory.path()),
        Vec::<PathBuf>::new(),
        "the ptrace engagement run left a private summary behind"
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
        "--backend",
        "dbt",
        "run",
        "--",
        "/bin/echo",
        "dbt-supported-ok",
    ];
    let supported = hermit(&supported_args);
    assert_success(&supported, &supported_args);
    assert_eq!(stdout(&supported), "dbt-supported-ok\n");

    // Negative bracket: the real unsupported restart_syscall must fail and name
    // itself before the guest can publish its former success marker.
    let default_args = ["--backend", "dbt", "run", "--", program];
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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

    let strict_args = ["--backend", "dbt", "run", "--strict", "--", program];
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        let args = ["--backend", "dbt", "run", "--strict", "--", program, mode];
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
    let args = ["--backend", "dbt", "run", "--strict", "--", program];
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
#[cfg(feature = "liteinst")]
fn run_liteinst_verifies_detcore_backend() {
    liteinst_runtime::ensure_liteinst_runtime();
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--strict",
        "--verify",
        "--",
        "/bin/echo",
        "liteinst-cli-ok",
    ];
    let mut command = Command::new(liteinst_runtime::hermit_binary());
    append_hermit_args(&mut command, &args);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to run selected LiteInst Hermit: {error}"));
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
    let debug_stderr = stderr(&debug_output);
    assert!(
        debug_stderr.contains("backend run complete backend=ptrace stats=ptrace activity stats: "),
        "{debug_stderr}"
    );
    let dispatch_marker = " dispatch=dispatch stats v1 backend=ptrace dispatches=";
    let dispatches = debug_stderr
        .split_once(dispatch_marker)
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("{debug_stderr}"));
    assert!(
        dispatches.starts_with(|c: char| c.is_ascii_digit()),
        "ptrace always measures its dispatches: {debug_stderr}"
    );
}

/// LiteInst reports its own counters, under the same DEBUG gate as ptrace.
///
/// The guest is Hermit's activation probe: 32 `getpid` calls from one
/// instruction site, of which the first traps and installs the hook and the
/// other 31 go through it. So a record that really came from this run counts
/// at least 31 direct hooks; other sites in the probe's own start-up may add
/// more. Exactly one record is required because the activation check Hermit
/// runs first happens before logging is set up.
///
/// Hermit reports from two call sites, one per way of running the guest: a
/// plain `run` waits for the exit status, while `--verify` (and `analyze`)
/// capture the output. Both are checked. Under `--verify --keep-logs` each of
/// the two compared runs writes its DEBUG log to its own file, and after a
/// match Hermit keeps only run 1's as the golden log and deletes run 2's
/// (https://github.com/rrnewton/hermit/issues/3301). So run 2's log must be
/// gone, and each log must hold exactly one record from its own run: the
/// golden log, and run 2's, through a link made while Hermit ran. Plain
/// `--verify` compares only selected DETLOG and scheduler COMMIT records
/// (`filter_deterministic` in `detcore/src/logdiff.rs`), so the match does not
/// imply that run 2's log holds this DEBUG record.
///
/// `process_reports=0` is today's architecture, not a gap: the ptrace host
/// counts every hook entry itself, so no guest process submits a report.
/// When Detcore moves into the guest (https://github.com/rrnewton/hermit/issues/3520)
/// each guest process reports its own counts and this becomes 1.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_backend_stats_report_the_guests_own_dispatch_paths() {
    liteinst_runtime::ensure_liteinst_runtime();
    let hermit = liteinst_runtime::hermit_binary();
    let hermit_path = hermit.to_str().expect("Hermit test binary path is UTF-8");
    // With `verify` as `Some((logs, capture))` the guest runs under
    // `--verify --keep-logs` into `logs`, and run 2's log is also returned
    // through a hard link at `capture`.
    let run = |log: &[&str], verify: Option<(&Path, &Path)>| {
        let mut args = log.to_vec();
        args.extend(["--backend", "liteinst", "run"]);
        if let Some((logs, _)) = verify {
            let logs = logs.to_str().expect("verify-log directory path is UTF-8");
            args.extend(["--verify", "--keep-logs", "--verify-log-dir", logs]);
        }
        args.extend([
            "--strict",
            "--env=HERMIT_INTERNAL_LITEINST_ACTIVATION_PROBE=1",
            "--",
            hermit_path,
        ]);
        let mut command = Command::new(&hermit);
        command
            .env_remove("RUST_LOG")
            .env_remove("HERMIT_LOG")
            .env_remove("HERMIT_LOG_FILE");
        append_hermit_args(&mut command, &args);
        let (output, run2_log) = match verify {
            Some((logs, capture)) => output_capturing_run2_log(&mut command, logs, capture),
            None => (
                command.output().unwrap_or_else(|error| {
                    panic!("failed to run LiteInst Hermit with {args:?}: {error}")
                }),
                None,
            ),
        };
        assert_success(&output, &args);
        assert_eq!(
            stdout(&output),
            "hermit-liteinst-activation calls=32 traps=1 hooks=31\n"
        );
        (stderr(&output), run2_log)
    };
    let check_record = |source: &str, text: &str| {
        let records: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("backend run complete"))
            .collect();
        let [record] = records[..] else {
            panic!(
                "expected exactly one backend statistics record in {source}, found {records:#?}"
            );
        };
        assert!(
            record.contains(
                "backend run complete backend=liteinst stats=LiteInst instrumentation stats: process_reports=0 "
            ),
            "{source}: {record}"
        );
        let direct_hooks: u64 = record
            .split_once("direct_hook=")
            .and_then(|(_, rest)| {
                rest.split(|character: char| !character.is_ascii_digit())
                    .next()
            })
            .and_then(|digits| digits.parse().ok())
            .unwrap_or_else(|| panic!("no direct_hook count in {source}: {record}"));
        assert!(
            direct_hooks >= 31,
            "the probe makes 31 hooked calls, but {source} counts {direct_hooks}: {record}"
        );
    };

    for log in [&[][..], &["--log", "info"][..]] {
        let (stderr, _) = run(log, None);
        assert!(
            !stderr.contains("backend run complete"),
            "the record is DEBUG-only, but {log:?} printed it:\n{stderr}"
        );
    }

    check_record("the run's stderr", &run(&["--log", "debug"], None).0);

    let directory = tempfile::Builder::new()
        .prefix("liteinst-stats-verify-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create LiteInst verify directory")
        .keep();
    eprintln!(
        "LiteInst statistics verification artifacts retained at {}",
        directory.display()
    );
    let logs = directory.join("verify-logs");
    fs::create_dir(&logs).expect("failed to create LiteInst verify-log directory");
    let link = directory.join("captured-run2.log");
    let (verify_stderr, run2_log) = run(&["--log", "debug"], Some((&logs, &link)));
    assert!(
        verify_stderr.contains("Success: deterministic. Determinism verified."),
        "{verify_stderr}"
    );
    let golden = retained_captures(&logs, "run1_log_");
    let [capture] = &golden[..] else {
        panic!("expected exactly one run1_log_ capture, found {golden:?}");
    };
    let duplicates = retained_captures(&logs, "run2_log_");
    assert!(
        duplicates.is_empty(),
        "a matched verification must delete run 2's log: {duplicates:?}"
    );
    let run2_log = run2_log.expect("run 2's log, captured while the command ran");
    for path in [capture, &run2_log] {
        let text = fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        check_record(&path.display().to_string(), &text);
    }
    fs::remove_file(&run2_log).expect("failed to remove run 2's checked log");
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
#[cfg(feature = "liteinst")]
fn run_liteinst_rejects_a_non_runtime_override_before_activation_claim() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create false LiteInst runtime directory");
    let runtime = directory.path().join("not-a-liteinst-runtime");
    fs::copy("/bin/true", &runtime).expect("failed to copy false LiteInst runtime fixture");
    write_matching_liteinst_revision(&runtime);
    let args = [
        "--backend",
        "liteinst",
        "run",
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
#[cfg(feature = "liteinst")]
fn run_liteinst_rejects_an_inert_dso_before_activation_claim() {
    let args = [
        "--backend",
        "liteinst",
        "run",
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

/// A build without the `liteinst` feature has no LiteInst backend, so
/// `--backend liteinst` must refuse before any guest runs and say which build
/// flag is missing -- not fall back to another backend, and not blame the host.
#[test]
#[cfg(not(feature = "liteinst"))]
fn run_liteinst_without_the_feature_refuses_and_names_the_flag() {
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--strict",
        "--",
        "/bin/echo",
        "liteinst-guest-ran",
    ];
    let output = hermit(&args);
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("this build was compiled without the liteinst backend"),
        "{stderr}"
    );
    assert!(stderr.contains("--features liteinst"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("liteinst-guest-ran"),
        "the guest must not run under a backend this build does not have: {output:?}"
    );
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
        "--backend",
        "dbt",
        "run",
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
        let directory = tempfile::Builder::new()
            .prefix("dbt-log-env-verify-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create DBT log-env verification directory")
            .keep();
        eprintln!(
            "DBT log-env verification artifacts retained at {}",
            directory.display()
        );
        let logs = directory.join("logs");
        fs::create_dir(&logs).expect("failed to create DBT log-env verification log directory");
        let verdict = directory.join("verdict.json");
        let expected_arg = format!("expect={expected}");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .args([
                "--log",
                "INFO",
                "--backend",
                "dbt",
                "run",
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
        let capture = directory.join("captured-run2.log");
        let (output, run2_log) = output_capturing_run2_log(&mut command, &logs, &capture);
        let report = read_terminal_dbt_verdict(&verdict);
        assert_eq!(
            output.status.success(),
            report["verified"] == true,
            "process status disagrees with terminal verdict: {report}"
        );
        let mut retained_logs = fs::read_dir(&logs)
            .expect("failed to read retained DBT log-env verification logs")
            .map(|entry| entry.expect("failed to read retained log entry").path())
            .collect::<Vec<_>>();
        retained_logs.sort();
        // `--keep-logs` keeps only run 1's golden log after a match, and both
        // logs after a divergence.
        let expected_sides: &[&str] = if report["verdict"] == "matched" {
            &["run1_log_"]
        } else {
            &["run1_log_", "run2_log_"]
        };
        assert_eq!(
            retained_logs.len(),
            expected_sides.len(),
            "unexpected logs for verdict {}: {retained_logs:?}",
            report["verdict"]
        );
        for (log, side) in retained_logs.iter().zip(expected_sides) {
            assert!(
                log.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(side)),
                "expected a {side} capture, found {log:?}"
            );
        }
        // The helper links run 2's log while the command runs, and once more
        // after it exits, which finds the log a divergence retains. A missing
        // capture fails here whatever the verdict.
        let run2_log = run2_log.expect("run 2's log was not captured while the command ran");
        // After a match Hermit deletes run 2's log, so the link made while the
        // command ran is checked in its place. The match compares only INFO
        // records, and leaked guest stdout could be in a record of another level.
        let mut checked_logs = retained_logs;
        if report["verdict"] == "matched" {
            checked_logs.push(run2_log.clone());
        }
        for log in checked_logs {
            let contents = fs::read_to_string(&log).expect("failed to read DBT verification log");
            assert!(contents.contains("INFO detcore"), "empty INFO log: {log:?}");
            assert!(
                !contents.contains("hermit_log="),
                "guest stdout leaked into DBT diagnostics: {log:?}"
            );
        }
        fs::remove_file(run2_log).expect("failed to remove run 2's checked log");
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
    let args = ["--backend", "dbt", "run", "--verify", "--", program];
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
        "--backend",
        "dbt",
        "run",
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
        "--backend=kvm",
        "run",
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
        let sigchld_deliveries = count_handled_inbound_signal(&log, "SIGCHLD");
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

fn count_handled_inbound_signal(log: &str, signal: &str) -> usize {
    log.lines()
        .filter(|line| {
            line.contains("handling inbound signal (#") && line.trim_end().ends_with(signal)
        })
        .count()
}

#[test]
fn handled_inbound_signal_count_rejects_legacy_alarm_and_other_signals() {
    let log = concat!(
        "INFO detcore::scheduler: Alarm fired, delivering signal SIGCHLD to guest.\n",
        "INFO detcore: [dtid 3] handling inbound signal (#0) SIGALRM\n",
        "INFO detcore: [dtid 3] finish delivering signal (#0) SIGCHLD\n",
        "INFO detcore: [dtid 3] handling inbound signal (#1) SIGCHLD\n",
    );
    assert_eq!(count_handled_inbound_signal(log, "SIGCHLD"), 1);
}

#[test]
fn run_kvm_self_sigkill_from_nonleader_is_group_fatal() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM self-SIGKILL regression: /dev/kvm is unavailable");
        return;
    }

    let _guard = hermit_run_guard();
    let program = kvm_exact_child_waits_guest()
        .to_str()
        .expect("self-SIGKILL guest path should be UTF-8");
    let args = [
        "--backend=kvm",
        "run",
        "--verify",
        "--verify-strict",
        "--",
        program,
        "self-sigkill",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(
        stdout(&output),
        "thread-kill: signalled sig=9 core=0\n\
         thread-tkill: signalled sig=9 core=0\n\
         thread-tgkill: signalled sig=9 core=0\n\
         failures=0\n"
    );
    let log = stderr(&output);
    assert!(
        log.contains(":: comparison=BitwiseInfoV1 relaxations=none"),
        "KVM self-SIGKILL comparison was not canonical:\n{log}"
    );
    assert!(
        log.contains(":: Success: deterministic. Determinism verified."),
        "KVM self-SIGKILL verification failed:\n{log}"
    );
    assert!(
        log.contains(":: Backend: KVM (reverie-kvm KvmGuest<Detcore>)"),
        "KVM self-SIGKILL regression did not use the KVM backend:\n{log}"
    );
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "dbt",
        "run",
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
        "--backend",
        "kvm",
        "run",
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

#[test]
fn run_kvm_gettimeofday_invalid_tv_returns_efault_and_guest_continues() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let program = kvm_gettimeofday_efault_guest()
        .to_str()
        .expect("KVM gettimeofday EFAULT guest path should be UTF-8");
    let args = [
        "--backend",
        "kvm",
        "run",
        "--strict",
        "--",
        program,
        "invalid-tv",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "invalid-tv: EFAULT\n");
}

#[test]
fn run_kvm_gettimeofday_faulting_tz_returns_efault_without_tool_error() {
    if !Path::new("/dev/kvm").exists() {
        return;
    }

    let program = kvm_gettimeofday_efault_guest()
        .to_str()
        .expect("KVM gettimeofday EFAULT guest path should be UTF-8");
    let args = [
        "--backend",
        "kvm",
        "run",
        "--strict",
        "--",
        program,
        "faulting-tz",
    ];
    let output = hermit(&args);

    assert_success(&output, &args);
    assert_eq!(stdout(&output), "faulting-tz: EFAULT tv=0.000000\n");
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
        "--strict",
        "--verify",
        "--base-env=minimal",
        "--epoch=2026-01-01T00:00:00Z",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        .arg(repository.join("tests/c/cpuid_probe.c"))
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
        "--backend",
        "kvm",
        "run",
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
            "--backend",
            "kvm",
            "run",
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
            "--backend",
            "kvm",
            "run",
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
            "--backend",
            "kvm",
            "run",
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
            "--backend",
            "kvm",
            "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
            "--backend",
            backend,
            "run",
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
        "--backend",
        "kvm",
        "run",
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
    // A run that samples its epoch from the host reports it on stderr, so pin
    // the epoch and require exactly that one diagnostic line and nothing else.
    let epoch = "2026-01-01T00:00:00.123456789+00:00";
    let epoch_arg = format!("--epoch={epoch}");
    let expected_stderr = format!(
        "hermit: virtual-time epoch={epoch} source=explicit; reproduce with --epoch={epoch}\n"
    );
    for backend in ["ptrace", "kvm"] {
        let args = [
            "--backend",
            backend,
            "run",
            "--strict",
            "--base-env=minimal",
            &epoch_arg,
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
        assert_eq!(stderr(&output), expected_stderr, "{backend} {mode}");
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
            "--backend",
            backend,
            "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
        "--backend=kvm",
        "run",
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
        "--backend=ptrace",
        "run",
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
        "--backend",
        "kvm",
        "run",
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
            "--backend",
            backend,
            "run",
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
        "--backend",
        "sabre",
        "run",
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
        "--backend",
        "sabre",
        "run",
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

#[test]
fn run_ptrace_nonleader_exec_preserves_identity_and_time() {
    nonleader_exec::run();
}

#[test]
fn run_ptrace_nonleader_exec_exit_only() {
    nonleader_exec::run_exit_only();
}

#[test]
fn run_ptrace_nonleader_exec_preserves_preemption() {
    nonleader_exec::run_preempted();
}

#[test]
fn run_ptrace_nonleader_exec_displaces_runnable_leader() {
    nonleader_exec::run_runnable_leader();
}

/// The panic of <https://github.com/rrnewton/hermit/issues/3413>, end to end.
///
/// A `--chaos` recording carries per-thread preemption points: timeslice ends
/// at absolute virtual instants, i.e. offsets from the recording's epoch. A
/// replay that omits `--epoch` used to sample a fresh host-clock epoch, so every
/// recorded end was already in the past and detcore panicked with "Cannot set
/// end of timeslice ... when current thread logical time is already ...". The
/// replay now starts from the recorded epoch
/// (<https://github.com/rrnewton/hermit/issues/3411>) and reproduces the whole
/// clock trajectory the recording printed.
///
/// PMU SUBJECT: chaos preemption points come from the PMU timer, so without a
/// PMU the recording has none and this case proves nothing. test.cli and
/// test.cli_on_host skip it by exact name; privileged-test.pmu_cli_cases runs
/// it after privileged-pmu.preemption has shown the PMU works. The PMU-free
/// epoch-adoption and refusal cases live in hermit-cli/tests/clock_determinism.rs.
#[test]
fn run_chaos_preemption_replay_reuses_the_recorded_epoch() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let record = directory.path().join("chaos-preemptions.json");
    let guest = replay_epoch_guest().to_str().unwrap();
    let record_arg = format!("--record-preemptions-to={}", record.display());
    let replay_arg = format!("--replay-preemptions-from={}", record.display());
    let recording_args = [
        "run",
        "--base-env=minimal",
        "--chaos",
        "--seed=3",
        "--epoch=2000-12-31T23:59:59.123456789Z",
        &record_arg,
        "--",
        guest,
    ];
    // Both runs must see only the epochs this test gives them: a harness that
    // pins HERMIT_EPOCH would otherwise supply the replay's epoch too.
    let recorded = hermit_command(&recording_args)
        .env_remove("HERMIT_EPOCH")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    assert_success(&recorded, &recording_args);

    // The premise: real preemption points, at absolute instants after the epoch.
    let epoch_nanos: u64 = 978_307_199_123_456_789;
    let json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    assert_eq!(json["epoch"], "2000-12-31T23:59:59.123456789Z", "{json}");
    let ends: Vec<u64> = json["per_thread"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|thread| thread["prio_changes"].as_array().unwrap().clone())
        .map(|change| change[0].as_u64().unwrap())
        .collect();
    assert!(
        !ends.is_empty(),
        "the chaos recording has no preemption points, so this case cannot \
         reach the replayed-timeslice path: {json}"
    );
    assert!(ends.iter().all(|end| *end > epoch_nanos), "{ends:?}");

    let replay_args = [
        "run",
        "--base-env=minimal",
        "--chaos",
        "--seed=3",
        &replay_arg,
        "--",
        guest,
    ];
    let replayed = hermit_command(&replay_args)
        .env_remove("HERMIT_EPOCH")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let replay_stderr = stderr(&replayed);
    assert!(
        !replay_stderr.contains("Cannot set end of timeslice"),
        "{replay_stderr}"
    );
    assert_success(&replayed, &replay_args);
    assert!(
        replay_stderr
            .contains("virtual-time epoch=2000-12-31T23:59:59.123456789+00:00 source=recording"),
        "{replay_stderr}"
    );
    assert_eq!(stdout(&replayed), stdout(&recorded));
    assert_eq!(
        stdout(&recorded)
            .lines()
            .filter(|line| line.starts_with("sample "))
            .count(),
        8
    );
}

#[test]
fn run_ptrace_nonleader_exec_refuses_preemption_artifacts() {
    nonleader_exec::run_preemption_artifacts();
}

#[test]
fn run_kvm_nonleader_exec_is_policy_refusal() {
    kvm_nonleader_exec::run();
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
    let epoch = "2026-01-01T00:00:00.123456789+00:00";
    let epoch_arg = format!("--epoch={epoch}");

    // `--max-timeslice=disabled` keeps the exact stderr comparison below about
    // the log destination: without it, a host lacking accessible PMU counters
    // prints the timeslice downgrade warning, which is unrelated to --log-file.
    let output = hermit(&[
        "--log=info",
        "--log-file",
        log.to_str().unwrap(),
        "run",
        "--max-timeslice=disabled",
        &epoch_arg,
        "--",
        "/bin/sh",
        "-c",
        "printf 'guest-stderr-control\n' >&2",
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
    let diagnostics = std::fs::read_to_string(&log).unwrap();
    assert!(diagnostics.contains(&format!(
        "hermit: virtual-time epoch={epoch} source=explicit; reproduce with --epoch={epoch}\n"
    )));
    assert_eq!(output.stderr, b"guest-stderr-control\n");
}

/// One `hermit --log=<level> --log-file=<log> run` of `/bin/true` at a pinned
/// epoch, for the log-diff tests below.
fn log_true_run(log: &Path, level: &str, epoch: &str) {
    let log_arg = format!("--log-file={}", log.display());
    let level_arg = format!("--log={level}");
    let epoch_arg = format!("--epoch={epoch}");
    let args = [
        level_arg.as_str(),
        log_arg.as_str(),
        "run",
        "--base-env=minimal",
        "--max-timeslice=disabled",
        epoch_arg.as_str(),
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    assert_success(&output, &args);
}

fn log_diff(args: &[&str]) -> Output {
    hermit_command(&[&["log-diff"], args].concat())
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit log-diff")
}

/// The log files of two separate runs are comparable
/// (<https://github.com/rrnewton/hermit/issues/3410>). The virtual-time epoch
/// notice is the first thing a run writes to `--log-file`; as a bare line it
/// made `hermit log-diff` refuse every such pair at "log line 0 has no
/// ERROR/WARN/INFO/DEBUG/TRACE tag". It is now a DEBUG record: the file
/// parses, and both the default DETLOG/COMMIT comparison and the canonical INFO
/// comparison accept two runs at one epoch. (It is deliberately outside both
/// selections; see `log_diff_does_not_count_the_epoch_notice_as_evidence`.)
#[test]
fn log_diff_compares_the_log_files_of_two_separate_runs() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let epoch = "2026-01-01T00:00:00.123456789+00:00";
    let logs = [
        directory.path().join("a.log"),
        directory.path().join("b.log"),
    ];
    let notice = format!(
        " DEBUG hermit::controller: hermit: virtual-time epoch={epoch} source=explicit; \
         reproduce with --epoch={epoch}\n"
    );
    let first_record = regex::Regex::new(&format!(
        r"^\d{{4}}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d+Z{}",
        regex::escape(&notice)
    ))
    .unwrap();
    for log in &logs {
        log_true_run(log, "info", epoch);
        let contents = std::fs::read_to_string(log).unwrap();
        assert!(first_record.is_match(&contents), "{contents}");
    }
    let (a, b) = (logs[0].to_str().unwrap(), logs[1].to_str().unwrap());
    for options in [&[][..], &["--canonical-info"][..]] {
        let output = log_diff(&[options, &[a, b]].concat());
        let stderr = stderr(&output);
        assert_eq!(output.status.code(), Some(0), "{options:?}: {stderr}");
        assert!(!stderr.contains("has no ERROR/WARN/INFO"), "{stderr}");
        assert!(!stderr.contains("no comparable"), "{stderr}");
    }
}

/// The epoch notice is harness context, not guest or Detcore evidence, so it
/// must never make a comparison non-vacuous. At the default log level a run's
/// `--log-file` holds nothing but the notice; two such logs must still be
/// refused as having no comparable messages in every mode, as they were before
/// the notice was a parseable record
/// (review of <https://github.com/rrnewton/hermit/pull/3427>).
#[test]
fn log_diff_does_not_count_the_epoch_notice_as_evidence() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let logs = [
        directory.path().join("a.log"),
        directory.path().join("b.log"),
    ];
    for log in &logs {
        let log_arg = format!("--log-file={}", log.display());
        let args = [
            log_arg.as_str(),
            "run",
            "--base-env=minimal",
            // Without CPUID faulting Reverie logs one ERROR record per exec;
            // not asking for CPUID interception keeps the notice the only
            // record on every host.
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--epoch=2026-01-01T00:00:00Z",
            "--",
            "/bin/true",
        ];
        let output = hermit_command(&args)
            .env_remove("HERMIT_LOG")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        assert_success(&output, &args);
        let contents = std::fs::read_to_string(log).unwrap();
        // The premise: the notice is the log's only record.
        assert_eq!(contents.lines().count(), 1, "{contents}");
        assert!(contents.contains("hermit::controller: hermit: virtual-time epoch="));
    }
    let (a, b) = (logs[0].to_str().unwrap(), logs[1].to_str().unwrap());
    let report = directory.path().join("report.json");
    let report_arg = report.to_str().unwrap();
    for options in [
        &[a, b][..],
        &["--canonical-info", a, b][..],
        &["--json", report_arg, a, b][..],
        &[a][..],
    ] {
        let output = log_diff(options);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{options:?} must refuse two logs without evidence: {}{}",
            stdout(&output),
            stderr(&output)
        );
    }
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).unwrap()).unwrap();
    assert_eq!(json["verdict"], "no_comparable_messages", "{json}");
}

/// Relaxed comparisons of two processes' logs canonicalize marked host
/// addresses when asked (<https://github.com/rrnewton/hermit/issues/3412>).
/// `--canonical-info` and `--json` always did; the options hermit-verify uses
/// (`--include-detlogs=...`) could not, so ASLR-varying launcher pointers such
/// as `syscall.intercept{... syscall=execve args=... <hostaddr 0x...>}` made
/// every trace-level comparison diverge.
///
/// The second log is the first with every marked address shifted by the same
/// amount -- an ASLR move with identical structure -- so the premise does not
/// depend on the host's ASLR. Without the flag it diverges (the addresses are
/// compared raw); with it the logs agree; and breaking one aliasing relation
/// still diverges under the flag, because identity is kept.
#[test]
fn relaxed_log_diff_canonicalizes_marked_host_addresses_on_request() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let original = directory.path().join("original.log");
    log_true_run(&original, "trace", "2026-01-01T00:00:00Z");
    let contents = std::fs::read_to_string(&original).unwrap();
    let marker = regex::Regex::new(r"<hostaddr 0x([0-9a-f]+)>").unwrap();
    let marked: std::collections::BTreeSet<&str> = marker
        .captures_iter(&contents)
        .map(|capture| capture.get(1).unwrap().as_str())
        .collect();
    assert!(
        marked.len() >= 2,
        "a trace-level run should mark several launcher pointers: {marked:?}"
    );

    let shifted = marker.replace_all(&contents, |capture: &regex::Captures<'_>| {
        let value = u64::from_str_radix(&capture[1], 16).unwrap();
        format!("<hostaddr {:#x}>", value + 0x10_0000)
    });
    let moved = directory.path().join("moved.log");
    std::fs::write(&moved, shifted.as_bytes()).unwrap();

    // In the first compared (DETLOG) record that carries a marked address,
    // give that ONE occurrence a value of its own, breaking its aliasing with
    // every other use of the same address.
    let line = contents
        .lines()
        .find(|line| line.contains("DETLOG") && marker.is_match(line))
        .expect("a DETLOG record carries a marked launcher pointer");
    let dealiased = contents.replacen(line, &marker.replace(line, "<hostaddr 0x1>"), 1);
    assert_ne!(dealiased, contents);
    let broken = directory.path().join("dealiased.log");
    std::fs::write(&broken, dealiased.as_bytes()).unwrap();

    // The DETLOG selection hermit-verify's run comparison passes.
    let relaxed = [
        "--include-detlogs=other",
        "--include-detlogs=syscallresult",
        "--include-detlogs=syscall",
        "--syscall-history=5",
    ];
    let (original, moved, broken) = (
        original.to_str().unwrap(),
        moved.to_str().unwrap(),
        broken.to_str().unwrap(),
    );
    let raw = log_diff(&[&relaxed[..], &[original, moved]].concat());
    // EXIT-CLASS: hermit (log-diff reports a divergence)
    assert_eq!(raw.status.code(), Some(1), "raw: {}", stderr(&raw));
    let flag = "--canonicalize-host-addresses";
    let canonical = log_diff(&[&relaxed[..], &[flag, original, moved]].concat());
    assert_eq!(
        canonical.status.code(),
        Some(0),
        "canonical: {}{}",
        stdout(&canonical),
        stderr(&canonical)
    );
    let dealiased = log_diff(&[&relaxed[..], &[flag, original, broken]].concat());
    // EXIT-CLASS: hermit (log-diff reports a divergence)
    assert_eq!(dealiased.status.code(), Some(1), "{}", stderr(&dealiased));
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
/// Staging the LiteInst runtime works in a source tree with no git metadata
/// (<https://github.com/rrnewton/hermit/issues/3419>). The fbsource Buck import
/// runs tests from such a tree, and `stage-liteinst-runtime.sh` used to derive
/// the pin through `ci/run-reverie-pin-check.sh --print-pin`, which lists
/// tracked files with git, so `run_liteinst_verifies_detcore_backend` failed
/// with "fatal: not a git repository". The test helper now hands the script
/// the pin embedded in the Hermit binary under test.
///
/// This drives the helper's own staging command with git made unreachable (a
/// stand-in `git` first on PATH, and `GIT_DIR` naming nothing) and a stand-in
/// `cargo` that writes the runtime, so only the pin plumbing is under test. The control run without the supplied
/// pin shows the premise: the git-based pin lookup fails here.
#[test]
fn liteinst_runtime_staging_does_not_require_a_git_checkout() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let stand_in_cargo = directory.path().join("cargo");
    fs::write(
        &stand_in_cargo,
        "#!/bin/sh\nprintf 'staged by a stand-in cargo\\n' > \"$HERMIT_LITEINST_STAGE\"\n",
    )
    .unwrap();
    fs::set_permissions(&stand_in_cargo, fs::Permissions::from_mode(0o755)).unwrap();
    // Make git unreachable for every tool, not only those that honour GIT_DIR:
    // a stand-in `git` first on PATH answers as git does outside a repository.
    let no_git_bin = directory.path().join("no-git-bin");
    fs::create_dir(&no_git_bin).unwrap();
    let stand_in_git = no_git_bin.join("git");
    fs::write(
        &stand_in_git,
        "#!/bin/sh\necho 'fatal: not a git repository (stand-in)' >&2\nexit 128\n",
    )
    .unwrap();
    fs::set_permissions(&stand_in_git, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        no_git_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let no_git = directory.path().join("no-git-metadata");
    // `None` keeps the helper's own pin, which is what this test is about.
    // `Some("")` removes it; any other value replaces it.
    let stage = |runtime: &Path, pin: Option<&str>| {
        let mut command = liteinst_runtime::liteinst_stage_command(runtime);
        command
            .env("CARGO", &stand_in_cargo)
            .env("GIT_DIR", &no_git)
            .env("PATH", &path);
        match pin {
            None => {}
            Some("") => {
                command.env_remove("HERMIT_LITEINST_REVERIE_PIN");
            }
            Some(pin) => {
                command.env("HERMIT_LITEINST_REVERIE_PIN", pin);
            }
        }
        command
            .output()
            .expect("failed to run stage-liteinst-runtime.sh")
    };

    let runtime = directory.path().join("staged/libreverie_liteinst.so");
    let output = stage(&runtime, None);
    assert!(
        output.status.success(),
        "staging without git failed:\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&runtime).unwrap(),
        "staged by a stand-in cargo\n"
    );
    assert_eq!(
        fs::read_to_string(format!("{}.revision", runtime.display())).unwrap(),
        format!("{}\n", env!("HERMIT_REVERIE_PIN"))
    );

    // A supplied pin must be a revision, and must be the one
    // liteinst-runtime-build builds from; either refusal stages nothing.
    for (pin, reason) in [
        (
            "1111111111111111111111111111111111111111",
            "liteinst-runtime-build builds Reverie at",
        ),
        ("not-a-revision", "must be a 40-hex Reverie revision"),
    ] {
        let refused_runtime = directory
            .path()
            .join(format!("refused-{}/libreverie_liteinst.so", &pin[..3]));
        let refused = stage(&refused_runtime, Some(pin));
        // EXIT-CLASS: hermit (the staging script's usage refusal)
        assert_eq!(
            refused.status.code(),
            Some(2),
            "{pin}: {}",
            stderr(&refused)
        );
        assert!(
            stderr(&refused).contains(reason),
            "{pin}: {}",
            stderr(&refused)
        );
        assert!(!refused_runtime.exists(), "{pin} staged a runtime");
    }

    let control = stage(
        &directory.path().join("control/libreverie_liteinst.so"),
        Some(""),
    );
    assert!(
        !control.status.success(),
        "control: the git-based pin lookup unexpectedly worked without git:\n{}",
        stderr(&control)
    );
    assert!(
        // "fatal: not a git repository" from the stand-in git, or the
        // uniformity checker's "not inside a git repository".
        stderr(&control).contains("git repository"),
        "control: {}",
        stderr(&control)
    );
}

/// The `hermit-dap` binary, when this build has one.
///
/// Cargo always builds it alongside `hermit`, so under Cargo these tests always
/// run. A build that does not ship it -- the fbsource Buck import builds no
/// hermit-dap (<https://github.com/rrnewton/hermit/issues/3419>) -- either leaves
/// `CARGO_BIN_EXE_hermit-dap` unset or points it at a stand-in so the crate
/// compiles; neither is the DAP adapter, and a failure against it says nothing
/// about hermit-dap. Setting `HERMIT_REQUIRE_DAP` turns that skip back into a
/// failure, so a job that means to cover hermit-dap cannot silently stop.
fn hermit_dap_binary(test: &str) -> Option<&'static Path> {
    let path = option_env!("CARGO_BIN_EXE_hermit-dap").map(Path::new);
    let Some(reason) = hermit_dap_unavailable_reason(path) else {
        return path;
    };
    assert!(
        std::env::var_os("HERMIT_REQUIRE_DAP").is_none(),
        "HERMIT_REQUIRE_DAP is set, but {reason}, so {test} cannot exercise hermit-dap"
    );
    eprintln!("skipping {test}: {reason}; build hermit-dap to exercise it");
    None
}

/// Why `path` is not a hermit-dap to test, or `None` when it is one.
///
/// A file named `hermit-dap` is ALWAYS tested, whatever it does: it is the
/// build product, and a broken `--help` or a missing file must fail the four
/// tests rather than skip them. Only a differently named stand-in (fbsource maps
/// the variable to /bin/false) is skipped, and then only if it does not identify
/// as hermit-dap either.
fn hermit_dap_unavailable_reason(path: Option<&Path>) -> Option<String> {
    let Some(path) = path else {
        return Some(
            "this build defines no hermit-dap binary (CARGO_BIN_EXE_hermit-dap unset)".to_owned(),
        );
    };
    if path.file_name() == Some(std::ffi::OsStr::new("hermit-dap")) {
        return None;
    }
    let identifies = Command::new(path)
        .arg("--help")
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).starts_with("Usage: hermit-dap ")
        });
    (!identifies).then(|| {
        format!(
            "CARGO_BIN_EXE_hermit-dap names {}, which is neither named nor identifies as hermit-dap",
            path.display()
        )
    })
}

#[test]
fn hermit_dap_skip_never_applies_to_a_binary_named_hermit_dap() {
    // The build product is tested even if it is missing or broken.
    for missing in [
        "/nonexistent/hermit-dap",
        "/bin/hermit-dap-is-not-here/hermit-dap",
    ] {
        assert_eq!(
            hermit_dap_unavailable_reason(Some(Path::new(missing))),
            None
        );
    }
    // Where coverage is required (validation sets HERMIT_REQUIRE_DAP on the
    // test.cli nodes), this build must really provide hermit-dap. A build that
    // does not ship it, such as fbsource's, does not set the variable.
    if std::env::var_os("HERMIT_REQUIRE_DAP").is_some() {
        assert_eq!(
            hermit_dap_unavailable_reason(option_env!("CARGO_BIN_EXE_hermit-dap").map(Path::new)),
            None
        );
    }
    // A stand-in that does not identify, and an unset variable, are skipped.
    assert!(hermit_dap_unavailable_reason(Some(Path::new("/bin/false"))).is_some());
    assert!(hermit_dap_unavailable_reason(None).is_some());
}

#[test]
fn hermit_dap_forwards_remote_settings_to_gdb() {
    let Some(hermit_dap) = hermit_dap_binary("hermit_dap_forwards_remote_settings_to_gdb") else {
        return;
    };
    let output = Command::new(hermit_dap)
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
    let Some(hermit_dap) = hermit_dap_binary("hermit_dap_reports_a_missing_gdb_path") else {
        return;
    };
    let output = Command::new(hermit_dap)
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
    let Some(hermit_dap) = hermit_dap_binary("hermit_dap_help_describes_managed_replay") else {
        return;
    };
    let output = Command::new(hermit_dap)
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
    let Some(hermit_dap) = hermit_dap_binary("hermit_dap_rejects_replay_options_without_replay")
    else {
        return;
    };
    let output = Command::new(hermit_dap)
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

/// Set where a DAP-capable GDB is installed and must be exercised. Validation
/// sets it on `test.cli`, whose pinned hermetic image ships GDB 17.2, so the
/// end-to-end hermit-dap tests below FAIL there instead of skipping when GDB
/// is missing, lacks a DAP interpreter, or is refused by managed replay.
const HERMIT_REQUIRE_DAP_GDB: &str = "HERMIT_REQUIRE_DAP_GDB";

/// The start of managed replay's refusal of a GDB whose DAP internals it
/// cannot hook. The replay tests skip on it like on a GDB with no DAP
/// interpreter: both are host-tool limits, and the pinned validation image,
/// which ships the tested GDB 17.2, sets [`HERMIT_REQUIRE_DAP_GDB`] so that a
/// refusal there fails instead.
const DAP_GDB_REFUSAL: &str = "managed replay does not support this GDB";

/// How long one DAP exchange may take before the test fails rather than hangs.
/// A reverse request restarts the replay and runs it forward, so it gets more.
/// Every wait is also cut off at the end of [`DAP_SESSION_BUDGET`].
const DAP_TIMEOUT: Duration = Duration::from_secs(120);
const DAP_REVERSE_TIMEOUT: Duration = Duration::from_secs(240);

/// How long a whole DAP session may run, from spawning hermit-dap, before
/// scaling by [`dap_wall_timeout_multiplier`].
///
/// Nextest kills a test at 57 s (`.config/nextest.toml`, scaled by the same
/// multiplier) and reports only `(test timed out)`, so a wait longer than that
/// can never fail on its own: the hosted runs
/// <https://github.com/rrnewton/hermit/actions/runs/37134916002> and
/// <https://github.com/rrnewton/hermit/actions/runs/37126714210> lost which
/// request hung (<https://github.com/rrnewton/hermit/issues/3651>). Ending the
/// session 7 s before that kill makes a hang fail with the request it waited
/// for and the DAP transcript. The 7 s cover what a test does before the
/// session starts: compiling the guest and starting the gdbserver or recording
/// the guest. The whole attach test, setup included, takes under 1 s locally.
const DAP_SESSION_BUDGET: Duration = Duration::from_secs(50);

/// The line of `square`'s body in [`DAP_GUEST_SOURCE`], where the tests break.
const DAP_GUEST_BREAK_LINE: u64 = 4;
/// The line in `main` that calls `square`.
const DAP_GUEST_CALL_LINE: u64 = 11;
// ⚠️ THE TWO LINE NUMBERS ABOVE ARE THIS LAYOUT. Keep them in step.
const DAP_GUEST_SOURCE: &str = r#"#include <stdio.h>

static int square(int x) {
  int y = x * x;
  return y;
}

int main(void) {
  int total = 0;
  for (int i = 0; i < 3; i++) {
    total += square(i);
  }
  printf("total=%d\n", total);
  return 0;
}
"#;

/// The GDB hermit-dap would pick (`/usr/bin/gdb`, else `gdb` on `PATH`), when it
/// has a DAP interpreter.
///
/// hermit-dap is only a launcher: everything after `exec` is GDB's DAP server,
/// so these tests need a real one. Where none is installed they skip loudly; a
/// job that sets [`HERMIT_REQUIRE_DAP_GDB`] turns the skip into a failure.
fn dap_capable_gdb(test: &str) -> Option<PathBuf> {
    let candidate = if Path::new("/usr/bin/gdb").is_file() {
        Some(PathBuf::from("/usr/bin/gdb"))
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("gdb"))
                .find(|gdb| gdb.is_file())
        })
    };
    let reason = match &candidate {
        None => "no gdb at /usr/bin/gdb or on PATH".to_owned(),
        Some(gdb) => match Command::new(gdb)
            .args(["--batch", "-nx", "-ex", "python import gdb.dap.server"])
            .output()
        {
            Ok(output) if output.status.success() => return candidate,
            Ok(output) => format!(
                "{} has no DAP interpreter (`python import gdb.dap.server` failed: {})",
                gdb.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => format!("{} could not be run: {error}", gdb.display()),
        },
    };
    dap_gdb_skip(test, &reason);
    None
}

/// Skips `test` loudly, or fails it where [`HERMIT_REQUIRE_DAP_GDB`] is set.
fn dap_gdb_skip(test: &str, reason: &str) {
    assert!(
        std::env::var_os(HERMIT_REQUIRE_DAP_GDB).is_none(),
        "{HERMIT_REQUIRE_DAP_GDB} is set, but {reason}, so {test} cannot drive hermit-dap \
         through a real GDB"
    );
    eprintln!(
        "skipping {test}: {reason}; install a GDB with DAP support (tested with GDB 17.2), \
         or set {HERMIT_REQUIRE_DAP_GDB}=1 to make this a failure"
    );
}

/// Writes and compiles [`DAP_GUEST_SOURCE`] under `directory`, returning the
/// source and program paths. Each test passes its own directory: nextest runs
/// tests in separate processes, so a shared output file would race.
fn dap_guest(directory: &Path) -> (PathBuf, PathBuf) {
    dap_compile(directory, "squares", DAP_GUEST_SOURCE)
}

/// Writes `text` to `directory/name.c` and compiles it, unoptimized and at a
/// fixed address so the tests' line numbers map to stable instructions.
fn dap_compile(directory: &Path, name: &str, text: &str) -> (PathBuf, PathBuf) {
    let source = directory.join(format!("{name}.c"));
    fs::write(&source, text).expect("failed to write the hermit-dap guest source");
    let program = directory.join(name);
    let output = Command::new("cc")
        .args(["-g", "-O0", "-fno-pie", "-no-pie"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("failed to run cc for the hermit-dap guest");
    assert!(
        output.status.success(),
        "hermit-dap guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (source, program)
}

/// A second guest for managed replay, shaped to put two kinds of arrival in
/// the recorded history that [`DAP_GUEST_SOURCE`] does not:
///
/// - `countdown`'s loop branches back to the first instruction of line 4, the
///   address its line breakpoint sits on, so the history holds consecutive
///   arrivals at one pc and one source line, each a separate point in time.
/// - `twice` is written under `#line 2`, so its first statement (line 2) and
///   its opening line (8) resolve to one breakpoint address while GDB reports
///   a stop there as line 2. The line breakpoint that records the arrival is
///   line 8's, so the recorded entry and the stop name different lines.
const DAP_LOOPS_SOURCE: &str = r#"#include <stdio.h>

static int countdown(int n) {
  do { n--; } while (n > 0);
  return n;
}

static int twice(int x) {
#line 2
  return x + x;
#line 12
}

int main(void) {
  int left = countdown(3);
  int sum = twice(1) + twice(2);
  printf("left=%d sum=%d\n", left, sum);
  return 0;
}
"#;
/// `countdown`'s loop, its call in `main`, `twice`'s body (renumbered to 2)
/// and closing brace, and the line in `main` that calls `twice`.
const DAP_LOOPS_COUNTDOWN_LINE: u64 = 4;
const DAP_LOOPS_COUNTDOWN_CALL_LINE: u64 = 15;
const DAP_LOOPS_TWICE_BODY_LINE: u64 = 2;
const DAP_LOOPS_TWICE_END_LINE: u64 = 12;
const DAP_LOOPS_TWICE_CALL_LINE: u64 = 16;
// ⚠️ THE FIVE LINE NUMBERS ABOVE ARE THIS LAYOUT. Keep them in step.

/// The `for` line of [`DAP_GUEST_SOURCE`]'s loop in `main`.
const DAP_GUEST_LOOP_LINE: u64 = 10;

/// Records `program` with `hermit record start` under `data_dir`, requires
/// `expected` in its stdout, and returns the recording id.
fn dap_record(program: &Path, data_dir: &Path, expected: &str) -> String {
    let program_arg = program.to_str().expect("guest path should be UTF-8");
    let recorded = hermit_command(&["record", "start", "--", program_arg])
        .env("HERMIT_DATA_DIR", data_dir)
        .output()
        .expect("failed to run hermit record start");
    assert!(
        recorded.status.success() && String::from_utf8_lossy(&recorded.stdout).contains(expected),
        "hermit record start: {}\nstdout:\n{}\nstderr:\n{}",
        recorded.status,
        String::from_utf8_lossy(&recorded.stdout),
        stderr(&recorded)
    );
    fs::read_to_string(data_dir.join("last"))
        .expect("hermit record start wrote no `last` recording id")
        .trim()
        .to_owned()
}

/// hermit-dap serving `recording` from `data_dir` through `gdb`.
fn dap_replay_adapter(
    hermit_dap: &Path,
    gdb: &Path,
    recording: &str,
    data_dir: &Path,
    port: u16,
) -> Command {
    let mut adapter = Command::new(hermit_dap);
    adapter
        .arg("--gdb")
        .arg(gdb)
        .args(["--replay", recording, "--data-dir"])
        .arg(data_dir)
        .args(["--gdbserver-port", &port.to_string()]);
    adapter
}

/// A loopback port nothing is listening on right now.
fn unused_local_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("failed to find an unused loopback port")
        .port()
}

/// Whether anything in this network namespace listens on TCP `port`.
///
/// Read from /proc rather than by connecting: Hermit's gdbserver accepts one
/// client, and a probe connection would be that client.
fn tcp_port_is_listening(port: u16) -> bool {
    let wanted = format!(":{port:04X}");
    ["/proc/net/tcp", "/proc/net/tcp6"].iter().any(|table| {
        fs::read_to_string(table).is_ok_and(|text| {
            text.lines().skip(1).any(|row| {
                let fields: Vec<&str> = row.split_whitespace().collect();
                fields.len() > 3 && fields[1].ends_with(&wanted) && fields[3] == "0A"
            })
        })
    })
}

/// Kills and reaps the child when dropped, so a failed assertion cannot leave a
/// gdbserver or guest running.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether `message` is a DAP event reporting that the debuggee stopped or
/// ended: the events [`DapClient::stopped`] waits for.
fn is_stop_class_event(message: &serde_json::Value) -> bool {
    message["type"] == "event"
        && matches!(
            message["event"].as_str(),
            Some("stopped" | "terminated" | "exited")
        )
}

/// Appends `message` to `early` when it is a stop-class event.
fn keep_stop_class_event(early: &mut Vec<serde_json::Value>, message: serde_json::Value) {
    if is_stop_class_event(&message) {
        early.push(message);
    }
}

/// A Debug Adapter Protocol client for hermit-dap, keeping a transcript so a
/// failure shows the whole exchange together with the adapter's stderr.
struct DapClient {
    adapter: KillOnDrop,
    stdin: std::process::ChildStdin,
    messages: std::sync::mpsc::Receiver<Result<serde_json::Value, String>>,
    next_seq: u64,
    transcript: Vec<String>,
    stderr_path: PathBuf,
    /// The scaled [`DAP_SESSION_BUDGET`] and the instant it runs out.
    session_budget: Duration,
    session_deadline: Instant,
}

impl DapClient {
    fn spawn(mut command: Command, stderr_path: PathBuf) -> Self {
        let session_budget = DAP_SESSION_BUDGET.mul_f64(dap_wall_timeout_multiplier());
        let session_deadline = Instant::now() + session_budget;
        let stderr =
            fs::File::create(&stderr_path).expect("failed to create the hermit-dap stderr file");
        let mut adapter = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("failed to spawn hermit-dap");
        let stdin = adapter
            .stdin
            .take()
            .expect("hermit-dap stdin should be piped");
        let stdout = adapter
            .stdout
            .take()
            .expect("hermit-dap stdout should be piped");
        let (sender, messages) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = None;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().ok();
                    }
                }
                let message = match length {
                    None => Err("a DAP header block had no Content-Length".to_owned()),
                    Some(length) => {
                        let mut body = vec![0; length];
                        match std::io::Read::read_exact(&mut reader, &mut body) {
                            Err(error) => Err(format!("a DAP body was cut short: {error}")),
                            Ok(()) => serde_json::from_slice(&body).map_err(|error| {
                                format!(
                                    "unparsable DAP message ({error}): {}",
                                    String::from_utf8_lossy(&body)
                                )
                            }),
                        }
                    }
                };
                let stop = message.is_err();
                if sender.send(message).is_err() || stop {
                    return;
                }
            }
        });
        Self {
            adapter: KillOnDrop(adapter),
            stdin,
            messages,
            next_seq: 0,
            transcript: Vec::new(),
            stderr_path,
            session_budget,
            session_deadline,
        }
    }

    /// `timeout`, cut off at the end of the session budget, and what to add
    /// to the failure when the cut-off is what runs out.
    fn session_bound(&self, timeout: Duration) -> (Duration, String) {
        let left = self
            .session_deadline
            .saturating_duration_since(Instant::now());
        if left < timeout {
            let note = format!(
                " (the end of the DAP session's {:?} budget, which is set below \
                 nextest's per-test wall kill)",
                self.session_budget
            );
            (left, note)
        } else {
            (timeout, String::new())
        }
    }

    fn adapter_stderr(&self) -> String {
        fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    fn fail(&self, problem: &str) -> ! {
        panic!(
            "{problem}\nDAP transcript:\n{}\nhermit-dap stderr:\n{}",
            self.transcript.join("\n"),
            self.adapter_stderr()
        )
    }

    fn request(&mut self, command: &str, arguments: serde_json::Value) -> u64 {
        self.next_seq += 1;
        let body = serde_json::json!({
            "seq": self.next_seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        self.transcript.push(format!("-> {body}"));
        let sent = write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len())
            .and_then(|()| self.stdin.flush());
        if let Err(error) = sent {
            self.fail(&format!("could not send {command}: {error}"));
        }
        self.next_seq
    }

    /// The next message `matches` accepts; earlier messages are only logged.
    fn wait_for(
        &mut self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        self.wait_for_noting(what, timeout, matches, |_| {})
    }

    /// Like [`Self::wait_for`], but each earlier message is also handed to
    /// `skipped` after it is logged.
    fn wait_for_noting(
        &mut self,
        what: &str,
        timeout: Duration,
        matches: impl Fn(&serde_json::Value) -> bool,
        mut skipped: impl FnMut(serde_json::Value),
    ) -> serde_json::Value {
        let (bound, cut_off) = self.session_bound(timeout);
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(message)) => {
                    let text = message.to_string();
                    self.transcript
                        .push(format!("<- {}", text.chars().take(600).collect::<String>()));
                    if matches(&message) {
                        return message;
                    }
                    skipped(message);
                }
                Ok(Err(problem)) => self.fail(&format!("waiting for {what}: {problem}")),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.fail(&format!(
                    "timed out after {bound:?} waiting for {what}{cut_off}"
                )),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => self.fail(&format!(
                    "hermit-dap closed its output while waiting for {what}"
                )),
            }
        }
    }

    /// Sends `command` and returns its response, which must report success.
    fn call(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> serde_json::Value {
        self.call_noting(command, arguments, timeout, |_| {})
    }

    /// Like [`Self::call`], but each message before the response is also
    /// handed to `skipped`.
    fn call_noting(
        &mut self,
        command: &str,
        arguments: serde_json::Value,
        timeout: Duration,
        skipped: impl FnMut(serde_json::Value),
    ) -> serde_json::Value {
        let seq = self.request(command, arguments);
        let response = self.wait_for_noting(
            &format!("the {command} response"),
            timeout,
            |message| message["type"] == "response" && message["request_seq"] == seq,
            skipped,
        );
        if response["success"] != true {
            self.fail(&format!("{command} failed: {response}"));
        }
        response
    }

    /// Sends `initialize` and returns its response, successful or not. The
    /// adapter closing its output first fails the test: a startup refusal
    /// must reach the client as this response, not only as stderr text.
    fn initialize_response(&mut self) -> serde_json::Value {
        let seq = self.request(
            "initialize",
            serde_json::json!({
                "adapterID": "hermit",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
            }),
        );
        self.wait_for("the initialize response", DAP_TIMEOUT, |message| {
            message["type"] == "response" && message["request_seq"] == seq
        })
    }

    /// Sends `initialize`, which must succeed, except that a managed-replay
    /// refusal of this GDB is returned as `Err(message)` for the caller to
    /// skip on. Any other failure fails the test.
    fn initialize(&mut self) -> Result<serde_json::Value, String> {
        let response = self.initialize_response();
        if response["success"] == true {
            return Ok(response);
        }
        match response["message"].as_str() {
            Some(message) if message.contains(DAP_GDB_REFUSAL) => Err(message.to_owned()),
            _ => self.fail(&format!("initialize failed: {response}")),
        }
    }

    /// Waits up to `timeout` for the adapter to exit and returns its status.
    fn exit_status(&mut self, timeout: Duration) -> std::process::ExitStatus {
        let (bound, cut_off) = self.session_bound(timeout);
        let deadline = Instant::now() + bound;
        loop {
            match self.adapter.0.try_wait() {
                Ok(Some(status)) => return status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Ok(None) => self.fail(&format!(
                    "hermit-dap did not exit within {bound:?}{cut_off}"
                )),
                Err(error) => self.fail(&format!("failed to poll hermit-dap: {error}")),
            }
        }
    }

    /// Sends `command` for `thread`, which must succeed, and returns the body
    /// of the `stopped` event that follows.
    fn resume(
        &mut self,
        command: &str,
        thread: &serde_json::Value,
        what: &str,
        timeout: Duration,
    ) -> serde_json::Value {
        self.call(command, serde_json::json!({"threadId": thread}), timeout);
        self.stopped(what, timeout)
    }

    /// Attaches to `target`, sets one breakpoint at `line` of `source`, finishes
    /// configuration, and returns the thread of the initial stop.
    ///
    /// GDB 16 and later defer the attach until `configurationDone`, so the
    /// attach stop follows it. GDB 15 runs `target remote` inside the `attach`
    /// request and reports the attach stop during configuration, before
    /// `configurationDone` is even sent. A stop-class event that arrives
    /// during configuration is therefore kept rather than discarded: waiting
    /// for a second stop after it would hang, as it did with GDB 15.1 on the
    /// hosted runner in
    /// <https://github.com/rrnewton/hermit/actions/runs/37134916002>.
    fn attach_and_break(
        &mut self,
        program: &Path,
        target: &str,
        source: &Path,
        line: u64,
    ) -> serde_json::Value {
        let mut early = Vec::new();
        let attach = self.request(
            "attach",
            serde_json::json!({"program": program, "target": target}),
        );
        let initialized = self.wait_for_noting(
            "the initialized event",
            DAP_TIMEOUT,
            |message| {
                message["event"] == "initialized"
                    || (message["type"] == "response" && message["request_seq"] == attach)
            },
            |message| keep_stop_class_event(&mut early, message),
        );
        if initialized["type"] == "response" && initialized["success"] != true {
            self.fail(&format!("attach failed: {initialized}"));
        }
        let breakpoints = self.call_noting(
            "setBreakpoints",
            serde_json::json!({"source": {"path": source}, "breakpoints": [{"line": line}]}),
            DAP_TIMEOUT,
            |message| keep_stop_class_event(&mut early, message),
        );
        // GDB 16 and later answer before `target remote` has run, so the
        // breakpoint is still pending here. That it resolved is proved later,
        // by a `breakpoint` stop at this line.
        if breakpoints["body"]["breakpoints"]
            .as_array()
            .is_none_or(|breakpoints| breakpoints.len() != 1)
        {
            self.fail(&format!(
                "expected one breakpoint for line {line}: {breakpoints}"
            ));
        }
        self.call_noting(
            "configurationDone",
            serde_json::json!({}),
            DAP_TIMEOUT,
            |message| keep_stop_class_event(&mut early, message),
        );
        let stop = match early.as_slice() {
            [] => self.stopped("the stop after attaching", DAP_TIMEOUT),
            [event] if event["event"] == "stopped" && event["body"]["reason"] == "attach" => {
                event["body"].clone()
            }
            events => self.fail(&format!(
                "expected nothing but one `attach` stop before configurationDone \
                 answered, got: {}",
                events
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        };
        if !stop["threadId"].is_u64() {
            self.fail(&format!("the attach stop names no thread: {stop}"));
        }
        stop["threadId"].clone()
    }

    /// The body of the next `stopped` event; the debuggee ending first fails.
    fn stopped(&mut self, what: &str, timeout: Duration) -> serde_json::Value {
        let event = self.wait_for(what, timeout, is_stop_class_event);
        if event["event"] != "stopped" {
            self.fail(&format!("expected {what}, but the debuggee ended: {event}"));
        }
        event["body"].clone()
    }

    /// Requires that `stop` is a `reason` stop of `thread` whose stack starts
    /// with `frames`, each (function, line), and whose innermost frame
    /// evaluates `watch.0` to `watch.1`. Checks the thread through `threads`
    /// and `stackTrace`, the two requests that lost it before the fix.
    fn assert_stop(
        &mut self,
        what: &str,
        stop: &serde_json::Value,
        thread: &serde_json::Value,
        reason: &str,
        frames: &[(&str, u64)],
        watch: (&str, &str),
    ) {
        let (variable, value) = watch;
        if stop["reason"] != reason || stop["threadId"] != *thread {
            self.fail(&format!(
                "{what}: expected a {reason} stop of thread {thread}, got {stop}"
            ));
        }
        let threads = self.call("threads", serde_json::json!({}), DAP_TIMEOUT);
        let listed = threads["body"]["threads"]
            .as_array()
            .is_some_and(|threads| threads.iter().any(|entry| entry["id"] == *thread));
        if !listed {
            self.fail(&format!(
                "{what}: thread {thread} is missing from {threads}"
            ));
        }
        let trace = self.call(
            "stackTrace",
            serde_json::json!({"threadId": thread}),
            DAP_TIMEOUT,
        );
        let stack: Vec<(String, u64, serde_json::Value)> = trace["body"]["stackFrames"]
            .as_array()
            .map(|frames| {
                frames
                    .iter()
                    .map(|frame| {
                        (
                            frame["name"].as_str().unwrap_or_default().to_owned(),
                            frame["line"].as_u64().unwrap_or_default(),
                            frame["id"].clone(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let observed: Vec<(&str, u64)> = stack
            .iter()
            .take(frames.len())
            .map(|(name, line, _)| (name.as_str(), *line))
            .collect();
        if observed != frames {
            self.fail(&format!(
                "{what}: expected the stack to start {frames:?}, got {observed:?}"
            ));
        }
        let evaluated = self.call(
            "evaluate",
            serde_json::json!({"expression": variable, "frameId": stack[0].2, "context": "watch"}),
            DAP_TIMEOUT,
        );
        if evaluated["body"]["result"] != value {
            self.fail(&format!(
                "{what}: expected {variable} = {value}, got {evaluated}"
            ));
        }
    }
}

/// Attaching hermit-dap to `hermit run --gdbserver` must keep the stopped
/// thread usable across `continue`, `threads` and `stackTrace`.
///
/// Before the fix, GDB's DAP server switched threads with `thread N` before
/// every resume and stack walk. GDB checks the thread with the remote `T`
/// (thread-alive) packet, and Reverie's gdbstub had its `T` handler commented
/// out, so it answered with an empty reply. GDB treats any reply other than
/// `OK` as a dead thread. Measured with GDB 17.2 before the fix: the first
/// `continue` failed with "Thread ID 1 has terminated." The fix is in Reverie
/// (<https://github.com/rrnewton/reverie/pull/885>), which answers `T` with
/// `OK` for a live thread and `E01` otherwise.
#[test]
fn hermit_dap_attach_keeps_the_thread_across_continue_and_stack_trace() {
    const TEST: &str = "hermit_dap_attach_keeps_the_thread_across_continue_and_stack_trace";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let (source, program) = dap_guest(work.path());
    let program_arg = program.to_str().expect("guest path should be UTF-8");
    let port = unused_local_port();
    let port_arg = port.to_string();
    let guest_stdout = work.path().join("hermit-run.stdout");
    let guest_stderr = work.path().join("hermit-run.stderr");
    let mut command = hermit_command(&[
        "run",
        "--gdbserver",
        "--gdbserver-port",
        &port_arg,
        "--",
        program_arg,
    ]);
    command
        .stdout(fs::File::create(&guest_stdout).expect("failed to create the run stdout file"))
        .stderr(fs::File::create(&guest_stderr).expect("failed to create the run stderr file"));
    let mut run = KillOnDrop(
        command
            .spawn()
            .expect("failed to spawn hermit run --gdbserver"),
    );

    let deadline = Instant::now() + DAP_TIMEOUT;
    while !tcp_port_is_listening(port) {
        let exited = run.0.try_wait().expect("failed to poll hermit run");
        assert!(
            exited.is_none() && Instant::now() < deadline,
            "hermit run --gdbserver never listened on port {port} (exit: {exited:?})\nstderr:\n{}",
            fs::read_to_string(&guest_stderr).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(50));
    }

    let mut adapter = Command::new(hermit_dap);
    adapter.arg("--gdb").arg(&gdb);
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    if let Err(refusal) = dap.initialize() {
        dap.fail(&format!(
            "attach mode needs no managed-replay hooks: {refusal}"
        ));
    }
    let thread = dap.attach_and_break(
        &program,
        &format!("127.0.0.1:{port}"),
        &source,
        DAP_GUEST_BREAK_LINE,
    );

    // Two continues: the thread must survive the first resume AND the second.
    for x in ["0", "1"] {
        dap.call(
            "continue",
            serde_json::json!({"threadId": thread}),
            DAP_TIMEOUT,
        );
        let what = format!("the breakpoint hit with x = {x}");
        let stop = dap.stopped(&what, DAP_TIMEOUT);
        dap.assert_stop(
            &what,
            &stop,
            &thread,
            "breakpoint",
            &[
                ("square", DAP_GUEST_BREAK_LINE),
                ("main", DAP_GUEST_CALL_LINE),
            ],
            ("x", x),
        );
    }

    // Run the guest to completion: it must finish with its real output.
    dap.call(
        "setBreakpoints",
        serde_json::json!({"source": {"path": source}, "breakpoints": []}),
        DAP_TIMEOUT,
    );
    dap.call(
        "continue",
        serde_json::json!({"threadId": thread}),
        DAP_TIMEOUT,
    );
    dap.wait_for("the guest's exit", DAP_TIMEOUT, |message| {
        message["type"] == "event"
            && matches!(message["event"].as_str(), Some("exited" | "terminated"))
    });
    drop(dap);

    let deadline = Instant::now() + DAP_TIMEOUT;
    let status = loop {
        if let Some(status) = run.0.try_wait().expect("failed to poll hermit run") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "hermit run did not exit after its guest finished\nstderr:\n{}",
            fs::read_to_string(&guest_stderr).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = fs::read_to_string(&guest_stdout).unwrap_or_default();
    assert!(
        status.success() && stdout.contains("total=5"),
        "hermit run --gdbserver: status {status}, stdout {stdout:?}\nstderr:\n{}",
        fs::read_to_string(&guest_stderr).unwrap_or_default()
    );
}

/// Managed replay must step backward and reverse-continue through a recording,
/// landing where the guest really was earlier.
///
/// Before the fix, the replay hook script failed on GDB 17.2 before serving a
/// request: it wrapped `gdb.dap.server.Server.send_event`, which GDB 17.2
/// renamed to `_send_event` (AttributeError). Repairing that exposed further
/// defects that this test also pins: stepBack moved FORWARD in time (lines 3
/// and 4 resolve to one breakpoint address, so each arrival was recorded twice
/// and the occurrence count overshot), and `readelf` output truncated without
/// `--wide` silently dropped every line breakpoint. The frame cache surviving
/// a restart to the replay's entry is pinned by
/// `hermit_dap_replay_steps_back_through_stops_off_line_breakpoints`; this
/// test's restarts all resume the replay, which drops the cache anyway.
///
/// The expected positions are the guest's real history: the second hit of
/// `square` has x = 1, one source line earlier is the call in `main` with
/// i = 1, and the breakpoint hit before that is the first call, x = 0.
#[test]
fn hermit_dap_replay_steps_back_and_reverse_continues_through_a_recording() {
    const TEST: &str = "hermit_dap_replay_steps_back_and_reverse_continues_through_a_recording";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let (source, program) = dap_guest(work.path());
    let data_dir = work.path().join("recordings");
    let recording = dap_record(&program, &data_dir, "total=5");

    let port = unused_local_port();
    let adapter = dap_replay_adapter(hermit_dap, &gdb, &recording, &data_dir, port);
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    let initialized = match dap.initialize() {
        Ok(response) => response,
        Err(refusal) => {
            dap_gdb_skip(TEST, &refusal);
            return;
        }
    };
    if initialized["body"]["supportsStepBack"] != true {
        dap.fail(&format!(
            "managed replay must advertise supportsStepBack: {initialized}"
        ));
    }
    let thread = dap.attach_and_break(
        &program,
        &format!("127.0.0.1:{port}"),
        &source,
        DAP_GUEST_BREAK_LINE,
    );
    let in_square = [
        ("square", DAP_GUEST_BREAK_LINE),
        ("main", DAP_GUEST_CALL_LINE),
    ];

    for x in ["0", "1"] {
        dap.call(
            "continue",
            serde_json::json!({"threadId": thread}),
            DAP_TIMEOUT,
        );
        let what = format!("the breakpoint hit with x = {x}");
        let stop = dap.stopped(&what, DAP_TIMEOUT);
        dap.assert_stop(&what, &stop, &thread, "breakpoint", &in_square, ("x", x));
    }

    dap.call(
        "stepBack",
        serde_json::json!({"threadId": thread}),
        DAP_REVERSE_TIMEOUT,
    );
    let stop = dap.stopped("the stop after stepBack", DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        "stepBack from the second hit",
        &stop,
        &thread,
        "step",
        &[("main", DAP_GUEST_CALL_LINE)],
        ("i", "1"),
    );

    dap.call(
        "reverseContinue",
        serde_json::json!({"threadId": thread}),
        DAP_REVERSE_TIMEOUT,
    );
    let stop = dap.stopped("the stop after reverseContinue", DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        "reverseContinue to the first hit",
        &stop,
        &thread,
        "breakpoint",
        &in_square,
        ("x", "0"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// A managed-replay startup refusal must reach the DAP client as a failed
/// response to its first request, and must also be written to stderr.
///
/// GDB's DAP interpreter points its descriptors 1 and 2 at internal pipes
/// before the replay extension runs, and drains them only once its server
/// loop starts. Before the fix, the extension wrote its refusal with
/// `gdb.write` and exited: measured with GDB 17.2, zero bytes reached either
/// stream, and the client saw only a closed connection.
///
/// Two refusals are driven through a real GDB: a GDB whose DAP module lacks
/// `gdb.dap.frames._clear_frame_ids` (simulated by deleting it in an earlier
/// init command, after GDB has loaded its DAP server), and a PATH with no
/// readelf. The PATH case runs first: if the real GDB is itself refused, the
/// test skips like the other replay tests (or fails under
/// `HERMIT_REQUIRE_DAP_GDB`).
///
/// The PATH refusal is then driven twice more with a client whose first
/// message is not a request: a framed event before `initialize` (skipped, so
/// `initialize` still gets the failed response), and a header block with no
/// Content-Length, which cannot be framed and so gets the refusal as an
/// `output` event instead of nothing.
#[test]
fn hermit_dap_replay_refusal_reaches_the_client() {
    const TEST: &str = "hermit_dap_replay_refusal_reaches_the_client";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    // Nothing is replayed: both refusals happen before the replay starts.
    let data_dir = work.path().join("recordings");

    let empty_path = work.path().join("empty-path");
    fs::create_dir(&empty_path).expect("failed to create an empty PATH directory");
    let mut adapter = dap_replay_adapter(
        hermit_dap,
        &gdb,
        "no-such-recording",
        &data_dir,
        unused_local_port(),
    );
    adapter.env("PATH", &empty_path);
    let mut dap = DapClient::spawn(adapter, work.path().join("no-readelf.stderr"));
    let response = dap.initialize_response();
    let message = response["message"].as_str().unwrap_or_default().to_owned();
    // This GDB's own support check runs before the tool check. If the real GDB
    // is refused, the hookless case below would prove nothing either.
    if message.contains(DAP_GDB_REFUSAL) {
        dap_gdb_skip(TEST, &message);
        return;
    }
    if response["success"] != false
        || !message.contains("managed replay requires readelf and setpriv on PATH")
        || !message.contains("readelf")
        || response["body"]["error"]["showUser"] != true
    {
        dap.fail(&format!(
            "initialize must fail, shown to the user, naming the missing tool: {response}"
        ));
    }
    let status = dap.exit_status(DAP_TIMEOUT);
    if status.success() || !dap.adapter_stderr().contains(&message) {
        dap.fail(&format!(
            "the refused adapter must exit nonzero ({status}) with the refusal on stderr"
        ));
    }

    let hookless_gdb = work.path().join("gdb-without-clear-frame-ids");
    fs::write(
        &hookless_gdb,
        format!(
            "#!/bin/sh\nexec '{}' '--init-eval-command=python import gdb.dap.frames; \
             del gdb.dap.frames._clear_frame_ids' \"$@\"\n",
            gdb.display()
        ),
    )
    .expect("failed to write the GDB wrapper");
    fs::set_permissions(&hookless_gdb, fs::Permissions::from_mode(0o755))
        .expect("failed to make the GDB wrapper executable");
    let adapter = dap_replay_adapter(
        hermit_dap,
        &hookless_gdb,
        "no-such-recording",
        &data_dir,
        unused_local_port(),
    );
    let mut dap = DapClient::spawn(adapter, work.path().join("hookless.stderr"));
    let refusal = match dap.initialize() {
        Err(refusal) => refusal,
        Ok(response) => dap.fail(&format!(
            "a GDB without _clear_frame_ids must be refused: {response}"
        )),
    };
    if !refusal.contains("gdb.dap.frames lacks _clear_frame_ids") {
        dap.fail(&format!(
            "the refusal must name the missing hook: {refusal}"
        ));
    }
    let status = dap.exit_status(DAP_TIMEOUT);
    if status.success() || !dap.adapter_stderr().contains(&refusal) {
        dap.fail(&format!(
            "the refused adapter must exit nonzero ({status}) with the refusal on stderr"
        ));
    }

    let no_readelf = |stderr: &str| {
        let mut adapter = dap_replay_adapter(
            hermit_dap,
            &gdb,
            "no-such-recording",
            &data_dir,
            unused_local_port(),
        );
        adapter.env("PATH", &empty_path);
        DapClient::spawn(adapter, work.path().join(stderr))
    };

    let mut dap = no_readelf("event-first.stderr");
    let event = r#"{"seq":1,"type":"event","event":"hermitTestNotARequest"}"#;
    let sent = write!(dap.stdin, "Content-Length: {}\r\n\r\n{event}", event.len())
        .and_then(|()| dap.stdin.flush());
    if let Err(error) = sent {
        dap.fail(&format!("could not send the leading event: {error}"));
    }
    dap.next_seq = 1;
    let response = dap.initialize_response();
    if response["success"] != false
        || !response["message"]
            .as_str()
            .is_some_and(|text| text.contains(&message))
    {
        dap.fail(&format!(
            "after a leading event, initialize must still get the refusal: {response}"
        ));
    }
    let status = dap.exit_status(DAP_TIMEOUT);
    if status.success() {
        dap.fail(&format!("the refused adapter must exit nonzero ({status})"));
    }

    let mut dap = no_readelf("no-content-length.stderr");
    let sent = write!(dap.stdin, "X-Not-Dap: 1\r\n\r\n{{}}").and_then(|()| dap.stdin.flush());
    if let Err(error) = sent {
        dap.fail(&format!("could not send the unframed header: {error}"));
    }
    let output = dap.wait_for("the refusal as an output event", DAP_TIMEOUT, |incoming| {
        incoming["type"] == "event" && incoming["event"] == "output"
    });
    if !output["body"]["output"]
        .as_str()
        .is_some_and(|text| text.contains(&message))
    {
        dap.fail(&format!(
            "an unframed first message must get the refusal as output: {output}"
        ));
    }
    let status = dap.exit_status(DAP_TIMEOUT);
    if status.success() || !dap.adapter_stderr().contains(&message) {
        dap.fail(&format!(
            "the refused adapter must exit nonzero ({status}) with the refusal on stderr"
        ));
    }
}

/// stepBack must land at the right point in time when the history holds stops
/// at addresses no line breakpoint covers, and reverseContinue with no earlier
/// breakpoint hit must stop at the replay's entry with a usable stack.
///
/// A stepOut lands on the call's return address, and a `next` from there on
/// the loop's increment; neither is the first instruction of a source line,
/// so no line breakpoint records arrivals there. Before the fix, stepBack
/// chose which arrival to restart to by counting the history's entries at the
/// target address, which held only the arrivals the client happened to stop
/// at. Measured before the fix on the sequence below: the second stepBack
/// landed in `main` with i = 0 instead of i = 1.
///
/// The restart to the replay's entry is the one reverse request that does not
/// resume the replay, so GDB's DAP frame cache, which it drops only on resume,
/// still held the killed replay's frames; the extension drops it after each
/// restart.
///
/// The expected positions are the guest's real history: the stepOut from the
/// second call returns to `main` with i = 1, one source line before the third
/// call is the call line with i = 2, and one before that is the return from
/// the second call, which is also one source line before the loop increment
/// that follows it.
#[test]
fn hermit_dap_replay_steps_back_through_stops_off_line_breakpoints() {
    const TEST: &str = "hermit_dap_replay_steps_back_through_stops_off_line_breakpoints";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let (source, program) = dap_guest(work.path());
    let data_dir = work.path().join("recordings");
    let recording = dap_record(&program, &data_dir, "total=5");

    let port = unused_local_port();
    let adapter = dap_replay_adapter(hermit_dap, &gdb, &recording, &data_dir, port);
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    if let Err(refusal) = dap.initialize() {
        dap_gdb_skip(TEST, &refusal);
        return;
    }
    let thread = dap.attach_and_break(
        &program,
        &format!("127.0.0.1:{port}"),
        &source,
        DAP_GUEST_BREAK_LINE,
    );
    let in_square = [
        ("square", DAP_GUEST_BREAK_LINE),
        ("main", DAP_GUEST_CALL_LINE),
    ];
    let at_call = [("main", DAP_GUEST_CALL_LINE)];

    let what = "the first breakpoint hit";
    let stop = dap.resume("continue", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "breakpoint", &in_square, ("x", "0"));

    // No breakpoint hit precedes this one, so reverseContinue restarts the
    // replay and stops at its entry, before main has run.
    let what = "reverseContinue to the replay's entry";
    let stop = dap.resume("reverseContinue", &thread, what, DAP_REVERSE_TIMEOUT);
    if stop["reason"] != "entry" || stop["threadId"] != thread {
        dap.fail(&format!(
            "{what}: expected an entry stop of thread {thread}, got {stop}"
        ));
    }
    let threads = dap.call("threads", serde_json::json!({}), DAP_TIMEOUT);
    if !threads["body"]["threads"]
        .as_array()
        .is_some_and(|threads| threads.iter().any(|entry| entry["id"] == thread))
    {
        dap.fail(&format!(
            "{what}: thread {thread} is missing from {threads}"
        ));
    }
    let trace = dap.call(
        "stackTrace",
        serde_json::json!({"threadId": thread}),
        DAP_TIMEOUT,
    );
    let names: Vec<String> = trace["body"]["stackFrames"]
        .as_array()
        .map(|frames| {
            frames
                .iter()
                .map(|frame| frame["name"].as_str().unwrap_or_default().to_owned())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() || names.iter().any(|name| name == "square" || name == "main") {
        dap.fail(&format!(
            "{what}: the stack must be the fresh replay's, before main: {names:?}"
        ));
    }

    for x in ["0", "1"] {
        let what = format!("the breakpoint hit with x = {x} after the restart");
        let stop = dap.resume("continue", &thread, &what, DAP_TIMEOUT);
        dap.assert_stop(&what, &stop, &thread, "breakpoint", &in_square, ("x", x));
    }
    let what = "stepOut of the second call";
    let stop = dap.resume("stepOut", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &at_call, ("i", "1"));
    let what = "the breakpoint hit with x = 2";
    let stop = dap.resume("continue", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "breakpoint", &in_square, ("x", "2"));

    let what = "stepBack from the third call";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &at_call, ("i", "2"));
    let what = "stepBack to the stepOut's stop";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &at_call, ("i", "1"));

    // The same target with no recorded line arrival after it: stepBack must
    // first run the replay forward to one. Counting to the replay's exit
    // instead would also count the third call's return, and land at i = 2.
    let what = "next to the loop increment";
    let stop = dap.resume("next", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("main", DAP_GUEST_LOOP_LINE)],
        ("i", "1"),
    );
    let what = "stepBack to the second call's return";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &at_call, ("i", "1"));

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// stepBack must count every arrival at an address once, including
/// consecutive arrivals at the same source line and arrivals recorded under a
/// different line than the stop reports.
///
/// The recorded history once dropped an arrival when the previous entry had
/// the same pc and line, which erases all but one pass of a loop whose body
/// starts at its breakpoint address, and merged a stop into the entry just
/// recorded only when both named the same line, which records an arrival
/// twice where two source lines share an address (see [`DAP_LOOPS_SOURCE`]).
/// Either one makes stepBack restart to the wrong arrival.
///
/// The expected positions are the guest's real history: before the loop's
/// third pass (n = 1) is its second (n = 2), and one source line before the
/// second call of `twice` (x = 2) is the end of its first call (x = 1).
#[test]
fn hermit_dap_replay_steps_back_through_repeated_and_shared_addresses() {
    const TEST: &str = "hermit_dap_replay_steps_back_through_repeated_and_shared_addresses";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let (source, program) = dap_compile(work.path(), "loops", DAP_LOOPS_SOURCE);
    let data_dir = work.path().join("recordings");
    let recording = dap_record(&program, &data_dir, "left=0 sum=6");

    let port = unused_local_port();
    let adapter = dap_replay_adapter(hermit_dap, &gdb, &recording, &data_dir, port);
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    if let Err(refusal) = dap.initialize() {
        dap_gdb_skip(TEST, &refusal);
        return;
    }
    let thread = dap.attach_and_break(
        &program,
        &format!("127.0.0.1:{port}"),
        &source,
        DAP_LOOPS_COUNTDOWN_LINE,
    );
    let in_countdown = [
        ("countdown", DAP_LOOPS_COUNTDOWN_LINE),
        ("main", DAP_LOOPS_COUNTDOWN_CALL_LINE),
    ];

    for n in ["3", "2", "1"] {
        let what = format!("the loop's pass with n = {n}");
        let stop = dap.resume("continue", &thread, &what, DAP_TIMEOUT);
        dap.assert_stop(&what, &stop, &thread, "breakpoint", &in_countdown, ("n", n));
    }
    let what = "stepBack to the loop's previous pass";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &in_countdown, ("n", "2"));

    let breakpoints = dap.call(
        "setBreakpoints",
        serde_json::json!({
            "source": {"path": source},
            "breakpoints": [{"line": DAP_LOOPS_TWICE_BODY_LINE}],
        }),
        DAP_TIMEOUT,
    );
    if breakpoints["body"]["breakpoints"][0]["verified"] != true {
        dap.fail(&format!(
            "the breakpoint at line {DAP_LOOPS_TWICE_BODY_LINE} must resolve: {breakpoints}"
        ));
    }
    let in_twice = [
        ("twice", DAP_LOOPS_TWICE_BODY_LINE),
        ("main", DAP_LOOPS_TWICE_CALL_LINE),
    ];
    for x in ["1", "2"] {
        let what = format!("the call of twice with x = {x}");
        let stop = dap.resume("continue", &thread, &what, DAP_TIMEOUT);
        dap.assert_stop(&what, &stop, &thread, "breakpoint", &in_twice, ("x", x));
    }
    let what = "stepBack to the end of the first call of twice";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[
            ("twice", DAP_LOOPS_TWICE_END_LINE),
            ("main", DAP_LOOPS_TWICE_CALL_LINE),
        ],
        ("x", "1"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// The recursive guest of the stepBack test below. `depth(3)` recurses to
/// `depth(0)`; line 5 holds the recursive call and its return address.
const DAP_RECURSION_SOURCE: &str = r#"#include <stdio.h>

static int depth(int n) {
  if (n == 0) return 0;
  int r = depth(n - 1);
  return r + 1;
}

int main(void) {
  int d = depth(3);
  printf("d=%d\n", d);
  return 0;
}
"#;
// ⚠️ THE LINE NUMBERS IN THE TEST BELOW ARE THIS LAYOUT: 4 is the base case,
// 5 the recursive call, 6 `return r + 1;`, 7 the closing brace, 10 main's call.

/// stepBack in recursion must land exactly, in the right activation.
///
/// Stopped in `depth(0)`, two stepOuts reach `depth(2)` at the recursive
/// call's return address, and three nexts walk lines 6 and 7 of `depth(2)` to
/// line 6 of `depth(3)`. The previous stop is line 7 of `depth(2)`. A
/// stepOut from there reaches `depth(3)`'s return address, in the middle of
/// line 5, and the stop before that is again line 7 of `depth(2)`.
///
/// The second stepOut starts at the return address it also targets, and GDB
/// steps over a breakpoint there. Reverie's gdbstub used to restore a whole
/// 8-byte word when it removed a software breakpoint, which erased the line-7
/// breakpoint 6 bytes further on for the rest of that stepOut, so the history
/// missed `depth(1)`'s line 7. Counting arrivals at line 7 then picked
/// `depth(1)`'s: measured before the frame check, stepBack reported a `step`
/// stop at line 7 with n = 1 and four frames, and with the frame check it
/// refused the landing. The gdbstub now restores only the breakpoint's own
/// byte (<https://github.com/rrnewton/reverie/pull/890>), and stepBack must
/// land exactly. Against a Reverie without that fix this test fails.
#[test]
fn hermit_dap_replay_step_back_in_recursion_lands_exactly() {
    const TEST: &str = "hermit_dap_replay_step_back_in_recursion_lands_exactly";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "recursion", DAP_RECURSION_SOURCE, "d=3", None)
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_recursion_walk_to_depth_3_line_6(&mut dap, &thread);

    let at_line_7_of_depth_2 = [("depth", 7), ("depth", 5), ("main", 10)];
    let what = "stepBack to line 7 of depth(2)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &at_line_7_of_depth_2,
        ("n", "2"),
    );

    let what = "stepOut to depth(3)'s return address";
    let stop = dap.resume("stepOut", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("depth", 5), ("main", 10)],
        ("n", "3"),
    );
    let what = "stepBack from the return address to line 7 of depth(2)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &at_line_7_of_depth_2,
        ("n", "2"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// Drives the recursion guest from its attach stop, with a breakpoint on
/// line 4, to line 6 of `depth(3)`, checking every stop on the way: the four
/// base-case checks, two stepOuts to `depth(2)`'s return address, and nexts
/// to lines 6 and 7 of `depth(2)` and line 6 of `depth(3)`.
fn dap_recursion_walk_to_depth_3_line_6(dap: &mut DapClient, thread: &serde_json::Value) {
    dap_recursion_walk_to_depth_2_line_7(dap, thread);
    let what = "next to line 6 of depth(3)";
    let stop = dap.resume("next", thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        thread,
        "step",
        &[("depth", 6), ("main", 10)],
        ("n", "3"),
    );
}

/// The first part of [`dap_recursion_walk_to_depth_3_line_6`]: from the
/// attach stop to line 7 of `depth(2)`.
fn dap_recursion_walk_to_depth_2_line_7(dap: &mut DapClient, thread: &serde_json::Value) {
    dap_recursion_walk_to_depth_2_return(dap, thread);
    // (what, expected frames, expected n) for each `next`.
    type Frames<'a> = &'a [(&'a str, u64)];
    let walk: [(&str, Frames<'_>, &str); 2] = [
        (
            "next to line 6 of depth(2)",
            &[("depth", 6), ("depth", 5), ("main", 10)],
            "2",
        ),
        (
            "next to line 7 of depth(2)",
            &[("depth", 7), ("depth", 5), ("main", 10)],
            "2",
        ),
    ];
    for (what, frames, n) in walk {
        let stop = dap.resume("next", thread, what, DAP_TIMEOUT);
        dap.assert_stop(what, &stop, thread, "step", frames, ("n", n));
    }
}

/// The first part of [`dap_recursion_walk_to_depth_2_line_7`]: from the
/// attach stop, through the four base-case checks, and two stepOuts to
/// `depth(2)` at the recursive call's return address.
fn dap_recursion_walk_to_depth_2_return(dap: &mut DapClient, thread: &serde_json::Value) {
    for n in ["3", "2", "1", "0"] {
        let what = format!("the base-case check with n = {n}");
        let stop = dap.resume("continue", thread, &what, DAP_TIMEOUT);
        dap.assert_stop(
            &what,
            &stop,
            thread,
            "breakpoint",
            &[("depth", 4)],
            ("n", n),
        );
    }
    let what = "stepOut into depth(1)";
    let stop = dap.resume("stepOut", thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        thread,
        "step",
        &[("depth", 5), ("depth", 5), ("depth", 5), ("main", 10)],
        ("n", "1"),
    );
    let what = "stepOut into depth(2)";
    let stop = dap.resume("stepOut", thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        thread,
        "step",
        &[("depth", 5), ("depth", 5), ("main", 10)],
        ("n", "2"),
    );
}

/// hermit-dap serving a fresh recording of `text` (compiled as `name`, whose
/// output must contain `expected`) to a client that has initialized it, with
/// the extension's test-only fault injection set to `drop_line_arrival`
/// when it is given (see [`DAP_TEST_ONLY_DROP_LINE_ARRIVAL`]). Returns
/// `None` after reporting a skip, as the other hermit-dap tests do.
fn dap_replay_session(
    test: &str,
    name: &str,
    text: &str,
    expected: &str,
    drop_line_arrival: Option<&str>,
) -> Option<(DapClient, tempfile::TempDir, PathBuf, PathBuf, u16)> {
    let hermit_dap = hermit_dap_binary(test)?;
    let gdb = dap_capable_gdb(test)?;
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let (source, program) = dap_compile(work.path(), name, text);
    let data_dir = work.path().join("recordings");
    let recording = dap_record(&program, &data_dir, expected);
    let port = unused_local_port();
    let mut adapter = dap_replay_adapter(hermit_dap, &gdb, &recording, &data_dir, port);
    adapter.env_remove(DAP_TEST_ONLY_DROP_LINE_ARRIVAL);
    if let Some(drop) = drop_line_arrival {
        adapter.env(DAP_TEST_ONLY_DROP_LINE_ARRIVAL, drop);
    }
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    if let Err(refusal) = dap.initialize() {
        dap_gdb_skip(test, &refusal);
        return None;
    }
    Some((dap, work, source, program, port))
}

/// The replay extension's test-only fault injection. `LINE:K` makes its line
/// breakpoints leave the K-th arrival they record at source line LINE out of
/// the history, as a debugger stub that loses a breakpoint would, so a test
/// can drive the frame check and its recovery paths without depending on
/// such a stub bug. A stop the client sees is still recorded.
const DAP_TEST_ONLY_DROP_LINE_ARRIVAL: &str = "HERMIT_DAP_TEST_ONLY_DROP_LINE_ARRIVAL";

/// The guest of the sibling-call stepBack tests below: `main` calls `g`
/// twice from the same depth, so both activations of `g` have the same stack
/// pointer. In `g`, line 9 (`return r + 1;`) and line 10 (the closing brace)
/// start 6 bytes apart at -O0.
const DAP_SIBLING_SOURCE: &str = r#"#include <stdio.h>

static int k(int x) {
  return x;
}

static int g(int x) {
  int r = k(x);
  return r + 1;
}

int main(void) {
  int a = g(1);
  int b = g(2);
  printf("s=%d\n", a + b);
  return 0;
}
"#;
// ⚠️ THE LINE NUMBERS IN THE TESTS BELOW ARE THIS LAYOUT: 4 is k's body, 8
// g's call of k, 9 `return r + 1;`, 10 g's closing brace, 13 and 14 the two
// calls of g, 15 the printf, 16 `return 0;`.

/// Drives the sibling-call guest from its attach stop, with a breakpoint on
/// line 4, to the printf after both calls of `g`, checking every stop: into
/// `k` from `g(1)`, out to `g` and to `main`, next to the second call, into
/// `k` from `g(2)`, out to `g`, and nexts over lines 9 and 10 of `g(2)` and
/// back into `main`.
fn dap_sibling_walk_to_the_printf(dap: &mut DapClient, thread: &serde_json::Value) {
    // (request, what, reason, expected frames, (watched expression, value)).
    type Step<'a> = (
        &'a str,
        &'a str,
        &'a str,
        &'a [(&'a str, u64)],
        (&'a str, &'a str),
    );
    let walk: [Step<'_>; 9] = [
        (
            "continue",
            "the stop in k called from g(1)",
            "breakpoint",
            &[("k", 4), ("g", 8), ("main", 13)],
            ("x", "1"),
        ),
        (
            "stepOut",
            "stepOut to g(1)",
            "step",
            &[("g", 8), ("main", 13)],
            ("x", "1"),
        ),
        (
            "stepOut",
            "stepOut to main from g(1)",
            "step",
            &[("main", 13)],
            ("sizeof a", "4"),
        ),
        (
            "next",
            "next to the second call",
            "step",
            &[("main", 14)],
            ("a", "2"),
        ),
        (
            "continue",
            "the stop in k called from g(2)",
            "breakpoint",
            &[("k", 4), ("g", 8), ("main", 14)],
            ("x", "2"),
        ),
        (
            "stepOut",
            "stepOut to g(2)",
            "step",
            &[("g", 8), ("main", 14)],
            ("x", "2"),
        ),
        (
            "next",
            "next to line 9 of g(2)",
            "step",
            &[("g", 9), ("main", 14)],
            ("x", "2"),
        ),
        (
            "next",
            "next to line 10 of g(2)",
            "step",
            &[("g", 10), ("main", 14)],
            ("x", "2"),
        ),
        (
            "next",
            "next to the printf",
            "step",
            &[("main", 15)],
            ("b", "3"),
        ),
    ];
    for (request, what, reason, frames, watch) in walk {
        let stop = dap.resume(request, thread, what, DAP_TIMEOUT);
        dap.assert_stop(what, &stop, thread, reason, frames, watch);
    }
}

/// stepBack between two calls from the same caller at the same depth must
/// land in the right one.
///
/// Stopped at the printf after `g(1)` and `g(2)`, the previous stop is line
/// 10 of `g(2)`. Both activations of `g` have the same stack pointer. Before
/// the Reverie fix (<https://github.com/rrnewton/reverie/pull/890>), GDB
/// stepping over line 9's breakpoint in `g(1)` erased line 10's 6 bytes
/// further on, the history missed `g(1)`'s line 10, and the arrival count
/// picked `g(1)`'s. Measured with a stack-pointer-only frame check: stepBack
/// reported a `step` stop at line 10 called from line 13, x = 1. With the
/// fix, it must land exactly: line 10 called from line 14, x = 2. Against a
/// Reverie without the fix this test fails.
#[test]
fn hermit_dap_replay_step_back_between_sibling_calls_lands_exactly() {
    const TEST: &str = "hermit_dap_replay_step_back_between_sibling_calls_lands_exactly";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "sibling", DAP_SIBLING_SOURCE, "s=5", None)
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_sibling_walk_to_the_printf(&mut dap, &thread);

    let what = "stepBack to line 10 of g(2)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("g", 10), ("main", 14)],
        ("x", "2"),
    );
    let what = "stepBack to line 9 of g(2)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("g", 9), ("main", 14)],
        ("x", "2"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// Sends a reverse request that must fail because Hermit could not land
/// exactly, and returns the `stopped` event that puts the client back at its
/// current stop.
fn dap_refused_reverse_request(
    dap: &mut DapClient,
    command: &str,
    thread: &serde_json::Value,
    what: &str,
) -> serde_json::Value {
    let seq = dap.request(command, serde_json::json!({"threadId": thread}));
    let response = dap.wait_for(
        &format!("the {command} response"),
        DAP_REVERSE_TIMEOUT,
        |message| message["type"] == "response" && message["request_seq"] == seq,
    );
    if response["success"] != false
        || !response["message"]
            .as_str()
            .is_some_and(|text| text.contains("could not reach the earlier stop exactly"))
    {
        dap.fail(&format!(
            "{what}: the {command} must fail and say it could not land exactly: {response}"
        ));
    }
    dap.stopped(what, DAP_REVERSE_TIMEOUT)
}

/// A stepBack whose arrival count is short must refuse to land in a sibling
/// call that has the same stack pointer, and stay at the current stop.
///
/// The test-only fault injection drops the first arrival the line
/// breakpoints record at line 10, `g(1)`'s, which is the history the Reverie
/// bug of [`hermit_dap_replay_step_back_between_sibling_calls_lands_exactly`]
/// produced. Counting arrivals at line 10 then picks `g(1)`'s, whose stack
/// pointer equals `g(2)`'s; only the caller's return address (line 13
/// against line 14) tells them apart. stepBack must fail, put the replay back
/// at the printf with a `stopped` event, and leave a working session.
#[test]
fn hermit_dap_replay_step_back_refuses_a_sibling_call_with_the_same_stack_pointer() {
    const TEST: &str =
        "hermit_dap_replay_step_back_refuses_a_sibling_call_with_the_same_stack_pointer";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "sibling", DAP_SIBLING_SOURCE, "s=5", Some("10:1"))
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_sibling_walk_to_the_printf(&mut dap, &thread);

    let what = "the printf after the refused stepBack";
    let stop = dap_refused_reverse_request(&mut dap, "stepBack", &thread, what);
    dap.assert_stop(what, &stop, &thread, "step", &[("main", 15)], ("b", "3"));
    let what = "next to `return 0` after the refused stepBack";
    let stop = dap.resume("next", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("main", 16)],
        ("a + b", "5"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// The guest of the frame-identity depth test below: `main` calls a chain of
/// seven helpers, `h6` down to `h0`, from two consecutive lines. At line 5 of
/// `h0` the two activations have 8 frames, and the newest 7 (`h0` to `h6`)
/// have the same pc and stack pointer in both; only frame 7, `main`'s return
/// address (line 26 against line 27), differs. Checked with `bt` and
/// `gdb.Frame.read_register("sp")` in GDB 17.2, on the host recorded in
/// docs/TESTING_ENVIRONMENTS.md under "Named measurement hosts".
const DAP_HELPER_CHAIN_SOURCE: &str = r#"#include <stdio.h>

static int h0(int x) {
  int y = x + 1;
  return y;
}
static int h1(int x) {
  return h0(x) + 1;
}
static int h2(int x) {
  return h1(x) + 1;
}
static int h3(int x) {
  return h2(x) + 1;
}
static int h4(int x) {
  return h3(x) + 1;
}
static int h5(int x) {
  return h4(x) + 1;
}
static int h6(int x) {
  return h5(x) + 1;
}
int main(void) {
  int a = h6(1);
  int b = h6(2);
  printf("s=%d\n", a + b);
  return 0;
}
"#;
// ⚠️ THE LINE NUMBERS IN THE TEST BELOW ARE THIS LAYOUT: 4 is h0's first
// statement, 5 `return y;`, 6 h0's closing brace, 8 to 23 the calls in h1 to
// h6, 26 and 27 the two calls of h6.

/// The frames of a stop at `line` of `h0` called from `main`'s line
/// `main_line`, newest first.
fn dap_helper_chain_frames(line: u64, main_line: u64) -> [(&'static str, u64); 8] {
    [
        ("h0", line),
        ("h1", 8),
        ("h2", 11),
        ("h3", 14),
        ("h4", 17),
        ("h5", 20),
        ("h6", 23),
        ("main", main_line),
    ]
}

/// A stepBack whose arrival count is short must refuse to land in an
/// activation that differs from the right one only at frame index 7, which
/// pins the frame identity at 8 frames.
///
/// With a breakpoint on line 4, the client stops in `h0` under the first
/// call of `h6`, continues to `h0` under the second call, and steps to line 5
/// and then line 6. The test-only fault injection drops the first arrival the
/// line breakpoints record at line 5, the first call's, which the client
/// never saw. stepBack from line 6 goes to line 5 of the second call, and the
/// count picks the first call's line 5 instead. The newest 7 frames of the
/// two are equal; only `main`'s return address tells them apart. With an
/// identity of 7 frames or fewer, the landing check passes and stepBack
/// reports success. With 8 it must fail, put the replay back at line 6 with
/// a `stopped` event, and leave a working session.
#[test]
fn hermit_dap_replay_step_back_refuses_an_activation_that_differs_only_at_frame_7() {
    const TEST: &str =
        "hermit_dap_replay_step_back_refuses_an_activation_that_differs_only_at_frame_7";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "chain", DAP_HELPER_CHAIN_SOURCE, "s=17", Some("5:1"))
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    // (request, what, reason, h0 line, main line, (watched expression, value)).
    let walk = [
        (
            "continue",
            "the stop in h0 under the first call",
            "breakpoint",
            4,
            26,
            ("x", "1"),
        ),
        (
            "continue",
            "the stop in h0 under the second call",
            "breakpoint",
            4,
            27,
            ("x", "2"),
        ),
        ("next", "next to line 5", "step", 5, 27, ("y", "3")),
        ("next", "next to line 6", "step", 6, 27, ("y", "3")),
    ];
    for (request, what, reason, line, main_line, watch) in walk {
        let stop = dap.resume(request, &thread, what, DAP_TIMEOUT);
        let frames = dap_helper_chain_frames(line, main_line);
        dap.assert_stop(what, &stop, &thread, reason, &frames, watch);
    }

    let what = "line 6 after the refused stepBack";
    let stop = dap_refused_reverse_request(&mut dap, "stepBack", &thread, what);
    let frames = dap_helper_chain_frames(6, 27);
    dap.assert_stop(what, &stop, &thread, "step", &frames, ("y", "3"));
    let what = "stepOut to h1 after the refused stepBack";
    let stop = dap.resume("stepOut", &thread, what, DAP_TIMEOUT);
    let frames = &dap_helper_chain_frames(0, 27)[1..];
    dap.assert_stop(what, &stop, &thread, "step", frames, ("x", "2"));

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// A refused stepBack from a stop in the middle of a source line must put the
/// replay back at that stop, not end the session.
///
/// The client stops at `depth(3)`'s return address, in the middle of line 5,
/// after the walk of [`hermit_dap_replay_step_back_in_recursion_lands_exactly`].
/// The test-only fault injection drops the first arrival the line breakpoints
/// record at line 7, `depth(0)`'s, so the count for line 7 of `depth(2)`
/// picks another activation and the frame check refuses the landing. Going
/// back to the current stop means counting arrivals at an address no line
/// breakpoint covers, which needs a line arrival after it in the history.
/// Before the fix none was recorded, and the request failed with "no later
/// source line to count arrivals against" and ended the session.
#[test]
fn hermit_dap_replay_refused_step_back_stays_at_a_mid_line_stop() {
    const TEST: &str = "hermit_dap_replay_refused_step_back_stays_at_a_mid_line_stop";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "recursion", DAP_RECURSION_SOURCE, "d=3", Some("7:1"))
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_recursion_walk_to_depth_2_line_7(&mut dap, &thread);
    let at_return = [("depth", 5), ("main", 10)];
    let what = "stepOut to depth(3)'s return address";
    let stop = dap.resume("stepOut", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &at_return, ("n", "3"));

    let what = "the return address after the refused stepBack";
    let stop = dap_refused_reverse_request(&mut dap, "stepBack", &thread, what);
    dap.assert_stop(what, &stop, &thread, "step", &at_return, ("n", "3"));
    let what = "next to line 6 of depth(3) after the refused stepBack";
    let stop = dap.resume("next", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("depth", 6), ("main", 10)],
        ("r", "2"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// A reverse request that can neither land exactly nor go back to the current
/// stop must fail and end the session, saying why, rather than leave the
/// client at an unknown position.
///
/// With a breakpoint on line 6, the client stops there in `depth(2)` and then
/// in `depth(3)`. The test-only fault injection drops the first arrival the
/// line breakpoints record at line 6, `depth(1)`'s, so every count for line 6
/// is one short: reverseContinue lands in `depth(1)` instead of `depth(2)`,
/// and going back to the current stop lands in `depth(2)` instead of
/// `depth(3)`. Both landings fail the frame check. The client must get a
/// failed response, an `output` event naming the request and the frames, and
/// a `terminated` event.
#[test]
fn hermit_dap_replay_reverse_continue_ends_the_session_when_it_cannot_return() {
    const TEST: &str = "hermit_dap_replay_reverse_continue_ends_the_session_when_it_cannot_return";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "recursion", DAP_RECURSION_SOURCE, "d=3", Some("6:1"))
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_recursion_walk_to_depth_2_return(&mut dap, &thread);
    let what = "next to line 6 of depth(2)";
    let stop = dap.resume("next", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("depth", 6), ("depth", 5), ("main", 10)],
        ("n", "2"),
    );
    let breakpoints = dap.call(
        "setBreakpoints",
        serde_json::json!({"source": {"path": source}, "breakpoints": [{"line": 6}]}),
        DAP_TIMEOUT,
    );
    if breakpoints["body"]["breakpoints"][0]["verified"] != true {
        dap.fail(&format!(
            "the breakpoint at line 6 must resolve: {breakpoints}"
        ));
    }
    let what = "the breakpoint at line 6 of depth(3)";
    let stop = dap.resume("continue", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "breakpoint",
        &[("depth", 6), ("main", 10)],
        ("n", "3"),
    );

    let seq = dap.request("reverseContinue", serde_json::json!({"threadId": thread}));
    let response = dap.wait_for(
        "the reverseContinue response",
        DAP_REVERSE_TIMEOUT,
        |message| message["type"] == "response" && message["request_seq"] == seq,
    );
    if response["success"] != false {
        dap.fail(&format!("the reverseContinue must fail: {response}"));
    }
    let output = dap.wait_for(
        "the output event of the failed reverseContinue",
        DAP_REVERSE_TIMEOUT,
        |message| {
            message["type"] == "event"
                && (message["event"] == "output"
                    || matches!(
                        message["event"].as_str(),
                        Some("stopped" | "terminated" | "exited")
                    ))
        },
    );
    let text = output["body"]["output"].as_str().unwrap_or_default();
    if output["event"] != "output"
        || !text.starts_with("hermit-dap: reverseContinue failed: ")
        || !text.contains("has the frames")
    {
        dap.fail(&format!(
            "the failed reverseContinue must first report why, naming the frames: {output}"
        ));
    }
    let ended = dap.wait_for(
        "the terminated event of the failed reverseContinue",
        DAP_REVERSE_TIMEOUT,
        |message| {
            message["type"] == "event"
                && matches!(
                    message["event"].as_str(),
                    Some("stopped" | "terminated" | "exited")
                )
        },
    );
    if ended["event"] != "terminated" {
        dap.fail(&format!(
            "the failed reverseContinue must end the session with terminated: {ended}"
        ));
    }

    let _ = dap.request("disconnect", serde_json::json!({"terminateDebuggee": true}));
}

/// The guest of the exit-path stepBack tests below: after the second call of
/// `g`, `main` calls `exit` in the middle of line 9, so no source line
/// arrival follows the return address of that call.
const DAP_EXIT_SOURCE: &str = r#"#include <stdio.h>
#include <stdlib.h>
static int g(int x) {
  return x + 1;
}
int main(void) {
  printf("start\n");
  int a = g(1);
  exit(g(a) - 3);
}
"#;
// ⚠️ THE LINE NUMBERS IN THE TESTS BELOW ARE THIS LAYOUT: 4 is `g`'s body, 5
// its closing brace, 8 the first call of `g`, 9 the second call and `exit`.

/// Drives the exit guest from its attach stop, with a breakpoint on line 4,
/// into `g(1)` and `g(2)` and out of `g(2)` to its return address in the
/// middle of line 9, checking every stop.
fn dap_exit_walk_to_the_last_return(dap: &mut DapClient, thread: &serde_json::Value) {
    let what = "the stop in g(1)";
    let stop = dap.resume("continue", thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        thread,
        "breakpoint",
        &[("g", 4), ("main", 8)],
        ("x", "1"),
    );
    let what = "the stop in g(2)";
    let stop = dap.resume("continue", thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        thread,
        "breakpoint",
        &[("g", 4), ("main", 9)],
        ("x", "2"),
    );
    let what = "stepOut of g(2) to the middle of line 9";
    let stop = dap.resume("stepOut", thread, what, DAP_TIMEOUT);
    dap.assert_stop(what, &stop, thread, "step", &[("main", 9)], ("a", "2"));
}

/// stepBack from a stop in the middle of a line after which the program
/// only exits must land exactly.
///
/// The client stops at `g(2)`'s return address in the middle of line 9,
/// after which the guest calls `exit`. Before it rewinds, the adapter runs
/// the replay forward to find a later line arrival to count the current stop
/// against; here the program exits first, so it counts the current stop
/// right away. The earlier stop is line 5 of `g(2)`, with x = 2.
#[test]
fn hermit_dap_replay_step_back_before_an_exit_lands_exactly() {
    const TEST: &str = "hermit_dap_replay_step_back_before_an_exit_lands_exactly";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "exit", DAP_EXIT_SOURCE, "start", None)
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_exit_walk_to_the_last_return(&mut dap, &thread);

    let what = "stepBack to line 5 of g(2)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "step",
        &[("g", 5), ("main", 9)],
        ("x", "2"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
}

/// A refused stepBack from a stop in the middle of a line after which the
/// program only exits must put the replay back at that stop.
///
/// The walk is that of [`hermit_dap_replay_step_back_before_an_exit_lands_exactly`].
/// The test-only fault injection drops the first arrival the line
/// breakpoints record at line 5, `g(1)`'s, so the count for line 5 of `g(2)`
/// picks `g(1)`'s and the frame check refuses the landing (the caller's
/// return address is in line 8, not line 9). Going back to the current stop
/// needs its own arrival count. No line arrival follows it, because the
/// program exits, so the adapter must have counted it when its forward run
/// reached the exit; without that count the request fails with "no later
/// source line to count arrivals against" and ends the session. The client
/// must stay at the return address with a working session: a `next` runs
/// the guest to its exit, with status 0 (`g(2) - 3`).
#[test]
fn hermit_dap_replay_refused_step_back_before_an_exit_stays_at_the_stop() {
    const TEST: &str = "hermit_dap_replay_refused_step_back_before_an_exit_stays_at_the_stop";
    let Some((mut dap, _work, source, program, port)) =
        dap_replay_session(TEST, "exit", DAP_EXIT_SOURCE, "start", Some("5:1"))
    else {
        return;
    };
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);
    dap_exit_walk_to_the_last_return(&mut dap, &thread);

    let what = "the return address after the refused stepBack";
    let stop = dap_refused_reverse_request(&mut dap, "stepBack", &thread, what);
    dap.assert_stop(what, &stop, &thread, "step", &[("main", 9)], ("a", "2"));

    dap.call("next", serde_json::json!({"threadId": thread}), DAP_TIMEOUT);
    let ended = dap.wait_for("the guest's exit after next", DAP_TIMEOUT, |message| {
        message["type"] == "event"
            && matches!(
                message["event"].as_str(),
                Some("stopped" | "terminated" | "exited")
            )
    });
    if ended["event"] != "exited" || ended["body"]["exitCode"] != 0 {
        dap.fail(&format!(
            "next from the return address must run the guest to exit status 0: {ended}"
        ));
    }

    let _ = dap.request("disconnect", serde_json::json!({"terminateDebuggee": true}));
}

/// The guest of the deep-recursion test below, recursing `depth` calls deep
/// before it calls `bottom`.
fn dap_deep_source(depth: u32) -> String {
    format!(
        r#"#include <stdio.h>

static int bottom(int n) {{
  return n + 1;
}}

static int depth(int n) {{
  if (n == 0) return bottom(n);
  int r = depth(n - 1);
  return r + 1;
}}

int main(void) {{
  int d = depth({depth});
  printf("d=%d\n", d);
  return 0;
}}
"#
    )
}
// ⚠️ THE LINE NUMBERS IN THE TEST BELOW ARE THIS LAYOUT: 4 is `bottom`'s
// body, 8 the base case and its call of `bottom`, 9 the recursive call.

/// How deep the guest of
/// [`hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly`]
/// recurses.
const DAP_DEEP_DEPTH: u32 = 1000;

/// The bound on that test's `continue` before scaling by
/// [`dap_wall_timeout_multiplier`]; see the test's documentation.
const DAP_DEEP_CONTINUE_BOUND: Duration = Duration::from_secs(20);

/// The runner's wall-clock timeout multiplier, which also scales its 57 s
/// per-test wall kill (`.config/nextest.toml`), read the way
/// `ci/manifest-plan/src/timeouts.rs` reads it: unset is 1, and anything else
/// must be a finite number greater than zero.
fn dap_wall_timeout_multiplier() -> f64 {
    const NAME: &str = "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER";
    match std::env::var(NAME) {
        Err(std::env::VarError::NotPresent) => 1.0,
        Ok(text) => match text.parse::<f64>() {
            Ok(value) if value.is_finite() && value > 0.0 => value,
            _ => panic!("{NAME} must be a finite number greater than zero, got {text:?}"),
        },
        Err(std::env::VarError::NotUnicode(_)) => panic!("{NAME} must be valid UTF-8"),
    }
}

/// The stop `stop` must be a `reason` stop of `thread` whose newest frames
/// are `frames`, each (function, line), and whose newest frame evaluates
/// `watch.0` to `watch.1`. Unlike [`DapClient::assert_stop`], this asks for
/// only as many frames as it checks, because the stack is thousands deep.
fn dap_assert_newest_frames(
    dap: &mut DapClient,
    what: &str,
    stop: &serde_json::Value,
    thread: &serde_json::Value,
    reason: &str,
    frames: &[(&str, u64)],
    watch: (&str, &str),
) {
    if stop["reason"] != reason || stop["threadId"] != *thread {
        dap.fail(&format!(
            "{what}: expected a {reason} stop of thread {thread}, got {stop}"
        ));
    }
    let trace = dap.call(
        "stackTrace",
        serde_json::json!({"threadId": thread, "startFrame": 0, "levels": frames.len()}),
        DAP_TIMEOUT,
    );
    let stack = trace["body"]["stackFrames"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let observed: Vec<(&str, u64)> = stack
        .iter()
        .map(|frame| {
            (
                frame["name"].as_str().unwrap_or_default(),
                frame["line"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    if observed != frames {
        dap.fail(&format!(
            "{what}: expected the newest frames {frames:?}, got {observed:?}"
        ));
    }
    let (variable, value) = watch;
    let evaluated = dap.call(
        "evaluate",
        serde_json::json!({"expression": variable, "frameId": stack[0]["id"], "context": "watch"}),
        DAP_TIMEOUT,
    );
    if evaluated["body"]["result"] != value {
        dap.fail(&format!(
            "{what}: expected {variable} = {value}, got {evaluated}"
        ));
    }
}

/// Runs the deep-recursion guest, `depth` calls deep, to its breakpoint in
/// `bottom` with one `continue` that must take less than `bound`, then steps
/// back twice, landing exactly. Returns how long the `continue` took.
fn dap_deep_recursion_session(test: &str, depth: u32, bound: Duration) -> Option<Duration> {
    let text = dap_deep_source(depth);
    let expected = format!("d={}", depth + 1);
    let (mut dap, _work, source, program, port) =
        dap_replay_session(test, "deep", &text, &expected, None)?;
    let thread = dap.attach_and_break(&program, &format!("127.0.0.1:{port}"), &source, 4);

    let what = "the stop in bottom";
    let start = Instant::now();
    dap.call("continue", serde_json::json!({"threadId": thread}), bound);
    let stop = dap.stopped(what, bound.saturating_sub(start.elapsed()));
    let elapsed = start.elapsed();
    if elapsed >= bound {
        dap.fail(&format!(
            "the continue through {depth} recursive calls took {elapsed:?}, not under {bound:?}"
        ));
    }
    dap_assert_newest_frames(
        &mut dap,
        what,
        &stop,
        &thread,
        "breakpoint",
        &[("bottom", 4), ("depth", 8), ("depth", 9), ("depth", 9)],
        ("n", "0"),
    );

    let what = "stepBack to line 8 of depth(0)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap_assert_newest_frames(
        &mut dap,
        what,
        &stop,
        &thread,
        "step",
        &[("depth", 8), ("depth", 9), ("depth", 9)],
        ("n", "0"),
    );
    let what = "stepBack to line 9 of depth(1)";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap_assert_newest_frames(
        &mut dap,
        what,
        &stop,
        &thread,
        "step",
        &[("depth", 9), ("depth", 9), ("depth", 9)],
        ("n", "1"),
    );

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
    );
    Some(elapsed)
}

/// A forward `continue` through deep recursion must stay fast, and stepBack
/// deep in that recursion must still land exactly.
///
/// The adapter records a frame identity at every line arrival while the
/// replay runs forward. Unwinding every frame for it made that cost the square
/// of the stack depth, so the identity holds only the newest 8 frames. One
/// `continue` from the attach stop to `bottom` passes about 2000 line
/// arrivals at stack depths up to 1000. Measured on the host recorded in
/// docs/TESTING_ENVIRONMENTS.md under "Named measurement hosts" with GDB 17.2
/// and Reverie 4f125805, it took 2.51 s to 2.66 s in eight runs with 8
/// frames, and 63.45 s with every frame. The bound, 20 s (times
/// `HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER` when set), is 7.5 times the slowest
/// capped run and 3.2 times under the uncapped one.
///
/// The validation runner kills a test at 57 s of wall time
/// (`.config/nextest.toml`, also times the wall multiplier) and at 22
/// CPU-seconds counted over the test's cgroup, GDB and Hermit included
/// (`DEFAULT_TEST_CPU_TIMEOUT_SECONDS` in `ci/manifest-plan/src/timeouts.rs`).
/// The whole test took 6.11 s to 6.29 s of wall time and 4.69 s to 4.75 s of
/// cgroup CPU in three runs in a systemd unit: 4.6 times under the CPU kill
/// and 9 times under the wall kill. With every frame, the test failed at its
/// bound after 20.97 s and 20.3 CPU-seconds in two runs, just under the CPU
/// kill; in the runner, whichever fires first fails the test.
///
/// Then stepBack must land exactly at the previous two line arrivals: line 8
/// of `depth(0)` and line 9 of `depth(1)`, more than 1000 frames deep, where
/// every arrival at those lines has the same newest frames except for their
/// stack pointers.
#[test]
fn hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly() {
    let bound = DAP_DEEP_CONTINUE_BOUND.mul_f64(dap_wall_timeout_multiplier());
    if let Some(elapsed) = dap_deep_recursion_session(
        "hermit_dap_replay_continue_through_deep_recursion_is_fast_and_lands_exactly",
        DAP_DEEP_DEPTH,
        bound,
    ) {
        eprintln!(
            "the continue through {DAP_DEEP_DEPTH} recursive calls took {elapsed:?} (bound {bound:?})"
        );
    }
}

/// The guest of the one-line-loop stepBack test below: a loop on one source
/// line whose body calls `sq`, which is linked from an object without debug
/// information.
const DAP_ONE_LINE_LOOP_SOURCE: &str = r#"#include <stdio.h>
int sq(int x);
int main(void) {
  int total = 0;
  for (int i = 0; i < 3; i++) total += sq(i);
  printf("total=%d\n", total);
  return 0;
}
"#;
/// The loop's line in [`DAP_ONE_LINE_LOOP_SOURCE`].
const DAP_ONE_LINE_LOOP_LINE: u64 = 5;
/// The `printf` line in [`DAP_ONE_LINE_LOOP_SOURCE`].
const DAP_ONE_LINE_LOOP_PRINT_LINE: u64 = 6;

/// stepBack from after a one-line loop must return to the pass where the
/// client stopped, not to a later pass of the same address.
///
/// The client stops in `sq` for i = 0, steps out to the return address in the
/// loop's line, then continues to line 6. The replay passes that return
/// address twice more (i = 1 and i = 2) without stopping, and no line
/// breakpoint covers it, so only the stop-address breakpoint records those
/// passes. stepBack goes to the previous stop the client saw: i = 0, with
/// total still 0. Without the stop-address breakpoint, without the
/// subtraction of the passes it recorded, or with stepBack targeting one of
/// those recorded passes, it lands at i = 2 with total = 1.
#[test]
fn hermit_dap_replay_step_back_skips_later_passes_of_a_one_line_loop() {
    const TEST: &str = "hermit_dap_replay_step_back_skips_later_passes_of_a_one_line_loop";
    let Some(hermit_dap) = hermit_dap_binary(TEST) else {
        return;
    };
    let Some(gdb) = dap_capable_gdb(TEST) else {
        return;
    };
    let work = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the hermit-dap work dir");
    let sq = work.path().join("sq.c");
    fs::write(&sq, "int sq(int x) { return x * x; }\n").expect("failed to write sq.c");
    let sq_object = work.path().join("sq.o");
    let output = Command::new("cc")
        .args(["-O0", "-fno-pie", "-c"])
        .arg(&sq)
        .arg("-o")
        .arg(&sq_object)
        .output()
        .expect("failed to run cc for sq.o");
    assert!(
        output.status.success(),
        "compiling sq.c without debug information failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let source = work.path().join("oneline.c");
    fs::write(&source, DAP_ONE_LINE_LOOP_SOURCE).expect("failed to write oneline.c");
    let program = work.path().join("oneline");
    let output = Command::new("cc")
        .args(["-g", "-O0", "-fno-pie", "-no-pie"])
        .arg(&source)
        .arg(&sq_object)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("failed to run cc for the one-line-loop guest");
    assert!(
        output.status.success(),
        "compiling the one-line-loop guest failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let data_dir = work.path().join("recordings");
    let recording = dap_record(&program, &data_dir, "total=5");

    let port = unused_local_port();
    let adapter = dap_replay_adapter(hermit_dap, &gdb, &recording, &data_dir, port);
    let mut dap = DapClient::spawn(adapter, work.path().join("hermit-dap.stderr"));
    if let Err(refusal) = dap.initialize() {
        dap_gdb_skip(TEST, &refusal);
        return;
    }
    let thread = dap.attach_and_break(
        &program,
        &format!("127.0.0.1:{port}"),
        &source,
        DAP_ONE_LINE_LOOP_PRINT_LINE,
    );
    let breakpoints = dap.call(
        "setFunctionBreakpoints",
        serde_json::json!({"breakpoints": [{"name": "sq"}]}),
        DAP_TIMEOUT,
    );
    if breakpoints["body"]["breakpoints"][0]["verified"] != true {
        dap.fail(&format!("the breakpoint on sq must resolve: {breakpoints}"));
    }
    let stop = dap.resume("continue", &thread, "the stop in sq", DAP_TIMEOUT);
    if stop["reason"] != "function breakpoint" && stop["reason"] != "breakpoint" {
        dap.fail(&format!("expected a breakpoint stop in sq, got {stop}"));
    }
    dap.call(
        "setFunctionBreakpoints",
        serde_json::json!({"breakpoints": []}),
        DAP_TIMEOUT,
    );
    let in_loop = [("main", DAP_ONE_LINE_LOOP_LINE)];
    let what = "stepOut of sq to the loop's first pass";
    let stop = dap.resume("stepOut", &thread, what, DAP_TIMEOUT);
    // GDB reports a stepOut of a function without debug information with
    // reason "stopped", not "step".
    dap.assert_stop(what, &stop, &thread, "stopped", &in_loop, ("i", "0"));
    let what = "the breakpoint after the loop";
    let stop = dap.resume("continue", &thread, what, DAP_TIMEOUT);
    dap.assert_stop(
        what,
        &stop,
        &thread,
        "breakpoint",
        &[("main", DAP_ONE_LINE_LOOP_PRINT_LINE)],
        ("total", "5"),
    );

    let what = "stepBack to the loop's first pass";
    let stop = dap.resume("stepBack", &thread, what, DAP_REVERSE_TIMEOUT);
    dap.assert_stop(what, &stop, &thread, "step", &in_loop, ("i", "0"));
    let trace = dap.call(
        "stackTrace",
        serde_json::json!({"threadId": thread}),
        DAP_TIMEOUT,
    );
    let frame = trace["body"]["stackFrames"][0]["id"].clone();
    let total = dap.call(
        "evaluate",
        serde_json::json!({"expression": "total", "frameId": frame, "context": "watch"}),
        DAP_TIMEOUT,
    );
    if total["body"]["result"] != "0" {
        dap.fail(&format!(
            "{what}: expected total = 0 before the first pass adds to it, got {total}"
        ));
    }

    dap.call(
        "disconnect",
        serde_json::json!({"terminateDebuggee": true}),
        DAP_TIMEOUT,
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

/// Retained entries of a verify-log directory whose names start with `prefix`.
fn retained_captures(log_dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut captures = fs::read_dir(log_dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", log_dir.display()))
        .map(|entry| entry.expect("failed to read a retained log entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect::<Vec<_>>();
    captures.sort();
    captures
}

/// Runs a `hermit run --verify --keep-logs` command whose `--verify-log-dir` is
/// `logs`, as [`Command::output`] does, and also returns run 2's log, which
/// Hermit deletes after a match. The log survives as a hard link at `capture`,
/// made by a poll while the command runs, or after it exits if Hermit retained
/// the log; the `run2_log` module explains why that link holds run 2's complete
/// log. The caller checks it like a retained log and
/// then removes it. `None` means no poll saw a run 2 log and Hermit did not
/// retain one.
fn output_capturing_run2_log(
    command: &mut Command,
    logs: &Path,
    capture: &Path,
) -> (Output, Option<PathBuf>) {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start the verify command");
    let waiter = thread::spawn(move || child.wait_with_output());
    let mut captured = false;
    while !waiter.is_finished() {
        if !captured {
            captured = run2_log::link_run2_log(logs, capture);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = waiter
        .join()
        .expect("the verify command's waiter panicked")
        .expect("failed to wait for the verify command");
    // The polls can miss the whole run. A run 2 log that Hermit retained, as
    // it does after a divergence, is still there to link.
    if !captured {
        captured = run2_log::link_run2_log(logs, capture);
    }
    (output, captured.then(|| capture.to_owned()))
}

/// `--keep-logs` keeps ONLY the first run's log after a matched verification.
///
/// Owner rule, https://github.com/rrnewton/hermit/issues/3301: after the two
/// runs are checked to match for determinism, only one log needs to be kept.
/// The first run's log is kept as the golden log, and `--keep-logs` deletes the
/// second run's log, which compared equal to it. This
/// drives the real ptrace backend with the exact retention flags the E2E runner
/// emits, `--keep-logs --verify-log-dir <dir>`, so the directory checked here
/// has the shape the pressure-test gate and the parity post-pass read.
#[test]
fn ptrace_keep_logs_retains_only_the_golden_log_after_a_match() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the golden-log test directory");
    let log_dir = root.path().join("verify-logs");
    fs::create_dir(&log_dir).expect("failed to create the verify-log directory");
    let verdict_path = root.path().join("verdict.json");
    let stdout_path = root.path().join("stdout");
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--keep-logs",
        "--verify-log-dir",
        log_dir
            .to_str()
            .expect("verify-log directory should be UTF-8"),
        "--verify-json",
        verdict_path.to_str().expect("verdict path should be UTF-8"),
        "--",
        "/bin/echo",
        "golden-log",
    ];
    let mut child = hermit_command(&args)
        .stdout(fs::File::create(&stdout_path).expect("failed to create the stdout capture"))
        .stderr(Stdio::piped())
        // Own process group, so `wait_bounded` can reach namespace descendants.
        .process_group(0)
        .spawn()
        .expect("failed to spawn the ptrace verification");
    let status = wait_bounded(
        &mut child,
        "ptrace_keep_logs_retains_only_the_golden_log_after_a_match",
    );
    let stderr = strip_ansi_sgr(&drain_bounded(child.stderr.take()));
    assert!(
        status.success(),
        "ptrace verification failed ({status}):\n{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&stdout_path).expect("failed to read the guest stdout"),
        "golden-log\n"
    );

    let report = VerificationReport::from_current_json_value(
        serde_json::from_slice(&fs::read(&verdict_path).expect("failed to read the verdict"))
            .expect("the verdict should be JSON"),
    )
    .expect("the verdict should be a current verification report");
    assert_eq!(report.verdict, Verdict::Matched, "{stderr}");
    assert!(report.verified && report.bitwise_parity, "{stderr}");
    let compared = report
        .compared_log_messages
        .expect("a canonical verification must report its compared INFO messages");
    assert!(
        compared.left > 0 && compared.left == compared.right,
        "the golden log must come from a nonempty compared log population: {compared:?}"
    );

    // Exactly one nonempty golden log, and it is the only retained file.
    let golden = retained_captures(&log_dir, "run1_log_");
    assert_eq!(golden.len(), 1, "expected one golden log: {golden:?}");
    let size = fs::metadata(&golden[0])
        .expect("failed to stat the golden log")
        .len();
    assert!(size > 0, "the golden log {} is empty", golden[0].display());
    let duplicates = retained_captures(&log_dir, "run2_log_");
    assert!(
        duplicates.is_empty(),
        "a matched verification must delete run 2's log: {duplicates:?}"
    );
    let everything = retained_captures(&log_dir, "");
    assert_eq!(
        everything, golden,
        "the golden log must be the only retained file"
    );
    // The retained path printed for the user names that same file, and no
    // second log is reported as retained.
    assert!(
        stderr.contains(&format!("::   run 1: {}", golden[0].display())),
        "the retained golden path was not reported:\n{stderr}"
    );
    assert!(
        !stderr.contains("::   run 2:"),
        "a deleted run 2 log was reported as retained:\n{stderr}"
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

/// One row per container fault label in `record_start.rs`, with a spelling that
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
/// thought: each reason states what the named test actually exercises.
const FAULT_SITES_DRIVEN_ELSEWHERE: [(&str, &str); 2] = [
    (
        "with_container",
        "the `run` path; covered by the existing run-mode fault-injection tests",
    ),
    (
        "owned-container-lifecycle",
        "private __hermit-cli-lifecycle success role; cli_owned_lifecycle::successful_completion_returns_original_guard reaches this label, without a panic/segv injection matrix",
    ),
];

/// ⚠️ A CLASSIFICATION SITE CANNOT SILENTLY OPT OUT OF BEING ADDRESSABLE.
///
/// This is the invariant made STRUCTURAL rather than left as a convention. Two
/// sites -- the replay stages -- were untestable for as long as they existed, and
/// nothing said so; they were discovered by a human reading the code. The failure
/// mode is not that a test broke, it is that no test could ever have existed, and
/// silence is indistinguishable from coverage.
///
/// So: every actual container-call label in the sources is parsed here and
/// must appear either in [`RECORD_FAULT_SITES`] or in
/// [`FAULT_SITES_DRIVEN_ELSEWHERE`] with a reason. A site added without a row
/// fails THIS test by name, at the moment it is added, rather than being
/// untestable by default and noticed years later.
#[test]
fn no_container_site_is_unreachable_by_the_fault_injector() {
    use sha2::Digest;
    const RECORD: &str = include_str!("../src/bin/hermit/record_start.rs");
    const CONTAINER: &str = include_str!("../src/bin/hermit/container.rs");
    const OWNER: &str = include_str!("../src/bin/hermit/owned_container.rs");
    const RUN: &str = include_str!("../src/bin/hermit/run.rs");
    const REPLAY: &str = include_str!("../src/bin/hermit/replay.rs");
    const LIFECYCLE: &str = include_str!("../src/bin/hermit/cli_owned_lifecycle.rs");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/hermit");
    let sources = fault_sites::source_inventory(&root).expect("complete binary source inventory");
    for (name, compiled) in [
        ("record_start.rs", RECORD),
        ("container.rs", CONTAINER),
        ("owned_container.rs", OWNER),
        ("run.rs", RUN),
        ("replay.rs", REPLAY),
        ("cli_owned_lifecycle.rs", LIFECYCLE),
    ] {
        assert_eq!(
            sources.get(name).map(String::as_str),
            Some(compiled),
            "source changed since compilation: {name}"
        );
    }
    assert!(fault_sites::forwards_parameter(
        OWNER,
        "run",
        4,
        "super::container::catch_child_panic_at"
    ));
    assert!(fault_sites::forwards_parameter(
        CONTAINER,
        "catch_child_panic_at",
        0,
        "inject_test_fault"
    ));
    let covered = RECORD_FAULT_SITES
        .iter()
        .map(|(site, _)| *site)
        .chain(FAULT_SITES_DRIVEN_ELSEWHERE.iter().map(|(site, _)| *site))
        .collect::<Vec<_>>();
    let audit = std::cell::RefCell::new(fault_sites::SourceAudit::default());
    let check_sources = |sources: &std::collections::BTreeMap<String, String>| {
        audit.borrow_mut().coverage(
            sources
                .iter()
                .map(|(name, source)| (name.as_str(), source.as_str())),
            &covered,
        )
    };
    let check = |record: &str| {
        let mut changed = sources.clone();
        changed.insert("record_start.rs".into(), record.into());
        check_sources(&changed)
    };
    check(RECORD).unwrap_or_else(|error| panic!("fault-site coverage: {error}"));
    assert_eq!(audit.borrow().inspected_sources(), sources.len());
    check(RECORD).unwrap();
    assert_eq!(
        audit.borrow().inspected_sources(),
        sources.len(),
        "byte-identical inputs must reuse only their pure inspections"
    );
    // These digests bind the exact in-memory bytes parsed above, not a later read.
    for (name, source) in &sources {
        eprintln!(
            "FAULT_SITE_SOURCE {}",
            serde_json::json!({"path": name, "bytes": source.len(), "sha256": format!("{:x}", sha2::Sha256::digest(source.as_bytes()))})
        );
    }
    let lifecycle_mutant = LIFECYCLE.replace(
        "\"owned-container-lifecycle\"",
        "\"new.uncovered.lifecycle\"",
    );
    assert_ne!(lifecycle_mutant, LIFECYCLE);
    let mut changed = sources.clone();
    changed.insert("cli_owned_lifecycle.rs".into(), lifecycle_mutant.clone());
    let inspected = audit.borrow().inspected_sources();
    let error = check_sources(&changed).unwrap_err();
    assert_eq!(
        audit.borrow().inspected_sources(),
        inspected + 1,
        "changed complete source bytes must miss"
    );
    assert!(
        error.contains("new.uncovered.lifecycle") && error.contains("owned-container-lifecycle"),
        "{error}"
    );
    // A newly created nested source is part of the filesystem universe too.
    let fixture = tempfile::tempdir().unwrap();
    std::fs::write(fixture.path().join("main.rs"), &sources["main.rs"]).unwrap();
    std::fs::create_dir(fixture.path().join("new_module")).unwrap();
    std::fs::write(
        fixture.path().join("new_module/new_caller.rs"),
        &lifecycle_mutant,
    )
    .unwrap();
    let discovered = fault_sites::source_inventory(fixture.path()).unwrap();
    assert_eq!(discovered["new_module/new_caller.rs"], lifecycle_mutant);
    let mut added = sources.clone();
    added.insert(
        "new_module/new_caller.rs".into(),
        discovered["new_module/new_caller.rs"].clone(),
    );
    let inspected = audit.borrow().inspected_sources();
    assert!(
        check_sources(&added)
            .unwrap_err()
            .contains("new.uncovered.lifecycle")
    );
    assert_eq!(
        audit.borrow().inspected_sources(),
        inspected + 1,
        "the same bytes under a new filename must miss"
    );
    std::os::unix::fs::symlink(
        root.join("record_start.rs"),
        fixture.path().join("outside.rs"),
    )
    .unwrap();
    assert!(
        fault_sites::source_inventory(fixture.path())
            .unwrap_err()
            .contains("symlink")
    );
    assert!(fault_sites::source_inventory(&fixture.path().join("absent")).is_err());
    // Direct boundary calls must contribute labels, including in the owner file.
    for file in ["cli_owned_lifecycle.rs", "owned_container.rs"] {
        let mut changed = sources.clone();
        changed.get_mut(file).unwrap().push_str("\nfn direct_boundary_mutant() { super::container::catch_child_panic_at(\"new.uncovered.boundary\", || Ok(())); }\n");
        assert!(
            check_sources(&changed)
                .unwrap_err()
                .contains("new.uncovered.boundary")
        );
    }
    let callee = "super::owned_container::run";
    for replacement in [
        format!("({callee})"),
        "<Owner>::run_guarded_at".into(),
        "container.run_guarded_at".into(),
        "opaque_call!".into(),
    ] {
        let mut changed = sources.clone();
        let mutant = LIFECYCLE.replacen(callee, &replacement, 1);
        assert_ne!(mutant, LIFECYCLE);
        // Removing a real call always retains the stale lifecycle row, even if
        // the new syntax contains no critical identifier (the macro case).
        changed.insert("cli_owned_lifecycle.rs".into(), mutant);
        assert!(
            check_sources(&changed)
                .unwrap_err()
                .contains("owned-container-lifecycle")
        );
    }
    for body in [
        "opaque! { super::owned_container::run((), (), (), true, \"with_container\", None, ()); }",
        "use super::owned_container::run as alias; alias((), (), (), true, \"with_container\", None, ());",
        "use super::owned_container::*; run((), (), (), true, \"with_container\", None, ());",
        "let alias = super::owned_container::run; alias((), (), (), true, \"with_container\", None, ());",
        "container.run_guarded_at(\"with_container\");",
        "(super::container::catch_child_panic_at)(\"with_container\", || Ok(()));",
    ] {
        let mut changed = sources.clone();
        changed
            .get_mut("cli_owned_lifecycle.rs")
            .unwrap()
            .push_str(&format!("\nfn opaque_callee_mutant() {{ {body} }}\n"));
        assert!(
            check_sources(&changed)
                .unwrap_err()
                .contains("unaccounted critical tokens"),
            "{body}"
        );
    }
    for (name, body) in [
        (
            "unproven.rs",
            "fn catch_child_panic_at(site: &str) { inject_test_fault(site); }",
        ),
        ("unproven.rs", "include!(\"other.rs\");"),
        ("unproven.rs", "std::include!(\"other.rs\");"),
        ("unproven.rs", "std::r#include!(\"other.rs\");"),
        ("unproven.rs", "#[path=\"../outside.rs\"] mod other;"),
        ("unproven.rs", "#[r#path=\"../outside.rs\"] mod other;"),
        (
            "unproven.rs",
            "#[cfg_attr(unix, path=\"../outside.rs\")] mod other;",
        ),
        (
            "unproven.rs",
            "#[cfg_attr(unix, r#path=\"../outside.rs\")] mod other;",
        ),
        (
            "unproven.rs",
            "#[r#cfg_attr(unix, path=\"../outside.rs\")] mod other;",
        ),
    ] {
        let mut changed = sources.clone();
        changed.insert(name.into(), body.into());
        assert!(check_sources(&changed).is_err(), "{body}");
    }
    for (file, extra) in [
        (
            "owned_container.rs",
            "fn run(a: (), b: (), c: (), d: (), site: &str) { super::container::catch_child_panic_at(site, || Ok(())); }",
        ),
        (
            "container.rs",
            "fn catch_child_panic_at(site: &str) { inject_test_fault(site); }",
        ),
        ("container.rs", "fn inject_test_fault(site: &str) {}"),
        ("main.rs", "mod owned_container;"),
    ] {
        let mut changed = sources.clone();
        changed
            .get_mut(file)
            .unwrap()
            .push_str(&format!("\n{extra}\n"));
        assert!(
            check_sources(&changed).is_err(),
            "duplicate declaration: {file}: {extra}"
        );
    }

    // Mutate the actual included source, not a second hand-written inventory.
    // Both a conditional label and a direct call label must be discovered.
    for label in ["record.main.deadline", "record_verify.record"] {
        let needle = format!("\"{label}\"");
        assert_eq!(RECORD.matches(&needle).count(), 1);
        let mutant = RECORD.replace(&needle, "\"renamed.uncovered.site\"");
        let error = check(&mutant).unwrap_err();
        assert!(
            error.contains("renamed.uncovered.site") && error.contains(label),
            "{error}"
        );
    }
    let missing_call = RECORD.replacen(
        "super::owned_container::run(",
        "super::owned_container::removed_call(",
        1,
    );
    let error = check(&missing_call).unwrap_err();
    assert!(
        error.contains("record.main.deadline") && error.contains("record.main"),
        "{error}"
    );
    let added_call = format!(
        "{RECORD}\nfn uncovered_fixture() {{ super::owned_container::run((), (), (), true, \"added.uncovered.site\", None, ()); }}"
    );
    assert!(
        check(&added_call)
            .unwrap_err()
            .contains("added.uncovered.site")
    );
    let decoys = format!(
        "{RECORD}\n// super::owned_container::run((), (), (), true, \"comment.decoy\");\nconst DECOY: &str = r#\"inject_test_fault(\"string.decoy\")\"#;"
    );
    check(&decoys).unwrap();
    // A test-only literal must not conceal deletion of every production use.
    let missing_run = RUN.replace("\"with_container\"", "\"renamed.run.site\"");
    let missing_replay = REPLAY.replace("\"with_container\"", "\"renamed.replay.site\"");
    let mut missing = sources.clone();
    missing.insert("run.rs".into(), missing_run);
    missing.insert("replay.rs".into(), missing_replay);
    assert!(
        check_sources(&missing)
            .unwrap_err()
            .contains("with_container")
    );

    let unknown = RECORD.replace("\"record_verify.record\"", "unsupported_label()");
    assert!(
        check(&unknown)
            .unwrap_err()
            .contains("unsupported site expression")
    );
    let shadowed_owner = OWNER.replace(
        "let state = Rc::new(RefCell::new((guards, work)));",
        "let site = \"wrong.forwarding\"; let state = Rc::new(RefCell::new((guards, work)));",
    );
    assert_ne!(shadowed_owner, OWNER);
    assert!(!fault_sites::forwards_parameter(
        &shadowed_owner,
        "run",
        4,
        "super::container::catch_child_panic_at"
    ));
    let shadowed_boundary = CONTAINER.replace(
        "inject_test_fault(site);",
        "let site = \"wrong.forwarding\"; inject_test_fault(site);",
    );
    assert_ne!(shadowed_boundary, CONTAINER);
    assert!(!fault_sites::forwards_parameter(
        &shadowed_boundary,
        "catch_child_panic_at",
        0,
        "inject_test_fault"
    ));
    for (file, mutant, function, index, callee) in [
        (
            "owned_container.rs",
            shadowed_owner.replace(
                "let site = \"wrong.forwarding\"",
                "let r#site = \"wrong.forwarding\"",
            ),
            "run",
            4,
            "super::container::catch_child_panic_at",
        ),
        (
            "container.rs",
            shadowed_boundary.replace(
                "let site = \"wrong.forwarding\"",
                "let r#site = \"wrong.forwarding\"",
            ),
            "catch_child_panic_at",
            0,
            "inject_test_fault",
        ),
    ] {
        assert!(mutant.contains("let r#site ="));
        assert!(!fault_sites::forwards_parameter(
            &mutant, function, index, callee
        ));
        let mut changed = sources.clone();
        changed.insert(file.into(), mutant);
        assert!(check_sources(&changed).is_err());
    }
    // Raw spelling is the same Rust name. Recognize it consistently while the
    // token ledger still retains exact source spelling and position.
    let mut raw_sources = sources.clone();
    for source in raw_sources.values_mut() {
        *source = source
            .replace("owned_container", "r#owned_container")
            .replace("catch_child_panic_at", "r#catch_child_panic_at")
            .replace("inject_test_fault", "r#inject_test_fault")
            .replace("run_guarded_at", "r#run_guarded_at");
    }
    check_sources(&raw_sources).unwrap();
    let raw_renamed = raw_sources["record_start.rs"]
        .replace("\"record_verify.record\"", "\"new.uncovered.raw_callee\"");
    raw_sources.insert("record_start.rs".into(), raw_renamed);
    assert!(
        check_sources(&raw_sources)
            .unwrap_err()
            .contains("new.uncovered.raw_callee")
    );
    // The sole computed production label is now directly at argument four.
    // Preserve both literal branches; local bindings of every shape are outside
    // this deliberately finite grammar, including the historical alias mutants.
    let conditional = "if record_timeout.is_some() {\n                    \"record.main.deadline\"\n                } else {\n                    \"record.main\"\n                }";
    assert_eq!(RECORD.matches(conditional).count(), 1);
    let call_start = RECORD.find("super::owned_container::run(").unwrap();
    let call_end = call_start
        + RECORD[call_start..].find("            )?;").unwrap()
        + "            )?;".len();
    let call = &RECORD[call_start..call_end];
    assert!(call.contains(conditional));
    let local_call = call.replace(conditional, "site");
    let initializer = format!("let site = {conditional};");
    let original_local = RECORD.replacen(
        call,
        &format!(
            "{{ {initializer} {} }}?;",
            local_call.strip_suffix("?;").unwrap()
        ),
        1,
    );
    assert_ne!(original_local, RECORD);
    assert!(
        check(&original_local)
            .unwrap_err()
            .contains("unsupported site expression")
    );
    for (before, after) in [
        (
            "let source = \"new.uncovered.alias\"; let site = source; let source = \"with_container\";",
            "",
        ),
        ("let r#site = \"new.uncovered.raw\";", ""),
        (
            "let é = \"with_container\"; let e\u{301} = \"new.uncovered.nfc\"; let site = é;",
            "",
        ),
        (
            "let source = site; let site = \"new.uncovered.cfg\"; #[cfg(any())] let site = source;",
            "",
        ),
        ("let binder!(site) = ();", ""),
        ("let ref site = \"new.uncovered.ref\";", ""),
        ("let site @ _ = \"new.uncovered.subpattern\";", ""),
        ("let (site,) = (\"new.uncovered.tuple\",);", ""),
        ("let site: &str = \"new.uncovered.typed\";", ""),
        ("let mut site = \"new.uncovered.mutable\";", ""),
        ("let site; site = \"new.uncovered.uninitialized\";", ""),
        ("(|site| {", "})(\"new.uncovered.closure\");"),
        ("for site in [\"new.uncovered.for\"] {", "}"),
        ("if let Some(site) = Some(\"new.uncovered.if\") {", "}"),
        (
            "while let Some(site) = Some(\"new.uncovered.while\") {",
            "break; }",
        ),
        (
            "match Some(\"new.uncovered.match\") { Some(site) => {",
            "}, None => {} }",
        ),
        ("fn nested(site: &str) {", "}"),
        (
            "macro_rules! replace_site { ($name:ident) => { let $name = \"new.uncovered.macro\"; } } replace_site!(site);",
            "",
        ),
        ("opaque_binding!(site);", ""),
        ("", "opaque_binding!(site);"),
        ("{ const site: &str = \"new.uncovered.item\";", "}"),
        ("{", "const site: &str = \"new.uncovered.item\"; }"),
        ("opaque_binding!(\u{212a});", ""),
    ] {
        // The appended mutant uses the complete real call, with only its label
        // expression replaced by an unsupported local. No label list is copied.
        let mutant = format!(
            "{RECORD}\nfn binding_mutant() {{ {initializer} {before} {local_call} {after} }}"
        );
        assert!(
            check(&mutant)
                .unwrap_err()
                .contains("unsupported site expression"),
            "{before} {after}"
        );
    }
    for condition in [
        "if let Some(source) = Some(\"new.uncovered.if_initializer\") { \"record.main.deadline\" } else { \"record.main\" }",
        "if true && let Some(source) = Some(\"new.uncovered.if_chain\") { \"record.main.deadline\" } else { \"record.main\" }",
        "if opaque_condition!() { \"record.main.deadline\" } else { \"record.main\" }",
    ] {
        let mutant = RECORD.replacen(conditional, condition, 1);
        assert_ne!(mutant, RECORD);
        assert!(
            check(&mutant)
                .unwrap_err()
                .contains("binding-bearing or opaque site condition"),
            "{condition}"
        );
    }
    let associated_constant = RECORD.replacen(conditional, "<Labels>::site", 1);
    assert!(
        check(&associated_constant)
            .unwrap_err()
            .contains("unsupported site expression")
    );
    for (text, function, index, callee, needle) in [
        (
            OWNER,
            "run",
            4,
            "super::container::catch_child_panic_at",
            "let state = Rc::new(RefCell::new((guards, work)));",
        ),
        (
            CONTAINER,
            "catch_child_panic_at",
            0,
            "inject_test_fault",
            "inject_test_fault(site);",
        ),
    ] {
        for prefix in [
            "opaque_binding!(site);",
            "opaque_binding!(r#site);",
            "const site: &str = \"wrong.forwarding\";",
            "let e\u{301} = \"wrong.forwarding\";",
        ] {
            let mutant = text.replacen(needle, &format!("{prefix} {needle}"), 1);
            assert_ne!(mutant, text);
            assert!(!fault_sites::forwards_parameter(
                &mutant, function, index, callee
            ));
        }
    }
    let broken_owner = OWNER.replace(
        "catch_child_panic_at(site,",
        "catch_child_panic_at(\"wrong.forwarding\",",
    );
    assert!(!fault_sites::forwards_parameter(
        &broken_owner,
        "run",
        4,
        "super::container::catch_child_panic_at"
    ));
    let broken_boundary = CONTAINER.replace(
        "inject_test_fault(site);",
        "inject_test_fault(\"wrong.forwarding\");",
    );
    assert!(!fault_sites::forwards_parameter(
        &broken_boundary,
        "catch_child_panic_at",
        0,
        "inject_test_fault"
    ));
    for (file, original, function, index, callee, mutant) in [
        (
            "owned_container.rs",
            OWNER,
            "run",
            4,
            "super::container::catch_child_panic_at",
            OWNER.replacen(
                "    site: &'static str,",
                "    #[cfg(any())] site: &'static str, _unused_label: &'static str,",
                1,
            ),
        ),
        // A conservative nested callable-scope refusal, not a claim that this
        // isolated mutation is a compiled runtime misforwarding demonstration.
        (
            "container.rs",
            CONTAINER,
            "catch_child_panic_at",
            0,
            "inject_test_fault",
            CONTAINER.replacen(
                "inject_test_fault(site);",
                "struct Inner; impl Inner { fn hidden() { inject_test_fault(site); } }",
                1,
            ),
        ),
    ] {
        assert_ne!(mutant, original);
        let mutant = format!("{mutant}\nconst site: &str = \"wrong.forwarding\";\n");
        assert!(!fault_sites::forwards_parameter(
            &mutant, function, index, callee
        ));
        let mut changed = sources.clone();
        changed.insert(file.into(), mutant);
        assert!(check_sources(&changed).is_err());
    }
    for replacement in [
        "let binder!(site) = (); inject_test_fault(site);",
        "inject_test_fault(<Labels>::site);",
        "<Labels>::inject_test_fault(site);",
    ] {
        let mutant = CONTAINER.replace("inject_test_fault(site);", replacement);
        assert_ne!(mutant, CONTAINER);
        assert!(
            !fault_sites::forwards_parameter(
                &mutant,
                "catch_child_panic_at",
                0,
                "inject_test_fault"
            ),
            "{replacement}"
        );
    }
    assert_eq!(
        fault_sites::source_inventory(&root).unwrap(),
        sources,
        "source inventory changed during guard execution"
    );
    let audit = audit.borrow();
    assert!(audit.requests() > audit.inspected_sources());
    eprintln!(
        "FAULT_SITE_PARSE_CACHE {}",
        serde_json::json!({"requests": audit.requests(), "inspected_exact_inputs": audit.inspected_sources()})
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

    // hermit -> container init -> guest. Poll: the two forks and the guest's
    // exec must all have happened before there is a guest to signal. Between
    // the fork and the exec the grandchild still runs Hermit's image, with comm
    // "hermit"; signalling it then would test a pre-exec Hermit process, not
    // the guest.
    let comm_of = |pid: i32| {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut last_seen = None;
    let guest = loop {
        let grandchildren: Vec<i32> = children_of(child.id() as i32)
            .into_iter()
            .flat_map(children_of)
            .collect();
        if let Some(&pid) = grandchildren.iter().find(|&&pid| comm_of(pid) == "cat") {
            break Some(pid);
        }
        if let Some(&pid) = grandchildren.first() {
            last_seen = Some((pid, comm_of(pid)));
        }
        if std::time::Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let guest = guest.unwrap_or_else(|| match last_seen {
        None => panic!("the guest never appeared under the container within 30s"),
        Some((pid, comm)) => panic!(
            "walked to the wrong process: no grandchild became cat within 30s; last seen pid {pid} comm {comm:?}"
        ),
    });

    // Verify WHICH process is being signalled rather than trusting the walk.
    let comm = comm_of(guest);
    assert_eq!(comm, "cat", "walked to the wrong process: {comm:?}");

    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: `kill` on a pid in this process's own tree, verified above.
    assert_eq!(
        unsafe { libc::kill(guest, libc::SIGINT) },
        0,
        "SIGINT to guest failed"
    );

    // ⚠️ LET THE SIGNAL INTERRUPT THE READ BEFORE CLOSING STDIN. Closed at once,
    // the pipe's EOF usually won the race: the guest's read returned 0
    // uninterrupted and SIGINT reached the Tool at an ordinary delivery stop.
    // The failing order is the one where the read is interrupted: detcore's
    // retried read then takes SIGINT into reverie's held-signal slot, and before
    // https://github.com/rrnewton/reverie/pull/831 the resume delivered it
    // without calling handle_signal_event, so Hermit died of SIGINT instead of
    // exiting 130 (https://github.com/rrnewton/hermit/issues/3468). The pause
    // makes that the order this test checks on every run.
    std::thread::sleep(std::time::Duration::from_millis(500));

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
    // The control and pressured invocation compare complete diagnostics, so
    // their explicit clock input must match too.
    let args = [
        "run",
        "--epoch=2026-01-01T00:00:00.123456789Z",
        "--",
        &program,
    ];

    // The truth to compare against, captured with an ordinary pipe.
    let control = hermit_command(&args).output().expect("control run");
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
    let mut child = hermit_command(&args)
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

/// A `hermit run` of `/bin/true` whose container child sleeps `stall_ms` after
/// publishing its result (test hook), with an optional finalize-budget
/// override.
fn run_with_post_publication_stall(stall_ms: u64, budget_ms: Option<&str>) -> (Output, Duration) {
    let args = [
        "run",
        "--base-env=minimal",
        "--max-timeslice=disabled",
        "--",
        "/bin/true",
    ];
    let mut command = hermit_command(&args);
    command
        .env(
            "HERMIT_INTERNAL_STALL_AFTER_PUBLICATION_MS",
            stall_ms.to_string(),
        )
        .env_remove("HERMIT_FINALIZE_BUDGET_MS")
        .stdin(Stdio::null());
    if let Some(budget) = budget_ms {
        command.env("HERMIT_FINALIZE_BUDGET_MS", budget);
    }
    let started = std::time::Instant::now();
    let output = command.output().expect("failed to run hermit");
    (output, started.elapsed())
}

/// https://github.com/rrnewton/hermit/issues/3414: after the container child
/// publishes its result, the parent waits a bounded time for it to exit. That
/// teardown was measured at up to 1.52 s on a saturated 316-CPU host, and the
/// old 2 s bound cancelled completed runs under a parallel Buck run. The
/// default is now 20 s, so a 3 s post-publication stall completes normally.
#[test]
fn a_teardown_stall_after_publication_completes_within_the_default_budget() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (output, elapsed) = run_with_post_publication_stall(3_000, None);
    assert!(
        output.status.success(),
        "a 3 s teardown stall must fit the default finalize budget (took {elapsed:?}): {}",
        stderr(&output)
    );
    assert!(
        elapsed >= Duration::from_secs(3),
        "the stall hook did not fire: {elapsed:?}"
    );
}

/// A child that does not exit within the finalize budget is cancelled, and the
/// error says so and names the budget, so it cannot be mistaken for a guest
/// killed by SIGKILL. The override bounds the wait: a 10 s stall under a
/// 500 ms budget ends long before the stall would.
#[test]
fn a_child_that_outlives_the_finalize_budget_is_cancelled_with_the_budget_named() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (output, elapsed) = run_with_post_publication_stall(10_000, Some("500"));
    let stderr = stderr(&output);
    // EXIT-CLASS: hermit
    assert_eq!(output.status.code(), Some(125), "{stderr}");
    assert!(
        stderr.contains("did not exit within the 500ms finalize budget"),
        "{stderr}"
    );
    assert!(
        stderr.contains("this error reports a teardown stall"),
        "{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "cancellation waited for the stall instead of the budget: {elapsed:?}"
    );

    // Within the overridden budget, the same kind of stall completes.
    let (output, _) = run_with_post_publication_stall(200, Some("5000"));
    assert!(output.status.success(), "{}", self::stderr(&output));
}

#[test]
fn a_malformed_finalize_budget_is_refused() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for budget in ["0", "soon", "-5", "+5"] {
        let (output, _) = run_with_post_publication_stall(0, Some(budget));
        let stderr = stderr(&output);
        assert!(!output.status.success(), "{budget}: {stderr}");
        assert!(
            stderr.contains(&format!(
                "HERMIT_FINALIZE_BUDGET_MS={budget} is not a positive number of milliseconds"
            )),
            "{budget}: {stderr}"
        );
    }
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
            "--backend",
            backend,
            "run",
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

/// In-guest LiteInst (`HERMIT_LITEINST_IN_GUEST=1`,
/// https://github.com/rrnewton/hermit/issues/3520) cannot deliver Detcore's
/// preemption timer yet: the in-guest Tool host refuses `set_timer` with
/// `ENOSYS`, which Detcore treats as fatal. A run with the default maximum
/// timeslice must be REFUSED before dispatch, not fail inside the guest. The
/// refusal comes from argument validation, so it holds in builds without the
/// `liteinst` feature or runtime too.
#[test]
fn liteinst_in_guest_refuses_a_maximum_timeslice_before_dispatch() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let output = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--",
        "/bin/echo",
        "unreachable",
    ])
    .env("HERMIT_LITEINST_IN_GUEST", "1")
    .stdin(Stdio::null())
    .output()
    .expect("failed to run hermit");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // EXIT-CLASS: hermit
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "in-guest LiteInst must refuse a maximum timeslice it cannot enforce. Got \
         {:?}. stderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("pass --max-timeslice=disabled"),
        "the refusal must say how to run without the timer. stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "the guest must not run"
    );
}

/// The selector accepts only "unset" (the ptrace-hosted hybrid) and `1`, so a
/// typo cannot silently pick the hybrid.
#[test]
fn liteinst_in_guest_selector_rejects_unknown_values() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let output = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--",
        "/bin/echo",
        "unreachable",
    ])
    .env("HERMIT_LITEINST_IN_GUEST", "yes")
    .stdin(Stdio::null())
    .output()
    .expect("failed to run hermit");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(!output.status.success(), "stderr:\n{stderr}");
    assert!(
        stderr.contains("HERMIT_LITEINST_IN_GUEST=\"yes\" is not a LiteInst runtime selection"),
        "stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "the guest must not run"
    );
}

/// In-guest LiteInst refuses `--verify`, and must refuse it before `--verify`
/// snapshots stdin. The snapshot reads stdin to its end, so a refusal checked
/// after it would wait for input that may never come instead of exiting. Stdin
/// here is a pipe that the test holds open and never writes to.
#[test]
fn liteinst_in_guest_refuses_verify_without_reading_stdin() {
    use std::io::Read;

    const DEADLINE: Duration = Duration::from_secs(20);

    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut child = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--verify",
        "--",
        "/bin/echo",
        "unreachable",
    ])
    .env("HERMIT_LITEINST_IN_GUEST", "1")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("failed to run hermit");
    let held_stdin = child.stdin.take().expect("stdin is piped");
    let deadline = Instant::now() + DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("cannot poll hermit") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "hermit was still running {DEADLINE:?} after start with stdin open: the \
                 --verify refusal waited for input"
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    drop(held_stdin);
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout is piped")
        .read_to_string(&mut stdout)
        .expect("cannot read hermit's stdout");
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_string(&mut stderr)
        .expect("cannot read hermit's stderr");

    // EXIT-CLASS: hermit
    assert_eq!(
        status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "status {status:?}, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--verify compares Detcore's internal logs"),
        "the refusal must name --verify. stderr:\n{stderr}"
    );
    assert!(!stdout.contains("unreachable"), "the guest must not run");
}

/// A statically linked guest has no dynamic loader to load the in-guest
/// runtime, so under in-guest LiteInst it would run entirely unmonitored. It
/// must be refused before it starts.
///
/// The CLI reports a missing runtime library as an unavailable backend before
/// it starts the container, and the program refusal is made inside the
/// container, where the guest is spawned. So this test needs the runtime too.
/// The refusal logic itself is covered without it by the
/// `in_guest_liteinst_*` unit tests in `hermit-cli/src/lib.rs`.
///
/// The guest is a hand-assembled 157-byte static x86-64 executable that writes
/// `RAN\n` and exits 0. It is run natively first, so the refusal cannot pass
/// because the image was broken.
#[test]
#[cfg(feature = "liteinst")]
#[ignore = "needs the in-guest Detcore runtime from `cargo build -p detcore-liteinst`"]
fn liteinst_in_guest_refuses_a_statically_linked_guest() {
    const BASE: u64 = 0x40_0000;
    const HEADERS: usize = 64 + 56;
    #[rustfmt::skip]
    const CODE: &[u8] = &[
        0xb8, 0x01, 0x00, 0x00, 0x00,             // mov eax, 1 (write)
        0xbf, 0x01, 0x00, 0x00, 0x00,             // mov edi, 1
        0x48, 0x8d, 0x35, 0x10, 0x00, 0x00, 0x00, // lea rsi, [rip + 16]
        0xba, 0x04, 0x00, 0x00, 0x00,             // mov edx, 4
        0x0f, 0x05,                               // syscall
        0xb8, 0x3c, 0x00, 0x00, 0x00,             // mov eax, 60 (exit)
        0x31, 0xff,                               // xor edi, edi
        0x0f, 0x05,                               // syscall
        b'R', b'A', b'N', b'\n',
    ];

    fn put(image: &mut [u8], offset: usize, value: &[u8]) {
        image[offset..offset + value.len()].copy_from_slice(value);
    }

    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let size = (HEADERS + CODE.len()) as u64;
    let mut image = vec![0u8; HEADERS];
    image.extend_from_slice(CODE);
    put(&mut image, 0, b"\x7fELF\x02\x01\x01"); // 64-bit, little-endian, version 1
    put(&mut image, 16, &2u16.to_le_bytes()); // ET_EXEC
    put(&mut image, 18, &62u16.to_le_bytes()); // EM_X86_64
    put(&mut image, 20, &1u32.to_le_bytes()); // EV_CURRENT
    put(&mut image, 24, &(BASE + HEADERS as u64).to_le_bytes()); // e_entry
    put(&mut image, 32, &64u64.to_le_bytes()); // e_phoff
    put(&mut image, 52, &64u16.to_le_bytes()); // e_ehsize
    put(&mut image, 54, &56u16.to_le_bytes()); // e_phentsize
    put(&mut image, 56, &1u16.to_le_bytes()); // e_phnum
    // One PT_LOAD maps the whole file, readable and executable, at BASE. There
    // is no PT_INTERP, so the kernel starts it with no dynamic loader.
    put(&mut image, 64, &1u32.to_le_bytes()); // PT_LOAD
    put(&mut image, 68, &5u32.to_le_bytes()); // PF_R | PF_X
    put(&mut image, 80, &BASE.to_le_bytes()); // p_vaddr
    put(&mut image, 88, &BASE.to_le_bytes()); // p_paddr
    put(&mut image, 96, &size.to_le_bytes()); // p_filesz
    put(&mut image, 104, &size.to_le_bytes()); // p_memsz
    put(&mut image, 112, &0x1000u64.to_le_bytes()); // p_align
    assert_eq!(image.len(), 157);

    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = directory.path().join("static-ran");
    fs::write(&guest, &image).unwrap();
    fs::set_permissions(&guest, fs::Permissions::from_mode(0o755)).unwrap();

    let native = Command::new(&guest)
        .stdin(Stdio::null())
        .output()
        .expect("cannot run the static guest natively");
    assert!(native.status.success(), "native run: {:?}", native.status);
    assert_eq!(native.stdout, b"RAN\n", "native run");

    let guest_arg = guest.to_str().expect("UTF-8 temporary path");
    let output = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--",
        guest_arg,
    ])
    .env("HERMIT_LITEINST_IN_GUEST", "1")
    .stdin(Stdio::null())
    .output()
    .expect("failed to run hermit");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // EXIT-CLASS: hermit
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "status {:?}, stderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("is statically linked") && stderr.contains(guest_arg),
        "the refusal must name the program and why. stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("RAN"),
        "the guest must not run"
    );
}

/// In-guest LiteInst runs Detcore's Tool in the guest preload, with no ptrace
/// tracer on the guest.
///
/// The guest is observed from OUTSIDE hermit. It cannot answer this question
/// about itself: Detcore virtualizes `/proc/<pid>/status` and reports
/// `TracerPid: 1` under every backend (`sanitize_status` in
/// `detcore/src/procfs.rs`), so a guest-side probe reads 1 under in-guest
/// LiteInst as well and cannot tell the backends apart.
///
/// The guest is `/bin/cat <fifo>`. Once a non-blocking open of the FIFO for
/// writing stops failing with `ENXIO`, the guest has reached its own `open`,
/// past the preload bootstrap, and it then blocks reading until this test
/// writes. While it is parked there the test reads the guest's host
/// `/proc/<pid>/status` and `/proc/<pid>/maps`. The ptrace backend is the
/// control for both checks: it must show a nonzero `TracerPid` and no in-guest
/// runtime mapping, so neither assertion can pass vacuously.
#[test]
#[cfg(feature = "liteinst")]
#[ignore = "needs the in-guest Detcore runtime from `cargo build -p detcore-liteinst`"]
fn liteinst_in_guest_runs_detcore_without_a_ptrace_tracer() {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    const IN_GUEST_RUNTIME: &str = "/libdetcore_liteinst.so";
    const DEADLINE: Duration = Duration::from_secs(20);

    /// Unblocks and reaps the run on every exit path, including a failed
    /// assertion, so a guest parked on the FIFO cannot outlive the test.
    struct Run {
        fifo: PathBuf,
        child: Option<std::process::Child>,
    }
    impl Drop for Run {
        fn drop(&mut self) {
            let Some(mut child) = self.child.take() else {
                return;
            };
            // An `O_RDWR` open of a FIFO never blocks on Linux and counts as a
            // writer, so closing it hands a parked reader end-of-file.
            drop(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&self.fifo),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if !matches!(child.try_wait(), Ok(None)) {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: `killpg` has no memory-safety preconditions; the group was
            // created for this run by `process_group(0)`.
            unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
            let _ = child.wait();
        }
    }

    struct GuestView {
        tracer_pids: Vec<String>,
        runtime_mapped: bool,
    }

    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let command_for = |backend: &str, program: &[&str]| {
        let mut args = vec![
            "--backend",
            backend,
            "run",
            "--max-timeslice=disabled",
            "--",
        ];
        args.extend(program);
        let mut command = hermit_command(&args);
        if backend == "liteinst" {
            command.env("HERMIT_LITEINST_IN_GUEST", "1");
        }
        command
    };
    let check_stderr = |backend: &str, stderr: &str| {
        if backend == "liteinst" {
            assert!(
                stderr.contains(
                    "hermit: [liteinst in-guest] selected: the guest preload is to host the \
                     Detcore Tool"
                ),
                "stderr:\n{stderr}"
            );
            assert!(
                !stderr.contains("[liteinst host hybrid]"),
                "stderr:\n{stderr}"
            );
        }
    };
    let tracer_pid = |status: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix("TracerPid:"))
            .map(|value| value.trim().to_owned())
            .unwrap_or_else(|| panic!("no TracerPid line in:\n{status}"))
    };

    let output = command_for("liteinst", &["/bin/echo", "hello", "in-guest"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "status {:?}, stderr:\n{stderr}",
        output.status
    );
    check_stderr("liteinst", &stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hello in-guest\n");

    let observe = |backend: &str| -> GuestView {
        let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create the FIFO directory");
        let fifo = dir.path().join("guest-blocks-here");
        let fifo_c = CString::new(fifo.as_os_str().as_bytes()).expect("NUL in temp path");
        // SAFETY: `fifo_c` is a valid NUL-terminated path and mode has no
        // additional preconditions.
        assert_eq!(
            unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) },
            0,
            "mkfifo {fifo:?}: {}",
            std::io::Error::last_os_error()
        );
        let fifo_arg = fifo.to_str().expect("temp path is not UTF-8");
        let mut command = command_for(backend, &["/bin/cat", fifo_arg]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut run = Run {
            fifo: fifo.clone(),
            child: Some(command.spawn().expect("failed to spawn hermit")),
        };

        let deadline = Instant::now() + DEADLINE;
        let mut writer = loop {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo)
            {
                Ok(file) => break file,
                Err(error)
                    if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
                {
                    let child = run.child.as_mut().expect("run is live");
                    if let Ok(Some(status)) = child.try_wait() {
                        panic!(
                            "{backend}: hermit exited {status:?} before the guest opened {fifo:?}"
                        );
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("{backend}: the guest never opened {fifo:?}: {error}"),
            }
        };

        let fifo_bytes = fifo.as_os_str().as_bytes();
        let guests: Vec<u32> = fs::read_dir("/proc")
            .expect("failed to list /proc")
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| {
                    let argv = cmdline.strip_suffix(&[0]).unwrap_or(&cmdline[..]);
                    argv.split(|byte| *byte == 0)
                        .eq([b"/bin/cat".as_slice(), fifo_bytes])
                })
            })
            .collect();
        assert_eq!(
            guests.len(),
            1,
            "{backend}: expected exactly one guest `/bin/cat {fifo:?}`, found {guests:?}"
        );
        let guest = guests[0];
        let tracer_pids = (0..3)
            .map(|_| {
                let status = fs::read_to_string(format!("/proc/{guest}/status"))
                    .unwrap_or_else(|error| panic!("{backend}: guest {guest} status: {error}"));
                thread::sleep(Duration::from_millis(50));
                tracer_pid(&status)
            })
            .collect();
        let maps = fs::read_to_string(format!("/proc/{guest}/maps"))
            .unwrap_or_else(|error| panic!("{backend}: guest {guest} maps: {error}"));
        let runtime_mapped = maps.lines().any(|line| line.ends_with(IN_GUEST_RUNTIME));

        writer
            .write_all(b"released\n")
            .expect("failed to write to the FIFO");
        drop(writer);
        let mut child = run.child.take().expect("run is live");
        let status = loop {
            if let Some(status) = child.try_wait().expect("failed to poll hermit") {
                break status;
            }
            if Instant::now() >= deadline {
                run.child = Some(child);
                panic!("{backend}: the guest did not exit after the FIFO was released");
            }
            thread::sleep(Duration::from_millis(20));
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        child
            .stdout
            .take()
            .expect("piped stdout")
            .read_to_string(&mut stdout)
            .expect("failed to read stdout");
        child
            .stderr
            .take()
            .expect("piped stderr")
            .read_to_string(&mut stderr)
            .expect("failed to read stderr");
        assert!(
            status.success(),
            "{backend}: status {status:?}, stderr:\n{stderr}"
        );
        assert_eq!(stdout, "released\n", "{backend}: stderr:\n{stderr}");
        check_stderr(backend, &stderr);
        GuestView {
            tracer_pids,
            runtime_mapped,
        }
    };

    let control = observe("ptrace");
    assert!(
        control.tracer_pids.iter().all(|pid| pid != "0"),
        "under the ptrace backend the guest's host TracerPid read {:?}, so this probe \
         cannot see a tracer and proves nothing about in-guest LiteInst",
        control.tracer_pids
    );
    assert!(
        !control.runtime_mapped,
        "the ptrace guest maps {IN_GUEST_RUNTIME}, so the mapping check cannot tell \
         the backends apart"
    );
    let in_guest = observe("liteinst");
    assert_eq!(
        in_guest.tracer_pids,
        ["0", "0", "0"],
        "in-guest LiteInst left a ptrace tracer on the guest"
    );
    assert!(
        in_guest.runtime_mapped,
        "the in-guest Detcore runtime {IN_GUEST_RUNTIME} is not mapped into the guest"
    );
}

/// `--timeout` qualification is a static policy fact, so an UNAVAILABLE
/// backend must get the same refusal as an available one
/// (https://github.com/rrnewton/hermit/issues/3418). Before the fix the run
/// path probed availability first, so a build without the `sabre` feature
/// answered `class=backend-unavailable` (125) where every other build refused
/// (122).
///
/// Pointing SaBRe's loader override at a missing file makes the backend
/// unavailable in EVERY build -- without the feature it is not compiled, with
/// it there is no loader -- so this case holds whichever features the test
/// binary was built with. The control run without `--timeout` proves the
/// premise: the same invocation really does report the backend unavailable.
#[test]
fn run_timeout_refusal_does_not_depend_on_backend_availability() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let missing_loader = "/nonexistent/hermit-cli-test/sabre";
    let run = |timeout: bool| {
        let mut args = vec!["--backend", "sabre", "run"];
        if timeout {
            args.extend(["--timeout", "3"]);
        }
        args.extend(["--", "/bin/echo", "unreachable"]);
        let output = hermit_command(&args)
            .env("HERMIT_SABRE_BINARY", missing_loader)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output, stderr)
    };

    let (control, control_stderr) = run(false);
    // EXIT-CLASS: hermit
    assert_eq!(
        control.status.code(),
        Some(125),
        "control: without --timeout the sabre backend must be unavailable here. \
         stderr:\n{control_stderr}"
    );
    assert!(
        control_stderr.contains("HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=sabre"),
        "control: {control_stderr}"
    );

    let (refused, stderr) = run(true);
    // EXIT-CLASS: hermit
    assert_eq!(
        refused.status.code(),
        Some(122),
        "`--timeout` on an unavailable backend must still REFUSE (122). stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "{stderr}"
    );
    assert!(stderr.contains("not qualified"), "{stderr}");
    assert!(!stderr.contains("backend-unavailable"), "{stderr}");
    assert!(
        refused.stdout.is_empty(),
        "the guest ran: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
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

// The run_kvm_ prefix is selected by all three existing KVM validation lanes.
#[test]
fn run_kvm_exit_group_cancels_queued_leader_exit() {
    kvm_cancellation::run(64, 95);
}

#[test]
fn run_kvm_exit_group_cancels_queued_worker_exit() {
    kvm_cancellation::run(256, 17);
}

#[test]
fn run_kvm_exit_group_cancels_rdtsc_posthook_wait() {
    kvm_cancellation::run(512, 17);
}

#[test]
fn run_kvm_itimer_real_interrupts_sleep_in_all_eight_modes() {
    kvm_itimer::run();
}

#[test]
fn run_ptrace_exec_deletes_posix_timers_and_preserves_itimer() {
    exec_posix_timers::run("ptrace");
}

#[test]
fn run_kvm_exec_deletes_posix_timers_and_preserves_itimer() {
    exec_posix_timers::run("kvm");
}

#[test]
fn run_kvm_root_exit_reparents_live_child_and_grandchild() {
    kvm_orphan_reparenting::run();
}

#[test]
fn run_kvm_process_timer_signal_and_sibling_group_exit_retire_both_roles() {
    kvm_signal_retirement::run();
}

#[test]
fn run_kvm_synchronous_root_segv_preserves_guest_exit() {
    kvm_synchronous_fault::run_root();
}

#[test]
fn run_kvm_synchronous_orphan_segv_preserves_root_success() {
    kvm_synchronous_fault::run_orphan();
}
