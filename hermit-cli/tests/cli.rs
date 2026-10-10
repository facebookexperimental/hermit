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

#[path = "common/kvm_waitid_copyout.rs"]
mod kvm_waitid_copyout;

// Its helpers test `scripts/stage-liteinst-runtime.sh`, which still stages the
// Reverie LiteInst runtime; the in-guest backend does not load that library.
#[cfg_attr(not(feature = "liteinst"), allow(dead_code))]
#[path = "common/liteinst.rs"]
mod liteinst_runtime;

// The LiteInst dispatch record and backend statistics are checked against
// in-guest runs in this binary.
#[cfg(feature = "liteinst")]
#[path = "common/dispatch_stats.rs"]
mod dispatch_stats;

// Real programs under in-guest LiteInst, formerly the liteinst_advanced test
// binary (https://github.com/rrnewton/hermit/issues/3520).
#[cfg(feature = "liteinst")]
#[path = "common/liteinst_in_guest_programs.rs"]
mod liteinst_in_guest_programs;

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
use hermit::HERMIT_LOG_CAP_EXIT;
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
static KVM_TASK_IDS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_UNSUPPORTED_SYSCALL_GUEST: OnceLock<PathBuf> = OnceLock::new();
static GETTIMEOFDAY_UNREADABLE_BUFFER_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_SELF_SIGQUEUE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_STDERR_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_LOG_ENV_GUEST: OnceLock<PathBuf> = OnceLock::new();
static DBT_GUEST_ENVIRONMENT_GUEST: OnceLock<PathBuf> = OnceLock::new();
#[cfg(feature = "liteinst")]
static LITEINST_INERT_RUNTIME: OnceLock<PathBuf> = OnceLock::new();
static EXEC_CLOCK_CONTINUITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_TWO_THREADS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_SOURCE_THEN_FUTEX_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_SIGNAL_WHILE_HELD_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_SPAWN_DUP2_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_SPAWN_ECHO_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_RELATIVE_ANCHOR_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_CHILD_WAIT_CYCLE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_KILL_HELD_CHILD_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_POSTHOOK_FUTEX_INTERRUPT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static HB_PIPE_READ_HELD_WRITER_GUEST: OnceLock<PathBuf> = OnceLock::new();
static POLL_TIMEOUT_VS_SPINNER_GUEST: OnceLock<PathBuf> = OnceLock::new();
static VFORK_PARENT_KILLED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static VFORK_KILL_EXEC_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_WAKE_COUNT_ZERO_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_REQUEUE_WAKE_OP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_LOCK_PI_REFUSED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_KEYED_ACCESS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_WAKE_OP_FILE_SHARED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FUTEX_REWAIT_CHANGED_WORD_GUEST: OnceLock<PathBuf> = OnceLock::new();
static KILL_MULTITHREADED_PROCESS_GUEST: OnceLock<PathBuf> = OnceLock::new();
static KILL_PROCESS_GROUP_GUEST: OnceLock<PathBuf> = OnceLock::new();
static TIMER_CREATE_RT_SIGNAL_REFUSED_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_LSEEK_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static STDIO_INODE_IDENTITY_GUEST: OnceLock<PathBuf> = OnceLock::new();
static REPLAY_EPOCH_GUEST: OnceLock<PathBuf> = OnceLock::new();
static FORK_CHILD_GETRANDOM_GUEST: OnceLock<PathBuf> = OnceLock::new();
static SIGSUSPEND_AFTER_WNOHANG_WAIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static SIGSUSPEND_SHARED_MASK_REWRITE_GUEST: OnceLock<PathBuf> = OnceLock::new();
static SIGSUSPEND_SHARED_STACK_MASK_GUEST: OnceLock<PathBuf> = OnceLock::new();
static SIGCHLD_ONCE_PER_CHILD_EXIT_GUEST: OnceLock<PathBuf> = OnceLock::new();
static CLONE_EXIT_SIGNAL_EFFECTIVE_GUEST: OnceLock<PathBuf> = OnceLock::new();
#[cfg(feature = "liteinst")]
static LITEINST_IN_GUEST_WAIT_SIGNALS_GUEST: OnceLock<PathBuf> = OnceLock::new();
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

/// Hermit refuses to start a guest under an inherited seccomp filter unless
/// told to ignore it; the tests that install one on purpose say so.
const UNSAFE_IGNORE_HOST_SECCOMP: &str = "--unsafe-ignore-host-seccomp";

/// [`hermit_command`] for a test that installs a seccomp filter in hermit's
/// process: the filter is inherited, so the command ignores it explicitly.
fn hermit_command_under_host_filter(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg(UNSAFE_IGNORE_HOST_SECCOMP);
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

/// A directory under `CARGO_TARGET_TMPDIR` owned by this test process. Nextest
/// runs every test in its own process, so each process builds its fixtures
/// again; with a shared path, two tests running at once would write, or run,
/// the same file.
fn process_build_root(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(name)
        .join("build")
        .join(std::process::id().to_string())
}

fn dbt_stderr_guest() -> &'static Path {
    DBT_STDERR_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("dbt-stderr-nostdlib");
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
        let build_root = process_build_root("dbt-hermit-log-env");
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

fn dbt_guest_environment_guest() -> &'static Path {
    DBT_GUEST_ENVIRONMENT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("dbt-guest-environment");
        fs::create_dir_all(&build_root)
            .expect("failed to create DBT guest-environment guest directory");
        let guest = build_root.join("guest_environment");
        let output = Command::new("cc")
            .args(["-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("hermit-cli/tests/fixtures/dbt/guest_environment.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile DBT guest-environment guest");
        assert!(
            output.status.success(),
            "DBT guest-environment guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
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
    // included at their stream positions, so the verdict names all_records_v1.
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
        let build_root = process_build_root("liteinst-inert-runtime");
        fs::create_dir_all(&build_root).expect("failed to create inert runtime directory");
        let runtime = build_root.join("libdetcore_liteinst_inert.so");
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
        runtime
    })
}

fn exec_clock_continuity_guest() -> &'static Path {
    EXEC_CLOCK_CONTINUITY_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("exec-clock-continuity");
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

fn hb_two_threads_guest() -> &'static Path {
    HB_TWO_THREADS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-two-threads");
        fs::create_dir_all(&build_root).expect("failed to create hb-two-threads guest directory");
        let guest = build_root.join("hb_two_threads");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_two_threads.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-two-threads guest");
        assert!(
            output.status.success(),
            "hb-two-threads guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_source_then_futex_guest() -> &'static Path {
    HB_SOURCE_THEN_FUTEX_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-source-then-futex");
        fs::create_dir_all(&build_root)
            .expect("failed to create hb-source-then-futex guest directory");
        let guest = build_root.join("hb_source_then_futex");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/hb_source_then_futex.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-source-then-futex guest");
        assert!(
            output.status.success(),
            "hb-source-then-futex guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_relative_anchor_guest() -> &'static Path {
    HB_RELATIVE_ANCHOR_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-relative-anchor");
        fs::create_dir_all(&build_root).expect("failed to create hb-relative-anchor directory");
        let guest = build_root.join("hb_relative_anchor");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_relative_anchor.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-relative-anchor guest");
        assert!(
            output.status.success(),
            "hb-relative-anchor guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_kill_held_child_guest() -> &'static Path {
    HB_KILL_HELD_CHILD_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-kill-held-child");
        fs::create_dir_all(&build_root).expect("failed to create hb-kill-held-child directory");
        let guest = build_root.join("hb_kill_held_child");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_kill_held_child.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-kill-held-child guest");
        assert!(
            output.status.success(),
            "hb-kill-held-child guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_child_wait_cycle_guest() -> &'static Path {
    HB_CHILD_WAIT_CYCLE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-child-wait-cycle");
        fs::create_dir_all(&build_root).expect("failed to create hb-child-wait-cycle directory");
        let guest = build_root.join("hb_child_wait_cycle");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_child_wait_cycle.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-child-wait-cycle guest");
        assert!(
            output.status.success(),
            "hb-child-wait-cycle guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_posthook_futex_interrupt_guest() -> &'static Path {
    HB_POSTHOOK_FUTEX_INTERRUPT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-posthook-futex-interrupt");
        fs::create_dir_all(&build_root)
            .expect("failed to create hb-posthook-futex-interrupt directory");
        let guest = build_root.join("hb_posthook_futex_interrupt");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_posthook_futex_interrupt.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-posthook-futex-interrupt guest");
        assert!(
            output.status.success(),
            "hb-posthook-futex-interrupt guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_pipe_read_held_writer_guest() -> &'static Path {
    HB_PIPE_READ_HELD_WRITER_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-pipe-read-held-writer");
        fs::create_dir_all(&build_root)
            .expect("failed to create hb-pipe-read-held-writer directory");
        let guest = build_root.join("hb_pipe_read_held_writer");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_pipe_read_held_writer.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-pipe-read-held-writer guest");
        assert!(
            output.status.success(),
            "hb-pipe-read-held-writer guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_spawn_echo_guest() -> &'static Path {
    HB_SPAWN_ECHO_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-spawn-echo");
        fs::create_dir_all(&build_root).expect("failed to create hb-spawn-echo directory");
        let guest = build_root.join("hb_spawn_echo");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/hb_spawn_echo.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-spawn-echo guest");
        assert!(
            output.status.success(),
            "hb-spawn-echo guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_spawn_dup2_guest() -> &'static Path {
    HB_SPAWN_DUP2_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-spawn-dup2");
        fs::create_dir_all(&build_root).expect("failed to create hb-spawn-dup2 guest directory");
        let guest = build_root.join("hb_spawn_dup2");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/hb_spawn_dup2.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-spawn-dup2 guest");
        assert!(
            output.status.success(),
            "hb-spawn-dup2 guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn poll_timeout_vs_spinner_guest() -> &'static Path {
    POLL_TIMEOUT_VS_SPINNER_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("poll-timeout-vs-spinner");
        fs::create_dir_all(&build_root)
            .expect("failed to create poll-timeout-vs-spinner guest directory");
        let guest = build_root.join("poll_timeout_vs_spinner");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/poll_timeout_vs_spinner.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile poll-timeout-vs-spinner guest");
        assert!(
            output.status.success(),
            "poll-timeout-vs-spinner guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn vfork_parent_killed_guest() -> &'static Path {
    VFORK_PARENT_KILLED_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("vfork-parent-killed");
        fs::create_dir_all(&build_root)
            .expect("failed to create vfork-parent-killed guest directory");
        let guest = build_root.join("vfork_parent_killed");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/vfork_parent_killed.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile vfork-parent-killed guest");
        assert!(
            output.status.success(),
            "vfork-parent-killed guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

/// Compiles a futex guest from `tests/c` once per test process.
fn futex_c_guest(cell: &'static OnceLock<PathBuf>, name: &str) -> &'static Path {
    cell.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root(&name.replace('_', "-"));
        fs::create_dir_all(&build_root).expect("failed to create the futex guest directory");
        let guest = build_root.join(name);
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join(format!("tests/c/{name}.c")))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the futex guest");
        assert!(
            output.status.success(),
            "{name} guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn timer_create_rt_signal_refused_guest() -> &'static Path {
    TIMER_CREATE_RT_SIGNAL_REFUSED_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("timer-create-rt-signal-refused");
        fs::create_dir_all(&build_root)
            .expect("failed to create timer-create-rt-signal-refused guest directory");
        let guest = build_root.join("timer_create_rt_signal_refused");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/timer_create_rt_signal_refused.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile timer-create-rt-signal-refused guest");
        assert!(
            output.status.success(),
            "timer-create-rt-signal-refused guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn futex_wake_count_zero_guest() -> &'static Path {
    FUTEX_WAKE_COUNT_ZERO_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("futex-wake-count-zero");
        fs::create_dir_all(&build_root)
            .expect("failed to create futex-wake-count-zero guest directory");
        let guest = build_root.join("futex_wake_count_zero");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/futex_wake_count_zero.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile futex-wake-count-zero guest");
        assert!(
            output.status.success(),
            "futex-wake-count-zero guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn kill_multithreaded_process_guest() -> &'static Path {
    KILL_MULTITHREADED_PROCESS_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("kill-multithreaded-process");
        fs::create_dir_all(&build_root)
            .expect("failed to create kill-multithreaded-process guest directory");
        let guest = build_root.join("kill_multithreaded_process");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/kill_multithreaded_process.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile kill-multithreaded-process guest");
        assert!(
            output.status.success(),
            "kill-multithreaded-process guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn kill_process_group_guest() -> &'static Path {
    KILL_PROCESS_GROUP_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("kill-process-group");
        fs::create_dir_all(&build_root)
            .expect("failed to create kill-process-group guest directory");
        let guest = build_root.join("kill_process_group");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/kill_process_group.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile kill-process-group guest");
        assert!(
            output.status.success(),
            "kill-process-group guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn vfork_kill_exec_guest() -> &'static Path {
    VFORK_KILL_EXEC_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("vfork-kill-exec");
        fs::create_dir_all(&build_root).expect("failed to create vfork-kill-exec guest directory");
        let guest = build_root.join("vfork_kill_exec");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/vfork_kill_exec.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile vfork-kill-exec guest");
        assert!(
            output.status.success(),
            "vfork-kill-exec guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn hb_signal_while_held_guest() -> &'static Path {
    HB_SIGNAL_WHILE_HELD_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("hb-signal-while-held");
        fs::create_dir_all(&build_root)
            .expect("failed to create hb-signal-while-held guest directory");
        let guest = build_root.join("hb_signal_while_held");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(repository.join("tests/c/hb_signal_while_held.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile hb-signal-while-held guest");
        assert!(
            output.status.success(),
            "hb-signal-while-held guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
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
        let build_root = process_build_root("stdio-lseek-identity");
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
        let build_root = process_build_root("stdio-inode-identity");
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
        let build_root = process_build_root("replay-epoch-probe");
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
        let build_root = process_build_root("fork-child-getrandom");
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

fn sigsuspend_after_wnohang_wait_guest() -> &'static Path {
    SIGSUSPEND_AFTER_WNOHANG_WAIT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("sigsuspend-after-wnohang-wait");
        fs::create_dir_all(&build_root)
            .expect("failed to create the sigsuspend-after-wnohang-wait guest directory");
        let guest = build_root.join("sigsuspend_after_wnohang_wait");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/sigsuspend_after_wnohang_wait.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the sigsuspend-after-wnohang-wait guest");
        assert!(
            output.status.success(),
            "sigsuspend-after-wnohang-wait guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn sigsuspend_shared_mask_rewrite_guest() -> &'static Path {
    SIGSUSPEND_SHARED_MASK_REWRITE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("sigsuspend-shared-mask-rewrite");
        fs::create_dir_all(&build_root)
            .expect("failed to create the sigsuspend-shared-mask-rewrite guest directory");
        let guest = build_root.join("sigsuspend_shared_mask_rewrite");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/sigsuspend_shared_mask_rewrite.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the sigsuspend-shared-mask-rewrite guest");
        assert!(
            output.status.success(),
            "sigsuspend-shared-mask-rewrite guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn sigchld_once_per_child_exit_guest() -> &'static Path {
    SIGCHLD_ONCE_PER_CHILD_EXIT_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("sigchld-once-per-child-exit");
        fs::create_dir_all(&build_root)
            .expect("failed to create the sigchld-once-per-child-exit guest directory");
        let guest = build_root.join("sigchld_once_per_child_exit");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/sigchld_once_per_child_exit.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the sigchld-once-per-child-exit guest");
        assert!(
            output.status.success(),
            "sigchld-once-per-child-exit guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn sigsuspend_shared_stack_mask_guest() -> &'static Path {
    SIGSUSPEND_SHARED_STACK_MASK_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("sigsuspend-shared-stack-mask");
        fs::create_dir_all(&build_root)
            .expect("failed to create the sigsuspend-shared-stack-mask guest directory");
        let guest = build_root.join("sigsuspend_shared_stack_mask");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/sigsuspend_shared_stack_mask.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the sigsuspend-shared-stack-mask guest");
        assert!(
            output.status.success(),
            "sigsuspend-shared-stack-mask guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn clone_exit_signal_effective_guest() -> &'static Path {
    CLONE_EXIT_SIGNAL_EFFECTIVE_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("clone-exit-signal-effective");
        fs::create_dir_all(&build_root)
            .expect("failed to create the clone-exit-signal-effective guest directory");
        let guest = build_root.join("clone_exit_signal_effective");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/clone_exit_signal_effective.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the clone-exit-signal-effective guest");
        assert!(
            output.status.success(),
            "clone-exit-signal-effective guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

#[cfg(feature = "liteinst")]
fn liteinst_in_guest_wait_signals_guest() -> &'static Path {
    LITEINST_IN_GUEST_WAIT_SIGNALS_GUEST.get_or_init(|| {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/liteinst_in_guest_wait_signals.c");
        let build_root = process_build_root("liteinst-in-guest-wait-signals");
        fs::create_dir_all(&build_root)
            .expect("failed to create the in-guest wait-signals guest directory");
        let guest = build_root.join("liteinst_in_guest_wait_signals");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile the in-guest wait-signals guest");
        assert!(
            output.status.success(),
            "in-guest wait-signals guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
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
        let build_root = process_build_root("dbt-mmap");
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
        let build_root = process_build_root("dbt-exec-failure");
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
        let build_root = process_build_root("dbt-execveat");
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
        let build_root = process_build_root("dbt-wait");
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
        let build_root = process_build_root("kvm-exact-child-waits");
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

fn kvm_task_ids_guest() -> &'static Path {
    KVM_TASK_IDS_GUEST.get_or_init(|| {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_task_ids.c");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-task-ids");
        fs::create_dir_all(&build_root).expect("failed to create KVM task-ID guest directory");
        let guest = build_root.join("kvm_task_ids");
        let output = Command::new("cc")
            .args(["-O0", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile KVM task-ID guest");
        assert!(
            output.status.success(),
            "KVM task-ID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
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
        let build_root = process_build_root("kvm-gettimeofday-efault");
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
        let build_root = process_build_root("dbt-pid");
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
        let build_root = process_build_root("dbt-prlimit-self");
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
fn gettimeofday_unreadable_buffer_guest() -> &'static Path {
    GETTIMEOFDAY_UNREADABLE_BUFFER_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("gettimeofday-unreadable-buffer");
        fs::create_dir_all(&build_root)
            .expect("failed to create gettimeofday unreadable-buffer guest directory");
        let guest = build_root.join("gettimeofday_unreadable_buffer");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c/gettimeofday_unreadable_buffer.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("failed to compile gettimeofday unreadable-buffer guest");
        assert!(
            output.status.success(),
            "gettimeofday unreadable-buffer guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        guest
    })
}

fn dbt_unsupported_syscall_guest() -> &'static Path {
    DBT_UNSUPPORTED_SYSCALL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = process_build_root("dbt-unsupported-syscall");
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
        let build_root = process_build_root("dbt-self-sigqueue");
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
            set_seccomp_filter(&mut filter)
        });
    }
}

/// A seccomp filter hermit inherits is an input it does not record, so by
/// default hermit refuses to start the guest: a policy refusal before anything
/// runs, naming the filter and the two ways forward
/// (https://github.com/rrnewton/hermit/issues/3942).
#[test]
fn run_refuses_an_inherited_seccomp_filter() {
    let _guard = hermit_run_guard();
    let args = ["run", "--", "/bin/echo", "the guest ran"];
    let mut command = hermit_command(&args);
    deny_syscall(&mut command, libc::SYS_acct);
    let output = command.stdin(Stdio::null()).output().unwrap();
    let stderr = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert_eq!(stdout(&output), "", "the guest must not start");
    for expected in [
        "hermit inherited a seccomp filter (Seccomp: 2 (filter mode), Seccomp_filters: ",
        "an input hermit does not record",
        "`--security-opt seccomp=unconfined`",
        "--unsafe-ignore-host-seccomp",
    ] {
        assert!(
            stderr.contains(expected),
            "missing {expected:?} in:\n{stderr}"
        );
    }
}

/// `--unsafe-ignore-host-seccomp` starts the guest anyway, warns on stderr,
/// records the filter in the INFO log, and is saved in a run config so that
/// loading it repeats the choice. It may follow the subcommand.
#[test]
fn run_under_an_inherited_seccomp_filter_with_the_unsafe_override_is_recorded() {
    let _guard = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log = directory.path().join("hermit.log");
    let config = directory.path().join("run.yaml");
    let log_arg = format!("--log-file={}", log.display());
    let config_arg = format!("--save-config={}", config.display());
    let args = [
        "--log=info",
        log_arg.as_str(),
        "run",
        UNSAFE_IGNORE_HOST_SECCOMP,
        config_arg.as_str(),
        "--",
        "/bin/echo",
        "the guest ran",
    ];
    let mut command = hermit_command(&args);
    deny_syscall(&mut command, libc::SYS_acct);
    let output = command.stdin(Stdio::null()).output().unwrap();
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "the guest ran\n");
    let stderr = stderr(&output);
    assert!(
        stderr.contains(
            "WARNING: --unsafe-ignore-host-seccomp: hermit is starting the guest under a \
             seccomp filter it inherited (Seccomp: 2 (filter mode), Seccomp_filters: "
        ),
        "{stderr}"
    );
    let log = fs::read_to_string(&log).unwrap();
    assert!(
        log.lines().any(|line| line.contains(
            " INFO hermit::host_seccomp: unverified host seccomp filter: Seccomp: 2 (filter mode)"
        )),
        "{log}"
    );
    let saved: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(
        saved["global"]["unsafe-ignore-host-seccomp"], true,
        "{saved:?}"
    );
}

/// Deny `ioctl(fd, TCGETS)`, the terminal query behind glibc's `isatty`, with
/// `errno` in hermit and everything it starts, and leave every other ioctl
/// alone. A terminal then looks to `isatty` like something that is not a
/// terminal. With `EPERM` the error still differs from the `ENOTTY` that a
/// descriptor which really is not a terminal answers; with `ENOTTY` nothing
/// does.
fn deny_terminal_query(command: &mut Command, errno: i32) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec.
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
                    jf: 3, // not ioctl: allow
                    k: libc::SYS_ioctl as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    // offsetof(seccomp_data, args[1]): the low 32 bits of the
                    // request on a little-endian host.
                    k: 24,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 1, // another request: allow
                    k: libc::TCGETS as u32,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | errno as u32, // SECCOMP_RET_ERRNO
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff_0000, // SECCOMP_RET_ALLOW
                },
            ];
            set_seccomp_filter(&mut filter)
        });
    }
}

/// Answer every question that tells what kind of file stderr is with
/// `errno`, in hermit and everything it starts: `statx`, `newfstatat` and
/// `fstat` with descriptor 2, `getsockopt` on descriptor 2, and
/// `fcntl(2, F_GETPIPE_SZ)`. Every other call is left alone, those on other
/// descriptors and other `fcntl` commands included. With `errno` 0 each of
/// those calls returns 0 and fills in nothing: it feigns success. With
/// `errno` `EINTR` each of them is interrupted, every time it is retried.
fn deny_file_type_queries_on_stderr(command: &mut Command, errno: i32) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec.
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
                    jt: 6,      // to the descriptor check
                    jf: 0,
                    k: libc::SYS_statx as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 5,
                    jf: 0,
                    k: libc::SYS_newfstatat as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 4,
                    jf: 0,
                    k: libc::SYS_fstat as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 3,
                    jf: 0,
                    k: libc::SYS_getsockopt as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 5, // another call: allow
                    k: libc::SYS_fcntl as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    // offsetof(seccomp_data, args[1]): the low 32 bits of the
                    // fcntl command on a little-endian host.
                    k: 24,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 3, // another fcntl command: allow
                    k: libc::F_GETPIPE_SZ as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    // offsetof(seccomp_data, args[0]): the low 32 bits of the
                    // descriptor on a little-endian host.
                    k: 16,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 1, // another descriptor: allow
                    k: 2,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: 0x0005_0000 | errno as u32, // SECCOMP_RET_ERRNO
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: 0x7fff_0000, // SECCOMP_RET_ALLOW
                },
            ];
            set_seccomp_filter(&mut filter)
        });
    }
}

/// Deny `statx` on descriptor 2 with `EPERM`, and make `fstat` and
/// `newfstatat` on descriptor 2 wait forever, in hermit and everything it
/// starts; leave every other call alone. The two wait on a seccomp user
/// notification that nothing answers, which stands in for a FUSE server that
/// has stopped answering, behind a FIFO on stderr whose cached attributes
/// have expired: `fstat` asks the server, `statx(AT_STATX_DONT_SYNC)` would
/// not. The notification listener is left open without `FD_CLOEXEC`, so
/// hermit inherits it and holds it for as long as it runs; with no listener
/// left, the calls would fail with `ENOSYS` instead of waiting.
fn deny_statx_and_hold_the_stat_of_stderr(command: &mut Command) {
    // SAFETY: The callback makes only async-signal-safe syscalls before exec.
    unsafe {
        command.pre_exec(|| {
            let mut filter = [
                libc::sock_filter {
                    code: 0x20, // BPF_LD | BPF_W | BPF_ABS
                    jt: 0,
                    jf: 0,
                    k: 0, // offsetof(seccomp_data, nr)
                },
                libc::sock_filter {
                    code: 0x15, // BPF_JMP | BPF_JEQ | BPF_K
                    jt: 2,      // to statx's descriptor check
                    jf: 0,
                    k: libc::SYS_statx as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 4, // to the stat calls' descriptor check
                    jf: 0,
                    k: libc::SYS_newfstatat as u32,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 3,
                    jf: 6, // another call: allow
                    k: libc::SYS_fstat as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    // offsetof(seccomp_data, args[0]): the low 32 bits of the
                    // descriptor on a little-endian host.
                    k: 16,
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 4, // another descriptor: allow
                    k: 2,
                },
                libc::sock_filter {
                    code: 0x06, // BPF_RET | BPF_K
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
                },
                libc::sock_filter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 16, // offsetof(seccomp_data, args[0])
                },
                libc::sock_filter {
                    code: 0x15,
                    jt: 0,
                    jf: 1, // another descriptor: allow
                    k: 2,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_USER_NOTIF,
                },
                libc::sock_filter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_ALLOW,
                },
            ];
            let program = libc::sock_fprog {
                len: filter.len() as u16,
                filter: filter.as_mut_ptr(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let listener = libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
                &program as *const libc::sock_fprog,
            );
            if listener < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(listener as libc::c_int, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Install `filter` as a seccomp filter on the calling thread, after
/// `PR_SET_NO_NEW_PRIVS`. Only two `prctl` calls, so a `pre_exec` callback may
/// use it.
fn set_seccomp_filter(filter: &mut [libc::sock_filter]) -> std::io::Result<()> {
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: `program` points at `filter`, which outlives both calls.
    unsafe {
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
    }
    Ok(())
}

fn readonly_proc_command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    // The mount filter below is inherited, which hermit refuses by default.
    command.arg(UNSAFE_IGNORE_HOST_SECCOMP);
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
    let mut command = hermit_command_under_host_filter(&args);
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

/// Default DBT networking must isolate the same port-zero/listen workload as
/// the common container path, even when its caller holds that assigned port.
/// The native helper creates its own outer user/net namespace, so this neither
/// depends on ambient host listeners nor occupies a port another lane can use.
#[test]
fn dbt_network_modes_preserve_the_outer_listener_and_isolate_local_runs() {
    if dbt_unavailable("dbt_network_modes_preserve_the_outer_listener_and_isolate_local_runs") {
        return;
    }
    let _guard = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("create DBT network collision evidence directory")
        .keep();
    println!("DBT network collision evidence: {}", directory.display());
    let guest = directory.join("network_namespace_collision");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dbt/network_namespace_collision.c");
    let compile = Command::new("cc")
        .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("compile DBT network collision fixture");
    fs::write(directory.join("compiler.stdout"), &compile.stdout).unwrap();
    fs::write(directory.join("compiler.stderr"), &compile.stderr).unwrap();
    assert!(
        compile.status.success(),
        "network collision fixture did not compile: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let run = |label: &str, options: &[&str], mode: &str| {
        let mut command = Command::new(&guest);
        command
            .arg("outer")
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log",
                "info",
                "--backend",
                "dbt",
                "run",
                "--base-env=minimal",
                "--epoch=2026-10-09T00:00:00Z",
            ])
            .args(options)
            .arg("--")
            .arg(&guest)
            .arg(mode)
            // Each command explicitly chooses its bound or unbound execution
            // path. In particular the plain local control must not acquire the
            // pinned root's /test marker and accidentally test only mounts.
            .env_remove(ISOLATED_WORKDIR_ENV);
        let argv = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        fs::write(
            directory.join(format!("{label}.argv.json")),
            serde_json::to_vec_pretty(&argv).unwrap(),
        )
        .unwrap();
        let output = command.output().expect("run native outer network helper");
        fs::write(directory.join(format!("{label}.stdout")), &output.stdout).unwrap();
        fs::write(directory.join(format!("{label}.stderr")), &output.stderr).unwrap();
        fs::write(
            directory.join(format!("{label}.status")),
            format!("{}\n", output.status),
        )
        .unwrap();
        assert!(
            stderr(&output).contains("native exclusive outer holder ready at 127.0.0.1:32768")
                && stderr(&output)
                    .contains("native exclusive outer holder verified after CLI exit"),
            "{label}: outer setup or held-listener check failed: {}",
            stderr(&output)
        );
        output
    };

    // Deliberately shared networking is a separate negative control. Do not
    // classify its expected collision as the local isolation success oracle.
    let host = run("host-collision", &["--network=host"], "guest-host");
    assert!(host.status.success(), "host control: {}", stderr(&host));
    assert_eq!(stdout(&host), "dbt-host-collision-observed\n");

    let bound = directory.join("bound-workdir");
    fs::create_dir(&bound).unwrap();
    let bind = format!("{}:/tmp/dbt-netns-regression", bound.display());
    let logs = directory.join("verify-logs");
    fs::create_dir(&logs).unwrap();
    let verdict = directory.join("default.verify.json");
    let verified = run(
        "default-verify",
        &[
            "--strict",
            "--verify-strict",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--verify-json",
            verdict.to_str().unwrap(),
            "--bind",
            &bind,
            "--workdir=/tmp/dbt-netns-regression",
        ],
        "guest-local",
    );
    assert!(
        verified.status.success(),
        "default strict verification: {}",
        stderr(&verified)
    );
    assert_eq!(stdout(&verified), "dbt-local-port-zero-listeners-ok\n");
    let report = read_terminal_dbt_verdict(&verdict);
    assert_eq!(report["verdict"], "matched", "{report}");
    assert_eq!(report["comparison"]["compare_io_buffers"], true, "{report}");
    assert_eq!(report["comparison"]["compare_logs"], true, "{report}");
    assert_eq!(
        report["compared_log_messages"]["left"], report["compared_log_messages"]["right"],
        "{report}"
    );

    // The unbound path previously created a shared runtime without any scoped
    // isolation. A bound verification passing does not exercise this path.
    let local = run(
        "unbound-local",
        &["--strict", "--network=local"],
        "guest-local",
    );
    assert!(local.status.success(), "unbound local: {}", stderr(&local));
    assert_eq!(stdout(&local), "dbt-local-port-zero-listeners-ok\n");

    let strict_host = run(
        "strict-host-refusal",
        &["--strict", "--network=host"],
        "guest-local",
    );
    assert!(!strict_host.status.success());
    assert!(stdout(&strict_host).is_empty());
    assert!(
        stderr(&strict_host).contains("--strict is fail-closed deterministic mode")
            && stderr(&strict_host).contains("--network=host"),
        "strict host refusal: {}",
        stderr(&strict_host)
    );

    // `none` is the pinned outer container's policy, not a Hermit CLI mode.
    let none = run("unsupported-none", &["--network=none"], "guest-local");
    assert_eq!(none.status.code(), Some(2), "{}", stderr(&none));
    assert!(stdout(&none).is_empty());
    assert!(
        stderr(&none).contains("Could not parse"),
        "{}",
        stderr(&none)
    );
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
#[ignore = "requires the pinned-root isolation validation node"]
fn run_dbt_binds_in_a_user_namespace_of_its_own() {
    if !cfg!(feature = "dbt") {
        panic!("the bind control requires the DBT feature");
    }
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    fs::write(directory.path().join("input"), "bound input\n").unwrap();
    let bind = format!("{}:/tmp/e2e/bound", directory.path().display());
    let mut command = hermit_command(&[
        "--backend",
        "dbt",
        "run",
        "--bind",
        &bind,
        "--",
        "/bin/sh",
        "-c",
        "cat /tmp/e2e/bound/input; cat /proc/self/uid_map",
    ]);
    // Hermit must work without CAP_SYS_ADMIN, the case its user namespace is
    // for: drop it from the bounding set, so even a root caller (the pinned
    // root) execs Hermit without it. An unprivileged caller cannot drop it and
    // never had it.
    // SAFETY: prctl is async-signal-safe and touches only this child.
    unsafe {
        command.pre_exec(|| {
            const CAP_SYS_ADMIN: libc::c_ulong = 21;
            libc::prctl(libc::PR_CAPBSET_DROP, CAP_SYS_ADMIN, 0, 0, 0);
            Ok(())
        });
    }
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("bound input"), "{text}");
    // The mounts --bind needs come from a user namespace of the DBT adapter's
    // own, which maps root to the caller alone (as reverie's Container::map_root
    // does for every other backend), not from a privileged host. A guest left in
    // the caller's namespace would show the caller's own map instead, whether
    // that is the host's full range or a container's map.
    let guest_map = lines.next().unwrap_or_default();
    let map = guest_map.split_whitespace().collect::<Vec<_>>();
    let euid = unsafe { libc::geteuid() }.to_string();
    assert_eq!(map, ["0", euid.as_str(), "1"], "{text}");
    let caller_map = fs::read_to_string("/proc/self/uid_map").unwrap();
    assert_ne!(
        caller_map.split_whitespace().collect::<Vec<_>>(),
        map,
        "the guest shares the caller's user namespace: {text}"
    );
    assert_eq!(lines.next(), None, "{text}");
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
/// The guest is the dispatch-record guest (`common/dispatch_stats.rs`): 64 raw
/// `getppid` calls through one `syscall` site in the C library. In-guest
/// LiteInst traps the site's first call, patches the site, and runs the other
/// 63 through the patched hook, so a record that really came from this run
/// counts at least 63 direct hooks; other sites in the guest's start-up may
/// add more. In-guest, each guest process submits its own counts, so the one
/// process here is `process_reports=1`; under the retired ptrace-hosted hybrid
/// the host counted every hook itself and this read 0.
///
/// The hybrid version of this test also ran the guest under `--verify
/// --keep-logs` and required exactly one record in run 1's golden log and in
/// run 2's log (https://github.com/rrnewton/hermit/issues/3301). This in-guest
/// version checks only the plain-run call site.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_backend_stats_report_the_guests_own_dispatch_paths() {
    let _guard = hermit_run_guard();
    let guest = dispatch_stats::build_guest("guest-liteinst-stats", &[]);
    let run = |log: &[&str]| {
        let mut args = log.to_vec();
        args.extend([
            "--backend",
            "liteinst",
            "run",
            "--strict",
            "--max-timeslice=disabled",
            "--",
        ]);
        args.push(guest.to_str().expect("dispatch guest path is UTF-8"));
        let mut command = hermit_command(&args);
        command
            .env_remove("RUST_LOG")
            .env_remove("HERMIT_LOG")
            .env_remove("HERMIT_LOG_FILE")
            .stdin(Stdio::null());
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to run LiteInst Hermit with {args:?}: {error}"));
        assert_success(&output, &args);
        assert_eq!(stdout(&output), "dispatch-stats-guest 1\n");
        stderr(&output)
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
                "backend run complete backend=liteinst stats=LiteInst instrumentation stats: process_reports=1 "
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
            direct_hooks >= dispatch_stats::GUEST_SYSCALLS - 1,
            "the guest makes {} hooked calls, but {source} counts {direct_hooks}: {record}",
            dispatch_stats::GUEST_SYSCALLS - 1
        );
    };

    for log in [&[][..], &["--log", "info"][..]] {
        let stderr = run(log);
        assert!(
            !stderr.contains("backend run complete"),
            "the record is DEBUG-only, but {log:?} printed it:\n{stderr}"
        );
    }

    check_record("the run's stderr", &run(&["--log", "debug"]));
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
fn run_liteinst_rejects_a_non_runtime_override_before_dispatch() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create false LiteInst runtime directory");
    let runtime = directory.path().join("not-a-liteinst-runtime");
    fs::copy("/bin/true", &runtime).expect("failed to copy false LiteInst runtime fixture");
    // The in-guest Tool host has no preemption timer, so the run disables it
    // and reaches runtime validation instead of the timeslice refusal.
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env("HERMIT_LITEINST_TOOL_RUNTIME", &runtime)
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
fn run_liteinst_rejects_an_inert_dso_before_dispatch() {
    // As above, the timer is disabled so that runtime validation decides.
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env("HERMIT_LITEINST_TOOL_RUNTIME", liteinst_inert_runtime())
        .output()
        .expect("failed to run Hermit with an inert LiteInst runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("does not register detcore_liteinst_initialize as a preload constructor"),
        "{stderr}"
    );
    assert!(!stderr.contains("activation verified"), "{stderr}");
    assert!(!stderr.contains("Success: deterministic"), "{stderr}");
}

/// An installed Hermit finds the in-guest runtime as the packaged resource
/// `rsrcs/libdetcore_liteinst.so`, which `hermit-install` stages beside the DBT
/// and SaBRe Detcore runtimes, and a packaged resource is consulted before the
/// Cargo artifact directory.
///
/// The installation is a directory holding a copy of this test's Hermit and an
/// `rsrcs/` directory, the layout a dereferenced copy of `target/install_pkg`
/// has. It is a copy and never a hard link: a link adds a name to the inode of
/// the Cargo-built Hermit, which changes its ctime and link count, and the
/// validation runner refuses a prepared test input whose metadata moves while
/// it hashes it, so another node starting at that moment failed with "prepared
/// input changed while hashing ... nlink 2 -> 3". The test checks that the
/// Cargo-built Hermit's inode is untouched at the end. Nothing else can supply
/// the runtime there: no
/// `libdetcore_liteinst.so` sits beside that Hermit, and
/// `HERMIT_LITEINST_TOOL_RUNTIME` and `HERMIT_INSTALL_DIR` are removed. So the
/// run fails while `rsrcs/` is empty, which is the control, and succeeds in-guest
/// once the runtime is staged there. Last, the Cargo-built Hermit, whose own
/// directory does hold the real runtime, is pointed at an installation whose
/// packaged runtime is the inert fixture, and must refuse that fixture.
#[test]
#[cfg(feature = "liteinst")]
fn run_liteinst_finds_the_runtime_staged_as_an_installed_resource() {
    use std::os::unix::fs::MetadataExt;

    let _guard = hermit_run_guard();
    let copy = |source: &Path, destination: &Path| {
        fs::copy(source, destination).unwrap_or_else(|error| {
            panic!(
                "failed to copy {} to {}: {error}",
                source.display(),
                destination.display()
            )
        });
    };
    let built = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let inode_state = |path: &Path| {
        let metadata = fs::metadata(path)
            .unwrap_or_else(|error| panic!("failed to stat {}: {error}", path.display()));
        (
            metadata.ino(),
            metadata.nlink(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    let built_before = inode_state(built);
    let runtime = built
        .parent()
        .expect("the Cargo-built Hermit has a profile directory")
        .join("libdetcore_liteinst.so");
    assert!(
        runtime.is_file(),
        "{} is missing; build it with `cargo build -p detcore-liteinst` in this profile",
        runtime.display()
    );
    let install = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the installation directory");
    let installed_hermit = install.path().join("hermit");
    copy(built, &installed_hermit);
    let resources = install.path().join("rsrcs");
    fs::create_dir(&resources).expect("failed to create the rsrcs directory");

    let args = [
        "--backend",
        "liteinst",
        "run",
        "--strict",
        "--max-timeslice=disabled",
        "--",
        "/bin/echo",
        "installed-liteinst-ok",
    ];
    let run_installed = || {
        let mut command = Command::new(&installed_hermit);
        append_hermit_args(&mut command, &args);
        command
            .env_remove("HERMIT_INSTALL_DIR")
            .env_remove("HERMIT_LITEINST_TOOL_RUNTIME")
            .stdin(Stdio::null())
            .output()
            .expect("failed to run the installed Hermit")
    };

    let missing = run_installed();
    let missing_stderr = stderr(&missing);
    assert!(!missing.status.success(), "{missing:?}");
    assert!(
        missing_stderr.contains("the in-guest Detcore runtime is unavailable"),
        "{missing_stderr}"
    );
    assert!(
        !stdout(&missing).contains("installed-liteinst-ok"),
        "the guest ran without a runtime: {missing:?}"
    );

    copy(&runtime, &resources.join("libdetcore_liteinst.so"));
    let installed = run_installed();
    assert_success(&installed, &args);
    assert_eq!(stdout(&installed), "installed-liteinst-ok\n");
    assert!(
        stderr(&installed).lines().any(|line| line
            == "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool"),
        "{}",
        stderr(&installed)
    );

    let inert_install = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the inert installation directory");
    fs::create_dir(inert_install.path().join("rsrcs")).expect("failed to create rsrcs");
    fs::copy(
        liteinst_inert_runtime(),
        inert_install.path().join("rsrcs/libdetcore_liteinst.so"),
    )
    .expect("failed to stage the inert runtime");
    let output = hermit_command(&args)
        .env("HERMIT_INSTALL_DIR", inert_install.path())
        .env_remove("HERMIT_LITEINST_TOOL_RUNTIME")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run Hermit with an inert packaged runtime");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(
        stderr.contains("does not register detcore_liteinst_initialize as a preload constructor"),
        "the packaged runtime must be consulted before the one beside Hermit: {stderr}"
    );
    assert_eq!(
        inode_state(built),
        built_before,
        "staging the installation changed the inode of {} (inode, link count, ctime); \
         other validation nodes hash that file as a prepared input",
        built.display()
    );
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
        stderr.contains("detcore-dbt: using coordinator-provided Detcore Config"),
        "DBT did not consume the coordinator-provided config:\n{stderr}",
    );
    assert!(
        !stderr.contains("detcore-dbt: using CLI-provided Detcore Config")
            && !stderr.contains("using strict default"),
        "DBT fell back from coordinator authority:\n{stderr}",
    );
    assert!(
        stderr.contains("requires_thread_directed_process_signals: true"),
        "DBT did not receive its required process-signal translation capability:\n{stderr}",
    );
    assert!(
        !stderr.contains("requires_thread_directed_process_signals: false"),
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

/// DBT numbers a guest's syscalls as ptrace does, counting the execve that
/// entered the guest's first image.
///
/// Ptrace intercepts that execve and counts it as syscall #1. DynamoRIO starts
/// the guest already executing its image, so Detcore never saw the execve, and
/// DBT's syscall numbers and syscall-derived virtual time ran one behind
/// ptrace's from the first record: this guest's first write was #1 under DBT
/// and #2 under ptrace. A static guest with no libc makes the same syscalls
/// under both backends, so the whole numbered sequence must match.
#[test]
fn run_dbt_numbers_syscalls_like_ptrace_from_the_initial_execve() {
    if dbt_unavailable("run_dbt_numbers_syscalls_like_ptrace_from_the_initial_execve") {
        return;
    }
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = process_build_root("dbt-syscall-numbering");
    fs::create_dir_all(&build_root).expect("failed to create the guest directory");
    let guest = build_root.join("static_nolibc_syscall_sites");
    let output = Command::new("cc")
        .args(["-O1", "-static", "-nostdlib"])
        .arg(repository.join("tests/c/static_nolibc_syscall_sites.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile the static guest");
    assert!(
        output.status.success(),
        "static guest compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    let guest = guest.to_str().expect("guest path should be UTF-8");
    let numbered = |backend: &str| {
        let args = [
            "--log",
            "info",
            "--backend",
            backend,
            "run",
            "--strict",
            "--epoch=2026-01-01T00:00:00Z",
            "--",
            guest,
        ];
        let output = hermit(&args);
        assert_success(&output, &args);
        stderr(&output)
            .lines()
            .filter_map(|line| {
                let finish = line.split("finish syscall #").nth(1)?;
                Some(finish.split('(').next()?.to_owned())
            })
            .collect::<Vec<_>>()
    };
    let ptrace = numbered("ptrace");
    assert_eq!(
        ptrace.first().map(String::as_str),
        Some("2: write"),
        "{ptrace:#?}"
    );
    assert_eq!(numbered("dbt"), ptrace);
}

/// A DBT guest sees the environment ptrace's does, before and after an exec.
///
/// DynamoRIO runs the guest on the stack the kernel built for drrun's exec. The
/// guest's environment used to carry DynamoRIO's variables and the runtime's
/// HERMIT_DBT_* ones, among them Detcore's configuration and a coordinator
/// socket path that is random per run, and an exec'd child also got
/// DYNAMORIO_OPTIONS and DynamoRIO's other propagation variables
/// (https://github.com/rrnewton/hermit/issues/3944). The bundled DynamoRIO now
/// removes them before the guest starts. /proc/self/environ is compared
/// without its empty entries: the kernel's range still covers the removed
/// strings, erased to NUL bytes.
#[test]
fn run_dbt_guest_sees_ptraces_environment_before_and_after_exec() {
    if dbt_unavailable("run_dbt_guest_sees_ptraces_environment_before_and_after_exec") {
        return;
    }
    let guest = dbt_guest_environment_guest()
        .to_str()
        .expect("DBT guest-environment guest path should be UTF-8");
    let environment = |backend: &str| {
        let args = [
            "--backend",
            backend,
            "run",
            "--strict",
            "--base-env=minimal",
            "--epoch=2026-01-01T00:00:00Z",
            "--",
            guest,
        ];
        let output = hermit(&args);
        assert_success(&output, &args);
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let ptrace = environment("ptrace");
    for line in ["parent env PATH=", "child env PATH=", "child procenv PATH="] {
        assert!(
            ptrace.lines().any(|printed| printed.starts_with(line)),
            "the ptrace guest did not print `{line}`:\n{ptrace}"
        );
    }
    let dbt = environment("dbt");
    for private in ["HERMIT_DBT_", "DYNAMORIO_", "__disabled__"] {
        assert!(
            !dbt.contains(private),
            "the DBT guest saw {private}:\n{dbt}"
        );
    }
    assert_eq!(dbt, ptrace);
}

/// DBT verification of a guest that prints its environment passes. It failed
/// with "Mismatch in stdout between run 1 and run 2" on the coordinator socket
/// path, which was random per run and in the guest's environment.
#[test]
fn run_dbt_verifies_a_guest_that_prints_its_environment() {
    if dbt_unavailable("run_dbt_verifies_a_guest_that_prints_its_environment") {
        return;
    }
    let guest = dbt_guest_environment_guest()
        .to_str()
        .expect("DBT guest-environment guest path should be UTF-8");
    let args = [
        "--backend",
        "dbt",
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--",
        guest,
        "child",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert!(
        stderr(&output).contains("Determinism verified"),
        "{}",
        stderr(&output)
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
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create DBT exec lifecycle test directory");
    for exec_self in [false, true] {
        let verdict_path = directory.path().join(format!("exec-{exec_self}.json"));
        let mut args = vec![
            "--backend",
            "dbt",
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            verdict_path.to_str().expect("verdict path should be UTF-8"),
            "--",
            program,
        ];
        if exec_self {
            args.push("--exec-self");
        }
        let output = hermit(&args);

        assert_success(&output, &args);
        let expected = if exec_self {
            "recovered after failed exec\nsuccessful exec after failed exec\n"
        } else {
            "recovered after failed exec\n"
        };
        assert_eq!(stdout(&output), expected);
        let report = read_terminal_dbt_verdict(&verdict_path);
        assert_eq!(report["verdict"], "matched", "{report}");
    }
}
/// The reference behavior for the guest the DBT backend-failure tests use:
/// ptrace reads the guest's unreadable timeval word through the tracer, so
/// Detcore confirms that the failed gettimeofday stored nothing, and the
/// guest sees EFAULT as on Linux.
#[test]
fn run_ptrace_returns_efault_for_gettimeofday_into_an_unreadable_page() {
    let guest = gettimeofday_unreadable_buffer_guest()
        .to_str()
        .expect("gettimeofday unreadable-buffer guest path should be UTF-8");
    let args = ["run", "--", guest];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "gettimeofday returned -1 errno=EFAULT\n");
}

/// A forked child makes the failing call before any exec. Hermit's DBT
/// runtime runs Detcore in such a child too, and its copied runtime used to
/// write no stats record, so its failure went unseen when its parent ignored
/// it.
#[test]
fn run_dbt_fails_when_a_forked_child_ends_on_a_backend_failure_before_exec() {
    if dbt_unavailable("run_dbt_fails_when_a_forked_child_ends_on_a_backend_failure_before_exec") {
        return;
    }
    let guest = gettimeofday_unreadable_buffer_guest()
        .to_str()
        .expect("gettimeofday unreadable-buffer guest path should be UTF-8");
    let args = [
        "--backend",
        "dbt",
        "run",
        "--allow-unsupported-syscalls",
        "--",
        guest,
        "fork",
    ];
    let output = hermit(&args);
    let stderr = stderr(&output);

    assert_eq!(
        stdout(&output),
        "parent ignored the child's status\n",
        "{stderr}"
    );
    assert!(
        stderr.contains("detcore-dbt: Detcore failed handling a syscall: "),
        "the child's diagnostic is missing:\n{stderr}"
    );
    assert!(
        stderr.contains("1 guest process(es) ended on a DBT backend or Tool failure"),
        "the run did not name the child's failure:\n{stderr}"
    );
    assert_eq!(output.status.code(), Some(101), "{stderr}");
}

/// A shell runs a child whose Detcore handler fails with a tool error, ignores
/// the child's exit status, and exits 0. Under `--allow-unsupported-syscalls`
/// without `--verify` the guest tree has no isolated process group, so the
/// DBT client's terminal path ends only the child. The run must still fail.
#[test]
fn run_dbt_fails_when_a_child_ends_on_a_backend_failure_its_parent_ignores() {
    if dbt_unavailable("run_dbt_fails_when_a_child_ends_on_a_backend_failure_its_parent_ignores") {
        return;
    }
    let child = gettimeofday_unreadable_buffer_guest()
        .to_str()
        .expect("gettimeofday unreadable-buffer guest path should be UTF-8");
    let script = format!("{child}; echo parent-saw-status:$?");
    let args = [
        "--backend",
        "dbt",
        "run",
        "--allow-unsupported-syscalls",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ];
    let output = hermit(&args);
    let stderr = stderr(&output);

    // The child never printed; the parent saw exit 101 and went on.
    assert_eq!(stdout(&output), "parent-saw-status:101\n", "{stderr}");
    assert!(
        stderr.contains("detcore-dbt: Detcore failed handling a syscall: "),
        "the child's diagnostic is missing:\n{stderr}"
    );
    assert!(
        stderr.contains("1 guest process(es) ended on a DBT backend or Tool failure"),
        "the run did not name the child's failure:\n{stderr}"
    );
    assert_eq!(output.status.code(), Some(101), "{stderr}");
}

/// The same failing guest under `--verify`, where the process group is
/// isolated and protected evidence turns the failure into an error. The
/// error must not hide the child's diagnostic, which the guest tree wrote to
/// its captured stderr.
#[test]
fn run_dbt_verify_keeps_the_diagnostic_of_a_backend_failure() {
    if dbt_unavailable("run_dbt_verify_keeps_the_diagnostic_of_a_backend_failure") {
        return;
    }
    let child = gettimeofday_unreadable_buffer_guest()
        .to_str()
        .expect("gettimeofday unreadable-buffer guest path should be UTF-8");
    let script = format!("{child}; echo parent-saw-status:$?");
    let args = [
        "--backend",
        "dbt",
        "run",
        "--allow-unsupported-syscalls",
        "--verify",
        "--",
        "/bin/sh",
        "-c",
        &script,
    ];
    let output = hermit(&args);
    let stderr = stderr(&output);

    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("detcore-dbt: Detcore failed handling a syscall: "),
        "the failed run's diagnostic was dropped:\n{stderr}"
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

/// Hermit numbers KVM guest tasks as its ptrace backend does
/// (`reverie::task_ids::HERMIT_PTRACE_IDS_PER_TASK`): the same guest sees the
/// same task IDs under both. Root 3; its first child 5 forks grandchild 7, so
/// the next fork is 9; thread 11 starts nested thread 13; after exec, forks 15
/// and 17. Plain Linux numbering would give 4, 5, 6, 7, 8, 9, 10.
#[test]
fn run_kvm_numbers_guest_tasks_as_the_ptrace_backend() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM task IDs: /dev/kvm is unavailable");
        return;
    }
    let _guard = hermit_run_guard();
    let program = kvm_task_ids_guest()
        .to_str()
        .expect("task-ID guest path should be UTF-8");
    let expected = "root 3\ngrandchild 7\nfork 5\nfork 9\nthread 11\nnested 13\n\
                    fork-after-exec 15\nfork-after-exec 17\n";
    for backend in ["--backend=kvm", "--backend=ptrace"] {
        let args = [backend, "run", "--strict", "--tmp=/tmp", "--", program];
        let output = hermit(&args);
        assert_success(&output, &args);
        assert_eq!(stdout(&output), expected, "{backend}");
    }
}

/// KVM names the filename execve was given in AT_EXECFN, as Linux does
/// under ptrace: a `#!` script's own path even though its argv now starts with
/// the interpreter, the PATH-resolved name `env` execs, and a relative script
/// name exactly as written. Linux copies that filename to the top of the
/// stack, so naming argv[0] instead also moves every stack address the guest
/// sees; AT_RANDOM, which lies just below the strings, shows it.
#[test]
fn run_kvm_names_the_execve_filename_in_at_execfn_as_ptrace_does() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping KVM AT_EXECFN: /dev/kvm is unavailable");
        return;
    }
    let _guard = hermit_run_guard();
    let root = process_build_root("kvm-at-execfn");
    fs::create_dir_all(root.join("sub")).expect("failed to create the AT_EXECFN scripts");
    let write_script = |path: &Path, body: &str| {
        fs::write(path, body).expect("failed to write an AT_EXECFN script");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .expect("failed to make an AT_EXECFN script executable");
    };
    // ld.so prints the auxiliary vector under LD_SHOW_AUXV.
    write_script(&root.join("sub/inner.sh"), "#!/bin/true\n");
    let outer = root.join("outer.sh");
    write_script(
        &outer,
        &format!(
            "#!/bin/bash\ncd {}\n\
             LD_SHOW_AUXV=1 env true | grep -E 'AT_EXECFN|AT_RANDOM'\n\
             LD_SHOW_AUXV=1 ./sub/inner.sh | grep -E 'AT_EXECFN|AT_RANDOM'\n",
            root.display()
        ),
    );
    let inner = root.join("sub/inner.sh");
    let inner = inner.to_str().expect("script path should be UTF-8");
    let outer = outer.to_str().expect("script path should be UTF-8");

    // The initial launch of a script, then a script's in-guest execs.
    let mut execfns = Vec::new();
    for launch in [vec!["--env=LD_SHOW_AUXV=1", "--", inner], vec!["--", outer]] {
        let mut outputs = Vec::new();
        for backend in ["--backend=ptrace", "--backend=kvm"] {
            let mut args = vec![backend, "run", "--strict"];
            args.extend(&launch);
            let output = hermit(&args);
            assert_success(&output, &args);
            outputs.push(stdout(&output));
        }
        assert_eq!(
            outputs[1], outputs[0],
            "KVM vs ptrace auxiliary vectors of {launch:?}"
        );
        execfns.extend(
            outputs[1]
                .lines()
                .filter_map(|line| line.strip_prefix("AT_EXECFN:"))
                .map(|execfn| execfn.trim().to_owned()),
        );
    }
    assert_eq!(execfns.len(), 4, "AT_EXECFN entries: {execfns:?}");
    assert_eq!(execfns[0], inner);
    assert!(
        execfns[1].starts_with('/') && execfns[1].ends_with("/env"),
        "{execfns:?}"
    );
    assert!(
        execfns[2].starts_with('/') && execfns[2].ends_with("/true"),
        "{execfns:?}"
    );
    assert_eq!(execfns[3], "./sub/inner.sh");
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
    // A tv past the end of the address space, a tv on a read-only page, and a
    // tv whose tv_usec lies on a read-only page. KVM enforces page
    // permissions on these stores as Linux does.
    for (mode, expected) in [
        ("invalid-tv", "invalid-tv: EFAULT\n"),
        ("readonly-tv", "readonly-tv: EFAULT tv unchanged\n"),
        (
            "straddle-tv",
            "straddle-tv: EFAULT tv_sec stored, tv_usec unchanged\n",
        ),
    ] {
        let args = ["--backend", "kvm", "run", "--strict", "--", program, mode];
        let output = hermit(&args);

        assert_success(&output, &args);
        assert_eq!(stdout(&output), expected, "mode {mode}");
    }
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
    assert_eq!(
        stdout(&output),
        "faulting-tz: EFAULT tv between the surrounding reads\n"
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
    let build_root = process_build_root("kvm-cpuid");
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
    let report_root = process_build_root("sabre-nested-host-tmpdir");
    fs::create_dir_all(&report_root).expect("failed to create the verify report directory");
    let verify_report = report_root.join("verify.json");
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

/// dash's `wait` for a background job, as tests/c/sigsuspend_after_wnohang_wait.c
/// copies it: the child's exit falls between the parent's `wait4(WNOHANG)` and
/// its `rt_sigsuspend`, so the child-exit SIGCHLD arrives while the parent
/// blocks every signal and is still stopped at its `rt_sigsuspend` request. The
/// call must wake for it. Before, Detcore counted the call as a wait no signal
/// could end and reported a deadlock (exit 125), which made
/// `happens_before_edge_reverses_two_processes_writes` fail on hosts whose
/// /bin/sh is dash. The timeslice is disabled so a host with a PMU schedules
/// the guest as one without does.
#[test]
fn a_sigchld_sent_at_an_rt_sigsuspend_request_wakes_the_call() {
    let _guard = hermit_run_guard();
    let guest = sigsuspend_after_wnohang_wait_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--max-timeslice=disabled",
        "--",
        guest,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    let mut lines: Vec<&str> = std::str::from_utf8(&output.stdout)
        .expect("guest stdout should be UTF-8")
        .lines()
        .collect();
    lines.sort_unstable();
    assert_eq!(lines, ["child", "parent"]);
    assert!(
        stderr(&output).contains("Determinism verified"),
        "missing verification success marker:\n{}",
        stderr(&output)
    );
}

/// tests/c/sigsuspend_shared_mask_rewrite.c: a peer process rewrites, in a
/// MAP_SHARED page, the mask the parent passed to rt_sigsuspend, after Detcore
/// read it and before the call runs, while a child's exit sends the parent the
/// SIGCHLD that mask let through. The call must sleep under the mask Detcore
/// read, so it wakes for that SIGCHLD, and the parent's next sigsuspend wakes
/// for the peer's later SIGUSR1. Before, the call ran under the rewritten
/// buffer, which blocks SIGCHLD; the scheduler, which had counted on that
/// signal to end the call, held every other thread waiting for its signal
/// stop, so the peer's SIGUSR1 was never sent, and after 30 s the run was
/// refused (exit 122). The guest reports each handler's run count and fails
/// unless each ran exactly once; the run is compared under the L2 envelope.
#[test]
fn a_shared_sigsuspend_mask_rewritten_before_the_call_runs_does_not_stall_the_run() {
    let _guard = hermit_run_guard();
    let guest = sigsuspend_shared_mask_rewrite_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--max-timeslice=disabled",
        "--",
        guest,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "sigchld=1 sigusr1=1\n");
    assert!(
        stderr(&output).contains("Determinism verified"),
        "missing verification success marker:\n{}",
        stderr(&output)
    );
}

/// tests/c/sigsuspend_shared_stack_mask.c: the shared-mask scenario of
/// `a_shared_sigsuspend_mask_rewritten_before_the_call_runs_does_not_stall_the_run`
/// with the parent's mask on a MAP_SHARED stack, 144 bytes below the stack
/// pointer, where Detcore used to place its private copy of the mask. The copy
/// must not be the guest's buffer, or the peer's rewrite reaches the real call
/// again. Before, the copy was that buffer: the call slept under the rewritten
/// mask, the scheduler held every thread for a SIGCHLD stop that never came,
/// and after 30 s the run was refused (exit 122).
#[test]
fn a_sigsuspend_mask_on_a_shared_stack_below_the_red_zone_is_copied_elsewhere() {
    let _guard = hermit_run_guard();
    let guest = sigsuspend_shared_stack_mask_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--max-timeslice=disabled",
        "--",
        guest,
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "sigchld=1 sigusr1=1\n");
    assert!(
        stderr(&output).contains("Determinism verified"),
        "missing verification success marker:\n{}",
        stderr(&output)
    );
}

/// tests/c/sigchld_once_per_child_exit.c: one SIGCHLD per child exit, with
/// Linux's siginfo (https://github.com/rrnewton/hermit/issues/3895). The
/// child exits with `_exit(7)`, and the parent keeps SIGCHLD unblocked for a
/// second after its first one, so a second SIGCHLD for the exit would be
/// delivered too. Both the kernel and the scheduler send one; the first
/// delivered stands for the exit, the other is dropped, and the scheduler's
/// carries CLD_EXITED, the child's pid and the status instead of the
/// tracer's SI_USER.
#[test]
fn a_child_exit_is_notified_once_with_linuxs_siginfo() {
    let _guard = hermit_run_guard();
    let guest = sigchld_once_per_child_exit_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    let args = [
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--",
        guest,
        "exit-group",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "sigchld=1 code=1 pid=child status=7\n");
    assert!(
        stderr(&output).contains("Determinism verified"),
        "missing verification success marker:\n{}",
        stderr(&output)
    );
}

/// tests/c/clone_exit_signal_effective.c: a child's effective exit signal
/// decides which waits reap it. A CLONE_VFORK child created with SIGUSR1 that
/// execs, and a CLONE_PARENT | SIGUSR1 grandchild (which inherits its creator's
/// SIGCHLD), are both reaped by a plain waitpid, and the parent gets SIGCHLD,
/// as natively. Before, Detcore kept the clone's requested SIGUSR1, counted
/// both as clone children, and the plain waitpid failed with ECHILD
/// (https://github.com/rrnewton/hermit/issues/3895).
#[test]
fn a_childs_exit_signal_follows_its_exec_and_clone_parent() {
    let _guard = hermit_run_guard();
    let guest = clone_exit_signal_effective_guest()
        .to_str()
        .expect("guest path should be UTF-8");
    for mode in ["exec", "clone-parent"] {
        let args = [
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--max-timeslice=disabled",
            "--",
            guest,
            mode,
        ];
        let output = hermit(&args);
        assert_success(&output, &args);
        assert_eq!(
            stdout(&output),
            format!("{mode} sigusr1=0 sigchld_seen=1\n"),
            "{mode}"
        );
        assert!(
            stderr(&output).contains("Determinism verified"),
            "missing verification success marker:\n{}",
            stderr(&output)
        );
    }
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
        let mut command = hermit_command_under_host_filter(&[
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

/// A guest that never stops, under a log level that makes hermit narrate it.
/// Without `--max-log-bytes` this writes until the `--timeout` backstop; the
/// 2026-08-17 incident was this shape at `--log=info`, ~4.5 TB per process.
const LOG_CAP_NOISY_GUEST: [&str; 3] = ["/bin/sh", "-c", "while :; do /bin/true; done"];

/// `--max-log-bytes` stops a run whose hermit log output runs away: exit 123
/// (not the `--timeout` backstop's 124, not 125), the final stderr names the
/// bound, and the stderr hermit wrote stays within cap + final report.
#[test]
fn max_log_bytes_aborts_a_run_whose_stderr_log_runs_away() {
    let _lock = hermit_run_guard();
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let output = hermit(&args);
    let stderr = stderr(&output);
    let tail: String = stderr
        .chars()
        .rev()
        .take(2000)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    assert_eq!(
        output.status.code(),
        Some(HERMIT_LOG_CAP_EXIT),
        "the log cap must end the run with its own status; stderr tail:\n{tail}"
    );
    assert!(
        stderr.contains(
            "hermit: log output exceeded --max-log-bytes=64K (65536 bytes); aborting the run"
        ),
        "{tail}"
    );
    assert!(stderr.contains("HERMIT_LOG_CAP class=log-cap"), "{tail}");
    assert!(!stderr.contains("HERMIT_INTERNAL_FAILURE"), "{tail}");
    // The guest's own output is empty, so all of this is hermit's: the capped
    // log plus the fixed final report, not an unbounded stream.
    assert!(
        output.stderr.len() < 65536 + 4096,
        "stderr was {} bytes",
        output.stderr.len()
    );
}

/// The same cap applies to `--log-file`, counting bytes before the
/// HERMIT_LOG_MAX_BYTES truncation, and the file itself says why it ends.
#[test]
fn max_log_bytes_aborts_a_run_whose_log_file_runs_away() {
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log = directory.path().join("hermit.log");
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "--log-file",
        log.to_str().unwrap(),
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let output = hermit(&args);
    let stderr = stderr(&output);
    assert_eq!(output.status.code(), Some(HERMIT_LOG_CAP_EXIT), "{stderr}");
    assert!(stderr.contains("exceeded --max-log-bytes=64K"), "{stderr}");
    let written = fs::read_to_string(&log).unwrap();
    assert!(
        written.len() < 65536 + 4096,
        "log was {} bytes",
        written.len()
    );
    // The file's copy of the reason is best effort: it is written only when
    // the file system takes a write that cannot wait (`RWF_NOWAIT`), and
    // omitted otherwise (btrfs answers EAGAIN, tmpfs EOPNOTSUPP). stderr, a
    // pipe this test drains, carries it either way (asserted above). What must
    // hold on every file system: the file ends on a complete line, and if the
    // reason is there at all, it and its class line are the last lines.
    let tail = &written[written.len().saturating_sub(600)..];
    assert!(written.ends_with('\n'), "the log ends mid-line:\n{tail}");
    if let Some(at) = written.find("hermit: log output exceeded --max-log-bytes") {
        assert!(
            written[at..].ends_with(
                "raise --max-log-bytes, to let the run finish.\nHERMIT_LOG_CAP class=log-cap\n"
            ),
            "the reason the log ends, then its class line, must be the last lines:\n{tail}"
        );
    }
}

/// A log that ends with the `--max-log-bytes` stop message lost its tail, and
/// `hermit log-diff` must refuse it as it refuses a log that ends with the
/// bounded writer's truncation marker
/// (<https://github.com/rrnewton/hermit/pull/3686>, round-2 review finding 5).
///
/// The records are a real run's `--log-file` at INFO; the stop message is the
/// exact text the cap writes. It is appended here rather than produced by a
/// capped run because the cap writes it to a regular file with
/// `pwritev2(RWF_NOWAIT)`, which this host's btrfs refuses with `EAGAIN` (see
/// `max_log_bytes_aborts_a_run_whose_log_file_runs_away`); where the write is
/// accepted (a FIFO sink, a file system that takes non-waiting appends) the
/// file ends exactly like this.
///
/// Three tails, each compared with itself through the JSON comparison:
/// - the stop message alone: the cap fired before the file bound did;
/// - the truncation marker, then the stop message: what the cap wrote before
///   this fix once the file bound had already truncated;
/// - the truncation marker alone: the control, refused before and after.
#[test]
fn max_log_bytes_stopped_log_is_refused_by_log_diff() {
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = directory.path().join("base.log");
    log_true_run(&base, "info", "2026-01-01T00:00:00.123456789+00:00");
    let records = fs::read_to_string(&base).unwrap();
    assert!(records.contains(" INFO "), "{records}");
    let stop = "hermit: log output exceeded --max-log-bytes=64K (65536 bytes); aborting the \
                run and killing the guest process tree (exit 123). Lower --log / RUST_LOG \
                verbosity, or raise --max-log-bytes, to let the run finish.\n\
                HERMIT_LOG_CAP class=log-cap\n";
    let marker = detcore::logdiff::TRUNCATION_MARKER;
    for (label, tail) in [
        ("stop message alone", stop.to_owned()),
        (
            "truncation marker, then stop message",
            format!("\n{marker}\n{stop}"),
        ),
        ("truncation marker alone", format!("\n{marker}\n")),
    ] {
        let log = directory.path().join("capped.log");
        fs::write(&log, format!("{records}{tail}")).unwrap();
        let json = directory.path().join("report.json");
        let _ = fs::remove_file(&json);
        let log_arg = log.to_str().unwrap();
        let output = log_diff(&["--json", json.to_str().unwrap(), log_arg, log_arg]);
        let stderr = stderr(&output);
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
        assert_ne!(output.status.code(), Some(0), "{label}: {stderr}");
        assert_eq!(report["verdict"], "refused", "{label}: {report}");
        assert!(
            report["refusal"]
                .as_str()
                .is_some_and(|reason| reason.contains("truncated at the configured size bound")),
            "{label}: {report}"
        );
    }
}

/// A pipe for a child's stderr whose buffer is already full and whose reader
/// (the first descriptor, held by the caller) stays open and never reads. The
/// write end is back in blocking mode, so a plain `write(2)` to it waits
/// forever.
fn full_unread_stderr_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    use std::os::fd::FromRawFd;
    let mut fds = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
        0
    );
    for chunk in [&[b'x'; 4096][..], &[b'x'; 1][..]] {
        while unsafe { libc::write(fds[1], chunk.as_ptr().cast(), chunk.len()) } > 0 {}
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
    }
    assert_eq!(unsafe { libc::fcntl(fds[1], libc::F_SETFL, 0) }, 0);
    // SAFETY: both descriptors were just created and are owned only here.
    unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    }
}

/// How long a log-cap test below waits for a capped hermit, from spawn, before
/// scaling by [`dap_wall_timeout_multiplier`].
///
/// Nextest ends a test at 57 s of wall time: `.config/nextest.toml` sets
/// `slow-timeout = { period = "57s", terminate-after = 1, grace-period = "2s" }`
/// in `[profile.default]`, and `[profile.ci]` inherits it. Before it runs
/// nextest, `ci/run-nextest-counted.sh` writes a temporary copy of that file
/// with every `slow-timeout` period multiplied by
/// `HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER` (unset is 1) and rounded up to whole
/// seconds. A wait as long as that kill can never fail on its own: round 2's
/// 60 s wait ended in nextest's `TIMEOUT` at 57 s instead, which names no cause
/// (round-3 review of https://github.com/rrnewton/hermit/pull/3686, finding
/// 8). So these tests wait at most 45 s times the same multiplier, then kill
/// hermit with SIGKILL and reap it ([`wait_at_most`]), close their own pipe
/// descriptors, and fail with the time they waited. Killing hermit also ends
/// the guest: every capped run here runs inside hermit's PID namespace (the
/// flag is refused with `--no-namespace`), whose container init dies with
/// hermit through its parent-death signal, and the guest dies with the
/// namespace. The 12 s left before nextest's kill cover the setup before spawn
/// and that cleanup.
const LOG_CAP_RUN_WAIT_BOUND: Duration = Duration::from_secs(45);

/// Wait for `child` for at most `bound`, killing it with SIGKILL and reaping it
/// on expiry. Returns the exit status (`None` when it had to be killed) and the
/// time waited, the kill and the reap included.
fn wait_at_most(
    child: &mut std::process::Child,
    bound: Duration,
) -> (Option<std::process::ExitStatus>, Duration) {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (Some(status), start.elapsed());
        }
        if start.elapsed() >= bound {
            let _ = child.kill();
            let _ = child.wait();
            return (None, start.elapsed());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Termination of a capped run must not depend on any diagnostic write
/// (round-2 review of https://github.com/rrnewton/hermit/pull/3686). Here
/// hermit's stderr is a pipe that is already full and whose reader never
/// reads, so every stderr diagnostic would block forever if it waited: the
/// crossing line from the container init (the crossing process) and the
/// outer process's `HERMIT_LOG_CAP` report after it classifies the init's
/// 123. The log goes to `--log-file` so no ordinary log line touches stderr.
///
/// The failure this catches is an indefinite wait, so the test waits at most
/// [`LOG_CAP_RUN_WAIT_BOUND`] from spawn, which ends before nextest's own kill
/// and leaves the test to report a hang itself, with the time waited. A capped
/// run like this one exited 123 after 0.06 s in round 3's runs, so the bound
/// only has to exceed startup plus 64 KiB of debug logging on a loaded host by
/// a wide margin. The measured time is printed.
#[test]
fn max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads() {
    CappedRun::default().assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// The same capped run on a host where `perf_event_open` fails, as on the
/// hosted GitHub runners, which have no PMU (docs/TESTING_ENVIRONMENTS.md).
/// There the outer process downgrades the default `--max-timeslice` and says
/// so on stderr while it validates its arguments, before any container
/// exists. That warning was written with a blocking `write(2)`, so with
/// stderr a full pipe nobody reads, the run waited forever and never reached
/// the cap: https://github.com/rrnewton/hermit/actions/runs/37489014478 was
/// killed at the 45 s bound on hosted while the test above passed on every
/// host with a PMU. This test denies `perf_event_open` with `EPERM`, which
/// Reverie's probe reads as "unsupported", and first checks that the warning
/// is printed at all, so the capped run below really does write it.
#[test]
fn max_log_bytes_exits_promptly_when_perf_is_unavailable_and_stderr_is_a_full_pipe() {
    {
        let _lock = hermit_run_guard();
        let mut command =
            hermit_command_under_host_filter(&["run", "--timeout", "120", "--", "/bin/true"]);
        deny_syscall(&mut command, libc::SYS_perf_event_open);
        let output = command
            .stdin(Stdio::null())
            .output()
            .expect("failed to run hermit without perf_event_open");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--max-timeslice requires user-space perf counters"),
            "the premise does not hold: without perf_event_open, an uncapped run printed no \
             --max-timeslice downgrade warning ({:?}); stderr:\n{stderr}",
            output.status
        );
    }
    CappedRun {
        global: &[UNSAFE_IGNORE_HOST_SECCOMP],
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|command| {
        deny_syscall(command, libc::SYS_perf_event_open);
    });
}

/// A capped run with stderr a full pipe nobody reads (see
/// [`max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads`]).
/// Its arguments are `--log=debug --max-log-bytes=64K --log-file <log>`,
/// `global`, `run`, `run_options`, `--timeout 120` when `timeout` is set, `--`
/// and `guest`. The default is the plain run of [`LOG_CAP_NOISY_GUEST`], which
/// the cap ends with 123.
struct CappedRun<'a> {
    /// Builds the command from the arguments: [`hermit_command`], or
    /// [`readonly_proc_command`] to deny every writable mount.
    build: fn(&[&str]) -> Command,
    global: &'a [&'a str],
    run_options: &'a [&'a str],
    timeout: bool,
    guest: &'a [&'a str],
    /// The exit code the run must reach within [`LOG_CAP_RUN_WAIT_BOUND`]. For
    /// 123, the log file must also be non-empty.
    exit_code: i32,
    /// A diagnostic this configuration writes to stderr. When set, the same
    /// run is made first with stderr a pipe that is read, and its stderr must
    /// contain this text. That shows the run with the full pipe writes it too,
    /// so it would hang there if the write waited.
    prints: Option<&'a str>,
    /// The `--max-log-bytes` value. 64K is crossed early in a run; a larger
    /// cap lets a quiet first verify run finish under it.
    max_log_bytes: &'a str,
}

impl Default for CappedRun<'_> {
    fn default() -> Self {
        CappedRun {
            build: hermit_command,
            global: &[],
            run_options: &[],
            timeout: true,
            guest: &LOG_CAP_NOISY_GUEST,
            exit_code: HERMIT_LOG_CAP_EXIT,
            prints: None,
            max_log_bytes: "64K",
        }
    }
}

impl CappedRun<'_> {
    fn command(&self, log: &Path, configure: &impl Fn(&mut Command)) -> Command {
        let cap = format!("--max-log-bytes={}", self.max_log_bytes);
        let mut args = vec![
            "--log=debug",
            cap.as_str(),
            "--log-file",
            log.to_str().unwrap(),
        ];
        args.extend_from_slice(self.global);
        args.push("run");
        args.extend_from_slice(self.run_options);
        if self.timeout {
            args.extend_from_slice(&["--timeout", "120"]);
        }
        args.push("--");
        args.extend_from_slice(self.guest);
        let mut command = (self.build)(&args);
        command.stdin(Stdio::null());
        // The guest inherits hermit's environment. Under a UTF-8 `LANG` every
        // program the guest runs opens the locale's files at startup, and the
        // debug log of those syscalls is large: measured, run 1 of
        // `max_log_bytes_verify_exits_promptly_when_run1_summary_is_unreadable_and_stderr_is_a_full_pipe`
        // logged 1272857 bytes with `LANG=en_US.UTF-8` against 970529 with
        // `LC_ALL=C`, and with `--preemption-stacktrace` the 448K cap crossed
        // while `/bin/sh` was still reading locale files, before its loop
        // was first preempted. The C locale opens no files, so how much of the
        // cap a guest's startup uses no longer depends on the caller's locale.
        command.env("LC_ALL", "C");
        configure(&mut command);
        command
    }

    /// Require [`Self::exit_code`] within [`LOG_CAP_RUN_WAIT_BOUND`] with
    /// stderr a full pipe nobody reads, after `configure` adjusts the command.
    /// Stdin is `/dev/null` unless `configure` replaces it.
    fn assert_exits_promptly_with_full_unread_stderr(&self, configure: impl Fn(&mut Command)) {
        let _lock = hermit_run_guard();
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        if let Some(text) = self.prints {
            let mut command = self.command(&directory.path().join("premise.log"), &configure);
            let (status, elapsed, stderr) = stderr_when_read(&mut command);
            assert!(
                stderr.contains(text),
                "the premise does not hold: with stderr read, the run ({status:?} after \
                 {elapsed:?}) did not print {text:?}; stderr:\n{stderr}"
            );
            assert_eq!(
                status.and_then(|status| status.code()),
                Some(self.exit_code),
                "with stderr read: {status:?} after {elapsed:?}; stderr:\n{stderr}"
            );
        }
        let log = directory.path().join("hermit.log");
        let status = exit_status_with_full_unread_stderr(&mut self.command(&log, &configure));
        assert_eq!(status.code(), Some(self.exit_code), "{status:?}");
        if self.exit_code == HERMIT_LOG_CAP_EXIT {
            assert!(
                fs::metadata(&log).unwrap().len() > 0,
                "the run logged to the file"
            );
        }
    }
}

/// Spawn `command` with stdout discarded and stderr a full pipe nobody reads
/// ([`full_unread_stderr_pipe`]), wait for it at most
/// [`LOG_CAP_RUN_WAIT_BOUND`], and return its exit status. Fails, with the
/// time waited, when hermit was still running at the bound and was killed.
fn exit_status_with_full_unread_stderr(command: &mut Command) -> std::process::ExitStatus {
    let (reader, writer) = full_unread_stderr_pipe();
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer))
        .spawn()
        .unwrap();
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let (status, elapsed) = wait_at_most(&mut child, bound);
    // hermit is reaped (killed first if it outlived the bound), and the guest
    // died with it; this closes the read end, the last descriptor of the pipe.
    drop(reader);
    eprintln!("capped run, stderr a full unread pipe: {status:?} after {elapsed:?}");
    status.unwrap_or_else(|| {
        panic!(
            "hermit was still running after {elapsed:?} (bound {bound:?}) and was killed: \
             a diagnostic waited on the full stderr pipe"
        )
    })
}

/// Spawn `command` with stdout discarded and stderr a pipe that a thread reads
/// to its end, wait for it at most [`LOG_CAP_RUN_WAIT_BOUND`] (killing it on
/// expiry), and return its exit status (`None` when it was killed), the time
/// waited and its stderr.
fn stderr_when_read(command: &mut Command) -> (Option<std::process::ExitStatus>, Duration, String) {
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stderr.take().unwrap();
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
        bytes
    });
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let (status, elapsed) = wait_at_most(&mut child, bound);
    let stderr = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    (status, elapsed, stderr)
}

/// The options [`run_uses_readonly_proc_after_permission_denial`] runs with
/// under [`readonly_proc_command`]: local networking would need another
/// writable mount, for sysfs.
const READONLY_PROC_RUN_OPTIONS: [&str; 3] = [
    "--network=host",
    "--max-timeslice=disabled",
    "--no-virtualize-cpuid",
];

/// The capped run of
/// [`max_log_bytes_exits_promptly_when_stderr_is_a_full_pipe_nobody_reads`] on a
/// host where `/proc` can only be mounted read-only, as
/// [`run_uses_readonly_proc_after_permission_denial`] arranges with
/// [`readonly_proc_command`]. The container init then warns about it
/// (`hermit::proc_mount::warn_if_readonly_proc` in `run_in_container`) before
/// it starts the guest, so before the cap can cross. That warning was a
/// blocking `write(2)`, and this run was still running when it was killed at
/// the 45 s bound.
#[test]
fn max_log_bytes_exits_promptly_when_proc_is_readonly_and_stderr_is_a_full_pipe() {
    CappedRun {
        build: readonly_proc_command,
        global: &["--backend=ptrace"],
        run_options: &READONLY_PROC_RUN_OPTIONS,
        prints: Some(hermit::proc_mount::READONLY_WARNING.trim()),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// The same read-only `/proc` warning under `--namespace-only`, where it is
/// written in the `pre_exec` callback, in the forked child before it executes
/// the guest. The non-waiting write is safe there: it makes only syscalls.
/// Nothing logs enough here to reach the cap, so the run must end with the
/// guest's own exit status, 0.
#[test]
fn max_log_bytes_namespace_only_exits_promptly_when_proc_is_readonly_and_stderr_is_a_full_pipe() {
    CappedRun {
        build: readonly_proc_command,
        run_options: &["--namespace-only", "--network=host"],
        guest: &["/bin/true"],
        exit_code: 0,
        prints: Some(hermit::proc_mount::READONLY_WARNING.trim()),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `run --verify` writes its `:: Run1...` banner before the first run starts,
/// so before the cap can cross.
#[test]
fn max_log_bytes_verify_exits_promptly_when_stderr_is_a_full_pipe() {
    CappedRun {
        run_options: &["--verify"],
        prints: Some("Run1..."),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `run --verify` writes its `:: Run2...` banner before the second run
/// starts. The cap counts both runs' logs together, so a first run that stays
/// under it is followed by that banner, and a second run that crosses the cap
/// comes after it. See [`assert_verify_run2_crosses_the_cap`] for the guest.
#[test]
fn max_log_bytes_verify_exits_promptly_when_run2_crosses_the_cap_and_stderr_is_a_full_pipe() {
    assert_verify_run2_crosses_the_cap(&["--verify"], "Run2...", "", None, |_| {});
}

/// `run --verify --print-verify-logs` echoes the first run's log to stderr
/// after that run and before the second starts, so before a second run that
/// crosses the cap. The echo is the whole log, far more than a pipe holds; the
/// premise run shows it on stderr through run 1's debug lines.
#[test]
fn max_log_bytes_print_verify_logs_exits_promptly_when_run2_crosses_the_cap_and_stderr_is_a_full_pipe()
 {
    assert_verify_run2_crosses_the_cap(
        &["--verify", "--print-verify-logs"],
        " DEBUG ",
        "",
        None,
        |_| {},
    );
}

/// `run --verify --print-verify-logs` warns on stderr when it cannot read the
/// first run's log to echo it, and that warning is also written before the
/// second run starts. With `--keep-logs --verify-log-dir` the log is in a
/// directory the guest can see, so run 1's guest makes it write-only (mode
/// 0200): hermit goes on writing the log through the descriptor it already
/// holds, and the outer hermit process cannot read it and warns. As in the
/// summary test below, the outer process is started without the capabilities
/// that override a file's mode ([`without_file_permission_override`]). The
/// guest changes the mode with one `chmod`, not by replacing the file, because
/// each program run 1 executes adds roughly 0.5 MB to its debug log, and
/// `assert_verify_run2_crosses_the_cap` needs run 1 to leave room under the
/// cap for run 2 to start.
#[test]
fn max_log_bytes_print_verify_logs_exits_promptly_when_run1_log_is_unreadable_and_stderr_is_a_full_pipe()
 {
    let logs = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log_dir = format!("--verify-log-dir={}", logs.path().to_str().unwrap());
    assert_verify_run2_crosses_the_cap(
        &["--verify", "--print-verify-logs", "--keep-logs", &log_dir],
        "WARNING: --print-verify-logs could not read first-run log",
        r#"chmod 200 "$3"/run1_log_*"#,
        Some(logs.path()),
        without_file_permission_override,
    );
}

/// Between the two `--verify` runs hermit reads run 1's summary from a private
/// file and warns on stderr when it cannot (`read_verify_summary` in
/// `hermit/run.rs`), then empties the file before run 2. The file is in the
/// work tree's `ignored/` directory, which the guest can see, so run 1's guest
/// makes it write-only (mode 0200): hermit inside the container still writes the
/// summary, the outer hermit process cannot read it and warns, and emptying it
/// before run 2 still succeeds. The outer process is started without the
/// capabilities that override a file's mode
/// ([`without_file_permission_override`]); a root caller, as in the pinned root
/// or a `--map-root-user` namespace, would otherwise read the file anyway.
///
/// The working directory is a work-tree root of the test's own (an empty
/// `.git` and a `.gitignore` naming `ignored/`), so the summary is the only
/// file in its `ignored/` and the guest's `chmod` cannot reach another run's.
#[test]
fn max_log_bytes_verify_exits_promptly_when_run1_summary_is_unreadable_and_stderr_is_a_full_pipe() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    fs::create_dir(root.path().join(".git")).unwrap();
    fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
    assert_verify_run2_crosses_the_cap(
        &["--verify"],
        "WARNING: verification runtime statistics unavailable",
        r#"chmod 200 "$3"/ignored/.hermit-verify-summary-*"#,
        Some(root.path()),
        without_file_permission_override,
    );
}

/// Exec `command` without `CAP_DAC_OVERRIDE` and `CAP_DAC_READ_SEARCH`, the
/// capabilities that let a process read a file its mode denies it. A root
/// caller would exec it with both, from its bounding set, so they are dropped
/// from that set and from the effective, permitted and inheritable sets, which
/// also lowers them in the ambient set. A caller without `CAP_SETPCAP` cannot
/// change its bounding set; when it is not root it does not need to, because
/// an exec gives it no capability its permitted, inheritable and ambient sets
/// lack, and when it is root the spawn fails rather than run with them.
fn without_file_permission_override(command: &mut Command) {
    // Linux capability ABI version 3 and the capability numbers, from
    // linux/capability.h.
    const VERSION_3: u32 = 0x2008_0522;
    const DAC_OVERRIDE: u32 = 1;
    const DAC_READ_SEARCH: u32 = 2;
    const SETPCAP: u32 = 8;
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[derive(Clone, Copy, Default)]
    #[repr(C)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    // SAFETY: the callback makes only async-signal-safe syscalls (capget,
    // prctl, geteuid and capset) and changes only the child before its exec.
    unsafe {
        command.pre_exec(|| {
            let mut header = Header {
                version: VERSION_3,
                pid: 0,
            };
            let mut data = [Data::default(); 2];
            if libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let can_drop_from_bounding_set = data[0].effective & (1 << SETPCAP) != 0;
            for capability in [DAC_OVERRIDE, DAC_READ_SEARCH] {
                if can_drop_from_bounding_set {
                    if libc::prctl(
                        libc::PR_CAPBSET_DROP,
                        libc::c_ulong::from(capability),
                        0,
                        0,
                        0,
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::geteuid() == 0
                    && libc::prctl(
                        libc::PR_CAPBSET_READ,
                        libc::c_ulong::from(capability),
                        0,
                        0,
                        0,
                    ) != 0
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
                data[0].effective &= !(1 << capability);
                data[0].permitted &= !(1 << capability);
                data[0].inheritable &= !(1 << capability);
            }
            if libc::syscall(libc::SYS_capset, &header, data.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Run `--verify` with `run_options` under a 2M cap, first with stderr read
/// (which must contain `prints`) and then with stderr a full unread pipe, and
/// require that the second run's guest started in both and the cap ended the
/// run (123) within the bound.
///
/// The guest finishes at once when the first marker is absent (run 1, which
/// creates it and then runs `run1_then`, a shell command whose `$3` is
/// `working_directory`) and, when it is present (run 2), creates the second
/// marker with a shell redirection, before it runs any program, and then loops
/// until the cap crosses. Measured at debug in the C locale
/// ([`CappedRun::command`]): run 1 logged 473316 bytes with no `run1_then` and
/// at most 974117 with either `chmod` one, and run 2 created its marker 449626
/// bytes into its log, so at most 1423743 bytes were logged before the marker.
/// The cap is therefore 2M rather than 64K. Each further program that
/// `run1_then` executes costs roughly another 0.5 MB, and run 1 with an `mv`
/// and a `mkdir` logged 1690977 bytes and did cross the cap before run 2's
/// marker. Both markers are removed before each hermit run, so neither
/// run can see one the previous run left; the second marker after each run
/// shows that run 2's guest started in that run, so its 123 came from run 2.
/// Hermit runs in `working_directory` when one is given, and `prepare` adjusts
/// each hermit command last.
fn assert_verify_run2_crosses_the_cap(
    run_options: &[&str],
    prints: &str,
    run1_then: &str,
    working_directory: Option<&Path>,
    prepare: fn(&mut Command),
) {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let run1_finished = directory.path().join("run1-finished");
    let run2_started = directory.path().join("run2-started");
    let script = format!(
        r#"if [ -e "$1" ]; then : > "$2"; while :; do /bin/true; done; else : > "$1"; {run1_then}
fi"#
    );
    let working_directory_arg = working_directory
        .map(|path| path.to_str().unwrap())
        .unwrap_or_default();
    let assert_run2_started = |run: &str| {
        assert!(
            run2_started.exists(),
            "in the {run}, run 2's guest did not create {}: the cap crossed before run 2 \
             started",
            run2_started.display()
        );
    };
    let runs_started = std::cell::Cell::new(0);
    CappedRun {
        run_options,
        guest: &[
            "/bin/sh",
            "-c",
            &script,
            "sh",
            run1_finished.to_str().unwrap(),
            run2_started.to_str().unwrap(),
            working_directory_arg,
        ],
        prints: Some(prints),
        max_log_bytes: "2M",
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|command| {
        // Called before each hermit run: the premise run, when there is one,
        // has finished by the second call.
        if runs_started.get() > 0 {
            assert_run2_started("premise run, with stderr read");
        }
        runs_started.set(runs_started.get() + 1);
        for marker in [&run1_finished, &run2_started] {
            match fs::remove_file(marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("removing {}: {error}", marker.display()),
            }
        }
        if let Some(path) = working_directory {
            command.current_dir(path);
        }
        prepare(command);
    });
    assert_eq!(
        runs_started.get(),
        2,
        "the premise run and the run under test"
    );
    assert_run2_started("run under test, with stderr a full unread pipe");
}

/// `run --verify` on a read-only `/proc`: the first run's container init
/// warns about it from `run_verify_in_container`, a different call site from
/// the plain run's.
#[test]
fn max_log_bytes_verify_exits_promptly_when_proc_is_readonly_and_stderr_is_a_full_pipe() {
    let mut run_options = vec!["--verify"];
    run_options.extend(READONLY_PROC_RUN_OPTIONS);
    CappedRun {
        build: readonly_proc_command,
        global: &["--backend=ptrace"],
        run_options: &run_options,
        prints: Some(hermit::proc_mount::READONLY_WARNING.trim()),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `run --verify` with stdin a pipe: the library notes on stderr that it is
/// buffering stdin (`hermit::reserve_output_stdin_snapshot`) before either run
/// starts. The pipe's writer is closed, so stdin ends at once.
#[test]
fn max_log_bytes_verify_exits_promptly_when_stdin_is_a_pipe_and_stderr_is_a_full_pipe() {
    CappedRun {
        run_options: &["--verify"],
        prints: Some("hermit: --verify is buffering stdin from a non-seekable stream"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|command| {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        command.stdin(reader);
    });
}

/// `--seed-from` names the seed it chose on stderr while the outer process
/// prepares the run, before any container exists. That line was a blocking
/// `eprintln!`, and this run was still running when it was killed at the
/// 45 s bound.
#[test]
fn max_log_bytes_exits_promptly_with_seed_from_and_stderr_a_full_pipe() {
    CappedRun {
        run_options: &["--seed-from=args"],
        prints: Some("[hermit] auto setting --seed"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `--allow-unsupported-syscalls` warns on stderr before the run starts.
#[test]
fn max_log_bytes_exits_promptly_with_allow_unsupported_syscalls_and_stderr_a_full_pipe() {
    CappedRun {
        run_options: &["--allow-unsupported-syscalls"],
        prints: Some("WARNING: --allow-unsupported-syscalls permits unmodeled syscalls"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// A `--bind` whose target is outside guest `/tmp` is ignored with a warning
/// on stderr while the outer process prepares the container's mounts.
#[test]
fn max_log_bytes_exits_promptly_with_a_bind_outside_tmp_and_stderr_a_full_pipe() {
    CappedRun {
        run_options: &["--bind=/usr"],
        prints: Some("WARNING: --bind target /usr is outside guest /tmp"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// A noisy shell script at `directory/name`, which every run in this file can
/// execute: `CARGO_TARGET_TMPDIR` is not under the host `/tmp` that hermit
/// hides.
fn noisy_guest_script(directory: &Path, name: &str) -> String {
    let script = directory.join(name);
    fs::write(&script, "#!/bin/sh\nwhile :; do /bin/true; done\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script.to_str().unwrap().to_owned()
}

/// A guest named like a QEMU system emulator gets the advisory about
/// virtualized time on stderr while the outer process prepares the run
/// (`vmm_time_virtualization_warning`). The guest is the noisy shell loop
/// under that name.
#[test]
fn max_log_bytes_exits_promptly_with_a_vmm_guest_and_stderr_a_full_pipe() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let emulator = noisy_guest_script(directory.path(), "qemu-system-x86_64");
    CappedRun {
        guest: &[&emulator],
        prints: Some("looks like a hardware emulator (VMM)"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `--backend=e9patch` (e9patch preprocessing with the ptrace backend) names
/// its engagement on stderr before the run starts. For a main executable that
/// is not ELF there is nothing to rewrite, and the tools are never run. Hermit
/// still requires both to be executable files before it starts, and no
/// validation node stages e9patch, so the test names `/bin/false` for each:
/// had hermit run either, the run would fail rather than reach the cap.
#[test]
fn max_log_bytes_exits_promptly_with_e9patch_on_a_script_and_stderr_a_full_pipe() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let script = noisy_guest_script(directory.path(), "noisy.sh");
    CappedRun {
        global: &["--backend=e9patch"],
        guest: &[&script],
        prints: Some(
            ":: Backend: e9patch preprocessing + ptrace runtime; mapped_sites=0; \
             main_executable=non-ELF; preprocessing=not-applicable",
        ),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|command| {
        command
            .env(hermit::e9patch::E9TOOL_ENV, "/bin/false")
            .env(hermit::e9patch::E9PATCH_BACKEND_ENV, "/bin/false");
    });
}

/// A `--happens-before` spec with one anchor at a function the guest does not
/// have.
fn unresolvable_happens_before_spec(directory: &Path) -> String {
    let spec = directory.join("happens-before.json");
    fs::write(
        &spec,
        r#"{"version": 1, "events": {"A": {"thread": "1", "func": "no_such_function"}}, "edges": []}"#,
    )
    .unwrap();
    spec.to_str().unwrap().to_owned()
}

/// `--happens-before` reports anchors it cannot resolve on stderr before the
/// run starts. Whatever debug info the guest has, one line says the anchor
/// will never fire.
#[test]
fn max_log_bytes_exits_promptly_with_an_unresolved_happens_before_anchor_and_stderr_a_full_pipe() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = unresolvable_happens_before_spec(directory.path());
    CappedRun {
        run_options: &["--happens-before", &spec],
        prints: Some("never fire"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// Scheduler turns of one `run --strict` of a syscall-heavy shell loop,
/// optionally carrying a `--happens-before` spec, read from `--summary-json`.
fn strict_shell_loop_turns(directory: &Path, spec: Option<&str>) -> (u64, u64) {
    let summary = directory.join(if spec.is_some() {
        "with-spec.json"
    } else {
        "no-spec.json"
    });
    let summary_arg = format!("--summary-json={}", summary.display());
    let mut args = vec!["run", "--strict", summary_arg.as_str()];
    if let Some(spec) = spec {
        args.extend(["--happens-before", spec]);
    }
    // Each `read` opens, reads and closes a file: about three syscalls per
    // iteration, on the root thread only.
    args.extend([
        "--",
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 300 ]; do read x < /proc/self/stat; i=$((i+1)); done",
    ]);
    let output = hermit(&args);
    assert_success(&output, &args);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&summary).expect("the summary was not written"))
            .expect("the summary is not JSON");
    (
        report["sched_turns"]
            .as_u64()
            .expect("summary has no sched_turns"),
        report["syscalls"]
            .as_u64()
            .expect("summary has no syscalls"),
    )
}

/// A `--happens-before` spec costs one scheduler checkpoint per thread that
/// reaches an anchored syscall count, not one per syscall
/// (https://github.com/rrnewton/hermit/issues/3877). The spec here can never
/// gate anything (its AFTER anchor is a syscall count the guest never reaches),
/// so it must leave the schedule almost untouched: before the fix, every
/// intercepted syscall issued a checkpoint and the turn count grew by about
/// one per syscall, which stalled QEMU (about 1.15 million syscalls a run).
#[test]
fn happens_before_inert_spec_adds_no_per_syscall_scheduler_turns() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = directory.path().join("inert.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"early": {"thread": "3", "syscalls": 2},
                       "never": {"thread": "3", "syscalls": 1000000000}},
            "edges": [{"before": "early", "after": "never", "strength": "hard"}]}"#,
    )
    .unwrap();
    let (base_turns, base_syscalls) = strict_shell_loop_turns(directory.path(), None);
    let (spec_turns, spec_syscalls) =
        strict_shell_loop_turns(directory.path(), Some(spec.to_str().unwrap()));
    assert!(
        base_syscalls >= 500,
        "the guest made only {base_syscalls} syscalls; the comparison needs a syscall-heavy guest"
    );
    assert_eq!(
        base_syscalls, spec_syscalls,
        "the inert spec changed what the guest did"
    );
    // Two anchored counts on one thread: at most two checkpoint turns.
    assert!(
        spec_turns <= base_turns + 2,
        "an inert --happens-before spec added {} scheduler turns over {base_turns} \
         for {base_syscalls} syscalls: the checkpoint is issued per syscall again",
        spec_turns.saturating_sub(base_turns)
    );
}

/// The two-process guest of the ordering test: a backgrounded subshell and its
/// parent each write one line to stdout.
const HB_ORDER_GUEST: [&str; 3] = ["/bin/sh", "-c", "(echo child) & echo parent; wait"];

/// One `write(1, ..)` the guest made, as Hermit logged it at INFO: the thread
/// and that thread's syscall count, which is the count a `syscalls` anchor uses
/// (both are `new_count` in `detcore/src/lib.rs`).
struct LoggedWrite {
    dettid: u64,
    count: u64,
    len: u64,
}

/// Every `finish syscall #N: write(1, PTR, LEN)` line in a Hermit INFO log.
fn logged_stdout_writes(log: &str) -> Vec<LoggedWrite> {
    log.lines()
        .filter_map(|line| {
            let after_dtid = line.split_once("[syscall][detcore, dtid ")?.1;
            let (dettid, rest) = after_dtid.split_once(']')?;
            let rest = rest.split_once("finish syscall #")?.1;
            let (count, rest) = rest.split_once(": write(1, ")?;
            let len = rest.split_once(", ")?.1.split_once(')')?.0;
            Some(LoggedWrite {
                dettid: dettid.trim().parse().ok()?,
                count: count.parse().ok()?,
                len: len.trim().parse().ok()?,
            })
        })
        .collect()
}

/// A hard `--happens-before` edge reverses the order of two processes' writes.
///
/// Calibration and the edge come from the same environment: an INFO run
/// finds each process's `write(1, ..)` and its syscall count (the parent writes
/// 7 bytes, "parent\n", the child 6). The run's default order names a first and
/// a second writer; the edge "second writer's next syscall after its write"
/// before "first writer's write" must then put the second writer's line first.
/// Without enforcement (for example, a checkpoint filter that never fires) the
/// default order comes back and the test fails.
#[test]
fn happens_before_edge_reverses_two_processes_writes() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut args = vec!["--log", "info", "run", "--strict", "--"];
    args.extend(HB_ORDER_GUEST);
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let default_stdout = String::from_utf8_lossy(&calibration.stdout).into_owned();
    let writes = logged_stdout_writes(&stderr(&calibration));
    let parent = writes.iter().find(|w| w.len == 7);
    let child = writes.iter().find(|w| w.len == 6);
    let (Some(parent), Some(child)) = (parent, child) else {
        panic!(
            "no parent and child write(1, ..) in the INFO log; found {} writes",
            writes.len()
        );
    };
    assert_ne!(
        parent.dettid, child.dettid,
        "the two writes came from one thread"
    );
    let (first, second, reversed) = match default_stdout.as_str() {
        "parent\nchild\n" => (parent, child, "child\nparent\n"),
        "child\nparent\n" => (child, parent, "parent\nchild\n"),
        other => panic!("unexpected default output {other:?}"),
    };
    let spec = directory.path().join("reverse.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"second_wrote": {{"thread": "{}", "syscalls": {}}},
                            "first_writes": {{"thread": "{}", "syscalls": {}}}}},
                "edges": [{{"before": "second_wrote", "after": "first_writes", "strength": "hard"}}]}}"#,
            second.dettid,
            second.count + 1,
            first.dettid,
            first.count
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let mut args = vec!["run", "--strict", "--happens-before", spec.as_str(), "--"];
    args.extend(HB_ORDER_GUEST);
    let ordered = hermit(&args);
    assert_success(&ordered, &args);
    assert_eq!(
        String::from_utf8_lossy(&ordered.stdout),
        reversed,
        "the edge {}:{} < {}:{} did not reverse the default order {default_stdout:?}",
        second.dettid,
        second.count + 1,
        first.dettid,
        first.count
    );
}

/// A `--happens-before` spec costs scheduler turns only on the threads it
/// names. The spec names the child of the two-process guest at its third
/// syscall (a count the parent also passes) and the parent at a count it never
/// reaches, so nothing is ever parked: the only effect is the checkpoint turn
/// at the child's anchor. With a thread-agnostic filter every thread passing
/// an anchored count checked in, here the parent too, and its extra turn
/// shifted the schedule of a thread the spec does not mention (on a QEMU guest
/// the main loop passes small counts within the first second, so an anchor
/// chosen on a vCPU thread from a reference run fell on another timeline).
#[test]
fn happens_before_spec_adds_checkpoint_turns_only_on_named_threads() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut args = vec!["--log", "info", "run", "--strict", "--"];
    args.extend(HB_ORDER_GUEST);
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let writes = logged_stdout_writes(&stderr(&calibration));
    let (Some(parent), Some(child)) = (
        writes.iter().find(|w| w.len == 7),
        writes.iter().find(|w| w.len == 6),
    ) else {
        panic!(
            "no parent and child write(1, ..) in the INFO log; found {} writes",
            writes.len()
        );
    };
    assert!(
        child.count > 3 && parent.count > 3,
        "both processes must pass syscall 3 (child wrote at {}, parent at {})",
        child.count,
        parent.count
    );
    let spec = directory.path().join("child-only.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"child_early": {{"thread": "{}", "syscalls": 3}},
                            "parent_never": {{"thread": "{}", "syscalls": 1000000000}}}},
                "edges": [{{"before": "child_early", "after": "parent_never", "strength": "hard"}}]}}"#,
            child.dettid, parent.dettid
        ),
    )
    .unwrap();
    let turns = |with_spec: bool| -> u64 {
        let summary = directory
            .path()
            .join(if with_spec { "spec.json" } else { "base.json" });
        let summary_arg = format!("--summary-json={}", summary.display());
        let spec_str = spec.to_str().unwrap().to_owned();
        let mut args = vec!["run", "--strict", summary_arg.as_str()];
        if with_spec {
            args.extend(["--happens-before", spec_str.as_str()]);
        }
        args.push("--");
        args.extend(HB_ORDER_GUEST);
        let output = hermit(&args);
        assert_success(&output, &args);
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&summary).expect("the summary was not written"))
                .expect("the summary is not JSON");
        report["sched_turns"]
            .as_u64()
            .expect("summary has no sched_turns")
    };
    let base = turns(false);
    let with_spec = turns(true);
    assert_eq!(
        with_spec,
        base + 1,
        "a spec naming only the child (dettid {}) at syscall 3 should add exactly its one \
         checkpoint turn to the {base} of the plain run; a thread-agnostic filter adds the \
         parent's (dettid {}) too",
        child.dettid,
        parent.dettid
    );
}

/// A hard edge orders two threads of ONE process (the worker's dettid is not
/// its process's detpid, so a filter or anchor that confused the two would
/// gate the wrong thread or none). Calibration as in the two-process test: an
/// INFO run finds each thread's `write(1, ..)` and syscall count ("worker\n"
/// is 7 bytes, "main!\n" 6). The edge fires at the second writer's write and
/// gates the first writer's, so the default order must reverse. The BEFORE
/// anchor sits at the write itself, not the next syscall: in this guest the
/// main thread's next syscall is the futex wait in `pthread_join`, and an
/// anchor on a blocking wait of the BEFORE thread currently ends in a false
/// "Deadlock detected" (https://github.com/rrnewton/hermit/issues/3149).
#[test]
fn happens_before_edge_reverses_two_threads_writes() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_two_threads_guest().to_str().unwrap().to_owned();
    let args = ["--log", "info", "run", "--strict", "--", guest.as_str()];
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let default_stdout = String::from_utf8_lossy(&calibration.stdout).into_owned();
    let writes = logged_stdout_writes(&stderr(&calibration));
    let (Some(worker), Some(main)) = (
        writes.iter().find(|w| w.len == 7),
        writes.iter().find(|w| w.len == 6),
    ) else {
        panic!(
            "no worker and main write(1, ..) in the INFO log; found {} writes",
            writes.len()
        );
    };
    assert_ne!(
        worker.dettid, main.dettid,
        "the two writes came from one thread"
    );
    let (first, second, reversed) = match default_stdout.as_str() {
        "worker\nmain!\n" => (worker, main, "main!\nworker\n"),
        "main!\nworker\n" => (main, worker, "worker\nmain!\n"),
        other => panic!("unexpected default output {other:?}"),
    };
    let spec = directory.path().join("reverse-threads.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"second_writes": {{"thread": "{}", "syscalls": {}}},
                            "first_writes": {{"thread": "{}", "syscalls": {}}}}},
                "edges": [{{"before": "second_writes", "after": "first_writes", "strength": "hard"}}]}}"#,
            second.dettid, second.count, first.dettid, first.count
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
    ];
    let ordered = hermit(&args);
    assert_success(&ordered, &args);
    assert_eq!(
        String::from_utf8_lossy(&ordered.stdout),
        reversed,
        "the edge {}:{} < {}:{} did not reverse the default order {default_stdout:?}",
        second.dettid,
        second.count,
        first.dettid,
        first.count
    );
}

/// Every `finish syscall #N: futex(ADDR, OP, ..)` line in a Hermit INFO log, as
/// (dettid, N, OP).
fn logged_futex_calls(log: &str) -> Vec<(u64, u64, u64)> {
    log.lines()
        .filter_map(|line| {
            let after_dtid = line.split_once("[syscall][detcore, dtid ")?.1;
            let (dettid, rest) = after_dtid.split_once(']')?;
            let rest = rest.split_once("finish syscall #")?.1;
            let (count, rest) = rest.split_once(": futex(")?;
            let op = rest.split_once(", ")?.1.split_once(',')?.0;
            Some((
                dettid.trim().parse().ok()?,
                count.parse().ok()?,
                op.trim().parse().ok()?,
            ))
        })
        .collect()
}

/// Index of the first INFO line finishing syscall `count` of `dettid`.
fn finish_line(log: &str, dettid: u64, count: u64) -> Option<usize> {
    let needle = format!("[syscall][detcore, dtid {dettid}] finish syscall #{count}:");
    log.lines().position(|line| line.contains(&needle))
}

/// A source whose thread blocks right after firing must still let the target
/// pass (https://github.com/rrnewton/hermit/issues/3149). The guest's parent
/// makes eight getpids and then FUTEX_WAITs on a word its forked child sets
/// and wakes after 256 getpids. The edge is "parent's last getpid" before "a
/// child getpid in the middle of its loop", so the child is parked, the
/// parent's source fires, and the parent blocks: only the child, whose gate
/// just opened, can run. The scheduler used to re-admit it only at
/// `step3_peek`, after the empty-queue step had already classified the state
/// as a futex deadlock (exit 125). The test requires completion, proves the
/// child was actually held (otherwise it shows nothing), and that the target
/// finished only after the source.
#[test]
fn happens_before_source_then_blocking_wait_lets_the_target_pass() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_source_then_futex_guest().to_str().unwrap().to_owned();
    let args = ["--log", "info", "run", "--strict", "--", guest.as_str()];
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let futexes = logged_futex_calls(&stderr(&calibration));
    // FUTEX_WAIT is op 0, FUTEX_WAKE op 1 (no FUTEX_PRIVATE_FLAG on a shared map).
    let Some(&(parent, wait_count, _)) = futexes.iter().find(|f| f.2 == 0) else {
        panic!("no FUTEX_WAIT in the calibration log: {futexes:?}");
    };
    let Some(&(child, wake_count, _)) = futexes.iter().find(|f| f.2 == 1 && f.0 != parent) else {
        panic!("no child FUTEX_WAKE in the calibration log: {futexes:?}");
    };
    assert!(
        wake_count > 200,
        "the child's wake came at its syscall {wake_count}; its 256-getpid loop should precede it"
    );
    let source = wait_count - 1;
    let target = wake_count - 128;
    let spec = directory.path().join("source-then-futex.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"source": {{"thread": "{parent}", "syscalls": {source}}},
                            "target": {{"thread": "{child}", "syscalls": {target}}}}},
                "edges": [{{"before": "source", "after": "target", "strength": "hard"}}]}}"#
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
    ];
    let ordered = hermit(&args);
    let log = stderr(&ordered);
    assert_success(&ordered, &args);
    assert_eq!(String::from_utf8_lossy(&ordered.stdout), "wake-completed\n");
    assert!(
        log.contains(&format!("SKIP dettid {child} held at happens-before")),
        "the child (dettid {child}) was never held at its gate, so the test exercised nothing"
    );
    let (Some(source_done), Some(target_done)) = (
        finish_line(&log, parent, source),
        finish_line(&log, child, target),
    ) else {
        panic!("source {parent}:{source} or target {child}:{target} missing from the INFO log");
    };
    assert!(
        source_done < target_done,
        "the target {child}:{target} finished (line {target_done}) before the source \
         {parent}:{source} (line {source_done})"
    );
}

/// A gate whose BEFORE anchor can never fire, on the only thread left, is a
/// deadlock to report (https://github.com/rrnewton/hermit/issues/3149). A held
/// thread is in neither the run queue nor a blocked pool, so the scheduler loop
/// used to conclude that no threads were left and exit, leaving the guest to
/// hang without a report. It must now fail promptly, naming the gate.
#[test]
fn happens_before_gate_that_cannot_open_reports_a_deadlock() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = directory.path().join("never-open.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"never": {"thread": "3", "syscalls": 1000000000},
                       "gate": {"thread": "3", "syscalls": 50}},
            "edges": [{"before": "never", "after": "gate", "strength": "hard"}]}"#,
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 100 ]; do read x < /proc/self/stat; i=$((i+1)); done; echo done",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a gate that cannot open must end with the policy-refusal status: {log}"
    );
    assert!(
        log.contains(
            "anchor 'never' on thread 3: after 1000000000 syscalls never fired; it holds dtid 3 \
             at anchor 'gate' (edge never -> gate)"
        ) && log.contains("held at a happens-before gate whose BEFORE anchor cannot fire")
            && log.contains("HappensBeforeCheckpoint(50)"),
        "no refusal naming the gate's BEFORE anchor, with the deadlock report:\n{log}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
}

/// The shell guest of the held-signal test: a short backgrounded child, a long
/// one, and the parent each write one line (6, 5 and 7 bytes).
const HB_HELD_SIGCHLD_GUEST: [&str; 3] = [
    "/bin/sh",
    "-c",
    "(echo early) & \
     (i=0; while [ $i -lt 300 ]; do read x < /proc/self/stat; i=$((i+1)); done; echo late) & \
     echo parent; wait",
];

/// A signal the scheduler sends to a thread held at a happens-before gate
/// leaves it held until the gate opens. The parent is held at its write until
/// the long child's write; meanwhile the short child exits and the scheduler
/// sends the parent its SIGCHLD. A held thread used to be reported as gone, so
/// `wake_signaled_guest` panicked and the run hung (the reproduction from the
/// review of https://github.com/rrnewton/hermit/pull/3897). The run must
/// complete in the edge's order, and the log must show the signal reaching the
/// held parent, or the test exercised nothing.
#[test]
fn happens_before_sigchld_to_a_held_thread_waits_for_its_gate() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut args = vec!["--log", "info", "run", "--strict", "--"];
    args.extend(HB_HELD_SIGCHLD_GUEST);
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let writes = logged_stdout_writes(&stderr(&calibration));
    let parent = writes.iter().find(|w| w.len == 7);
    let late = writes.iter().find(|w| w.len == 5);
    let (Some(parent), Some(late)) = (parent, late) else {
        panic!(
            "no parent and late-child write(1, ..) in the INFO log; found {} writes",
            writes.len()
        );
    };
    let spec = directory.path().join("held-sigchld.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"late_writes": {{"thread": "{}", "syscalls": {}}},
                            "parent_writes": {{"thread": "{}", "syscalls": {}}}}},
                "edges": [{{"before": "late_writes", "after": "parent_writes", "strength": "hard"}}]}}"#,
            late.dettid, late.count, parent.dettid, parent.count
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let mut args = vec![
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
    ];
    args.extend(HB_HELD_SIGCHLD_GUEST);
    let ordered = hermit(&args);
    let log = stderr(&ordered);
    assert_success(&ordered, &args);
    let stdout = String::from_utf8_lossy(&ordered.stdout).into_owned();
    assert!(
        stdout.ends_with("late\nparent\n") && stdout.contains("early\n"),
        "the edge did not put the late child's line before the parent's: {stdout:?}"
    );
    let parent = parent.dettid;
    assert!(
        log.contains(&format!("SKIP dettid {parent} held at happens-before")),
        "the parent (dettid {parent}) was never held at its gate, so the test exercised nothing"
    );
    assert!(
        log.contains(&format!(
            "[dtid {parent}] held at a happens-before gate; leaving it held, its signal pending"
        )),
        "no signal reached the held parent (dettid {parent}), so the test exercised nothing"
    );
}

/// An alarm that fires at a thread held at a gate that can never open must
/// end the run in a deadlock report. The gate is the syscall after `alarm(1)`
/// (its count comes from an INFO run, since the loader's syscalls vary with
/// the environment). The alarm then fires through the empty-queue time skip
/// with only the held thread left; it used to panic the scheduler (the held
/// thread was reported as gone) and hang the run.
#[test]
fn happens_before_alarm_at_a_held_thread_ends_in_a_deadlock_report() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_signal_while_held_guest().to_str().unwrap().to_owned();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--",
        guest.as_str(),
        "alarm",
    ];
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let calibration_log = stderr(&calibration);
    let Some(alarm) = calibration_log.lines().find_map(|line| {
        let rest = line
            .split_once("[syscall][detcore, dtid 3] finish syscall #")?
            .1;
        let (count, rest) = rest.split_once(": ")?;
        rest.starts_with("alarm(1)")
            .then(|| count.parse::<u64>().ok())
            .flatten()
    }) else {
        panic!("no alarm(1) by dettid 3 in the calibration log");
    };
    let gate = alarm + 1;
    let spec = directory.path().join("never-open.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"never": {{"thread": "3", "syscalls": 1000000000}},
                            "gate": {{"thread": "3", "syscalls": {gate}}}}},
                "edges": [{{"before": "never", "after": "gate", "strength": "hard"}}]}}"#
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
        "alarm",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert!(
        !output.status.success(),
        "a gate that cannot open let the run succeed: {log}"
    );
    assert!(
        log.contains("[dtid 3] held at a happens-before gate; leaving it held, its signal pending"),
        "the alarm never reached the held thread, so the test exercised nothing:\n{log}"
    );
    assert!(
        log.contains("held at a happens-before gate whose BEFORE anchor cannot fire")
            && log.contains(&format!("HappensBeforeCheckpoint({gate})")),
        "no deadlock report naming the gate:\n{log}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).is_empty());
}

/// A process may exit while one of its threads is held at a gate that can
/// never open. The held worker is killed with its process and must not keep
/// the scheduler loop alive or be reported as a deadlock: the run exits 0.
#[test]
fn happens_before_process_exit_with_a_held_worker_exits_zero() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_signal_while_held_guest().to_str().unwrap().to_owned();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--",
        guest.as_str(),
        "exit",
    ];
    let calibration = hermit(&args);
    assert_success(&calibration, &args);
    let writes = logged_stdout_writes(&stderr(&calibration));
    let Some(worker) = writes.iter().find(|w| w.len == 7) else {
        panic!(
            "no worker write(1, ..) in the INFO log; found {} writes",
            writes.len()
        );
    };
    let worker = worker.dettid;
    let spec = directory.path().join("held-worker.json");
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"never": {{"thread": "3", "syscalls": 1000000000}},
                            "gate": {{"thread": "{worker}", "syscalls": 3}}}},
                "edges": [{{"before": "never", "after": "gate", "strength": "hard"}}]}}"#
        ),
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
        "exit",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_success(&output, &args);
    assert_eq!(String::from_utf8_lossy(&output.stdout), "main-exits\n");
    assert!(
        log.contains(&format!("SKIP dettid {worker} held at happens-before")),
        "the worker (dettid {worker}) was never held at its gate, so the test exercised nothing"
    );
}

/// A spec written against `tests/c/hb_two_threads.c` with syscall-occurrence
/// anchors: "thread T's 1st `write` to fd 1". No calibration run, no syscall
/// count. The main thread is dettid 3 and its pthread worker dettid 5, which
/// Hermit allocates deterministically.
fn fd_anchor_order_spec(directory: &Path, name: &str, first: &str, second: &str) -> String {
    let spec = directory.join(name);
    fs::write(
        &spec,
        format!(
            r#"{{"version": 1,
                "events": {{"first_writes": {{"thread": "{first}", "syscall": "write", "fd": 1, "nth": 1}},
                            "second_writes": {{"thread": "{second}", "syscall": "write", "fd": 1, "nth": 1}}}},
                "edges": [{{"before": "first_writes", "after": "second_writes", "strength": "hard"}}]}}"#
        ),
    )
    .unwrap();
    spec.to_str().unwrap().to_owned()
}

/// Syscall-occurrence anchors order two threads without calibration. The
/// same guest runs under two specs that differ only in which thread's first
/// `write` to fd 1 comes first; each output must follow its spec. One of the
/// two orders reverses the guest's default order, and that run must show the
/// thread held at its gate, so the test fails if the anchors never act (a
/// spec that is silently ignored leaves the default order in both runs).
#[test]
fn happens_before_fd_anchors_order_two_threads_without_calibration() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_two_threads_guest().to_str().unwrap().to_owned();
    let args = ["run", "--strict", "--", guest.as_str()];
    let default_run = hermit(&args);
    assert_success(&default_run, &args);
    let default_stdout = String::from_utf8_lossy(&default_run.stdout).into_owned();
    for (name, first, second, expected) in [
        ("main-first.json", "3", "5", "main!\nworker\n"),
        ("worker-first.json", "5", "3", "worker\nmain!\n"),
    ] {
        let spec = fd_anchor_order_spec(directory.path(), name, first, second);
        let args = [
            "--log",
            "info",
            "run",
            "--strict",
            "--happens-before",
            spec.as_str(),
            "--",
            guest.as_str(),
        ];
        let ordered = hermit(&args);
        let log = stderr(&ordered);
        assert_success(&ordered, &args);
        assert_eq!(
            String::from_utf8_lossy(&ordered.stdout),
            expected,
            "{name}: thread {first}'s first write to fd 1 did not come before thread {second}'s"
        );
        if expected != default_stdout {
            assert!(
                log.contains(&format!(
                    "SKIP dettid {second} held at happens-before anchor(s) [\"second_writes\"]"
                )),
                "{name} reversed the default order {default_stdout:?} without holding thread \
                 {second} at its gate"
            );
        }
    }
}

/// A syscall-occurrence anchor that never fires is refused by name: the
/// guest's worker never writes to fd 77, so the anchor naming its first such
/// write never fires, and the run must fail with HERMIT_HB_ANCHOR_NEVER_FIRED
/// naming it, after the guest itself completed, rather than exit 0 with the
/// edge unexercised.
#[test]
fn happens_before_fd_anchor_that_never_fires_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_two_threads_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("phantom.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"main_writes": {"thread": "3", "syscall": "write", "fd": 1, "nth": 1},
                       "phantom": {"thread": "5", "syscall": "write", "fd": 77, "nth": 1}},
            "edges": [{"before": "main_writes", "after": "phantom", "strength": "hard"}]}"#,
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a happens-before anchor that never fired must end the run with the policy-refusal \
         status, not the guest's:\n{log}"
    );
    assert!(
        log.contains("HERMIT_HB_ANCHOR_NEVER_FIRED: 1 happens-before syscall-occurrence anchor(s)")
            && log.contains(
                "anchor 'phantom' on thread 5: write(fd=77)#1 never fired (edges: main_writes -> phantom)"
            ),
        "no refusal naming the anchor that never fired:\n{log}"
    );
    assert!(!log.contains("anchor 'main_writes'"), "{log}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("worker\n") && stdout.contains("main!\n"),
        "the guest did not complete before the refusal: {stdout:?}"
    );
}

/// A shell guest whose backgrounded child writes three lines with other
/// syscalls (`read`) in between, while the parent writes three lines.
const HB_FD_NTH_GUEST: [&str; 3] = [
    "/bin/sh",
    "-c",
    "(echo c1; read x < /proc/self/stat; echo c2; read x < /proc/self/stat; echo c3) & \
     echo p1; echo p2; echo p3; wait",
];

/// Syscall-occurrence anchors past the first occurrence, end to end. The
/// child (the first spawned thread, `spawn_ordinal` 1) and the parent
/// (dettid 3) each write three lines to fd 1, the child with non-matching
/// `read`s in between. One spec requires the child's 3rd write before the
/// parent's 2nd, the other the parent's 3rd before the child's 2nd; each
/// output must respect its edge, and a run whose order differs from the
/// default must show a thread held at the gate.
#[test]
fn happens_before_fd_anchor_counts_past_the_first_occurrence() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut args = vec!["run", "--strict", "--"];
    args.extend(HB_FD_NTH_GUEST);
    let default_run = hermit(&args);
    assert_success(&default_run, &args);
    let default_stdout = String::from_utf8_lossy(&default_run.stdout).into_owned();
    for (name, first, first_nth, second, second_nth, before, after) in [
        ("child-c3-first.json", "child", 3, "parent", 2, "c3", "p2"),
        ("parent-p3-first.json", "parent", 3, "child", 2, "p3", "c2"),
    ] {
        let spec = directory.path().join(name);
        fs::write(
            &spec,
            format!(
                r#"{{"version": 1,
                    "threads": {{"parent": {{"dettid": 3}}, "child": {{"spawn_ordinal": 1}}}},
                    "events": {{"first_writes": {{"thread": "{first}", "syscall": "write", "fd": 1, "nth": {first_nth}}},
                                "second_writes": {{"thread": "{second}", "syscall": "write", "fd": 1, "nth": {second_nth}}}}},
                    "edges": [{{"before": "first_writes", "after": "second_writes", "strength": "hard"}}]}}"#
            ),
        )
        .unwrap();
        let spec = spec.to_str().unwrap().to_owned();
        let mut args = vec![
            "--log",
            "info",
            "run",
            "--strict",
            "--happens-before",
            spec.as_str(),
            "--",
        ];
        args.extend(HB_FD_NTH_GUEST);
        let ordered = hermit(&args);
        let log = stderr(&ordered);
        assert_success(&ordered, &args);
        let stdout = String::from_utf8_lossy(&ordered.stdout).into_owned();
        let lines: Vec<&str> = stdout.lines().collect();
        let at = |line: &str| {
            lines
                .iter()
                .position(|l| *l == line)
                .unwrap_or_else(|| panic!("{name}: no line {line:?} in {stdout:?}"))
        };
        assert!(
            at(before) < at(after),
            "{name}: {first}'s write #{first_nth} ({before}) did not come before {second}'s \
             write #{second_nth} ({after}): {stdout:?}"
        );
        if stdout != default_stdout {
            assert!(
                log.contains("held at happens-before anchor(s) [\"second_writes\"]"),
                "{name} changed the default order {default_stdout:?} to {stdout:?} without \
                 holding a thread at the gate"
            );
        }
    }
}

/// A BEFORE anchor that never fires, while its AFTER thread is held, is the
/// likeliest spec mistake (a wrong `fd` or `nth`). The held main thread can
/// never proceed and nothing else can run: the run must end with the named
/// refusal, naming the BEFORE anchor and the thread it holds, and the
/// policy-refusal status, not as a Hermit internal failure.
#[test]
fn happens_before_before_anchor_that_never_fires_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_two_threads_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("phantom-before.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"phantom_before": {"thread": "5", "syscall": "write", "fd": 77, "nth": 1},
                       "main_writes": {"thread": "3", "syscall": "write", "fd": 1, "nth": 1}},
            "edges": [{"before": "phantom_before", "after": "main_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a BEFORE anchor that never fired must end the run with the policy-refusal status:\n{log}"
    );
    assert!(
        log.contains("HERMIT_HB_ANCHOR_NEVER_FIRED: happens-before BEFORE anchor(s) never fired")
            && log.contains(
                "anchor 'phantom_before' on thread 5: write(fd=77)#1 never fired; it holds dtid 3 \
                 at anchor 'main_writes' (edge phantom_before -> main_writes)"
            ),
        "no refusal naming the BEFORE anchor that never fired:\n{log}"
    );
}

/// A version-2 spec for `tests/c/hb_relative_anchor.c`: hold the main
/// thread (dettid 3) at its `nth` getppid after its second write to fd 1,
/// whose own `fd` is `base_fd`, until the worker's (dettid 5) first write to
/// fd 1. `extra` is spliced into the top level (for `max_hold_ns`).
fn hb_relative_spec(directory: &Path, name: &str, base_fd: i32, nth: u64, extra: &str) -> String {
    let spec = directory.join(name);
    fs::write(
        &spec,
        format!(
            r#"{{"version": 2, {extra}
                "events": {{"a2": {{"thread": "3", "syscall": "write", "fd": {base_fd}, "nth": 2}},
                           "a_hold": {{"thread": "3", "syscall": "getppid", "from": "a2", "nth": {nth}}},
                           "b_write": {{"thread": "5", "syscall": "write", "fd": 1, "nth": 1}}}},
                "edges": [{{"before": "b_write", "after": "a_hold"}}]}}"#
        ),
    )
    .unwrap();
    spec.to_str().unwrap().to_owned()
}

/// A relative anchor, end to end. Ungated, the main thread writes `A-after`
/// before the worker's `B` (the worker first yields 200 times). Held at "the
/// first getppid after its second write" until the worker's write, the order
/// reverses, and the INFO log shows the hold at that getppid, then the
/// worker's write, then that getppid completing. A spec silently ignored
/// would leave the ungated order; a gate at a later getppid would still let
/// the first two through before the hold.
#[test]
fn happens_before_relative_anchor_holds_the_first_call_after_its_base() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_relative_anchor_guest().to_str().unwrap().to_owned();
    let args = ["run", "--strict", "--", guest.as_str()];
    let ungated = hermit(&args);
    assert_success(&ungated, &args);
    assert_eq!(stdout(&ungated), "a1\na2\nA-after\nB\n");

    let spec = hb_relative_spec(directory.path(), "first-after-a2.json", 1, 1, "");
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.as_str(),
        "--",
        guest.as_str(),
    ];
    let gated = hermit(&args);
    assert_success(&gated, &args);
    assert_eq!(stdout(&gated), "a1\na2\nB\nA-after\n");
    let log = stderr(&gated);
    let lines: Vec<&str> = log.lines().collect();
    let first = |what: &str, from: usize, matches: &dyn Fn(&str) -> bool| {
        from + lines[from..]
            .iter()
            .position(|line| matches(line))
            .unwrap_or_else(|| panic!("no {what} after line {from} in the INFO log:\n{log}"))
    };
    let second_write = first("second write by dtid 3", 0, &|l| {
        l.contains("dtid 3] finish syscall #") && l.contains(": write(1, ") && l.contains("Ok(3)")
    });
    let second_write = first("second write by dtid 3", second_write + 1, &|l| {
        l.contains("dtid 3] finish syscall #") && l.contains(": write(1, ")
    });
    let held = first("the hold", second_write, &|l| {
        l.contains("SKIP dettid 3 held at happens-before anchor(s) [\"a_hold\"]")
    });
    let entered = first("dtid 3's getppid entry", second_write, &|l| {
        l.contains("dtid 3] inbound syscall: getppid()")
    });
    // The hold is at the FIRST getppid after the base: no getppid of dtid 3
    // completes between its second write and the hold.
    assert!(
        !lines[second_write..held]
            .iter()
            .any(|l| l.contains("dtid 3] finish syscall #") && l.contains(": getppid()")),
        "a getppid of dtid 3 completed before the hold, so the anchor held a later call"
    );
    let b_write = first("dtid 5's write", held, &|l| {
        l.contains("dtid 5] inbound syscall: write(1, ")
    });
    let granted = first("dtid 3's getppid completing", held, &|l| {
        l.contains("dtid 3] finish syscall #") && l.contains(": getppid()")
    });
    assert!(
        entered < held && held < b_write && b_write < granted,
        "expected dtid 3's first getppid after its second write to be held until dtid 5's \
         write: entry {entered}, hold {held}, write {b_write}, completion {granted}"
    );
}

/// A new thread starts with no relative-anchor state, whatever its parent
/// reached. In "late" mode the main thread writes both markers before it
/// creates the worker. The spec names the worker by spawn ordinal, so the main
/// thread's first write also reaches the worker anchor's base on the guest
/// side. The worker reaches its own base only at its final write, after all
/// its sched_yield calls, so "its first sched_yield after its first write"
/// never happens and must be refused by name, naming 'wy'. A worker that
/// inherited the main thread's origins alone would fire 'wy' at its first
/// yield and exit 0; one that inherited its counters as well would never reach
/// its own 'wb' (its write count would already be past 1), so the report would
/// name 'wb' instead. The exact report text catches both.
#[test]
fn happens_before_relative_anchor_state_is_not_inherited_by_a_new_thread() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_relative_anchor_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("fresh-child.json");
    fs::write(
        &spec,
        r#"{"version": 2, "threads": {"w": {"spawn_ordinal": 1}},
            "events": {"wb": {"thread": "w", "syscall": "write", "fd": 1, "nth": 1},
                       "wy": {"thread": "w", "syscall": "sched_yield", "from": "wb", "nth": 1}}}"#,
    )
    .unwrap();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.to_str().unwrap(),
        "--",
        guest.as_str(),
        "late",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "the worker's relative anchor must not fire from its parent's state:\n{log}"
    );
    assert!(
        log.contains(
            "HERMIT_HB_ANCHOR_NEVER_FIRED: 1 happens-before syscall-occurrence anchor(s)"
        ) && log.contains(
            "anchor 'wy' on thread w: sched_yield#1 never fired (counted from 'wb'; edges: none)"
        ),
        "{log}"
    );
    assert_eq!(stdout(&output), "a1\na2\nA-after\nB\n");
}

/// An unfired relative anchor is refused by name after the guest finishes:
/// one that points past the last getppid (the 4th of 3), and one whose base
/// (a write to fd 77) never happens, which names the missing base as well.
#[test]
fn happens_before_unfired_relative_anchor_is_refused_with_its_base() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_relative_anchor_guest().to_str().unwrap().to_owned();
    for (name, base_fd, nth, expected) in [
        (
            "fourth.json",
            1,
            4,
            "anchor 'a_hold' on thread 3: getppid#4 never fired (counted from 'a2'; edges: \
             b_write -> a_hold)",
        ),
        (
            "no-base.json",
            77,
            1,
            "anchor 'a_hold' on thread 3: getppid#1 never fired (counted from 'a2', which never \
             fired either; edges: b_write -> a_hold)",
        ),
    ] {
        let spec = hb_relative_spec(directory.path(), name, base_fd, nth, "");
        let args = [
            "run",
            "--strict",
            "--happens-before",
            spec.as_str(),
            "--",
            guest.as_str(),
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{name}: an unfired relative anchor must be refused:\n{log}"
        );
        assert!(
            log.contains("HERMIT_HB_ANCHOR_NEVER_FIRED: ") && log.contains(expected),
            "{name}: no refusal naming the relative anchor:\n{log}"
        );
        assert_eq!(
            stdout(&output),
            "a1\na2\nA-after\nB\n",
            "{name}: the guest did not run to completion before the refusal"
        );
    }
}

/// A hold the spinning worker can never release ends at the version-2 hold
/// budget, by name, instead of spinning until an outside deadline. With a
/// budget too large to reach, the same run does spin until `--timeout`, so it
/// is the budget that ends the first run.
#[test]
fn happens_before_hold_budget_ends_a_hold_a_spinner_never_releases() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_relative_anchor_guest().to_str().unwrap().to_owned();
    let budget = hb_relative_spec(
        directory.path(),
        "budget.json",
        1,
        1,
        r#""max_hold_ns": 50000000,"#,
    );
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "run",
            "--strict",
            "--happens-before",
            budget.as_str(),
            "--",
            guest.as_str(),
            "spin",
        ]),
        directory.path(),
        Duration::from_secs(60),
        false,
    );
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "an exhausted hold budget must be a policy refusal:\n{log}"
    );
    assert!(
        log.contains(
            "HERMIT_HB_HOLD_BUDGET_EXCEEDED: dtid 3 has been held at happens-before anchor(s) \
             [\"a_hold\"] for more than max_hold_ns = 50000000 virtual ns, waiting on:\n  anchor \
             'b_write' on thread 5: write(fd=1)#1 never fired (edge b_write -> a_hold)"
        ),
        "no refusal naming the held thread and its missing BEFORE anchor:\n{log}"
    );

    let unreachable = hb_relative_spec(
        directory.path(),
        "unreachable.json",
        1,
        1,
        r#""max_hold_ns": 1000000000000000,"#,
    );
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "run",
            "--strict",
            "--timeout",
            "5",
            "--happens-before",
            unreachable.as_str(),
            "--",
            guest.as_str(),
            "spin",
        ]),
        directory.path(),
        Duration::from_secs(60),
        false,
    );
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(124),
        "the control without a reachable budget should spin until --timeout:\n{log}"
    );
    assert!(!log.contains("HERMIT_HB_HOLD_BUDGET_EXCEEDED"), "{log}");
}

/// Version-2 load refusals end the run with the policy-refusal status, name
/// the event and run nothing.
#[test]
fn happens_before_version_2_load_refusals_name_the_event_and_run_nothing() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_relative_anchor_guest().to_str().unwrap().to_owned();
    for (name, json, expected) in [
        (
            "v1-from.json",
            r#"{"version": 1, "events": {
                "a": {"thread": "3", "syscall": "write", "fd": 1},
                "r": {"thread": "3", "syscall": "getppid", "from": "a"}}}"#,
            "event 'r' sets 'from', which needs \"version\": 2",
        ),
        (
            "unknown-field.json",
            r#"{"version": 2, "events": {"r": {"thread": "3", "syscall": "getppid", "form": "a"}}}"#,
            "event 'r' has unknown field 'form'",
        ),
        (
            "other-thread.json",
            r#"{"version": 2, "events": {
                "a": {"thread": "5", "syscall": "write", "fd": 1},
                "r": {"thread": "3", "syscall": "getppid", "from": "a"}}}"#,
            "event 'r' counts from 'a', which cannot be shown to be on the same thread",
        ),
        (
            "futex-op-on-write.json",
            r#"{"version": 2, "events": {"w": {"thread": "3", "syscall": "write", "futex_op": 0}}}"#,
            "event 'w' sets 'futex_op', which applies only to \"syscall\": \"futex\"",
        ),
        (
            "from-cycle.json",
            r#"{"version": 2, "events": {
                "a": {"thread": "3", "syscall": "write", "fd": 1},
                "r": {"thread": "3", "syscall": "getppid", "from": "a"}},
                "edges": [{"before": "r", "after": "a"}]}"#,
            "relative bases and hard edges together contain a cycle: a -> r -> a",
        ),
    ] {
        let spec = directory.path().join(name);
        fs::write(&spec, json).unwrap();
        let args = [
            "run",
            "--strict",
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{name}: a spec that does not load must be a policy refusal:\n{log}"
        );
        assert!(log.contains(expected), "{name}: {log}");
        assert_eq!(stdout(&output), "", "{name}: the guest ran");
    }
}

/// A version-2 spec on a host whose perf counters are unavailable: argument
/// validation turns the default preemption timer off with a warning, and the
/// check right after it must refuse the run, naming the reason, before stdin
/// is read or the guest starts. `--verify` reads stdin to its end and the
/// test holds stdin open, so a check after the read would block until the
/// deadline; the guest's first action creates a marker file, which a control
/// run (no spec) shows is visible from here.
#[test]
fn happens_before_version_2_without_perf_counters_is_refused_before_stdin() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let marker = directory.path().join("guest-ran");
    let marker_str = marker.to_str().unwrap().to_owned();
    let script = format!("touch {marker_str}; echo ran");
    let guest = ["/bin/sh", "-c", script.as_str()];

    let mut control =
        hermit_command_under_host_filter(&["run", "--", guest[0], guest[1], guest[2]]);
    deny_syscall(&mut control, libc::SYS_perf_event_open);
    let (status, log) =
        run_with_deadline(control, directory.path(), Duration::from_secs(60), false);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(0),
        "control run failed:\n{log}"
    );
    assert!(
        marker.exists(),
        "the control run's guest did not create {marker_str}, so its absence below would prove \
         nothing"
    );
    fs::remove_file(&marker).unwrap();

    let spec = hb_relative_spec(directory.path(), "v2.json", 1, 1, "");
    let mut refused = hermit_command_under_host_filter(&[
        "run",
        "--verify",
        "--happens-before",
        spec.as_str(),
        "--",
        guest[0],
        guest[1],
        guest[2],
    ]);
    deny_syscall(&mut refused, libc::SYS_perf_event_open);
    let (status, log) = run_with_deadline(refused, directory.path(), Duration::from_secs(30), true);
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a version-2 spec without the preemption timer must be refused promptly (a kill at the \
         deadline is a failure):\n{log}"
    );
    assert!(
        log.contains("--max-timeslice requires user-space perf counters")
            && log.contains("HERMIT_HB_HOLD_WITHOUT_PREEMPTION: happens-before spec ")
            && log.contains("is version 2")
            && log.contains("this host cannot arm it: perf_event_open is unavailable"),
        "{log}"
    );
    assert!(!marker.exists(), "the guest ran before the refusal");
}

/// Run `command` with stdout and stderr redirected to files in `directory`,
/// waiting at most `limit`, and return its exit status (`None` when it had to
/// be killed at the deadline) and its stderr. Files rather than pipes: a child
/// that writes more than a pipe holds before it exits would block on the full
/// pipe and look like a hang. With `open_stdin`, stdin is a pipe whose writer
/// stays open for the whole wait, so a child that reads its input first never
/// returns.
fn run_with_deadline(
    mut command: Command,
    directory: &Path,
    limit: Duration,
    open_stdin: bool,
) -> (Option<std::process::ExitStatus>, String) {
    let stdout_path = directory.join("deadline-run.stdout");
    let stderr_path = directory.join("deadline-run.stderr");
    command
        .stdout(fs::File::create(&stdout_path).expect("failed to create the stdout file"))
        .stderr(fs::File::create(&stderr_path).expect("failed to create the stderr file"));
    if open_stdin {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().expect("failed to spawn hermit");
    let stdin_writer = child.stdin.take();
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait().expect("failed to poll hermit") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    drop(stdin_writer);
    let stderr = fs::read_to_string(&stderr_path).expect("failed to read the stderr file");
    (status, stderr)
}

/// The most `hermit run` attempts a PMU-preempted spinner case makes when its
/// earlier attempts each end in a typed skid-overshoot refusal
/// ([`skid_overshoot_refusal`]); see [`run_pmu_spinner_retrying_skid`].
///
/// Derived from the measured refusal rate, not chosen. On the AMD EPYC
/// development host recorded in docs/TESTING_ENVIRONMENTS.md, the
/// `poll_timeout_vs_spinner` guest built by the pinned validation root's
/// compiler drew the refusal, with correct guest output every time
/// (https://github.com/rrnewton/reverie/issues/990), in 3 of 13
/// privileged-test.pmu_cli_cases attempts in local validation, 5 of 15 direct
/// runs, 8 of 20 runs of this case inside the pinned root and 11 of 28
/// attempts of this retrying case inside it: 27 of 76, p = 0.36. With
/// independent attempts the chance that all `n` attempts refuse is p^n: 4.5%
/// for 3 attempts, 1.6% for 4 and 0.57% for 5 (1.0% for 5 at the highest
/// measured rate, p = 0.40).
///
/// Each attempt costs 8.2-10.7 CPU-s and 8.4-10.9 s of wall time in that
/// node, so five attempts need up to about 54 CPU-s and 55 s. The node
/// therefore scales its per-test CPU budget (22 CPU-s by default) by 4 to 88
/// CPU-s and its per-test wall bound (57 s by default) by 2 to 114 s, about
/// 1.6 and 2.1 times what five attempts need; at the defaults a third attempt
/// would be killed by the CPU budget.
const SKID_RETRY_MAX_ATTEMPTS: u32 = 5;

/// The prefix of the canonical line Hermit prints when it refuses a run for
/// PMU skid overshoot: `classify_failure` in hermit-cli/src/bin/hermit/main.rs
/// writes `HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot
/// count=<n>`, the same line the manifest runner requires of a skid attempt
/// (`skid_overshoot_only_reports` in ci/manifest-plan/src/runner.rs).
const SKID_OVERSHOOT_REFUSAL_PREFIX: &str =
    "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=";

/// Line prefixes that mark a second outcome beside a skid refusal: Hermit's
/// other one-line classifications (another policy refusal, an internal
/// failure, a signal death, a run timeout, a log cap) and a task panic.
const SKID_RETRY_DISQUALIFYING_PREFIXES: [&str; 6] = [
    "HERMIT_POLICY_REFUSAL",
    "HERMIT_INTERNAL_FAILURE",
    "HERMIT_SIGNAL_DEATH",
    "HERMIT_RUN_TIMEOUT",
    "HERMIT_LOG_CAP",
    "HERMIT_TASK_PANIC",
];

/// The canonical skid-overshoot refusal line of one finished `hermit run`,
/// or `None` unless that run is a typed skid-overshoot refusal and nothing
/// else (https://github.com/rrnewton/hermit/issues/1845). All must hold:
///
/// - the run exited with `HERMIT_POLICY_REFUSAL_EXIT` (122); a signal, a
///   deadline kill (`None`) or any other status never qualifies;
/// - its stderr carries exactly one refusal line, and it is
///   [`SKID_OVERSHOOT_REFUSAL_PREFIX`] followed by a decimal count above zero;
/// - its stderr carries at least one line that starts with
///   `HERMIT_SKID_OVERSHOOT `, Hermit's own report of the late PMU interrupt;
/// - no other line starts with a [`SKID_RETRY_DISQUALIFYING_PREFIXES`] entry
///   and none contains `panicked at`.
///
/// A refusal for any other cause, a refusal beside a second failure, or a
/// marker without the exit status is a result the caller must report as it
/// is, not a reason to run again.
fn skid_overshoot_refusal(code: Option<i32>, stderr: &str) -> Option<&str> {
    if code != Some(HERMIT_POLICY_REFUSAL_EXIT) {
        return None;
    }
    let mut refusal = None;
    let mut overshoot_reported = false;
    for line in stderr.lines() {
        let line = line.trim_end();
        if line.starts_with("HERMIT_SKID_OVERSHOOT ") {
            overshoot_reported = true;
        } else if let Some(count) = line.strip_prefix(SKID_OVERSHOOT_REFUSAL_PREFIX) {
            let counted = !count.is_empty()
                && count.bytes().all(|byte| byte.is_ascii_digit())
                && count.parse::<u64>().is_ok_and(|count| count > 0);
            if !counted || refusal.is_some() {
                return None;
            }
            refusal = Some(line);
        } else if line.contains("panicked at")
            || SKID_RETRY_DISQUALIFYING_PREFIXES
                .iter()
                .any(|prefix| line.starts_with(prefix))
        {
            return None;
        }
    }
    refusal.filter(|_| overshoot_reported)
}

/// One finished attempt of [`run_pmu_spinner_retrying_skid`].
struct SpinnerRun {
    /// The exit status, `None` when the deadline killed the run.
    status: Option<std::process::ExitStatus>,
    stdout: String,
    stderr: String,
}

/// Runs `hermit <args>` for a case whose guest spins without syscalls and is
/// preempted only by the PMU timer, and repeats the run only when an attempt
/// is a typed skid-overshoot refusal ([`skid_overshoot_refusal`]), at most
/// [`SKID_RETRY_MAX_ATTEMPTS`] times in all
/// (https://github.com/rrnewton/hermit/issues/1845). Such a refusal measures
/// the host's PMU interrupt latency, not the guest
/// (https://github.com/rrnewton/reverie/issues/990), so a fresh attempt is a
/// new measurement.
///
/// Every attempt has its own directory and the `limit` deadline. Before a
/// retry, the refused attempt's guest stdout must pass `check_stdout`, the
/// case's own stdout assertions, so a refusal that also shows a wrong result
/// fails at once. Every retried attempt prints a `SKID-RETRY` line with its
/// refusal line to the test's stderr, and an attempt that refuses at the cap
/// prints `SKID-RETRY LIMIT`. Any other outcome, and the last attempt, is
/// returned unchanged for the caller's assertions.
fn run_pmu_spinner_retrying_skid(
    test: &str,
    args: &[&str],
    limit: Duration,
    check_stdout: impl Fn(&str),
) -> SpinnerRun {
    for attempt in 1..=SKID_RETRY_MAX_ATTEMPTS {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let (status, stderr) =
            run_with_deadline(hermit_command(args), directory.path(), limit, false);
        let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
            .expect("failed to read the guest's stdout");
        let code = status.and_then(|status| status.code());
        let Some(refusal) = skid_overshoot_refusal(code, &stderr) else {
            return SpinnerRun {
                status,
                stdout,
                stderr,
            };
        };
        if attempt == SKID_RETRY_MAX_ATTEMPTS {
            eprintln!(
                "SKID-RETRY LIMIT {test}: attempt {attempt} of at most \
                 {SKID_RETRY_MAX_ATTEMPTS} exited {HERMIT_POLICY_REFUSAL_EXIT} with `{refusal}`; \
                 no further retry is made"
            );
            return SpinnerRun {
                status,
                stdout,
                stderr,
            };
        }
        eprintln!(
            "SKID-RETRY {test}: attempt {attempt} of at most {SKID_RETRY_MAX_ATTEMPTS} exited \
             {HERMIT_POLICY_REFUSAL_EXIT} with `{refusal}`; retrying once its guest stdout passes \
             the case's checks (https://github.com/rrnewton/hermit/issues/1845)"
        );
        check_stdout(&stdout);
    }
    unreachable!("the last attempt always returns")
}

/// [`skid_overshoot_refusal`] admits exactly a typed skid-overshoot refusal:
/// exit 122 with the canonical refusal line and Hermit's overshoot report,
/// and nothing beside them. It does not admit 122 for another cause, a skid
/// refusal beside a second failure, or any other exit status.
#[test]
fn skid_retry_admits_only_a_typed_skid_overshoot_refusal() {
    // The shape of a refused poll_timeout_returns_on_time_while_another_thread_spins
    // attempt, from the measurement on https://github.com/rrnewton/reverie/issues/990.
    let skid = "\
hermit: virtual-time epoch=2026-10-10T00:44:57.378767176+00:00 source=host-now
HERMIT_SKID_OVERSHOOT rcb_actual=6000317 rcb_target=5999950 skid_margin=1000 overshoot=367
2026-10-10T00:45:01.159474Z ERROR detcore: HERMIT_SKID_OVERSHOOT prehook: PMU RCB overshoot!
HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=4
Error: Hermit refused the run: a fail-closed policy stopped it before completion.
     > observed 4 HERMIT_SKID_OVERSHOOT report(s); refusing the result
";
    let refusal = "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot count=4";
    assert_eq!(
        skid_overshoot_refusal(Some(HERMIT_POLICY_REFUSAL_EXIT), skid),
        Some(refusal)
    );
    assert_eq!(
        skid_overshoot_refusal(
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            &skid.replace('\n', "\r\n")
        ),
        Some(refusal),
        "a CRLF line ending is still the canonical line"
    );

    // 122 for another cause, or for no named cause, is not a skid refusal.
    let overshoot = "HERMIT_SKID_OVERSHOOT rcb_actual=6000317 rcb_target=5999950 skid_margin=1000 overshoot=367\n";
    for other in [
        "HERMIT_POLICY_REFUSAL class=policy-refusal\n",
        "HERMIT_POLICY_REFUSAL class=policy-refusal cause=unsupported-syscall count=4\n",
        "HERMIT_POLICY_REFUSAL class=policy-refusal cause=skid-overshoot-x count=4\n",
    ] {
        assert_eq!(
            skid_overshoot_refusal(
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                &format!("{overshoot}{other}")
            ),
            None,
            "{other:?}"
        );
    }
    // The refusal line without Hermit's overshoot report, or with no positive
    // decimal count, is not one.
    assert_eq!(
        skid_overshoot_refusal(Some(HERMIT_POLICY_REFUSAL_EXIT), &format!("{refusal}\n")),
        None
    );
    for count in ["0", "", "x", "+4", "4 extra", "-1"] {
        let line = format!("{SKID_OVERSHOOT_REFUSAL_PREFIX}{count}\n");
        assert_eq!(
            skid_overshoot_refusal(
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                &format!("{overshoot}{line}")
            ),
            None,
            "{line:?}"
        );
    }
    // A skid refusal beside a second outcome is not skid-only.
    for second in [
        "HERMIT_POLICY_REFUSAL class=policy-refusal\n",
        &format!("{refusal}\n"),
        "HERMIT_INTERNAL_FAILURE class=internal-failure\n",
        "HERMIT_SIGNAL_DEATH class=signal-death signal=9\n",
        "HERMIT_RUN_TIMEOUT class=run-timeout seconds=30\n",
        "HERMIT_LOG_CAP class=log-cap\n",
        "HERMIT_TASK_PANIC detcore\n",
        "thread 'main' panicked at detcore/src/lib.rs:1:1:\n",
    ] {
        assert_eq!(
            skid_overshoot_refusal(Some(HERMIT_POLICY_REFUSAL_EXIT), &format!("{skid}{second}")),
            None,
            "{second:?}"
        );
    }
    // Any other exit status, a signal or a deadline kill (no code) never
    // qualifies, whatever stderr says.
    // EXIT-CLASS: arbitrary statuses (guest or hermit) that must never qualify as a skid refusal.
    for code in [
        Some(0),
        Some(1),
        Some(101),
        Some(123),
        Some(124),
        Some(125),
        None,
    ] {
        assert_eq!(skid_overshoot_refusal(code, skid), None, "{code:?}");
    }
}

/// A cycle through a child wait is a deadlock to refuse by name, not a hang
/// (https://github.com/rrnewton/hermit/issues/3904). The parent waits for its
/// child and only then writes; the spec holds the child's write until the
/// parent's write. Nothing can run: the child is held at its gate, and the
/// parent's wait can only end when the held child exits. The run used to spin
/// until an outside timeout, because a child wait counted as input that could
/// still arrive. Without the spec the same guest runs to completion.
#[test]
fn happens_before_cycle_through_a_child_wait_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_child_wait_cycle_guest().to_str().unwrap().to_owned();
    let args = ["run", "--strict", "--", guest.as_str()];
    let ungated = hermit(&args);
    assert_success(&ungated, &args);
    assert_eq!(stdout(&ungated), "child\nparent\n");

    let spec = directory.path().join("cycle.json");
    fs::write(&spec, HB_CHILD_WRITE_AFTER_PARENT_WRITE).unwrap();
    // "nested": the root forks the parent of the cycle and waits for it, so
    // an ordinary wait sits above the cycle and must not hide it (the Codex
    // design re-check of https://github.com/rrnewton/hermit/issues/3929).
    let nested = directory.path().join("nested.json");
    fs::write(
        &nested,
        r#"{"version": 1,
            "threads": {"parent": {"spawn_ordinal": 1}, "child": {"spawn_ordinal": 2}},
            "events": {"parent_writes": {"thread": "parent", "syscall": "write", "fd": 1, "nth": 1},
                       "child_writes": {"thread": "child", "syscall": "write", "fd": 1, "nth": 1}},
            "edges": [{"before": "parent_writes", "after": "child_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    let args = ["run", "--strict", "--", guest.as_str(), "nested"];
    let ungated = hermit(&args);
    assert_success(&ungated, &args);
    assert_eq!(stdout(&ungated), "child\nparent\nroot\n");
    // "joined": the child first creates and joins a helper thread, which stays
    // in the thread tree after it exits and must not count as a live thread.
    for (mode, spec, parent) in [
        ("plain", &spec, "3"),
        ("joined", &spec, "3"),
        ("nested", &nested, "parent"),
    ] {
        let (status, log) = run_with_deadline(
            hermit_command(&[
                "run",
                "--strict",
                "--happens-before",
                spec.to_str().unwrap(),
                "--",
                guest.as_str(),
                mode,
            ]),
            directory.path(),
            Duration::from_secs(60),
            false,
        );
        let status = status.unwrap_or_else(|| {
        panic!(
            "a cycle through a child wait spun instead of being refused: no exit within 60s\n{log}"
        )
    });
        assert_eq!(
            status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "a cycle through a child wait must be refused with the policy-refusal status:\n{log}"
        );
        assert!(
            log.contains(
                "HERMIT_HB_ANCHOR_NEVER_FIRED: happens-before BEFORE anchor(s) never fired"
            ) && log.contains(&format!(
                "anchor 'parent_writes' on thread {parent}: write(fd=1)#1 never fired; it holds \
                 dtid"
            )),
            "{mode}: no refusal naming the BEFORE anchor of the cycle:\n{log}"
        );
    }
}

/// A BEFORE anchor at the posthook orders the BEFORE syscall's completed
/// effect (https://github.com/rrnewton/hermit/issues/3929). The parent
/// posix_spawns `/bin/echo child`, then writes `parent`; an edge orders the
/// parent's write before the child's. Fired at the prehook, the edge only
/// orders the parent *reaching* its write: the held child is re-admitted
/// before the parent's write commits, so stdout is `child` then `parent`.
/// Fired at the posthook, the child is held until the parent's write has
/// completed, so stdout is `parent` then `child`.
#[test]
fn happens_before_posthook_before_anchor_orders_the_completed_write() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_spawn_echo_guest().to_str().unwrap().to_owned();
    for (name, phase, expected) in [
        ("prehook.json", "prehook", "child\nparent\n"),
        ("posthook.json", "posthook", "parent\nchild\n"),
    ] {
        let spec = directory.path().join(name);
        fs::write(
            &spec,
            format!(
                r#"{{"version": 1,
                    "threads": {{"child": {{"spawn_ordinal": 1}}}},
                    "events": {{"parent_writes": {{"thread": "3", "syscall": "write", "fd": 1,
                                                   "phase": "{phase}"}},
                               "child_writes": {{"thread": "child", "syscall": "write", "fd": 1}}}},
                    "edges": [{{"before": "parent_writes", "after": "child_writes", "strength": "hard"}}]}}"#
            ),
        )
        .unwrap();
        let args = [
            "--log",
            "info",
            "run",
            "--strict",
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert_success(&output, &args);
        assert_eq!(stdout(&output), expected, "{name}");
        assert!(
            log.contains("held at happens-before anchor(s) [\"child_writes\"]"),
            "{name}: the child was never held at its gate"
        );
    }
}

/// The spec of the child-wait tests: the first spawned process's first write
/// to fd 1 is held until the root's first write to fd 1.
const HB_CHILD_WRITE_AFTER_PARENT_WRITE: &str = r#"{"version": 1,
    "threads": {"child": {"spawn_ordinal": 1}},
    "events": {"parent_writes": {"thread": "3", "syscall": "write", "fd": 1, "nth": 1},
               "child_writes": {"thread": "child", "syscall": "write", "fd": 1, "nth": 1}},
    "edges": [{"before": "parent_writes", "after": "child_writes", "strength": "hard"}]}"#;

/// A held child that dies is not a deadlock: its parent's wait ends
/// (https://github.com/rrnewton/hermit/issues/3904; the probes from the
/// reviews of https://github.com/rrnewton/hermit/pull/4013). On ptrace the
/// child is retired only at its host-timed deregistration, so the scheduler
/// marks it doomed at a committed point instead: a SIGKILL sent by another
/// process (`other`), a process-group SIGKILL recorded before the kill
/// (`group`, and `group-self`, where the sender dies too), or a sibling thread
/// of the child that leaves without a committed exit (`raise-kill`, `abort`,
/// `raise-term`, `segv`). The run must complete: the parent prints how the
/// child ended, and only the end-of-run report refuses the child's write
/// anchor, which the child never reached. A false deadlock refusal ends the
/// run early instead, with no output. A process armed with
/// PR_SET_PDEATHSIG(SIGKILL) is doomed too (`pdeathsig`, the held child;
/// `pdeathsig-middle`, the middle of a chain whose orphaned held child then
/// completes and the run exits 0; the second Claude re-check). The window is
/// host-timed, so each case runs several times; the scheduler unit tests
/// `a_child_wait_settles_only_on_children_held_or_in_futex_waits` and
/// `a_process_armed_with_a_parent_death_sigkill_is_not_stuck` pin the rule
/// itself.
#[test]
fn happens_before_held_child_killed_by_another_process_is_not_a_deadlock() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_kill_held_child_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("kill-held.json");
    fs::write(&spec, HB_CHILD_WRITE_AFTER_PARENT_WRITE).unwrap();
    // In the pdeathsig modes a helper thread of the root is spawned first, so
    // the held child is the second spawn (the third behind a middle process).
    let spawned = |ordinal: u32| {
        let path = directory.path().join(format!("held-spawn-{ordinal}.json"));
        fs::write(
            &path,
            HB_CHILD_WRITE_AFTER_PARENT_WRITE.replace(
                r#""spawn_ordinal": 1"#,
                &format!(r#""spawn_ordinal": {ordinal}"#),
            ),
        )
        .unwrap();
        path
    };
    let (second, third) = (spawned(2), spawned(3));
    let killed = "parent h-status=killed\n";
    let mut cases: Vec<(&str, &Path, &str, bool, u32)> = [
        "other",
        "group",
        "group-self",
        "raise-kill",
        "abort",
        "raise-term",
        "segv",
    ]
    .into_iter()
    .map(|mode| (mode, spec.as_path(), killed, true, 6))
    .collect();
    // The parent-death window was 4 in 24 to 36 runs before the fix.
    cases.push(("pdeathsig", second.as_path(), killed, true, 16));
    cases.push((
        "pdeathsig-middle",
        third.as_path(),
        "parent m-status=killed\nchild\n",
        false,
        16,
    ));
    for (mode, spec, expected, child_dies, runs) in cases {
        for run in 1..=runs {
            let (status, log) = run_with_deadline(
                hermit_command(&[
                    "run",
                    "--strict",
                    "--happens-before",
                    spec.to_str().unwrap(),
                    "--",
                    guest.as_str(),
                    mode,
                ]),
                directory.path(),
                Duration::from_secs(60),
                false,
            );
            let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
                .expect("failed to read the guest's stdout");
            assert!(
                !log.contains("BEFORE anchor(s) never fired, and the thread(s) they hold"),
                "{mode} run {run}: a held child that died was reported as a deadlock:\n{log}"
            );
            assert_eq!(stdout, expected, "{mode} run {run}:\n{log}");
            if !child_dies {
                assert_eq!(
                    status.and_then(|s| s.code()),
                    Some(0),
                    "{mode} run {run}:\n{log}"
                );
                continue;
            }
            assert_eq!(
                status.and_then(|s| s.code()),
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                "{mode} run {run}: the dead child's write anchor never fired, so the run ends with \
             the end-of-run refusal:\n{log}"
            );
            assert!(
                log.contains("anchor 'child_writes' on thread child: write(fd=1)#1 never fired"),
                "{mode} run {run}:\n{log}"
            );
        }
    }
}

/// An AFTER anchor at the posthook holds its thread after the anchored call
/// has completed (https://github.com/rrnewton/hermit/issues/3929). In the
/// two-thread guest, an edge from the main thread's completed write (a
/// posthook BEFORE anchor) to the worker's write holds the worker *before*
/// its write when the AFTER anchor is at the prehook, so `main!` comes first;
/// at the posthook the worker's write has already happened when it is held,
/// so `worker` comes first, and the worker is held all the same. The worker
/// writes `returned` as soon as its write returns ("marker" mode): at the
/// posthook that marker comes only after `main!`, so the worker was held
/// between its write's effect and its return to user code.
#[test]
fn happens_before_posthook_after_anchor_holds_the_thread_after_its_call() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_two_threads_guest().to_str().unwrap().to_owned();
    for (name, phase, expected) in [
        ("after-prehook.json", "prehook", "main!\nworker\nreturned\n"),
        (
            "after-posthook.json",
            "posthook",
            "worker\nmain!\nreturned\n",
        ),
    ] {
        let spec = directory.path().join(name);
        fs::write(
            &spec,
            format!(
                r#"{{"version": 1,
                    "events": {{"main_writes": {{"thread": "3", "syscall": "write", "fd": 1,
                                                "phase": "posthook"}},
                               "worker_writes": {{"thread": "5", "syscall": "write", "fd": 1,
                                                  "phase": "{phase}"}}}},
                    "edges": [{{"before": "main_writes", "after": "worker_writes", "strength": "hard"}}]}}"#
            ),
        )
        .unwrap();
        let args = [
            "--log",
            "info",
            "run",
            "--strict",
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
            "marker",
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert_success(&output, &args);
        assert_eq!(stdout(&output), expected, "{name}");
        assert!(
            log.contains("SKIP dettid 5 held at happens-before anchor(s) [\"worker_writes\"]"),
            "{name}: the worker was never held at its gate"
        );
        // Which write completed first: at the posthook the worker's write
        // returns before the main thread's although the worker is held after
        // it, which only a hold after the call allows.
        let finished = |dtid: u32| {
            let prefix = format!("[syscall][detcore, dtid {dtid}] finish syscall #");
            log.lines()
                .position(|line| line.contains(&prefix) && line.contains(": write(1,"))
                .unwrap_or_else(|| panic!("{name}: no write(1, ...) finished by dtid {dtid}"))
        };
        assert_eq!(
            finished(5) < finished(3),
            phase == "posthook",
            "{name}: the worker's write completed on the wrong side of the main thread's"
        );
    }
}

/// The posix_spawn echo case with the posthook edge passes at L2 on ptrace:
/// `--verify --verify-strict` gives a canonical matched report with nonzero
/// INFO counts on both sides (https://github.com/rrnewton/hermit/issues/3929).
#[test]
fn happens_before_posthook_edge_passes_strict_verify() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_spawn_echo_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("posthook.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "threads": {"child": {"spawn_ordinal": 1}},
            "events": {"parent_writes": {"thread": "3", "syscall": "write", "fd": 1,
                                         "phase": "posthook"},
                       "child_writes": {"thread": "child", "syscall": "write", "fd": 1}},
            "edges": [{"before": "parent_writes", "after": "child_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    let report_path = directory.path().join("verify.json");
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--verify",
        "--verify-strict",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--happens-before",
        spec.to_str().unwrap(),
        "--",
        guest.as_str(),
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "parent\nchild\n");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).expect("no verify report")).unwrap();
    let left = report["compared_log_messages"]["left"]
        .as_u64()
        .unwrap_or(0);
    let right = report["compared_log_messages"]["right"]
        .as_u64()
        .unwrap_or(0);
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["log_scope"] == "info"
            && left > 0
            && left == right,
        "the posthook edge did not pass strict verification:\n{report}"
    );
}

/// A posthook anchor on a call interrupted by a caught signal is refused by
/// name (https://github.com/rrnewton/hermit/issues/3929). Detcore's untimed
/// futex wait returns the internal restart code; with `SA_RESTART` the kernel
/// would restart the entry, which then never returns, and without it the
/// entry returns `EINTR`. That is decided after Hermit's checkpoint, so both
/// end in `HERMIT_HB_POSTHOOK_INTERRUPTED` rather than a guessed firing. The
/// wait is the guest's one shared `FUTEX_WAIT` (operation 0); glibc's own
/// waits are private.
#[test]
fn happens_before_posthook_anchor_on_an_interrupted_call_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_posthook_futex_interrupt_guest()
        .to_str()
        .unwrap()
        .to_owned();
    let spec = directory.path().join("interrupted.json");
    fs::write(
        &spec,
        r#"{"version": 2,
            "threads": {"helper": {"spawn_ordinal": 1}},
            "events": {"main_wait": {"thread": "3", "syscall": "futex", "futex_op": 0,
                                     "phase": "posthook"},
                       "helper_writes": {"thread": "helper", "syscall": "write", "fd": 1}},
            "edges": [{"before": "main_wait", "after": "helper_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    for mode in ["restart", "eintr"] {
        let args = ["run", "--strict", "--", guest.as_str(), mode];
        let ungated = hermit(&args);
        assert_success(&ungated, &args);
        assert_eq!(stdout(&ungated), "helper\nmain\n", "{mode}");

        let args = [
            "--log",
            "info",
            "run",
            "--strict",
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
            mode,
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert!(
            log.contains("futex(") && log.contains("= Err(Errno(ERESTARTSYS))"),
            "{mode}: the wait was never interrupted, so the test exercised nothing:\n{log}"
        );
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{mode}:\n{log}"
        );
        assert!(
            log.contains(
                "HERMIT_HB_POSTHOOK_INTERRUPTED: dtid 3 reached posthook happens-before \
                 anchor(s) [\"main_wait\"]"
            ) && log.contains("interrupted (ERESTARTSYS)"),
            "{mode}: no refusal naming the interrupted anchor:\n{log}"
        );
        assert!(stdout(&output).is_empty(), "{mode}: the held helper wrote");
    }

    // A spawn-ordinal posthook anchor on the helper's own wait: the guest
    // names it at every thread's matching call, the main thread's interrupted
    // wait included, and only the helper's wait may decide it. The run
    // completes (the Claude review of https://github.com/rrnewton/hermit/pull/4048).
    let other = directory.path().join("other-thread.json");
    fs::write(
        &other,
        r#"{"version": 2,
            "threads": {"helper": {"spawn_ordinal": 1}},
            "events": {"helper_wait": {"thread": "helper", "syscall": "futex", "futex_op": 0,
                                       "phase": "posthook"},
                       "main_writes": {"thread": "3", "syscall": "write", "fd": 1}},
            "edges": [{"before": "helper_wait", "after": "main_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        other.to_str().unwrap(),
        "--",
        guest.as_str(),
        "other-thread",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert!(
        log.contains("[syscall][detcore, dtid 3] finish syscall #")
            && log.contains("= Err(Errno(ERESTARTSYS))"),
        "the main thread's wait was never interrupted, so the case exercised nothing:\n{log}"
    );
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "helper\nmain\n");
    assert!(!log.contains("HERMIT_HB_POSTHOOK_INTERRUPTED"), "{log}");
}

/// A thread that leaves without completing its posthook-anchored call
/// publishes nothing: a worker blocks reading a pipe the process keeps open,
/// and the main thread exits the group. The anchor never fired, so the run
/// ends with the end-of-run report naming it, after the guest's own output
/// (https://github.com/rrnewton/hermit/issues/3929, design revision 2; the
/// Claude review of https://github.com/rrnewton/hermit/pull/4048).
#[test]
fn happens_before_posthook_anchor_of_a_thread_that_exits_mid_call_never_fires() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_pipe_read_held_writer_guest()
        .to_str()
        .unwrap()
        .to_owned();
    let spec = directory.path().join("mid-call.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"worker_read": {"thread": "5", "syscall": "read", "fd": 40,
                                       "phase": "posthook"}}}"#,
    )
    .unwrap();
    let args = [
        "run",
        "--strict",
        "--happens-before",
        spec.to_str().unwrap(),
        "--",
        guest.as_str(),
        "exit-mid-read",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(stdout(&output), "main\n", "{log}");
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "{log}"
    );
    assert!(
        log.contains("anchor 'worker_read' on thread 5: read(fd=40)@Posthook#1 never fired"),
        "{log}"
    );
}

/// A posthook BEFORE anchor on a child wait whose child is held behind it
/// can never fire, and the run is refused by name within a bound
/// (https://github.com/rrnewton/hermit/issues/3929, through the child-wait
/// rule of https://github.com/rrnewton/hermit/issues/3904). The control, a
/// posthook wait gating the parent's own later write, completes: a wait for
/// a child that can still run is not refused.
#[test]
fn happens_before_posthook_child_wait_cycle_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_child_wait_cycle_guest().to_str().unwrap().to_owned();
    let spec = |name: &str, after: &str| {
        let path = directory.path().join(name);
        fs::write(
            &path,
            format!(
                r#"{{"version": 1,
                    "threads": {{"child": {{"spawn_ordinal": 1}}}},
                    "events": {{"parent_reaps": {{"thread": "3", "syscall": "wait4",
                                                 "phase": "posthook"}},
                               "gated": {{"thread": "{after}", "syscall": "write", "fd": 1}}}},
                    "edges": [{{"before": "parent_reaps", "after": "gated", "strength": "hard"}}]}}"#
            ),
        )
        .unwrap();
        path
    };
    let control = spec("control.json", "3");
    let args = [
        "run",
        "--strict",
        "--happens-before",
        control.to_str().unwrap(),
        "--",
        guest.as_str(),
    ];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert_eq!(stdout(&output), "child\nparent\n");

    // The review's three-process case: in "nested" mode the root waits for
    // the cycle's parent, an ordinary wait above the cycle that must not hide
    // it (the Codex re-check of the design).
    let nested = directory.path().join("nested.json");
    fs::write(
        &nested,
        r#"{"version": 1,
            "threads": {"parent": {"spawn_ordinal": 1}, "child": {"spawn_ordinal": 2}},
            "events": {"parent_reaps": {"thread": "parent", "syscall": "wait4",
                                        "phase": "posthook"},
                       "gated": {"thread": "child", "syscall": "write", "fd": 1}},
            "edges": [{"before": "parent_reaps", "after": "gated", "strength": "hard"}]}"#,
    )
    .unwrap();
    let cycle = spec("cycle.json", "child");
    for (spec, mode, parent) in [(&cycle, "plain", "3"), (&nested, "nested", "parent")] {
        let (status, log) = run_with_deadline(
            hermit_command(&[
                "run",
                "--strict",
                "--happens-before",
                spec.to_str().unwrap(),
                "--",
                guest.as_str(),
                mode,
            ]),
            directory.path(),
            Duration::from_secs(60),
            false,
        );
        let status = status.unwrap_or_else(|| {
            panic!("{mode}: a posthook child-wait cycle spun instead of being refused: no exit within 60s\n{log}")
        });
        assert_eq!(
            status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{mode}: {log}"
        );
        assert!(
            log.contains("HERMIT_HB_ANCHOR_NEVER_FIRED: happens-before BEFORE anchor(s) never fired")
                && log.contains(&format!(
                    "anchor 'parent_reaps' on thread {parent}: wait4@Posthook#1 never fired; it holds dtid"
                )),
            "{mode}: no refusal naming the posthook BEFORE anchor:\n{log}"
        );
    }
}

/// The design's stated limitation (https://github.com/rrnewton/hermit/issues/3929):
/// a posthook BEFORE anchor on a pipe read whose only writer is held behind
/// it can never fire, but the read is external input to the scheduler, so
/// the run is not refused and ends at `--timeout` (exit 124,
/// `HERMIT_RUN_TIMEOUT`). Pinned so that a later fix shows up as a change.
#[test]
fn happens_before_posthook_read_with_a_held_writer_ends_at_the_timeout() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_pipe_read_held_writer_guest()
        .to_str()
        .unwrap()
        .to_owned();
    let spec = directory.path().join("pipe.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "threads": {"child": {"spawn_ordinal": 1}},
            "events": {"parent_reads": {"thread": "3", "syscall": "read", "fd": 40,
                                        "phase": "posthook"},
                       "child_writes": {"thread": "child", "syscall": "write", "fd": 41}},
            "edges": [{"before": "parent_reads", "after": "child_writes", "strength": "hard"}]}"#,
    )
    .unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--log",
            "info",
            "run",
            "--strict",
            "--timeout",
            "5",
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(60),
        false,
    );
    let status = status.unwrap_or_else(|| panic!("--timeout 5 did not end the run in 60s\n{log}"));
    assert!(
        log.contains("SKIP dettid 5 held at happens-before anchor(s) [\"child_writes\"]"),
        "the writer was never held, so the test exercised nothing:\n{log}"
    );
    assert_eq!(status.code(), Some(124), "{log}");
    assert!(
        log.contains("HERMIT_RUN_TIMEOUT class=run-timeout"),
        "{log}"
    );
}

/// An alarm that fires at a thread held at a posthook gate leaves it held,
/// its request intact, and the run ends in the deadlock report, as for a
/// prehook gate (https://github.com/rrnewton/hermit/issues/3929). The gate is
/// the posthook of `alarm(1)` itself.
#[test]
fn happens_before_alarm_at_a_thread_held_at_a_posthook_gate_leaves_it_held() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_signal_while_held_guest().to_str().unwrap().to_owned();
    let spec = directory.path().join("posthook-gate.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"never": {"thread": "3", "syscalls": 1000000000},
                       "gate": {"thread": "3", "syscall": "alarm", "phase": "posthook"}},
            "edges": [{"before": "never", "after": "gate", "strength": "hard"}]}"#,
    )
    .unwrap();
    let args = [
        "--log",
        "info",
        "run",
        "--strict",
        "--happens-before",
        spec.to_str().unwrap(),
        "--",
        guest.as_str(),
        "alarm",
    ];
    let output = hermit(&args);
    let log = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "{log}"
    );
    assert!(
        log.contains("[dtid 3] held at a happens-before gate; leaving it held, its signal pending"),
        "the alarm never reached the held thread, so the test exercised nothing:\n{log}"
    );
    assert!(
        log.contains("held at a happens-before gate whose BEFORE anchor cannot fire")
            && log.contains("HappensBeforeSyscallCheckpoint"),
        "no deadlock report naming the posthook gate:\n{log}"
    );
    assert!(stdout(&output).is_empty());
}

/// A posthook anchor is refused at launch with `--replay-schedule-from`,
/// whose recorded schedule has no event for the gate's turn
/// (https://github.com/rrnewton/hermit/issues/3929). The refusal comes before
/// the schedule file is read, so a missing file shows it is this refusal; a
/// prehook spec on the same run gets past it.
#[test]
fn happens_before_posthook_anchor_with_replay_schedule_is_refused() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_spawn_echo_guest().to_str().unwrap().to_owned();
    let missing = directory.path().join("missing.schedule");
    for phase in ["posthook", "prehook"] {
        let spec = directory.path().join(format!("{phase}.json"));
        fs::write(
            &spec,
            format!(
                r#"{{"version": 1,
                    "events": {{"parent_writes": {{"thread": "3", "syscall": "write", "fd": 1,
                                                  "phase": "{phase}"}}}}}}"#
            ),
        )
        .unwrap();
        let args = [
            "run",
            "--strict",
            "--replay-schedule-from",
            missing.to_str().unwrap(),
            "--happens-before",
            spec.to_str().unwrap(),
            "--",
            guest.as_str(),
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        let refused = log.contains(
            "anchor 'parent_writes' (write(fd=1)@Posthook#1) fires at the posthook, which is \
             not supported with --replay-schedule-from",
        );
        assert!(!output.status.success(), "{phase}: {log}");
        if phase == "posthook" {
            assert!(refused, "{phase}: {log}");
            assert_eq!(
                output.status.code(),
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                "{log}"
            );
        } else {
            assert!(!refused, "{phase}: {log}");
        }
    }
}

/// A gate on a vfork child before its exec can never open: while the child
/// runs, no other thread can (https://github.com/rrnewton/hermit/issues/3930).
/// The child of `posix_spawn` calls dup2 before execve; a spec holding it there
/// must be refused by name with the policy-refusal status, not spin until an
/// outside timeout.
#[test]
fn happens_before_hold_in_a_vfork_child_is_refused_by_name() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = hb_spawn_dup2_guest().to_str().unwrap().to_owned();
    // The same hold through an absolute anchor and through a version-2
    // relative anchor (the child's first dup2 after its first syscall).
    for (name, json) in [
        (
            "vfork-hold.json",
            r#"{"version": 1,
            "threads": {"child": {"spawn_ordinal": 1}},
            "events": {"parent_writes": {"thread": "3", "syscall": "write", "fd": 1, "nth": 1},
                       "child_dup2": {"thread": "child", "syscall": "dup2", "fd": 5, "nth": 1}},
            "edges": [{"before": "parent_writes", "after": "child_dup2", "strength": "hard"}]}"#,
        ),
        (
            "vfork-hold-relative.json",
            r#"{"version": 2,
            "threads": {"child": {"spawn_ordinal": 1}},
            "events": {"parent_writes": {"thread": "3", "syscall": "write", "fd": 1, "nth": 1},
                       "child_first": {"thread": "child", "syscall": "rt_sigprocmask", "nth": 1},
                       "child_dup2": {"thread": "child", "syscall": "dup2", "fd": 5,
                                      "from": "child_first", "nth": 1}},
            "edges": [{"before": "parent_writes", "after": "child_dup2", "strength": "hard"}]}"#,
        ),
    ] {
        let spec = directory.path().join(name);
        fs::write(&spec, json).unwrap();
        let spec = spec.to_str().unwrap().to_owned();
        // A deadline, so a regression to the old spin fails with a message instead
        // of hanging the suite.
        let (status, log) = run_with_deadline(
            hermit_command(&[
                "run",
                "--strict",
                "--happens-before",
                spec.as_str(),
                "--",
                guest.as_str(),
            ]),
            directory.path(),
            Duration::from_secs(60),
            false,
        );
        let status = status.unwrap_or_else(|| {
        panic!(
            "a hold inside a vfork child spun instead of being refused: no exit within 60s\n{log}"
        )
    });
        assert_eq!(
            status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "a hold inside a vfork child must be refused with the policy-refusal status:\n{log}"
        );
        assert!(
            log.contains(
                "HERMIT_HB_HOLD_IN_VFORK_CHILD: happens-before anchor(s) [\"child_dup2\"]"
            ),
            "{name}: no refusal naming the anchor that would hold the vfork child:\n{log}"
        );
    }
}

/// A syscall-occurrence anchor on a launch that bypasses interception is
/// refused by name before the guest starts, under both spellings of the
/// option: `--namespace-only` runs the guest with no tracer and no scheduler,
/// so the anchor could not be enforced and the run used to pass silently.
#[test]
fn happens_before_fd_anchor_is_refused_without_interception() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = directory.path().join("echo-phantom.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"phantom": {"thread": "3", "syscall": "write", "fd": 77}}}"#,
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    for spelling in ["--namespace-only", "--lite"] {
        let args = [
            "run",
            spelling,
            "--happens-before",
            spec.as_str(),
            "--",
            "/bin/echo",
            "ran",
        ];
        let output = hermit(&args);
        let log = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{spelling}: an anchor that cannot be enforced must be refused:\n{log}"
        );
        assert!(
            log.contains(
                "anchor 'phantom' (write(fd=77)#1) is a syscall-occurrence anchor, but \
                 --namespace-only runs the guest without interception or a scheduler"
            ),
            "{spelling}: no refusal naming the anchor:\n{log}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).is_empty(),
            "{spelling}: the guest ran although the spec was refused"
        );
    }
}

/// A launch that refuses the spec must not first wait for input. With
/// `--verify`, Hermit reads stdin to its end to replay it in both runs; the
/// refusal of a syscall-occurrence anchor on another backend needs no input, so
/// with an undrained stdin pipe it must still come promptly, before any
/// backend is probed.
#[test]
fn happens_before_unsupported_fd_anchor_is_refused_before_reading_stdin() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = directory.path().join("kvm-phantom.json");
    fs::write(
        &spec,
        r#"{"version": 1,
            "events": {"phantom": {"thread": "3", "syscall": "write", "fd": 77}}}"#,
    )
    .unwrap();
    let spec = spec.to_str().unwrap().to_owned();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--backend=kvm",
            "run",
            "--strict",
            "--verify",
            "--happens-before",
            spec.as_str(),
            "--",
            "/bin/true",
        ]),
        directory.path(),
        Duration::from_secs(60),
        true,
    );
    let status = status.unwrap_or_else(|| {
        panic!("the refusal waited for stdin: no exit within 60s with an open stdin pipe\n{log}")
    });
    assert_eq!(
        status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "an unsupported anchor must be a policy refusal:\n{log}"
    );
    assert!(
        log.contains(
            "anchor 'phantom' (write(fd=77)#1) is a syscall-occurrence anchor, which is \
             enforced only on the ptrace backend; this run selected the Kvm backend"
        ),
        "no refusal naming the anchor:\n{log}"
    );
}

/// A happens-before spec the launch cannot use is a policy refusal (exit 122)
/// that names the problem, never a silent pass or a Hermit internal failure
/// (https://github.com/rrnewton/hermit/issues/3943):
/// - a count anchor under `--namespace-only`, which has no tracer or
///   scheduler to enforce it (it used to run the guest and exit 0);
/// - a spec that does not load, here `nth: 0` (it used to exit 125), on the
///   run path and on the `--hb-list-events` preview path.
///
/// The guest never runs in any case.
#[test]
fn happens_before_unusable_spec_is_a_named_policy_refusal() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let count_spec = directory.path().join("count.json");
    fs::write(
        &count_spec,
        r#"{"version": 1, "events": {"early": {"thread": "3", "syscalls": 5}}}"#,
    )
    .unwrap();
    let bad_spec = directory.path().join("nth0.json");
    fs::write(
        &bad_spec,
        r#"{"version": 1,
            "events": {"zeroth": {"thread": "3", "syscall": "write", "fd": 1, "nth": 0}}}"#,
    )
    .unwrap();
    let count_spec = count_spec.to_str().unwrap().to_owned();
    let bad_spec = bad_spec.to_str().unwrap().to_owned();
    for (args, expected) in [
        (
            vec![
                "run",
                "--namespace-only",
                "--happens-before",
                count_spec.as_str(),
                "--",
                "/bin/echo",
                "ran",
            ],
            "anchor 'early' (after 5 syscalls) is a count anchor, but --namespace-only runs the \
             guest without interception or a scheduler",
        ),
        (
            vec![
                "run",
                "--strict",
                "--happens-before",
                bad_spec.as_str(),
                "--",
                "/bin/echo",
                "ran",
            ],
            "event 'zeroth' sets 'nth' to 0; occurrences are counted from 1",
        ),
        (
            vec![
                "run",
                "--happens-before",
                bad_spec.as_str(),
                "--hb-list-events",
                "--",
                "/bin/echo",
                "ran",
            ],
            "event 'zeroth' sets 'nth' to 0; occurrences are counted from 1",
        ),
    ] {
        let output = hermit(&args);
        let log = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{args:?}: an unusable spec must be a policy refusal:\n{log}"
        );
        assert!(
            log.contains(expected),
            "{args:?}: no refusal naming the problem:\n{log}"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).is_empty(),
            "{args:?}: the guest ran although the spec was refused"
        );
    }
}

/// `--hb-list-events` prints the resolved spec and exits 0 without running
/// the guest; the anchors it cannot resolve are named on stderr.
#[test]
fn max_log_bytes_hb_list_events_exits_promptly_with_stderr_a_full_pipe() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let spec = unresolvable_happens_before_spec(directory.path());
    CappedRun {
        run_options: &["--happens-before", &spec, "--hb-list-events"],
        exit_code: 0,
        prints: Some("anchor(s) with unresolved code locations: A"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `--stacktrace-event=0` without a path prints the stack of the first
/// recorded event to stderr: a heading from the scheduler
/// (`try_pop_stacktrace_event` in `detcore/src/scheduler.rs`), then the stack
/// itself (`write_backtrace` in `detcore/src/tool_global.rs`), whose first line
/// names the thread. Event 0 is the guest's first, so both come before the cap
/// can cross.
#[test]
fn max_log_bytes_stacktrace_event_exits_promptly_when_stderr_is_a_full_pipe() {
    CappedRun {
        run_options: &["--record-preemptions", "--stacktrace-event=0"],
        prints: Some("Stack trace for thread"),
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// When a requested `--stacktrace-event` is a thread's exit, the scheduler
/// (`simulate_exit_posthook` in `detcore/src/scheduler.rs`) says on stderr that
/// no stack is available. Every event index up to 1000, each with a path so
/// that the other events' stacks go to a file rather than stderr, includes the
/// exit of `/bin/true`, which recorded 220 events when measured. That message
/// comes as the guest exits, so the run must end with its status, 0, under a
/// cap the run stays under (it logged 288788 bytes at debug).
#[test]
fn max_log_bytes_stacktrace_event_at_a_thread_exit_exits_promptly_when_stderr_is_a_full_pipe() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let stacks = directory.path().join("stack.json");
    let events: Vec<String> = (0..1000)
        .map(|index| format!("--stacktrace-event={index},{}", stacks.display()))
        .collect();
    let mut run_options = vec!["--record-preemptions"];
    run_options.extend(events.iter().map(String::as_str));
    CappedRun {
        run_options: &run_options,
        guest: &["/bin/true"],
        exit_code: 0,
        prints: Some("backtrace requested but not available post-exit"),
        max_log_bytes: "2M",
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// `--chaos --preemption-stacktrace` without `--preemption-stacktrace-log-file`
/// prints the guest's stack to stderr at every PMU timer preemption
/// (`handle_timer_event` in `detcore/src/lib.rs`), and the guest keeps running
/// after each one. A busy shell loop makes no system calls, so only the timer
/// preempts it. When measured, its first preemption came after 350852 bytes of
/// debug log, and each one logged about 3.2 KB more and took about 0.25 s, so a
/// 448K cap crossed after 34 of them, 8.7 s into the run.
///
/// The PMU timer is this case's subject: without one there is no preemption and
/// the premise fails. test.cli and test.cli_on_host skip it by exact name, and
/// privileged-test.pmu_cli_cases runs it, as for
/// [`run_chaos_preemption_replay_reuses_the_recorded_epoch`].
#[test]
fn max_log_bytes_preemption_stacktrace_exits_promptly_when_stderr_is_a_full_pipe() {
    CappedRun {
        run_options: &["--chaos", "--preemption-stacktrace"],
        guest: &["/bin/sh", "-c", "while :; do :; done"],
        prints: Some("preempted at thread time"),
        max_log_bytes: "448K",
        ..CappedRun::default()
    }
    .assert_exits_promptly_with_full_unread_stderr(|_| {});
}

/// Without `--log-file`, hermit writes its virtual-time epoch line to stderr
/// (`GlobalOpts::write_controller_diagnostic`) before the guest starts. The
/// default log level writes nothing else there for `/bin/true`, so the run
/// must end with the guest's exit status, 0.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_stderr_is_a_full_pipe() {
    let _lock = hermit_run_guard();
    let args = [
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
        "/bin/true",
    ];
    let (status, elapsed, stderr) = stderr_when_read(hermit_command(&args).stdin(Stdio::null()));
    assert!(
        stderr.contains("hermit: virtual-time epoch="),
        "the premise does not hold: with stderr read, the run ({status:?} after {elapsed:?}) \
         printed no epoch line; stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "with stderr read: {status:?} after {elapsed:?}; stderr:\n{stderr}"
    );
    let status = exit_status_with_full_unread_stderr(hermit_command(&args).stdin(Stdio::null()));
    assert_eq!(status.code(), Some(0), "{status:?}");
}

/// Without `--log-file`, the log itself goes to stderr through
/// `CappedWriter::stderr` in `hermit/tracing.rs`, and at debug the noisy guest
/// fills a pipe long before 64K is logged. A sink that waited for the reader
/// would never let the cap cross; one that drops what the pipe refuses still
/// counts it, so the cap ends the run with 123.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_the_log_fills_the_stderr_pipe() {
    let _lock = hermit_run_guard();
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let (status, elapsed, stderr) = stderr_when_read(hermit_command(&args).stdin(Stdio::null()));
    assert!(
        stderr.contains(" DEBUG "),
        "the premise does not hold: with stderr read, the run ({status:?} after {elapsed:?}) \
         logged no debug line there; stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_LOG_CAP_EXIT),
        "with stderr read: {status:?} after {elapsed:?}; stderr:\n{stderr}"
    );
    let status = exit_status_with_full_unread_stderr(hermit_command(&args).stdin(Stdio::null()));
    assert_eq!(status.code(), Some(HERMIT_LOG_CAP_EXIT), "{status:?}");
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_the_log_fills_the_stderr_pipe`]
/// where `statx` fails, as under a seccomp policy that denies it. The stderr
/// log sink learns from `statx` whether fd 2 is a pipe, a socket or a regular
/// file. When `statx` failed it used to fall back to a plain `write(2)`, which
/// waits forever on a full pipe, and this run was killed at the 45 s bound.
/// It now asks the pipe itself (`fcntl(F_GETPIPE_SZ)`), learns that fd 2 is a
/// pipe, and writes through a non-blocking open of it, which the full pipe
/// refuses; the dropped bytes still count against the cap, so the run ends
/// with 123.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_statx_is_denied_and_the_log_fills_the_stderr_pipe()
 {
    let _lock = hermit_run_guard();
    // Premise: hermit runs, and logs to stderr, without statx (the Rust
    // standard library falls back to fstatat). Uncapped, so the log takes the
    // plain write it always took.
    let mut premise = hermit_command_under_host_filter(&[
        "--log=debug",
        "run",
        "--timeout",
        "120",
        "--",
        "/bin/true",
    ]);
    premise.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_syscall(&mut premise, libc::SYS_statx);
    let (status, elapsed, stderr) = stderr_when_read(&mut premise);
    assert!(
        stderr.contains(" DEBUG "),
        "the premise does not hold: without statx and with stderr read, the uncapped run \
         ({status:?} after {elapsed:?}) logged no debug line there; stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "without statx and with stderr read: {status:?} after {elapsed:?}; stderr:\n{stderr}"
    );
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut command = hermit_command_under_host_filter(&args);
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_syscall(&mut command, libc::SYS_statx);
    let status = exit_status_with_full_unread_stderr(&mut command);
    assert_eq!(status.code(), Some(HERMIT_LOG_CAP_EXIT), "{status:?}");
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_statx_is_denied_and_the_log_fills_the_stderr_pipe`]
/// with stderr read. When `statx` failed, the capped stderr writers wrote
/// nothing at all, so under such a policy a capped run lost every debug line
/// and the cap's own class line, and this test found neither. They now learn
/// that fd 2 is a pipe from `fcntl(F_GETPIPE_SZ)`, and both arrive.
#[test]
fn max_log_bytes_without_a_log_file_still_logs_to_a_read_stderr_when_statx_is_denied() {
    let _lock = hermit_run_guard();
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut command = hermit_command_under_host_filter(&args);
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_syscall(&mut command, libc::SYS_statx);
    let (status, elapsed, stderr) = stderr_when_read(&mut command);
    assert!(
        stderr.contains(" DEBUG "),
        "without statx and with stderr read, the capped run ({status:?} after {elapsed:?}) \
         logged no debug line there; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("HERMIT_LOG_CAP class=log-cap"),
        "without statx and with stderr read, the capped run ({status:?} after {elapsed:?}) \
         printed no class line; stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_LOG_CAP_EXIT),
        "without statx and with stderr read: {status:?} after {elapsed:?}; stderr:\n{stderr}"
    );
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_statx_is_denied_and_the_log_fills_the_stderr_pipe`]
/// where every other question about stderr's file type fails too (see
/// [`deny_file_type_queries_on_stderr`]), so nothing can tell what kind of
/// file stderr is. The capped stderr writers must then write nothing,
/// since a plain `write(2)` to the full pipe would wait forever; the dropped
/// bytes still count against the cap, so the run ends with 123. A writer that
/// fell back to a plain write here would be killed at the 45 s bound.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_no_file_type_query_answers_and_the_log_fills_the_stderr_pipe()
 {
    capped_run_exits_promptly_on_a_full_pipe_with_the_file_type_queries_on_stderr_denied(
        libc::EPERM,
    );
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_no_file_type_query_answers_and_the_log_fills_the_stderr_pipe`]
/// where every file-type query on stderr returns 0 and fills in nothing. The
/// stderr log sink used to read the zeroed type as "anything else", which
/// gets a plain `write(2)`, so this run was killed at the 45 s bound. A query
/// that names no type now counts as failed.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_every_file_type_query_feigns_success_and_the_log_fills_the_stderr_pipe()
 {
    capped_run_exits_promptly_on_a_full_pipe_with_the_file_type_queries_on_stderr_denied(0);
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_no_file_type_query_answers_and_the_log_fills_the_stderr_pipe`]
/// where every file-type query on stderr answers `EINTR`, every time. The
/// stderr writer then has no type and returns `statx`'s `EINTR`, and the
/// retrying stderr sink used to retry an interrupted write without limit, so
/// this run was killed at the 45 s bound. Under the cap it now retries only
/// within the shared stderr deadline and then gives the line up; the dropped
/// bytes still count against the cap, so the run ends with 123.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_every_file_type_query_answers_eintr_and_the_log_fills_the_stderr_pipe()
 {
    capped_run_exits_promptly_on_a_full_pipe_with_the_file_type_queries_on_stderr_denied(
        libc::EINTR,
    );
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_statx_is_denied_and_the_log_fills_the_stderr_pipe`]
/// where `fstat` on stderr never returns, as when stderr is a FIFO on a FUSE
/// file system whose server has stopped answering (see
/// [`deny_statx_and_hold_the_stat_of_stderr`]). When `statx` failed, the
/// capped stderr writers used to ask `fstat` and wait there forever, the
/// crossing writer before `_exit(123)` included, so this run was killed at
/// the 45 s bound. They now ask the pipe itself (`fcntl(F_GETPIPE_SZ)`), which
/// answers from memory.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_the_stat_of_stderr_never_returns_and_the_log_fills_the_stderr_pipe()
 {
    let _lock = hermit_run_guard();
    // Premise: uncapped, nothing hermit or the guest does stats stderr, so
    // the run neither waits on the held calls nor needs them, and it logs to
    // stderr.
    let mut premise = hermit_command_under_host_filter(&[
        "--log=debug",
        "run",
        "--timeout",
        "120",
        "--",
        "/bin/true",
    ]);
    premise.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_statx_and_hold_the_stat_of_stderr(&mut premise);
    let (status, elapsed, stderr) = stderr_when_read(&mut premise);
    assert!(
        stderr.contains(" DEBUG "),
        "the premise does not hold: with the stat of stderr held and stderr read, the \
         uncapped run ({status:?} after {elapsed:?}) logged no debug line there; \
         stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "with the stat of stderr held and stderr read: {status:?} after {elapsed:?}; \
         stderr:\n{stderr}"
    );
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut command = hermit_command_under_host_filter(&args);
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_statx_and_hold_the_stat_of_stderr(&mut command);
    let status = exit_status_with_full_unread_stderr(&mut command);
    assert_eq!(status.code(), Some(HERMIT_LOG_CAP_EXIT), "{status:?}");
}

/// A capped run at debug, with every file-type query on stderr answered with
/// `errno` and stderr a full pipe that nobody reads, ends with 123; uncapped,
/// with stderr read, it logs and exits 0.
fn capped_run_exits_promptly_on_a_full_pipe_with_the_file_type_queries_on_stderr_denied(
    errno: i32,
) {
    let _lock = hermit_run_guard();
    // Premise: hermit runs, and logs to stderr, when no file-type query on
    // stderr answers. Uncapped, so the log takes the plain write it always
    // took.
    let mut premise = hermit_command_under_host_filter(&[
        "--log=debug",
        "run",
        "--timeout",
        "120",
        "--",
        "/bin/true",
    ]);
    premise.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_file_type_queries_on_stderr(&mut premise, errno);
    let (status, elapsed, stderr) = stderr_when_read(&mut premise);
    assert!(
        stderr.contains(" DEBUG "),
        "the premise does not hold: with no file-type query on stderr answering and stderr \
         read, the uncapped run ({status:?} after {elapsed:?}) logged no debug line there; \
         stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "with no file-type query on stderr answering and stderr read: {status:?} after \
         {elapsed:?}; stderr:\n{stderr}"
    );
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut command = hermit_command_under_host_filter(&args);
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_file_type_queries_on_stderr(&mut command, errno);
    let status = exit_status_with_full_unread_stderr(&mut command);
    assert_eq!(status.code(), Some(HERMIT_LOG_CAP_EXIT), "{status:?}");
}

/// A pseudo-terminal whose master end nobody reads, filled until its slave end
/// refuses more. Returns the master, which must stay open while the slave is
/// in use, and a BLOCKING descriptor for the slave: the filling goes through a
/// second, non-blocking open of the slave, so the descriptor returned for
/// hermit's stderr does not share its `O_NONBLOCK`.
fn full_unread_pseudo_terminal() -> (fs::File, fs::File) {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: posix_openpt returns a new descriptor or -1.
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    assert!(
        master >= 0,
        "posix_openpt: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: `master` is a new descriptor that nothing else owns.
    let master = unsafe { fs::File::from_raw_fd(master) };
    let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
    // SAFETY: grantpt and unlockpt only act on `fd`; ptsname_r writes at most
    // `name.len()` bytes, NUL included, into `name`.
    let mut name = [0 as libc::c_char; 128];
    unsafe {
        assert_eq!(
            libc::grantpt(fd),
            0,
            "grantpt: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::unlockpt(fd),
            0,
            "unlockpt: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            libc::ptsname_r(fd, name.as_mut_ptr(), name.len()),
            0,
            "ptsname_r"
        );
    }
    // SAFETY: ptsname_r succeeded, so `name` is NUL-terminated.
    let slave_path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    let open_slave = |flags| {
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOCTTY | flags)
            .open(&slave_path)
            .unwrap_or_else(|err| panic!("open {slave_path}: {err}"))
    };
    let slave = open_slave(0);
    let mut filler = open_slave(libc::O_NONBLOCK);
    let chunk = [b'x'; 4096];
    let mut filled = 0usize;
    loop {
        match filler.write(&chunk) {
            Ok(n) => filled += n,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(err) => panic!("filling {slave_path} after {filled} bytes: {err}"),
        }
        assert!(
            filled < 64 << 20,
            "{slave_path} took {filled} bytes and never filled"
        );
    }
    assert!(filled > 0, "{slave_path} took no byte");
    (master, slave)
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_the_log_fills_the_stderr_pipe`]
/// with stderr a terminal that nobody reads and that is full, under a seccomp
/// policy that denies the terminal query `isatty` makes with `EPERM`. The
/// stderr log sink used to write plainly to any character device the terminal
/// query did not call a terminal, so here it made a plain `write(2)` to the
/// full terminal, which waits for a reader forever, and this run was killed at
/// the 45 s bound. It now writes to every character device through a
/// non-blocking open of it, and the run ends with 123.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_the_terminal_query_is_denied_and_the_log_fills_a_terminal()
 {
    capped_run_exits_promptly_on_a_full_terminal_with_the_terminal_query_denied(libc::EPERM);
}

/// The run of
/// [`max_log_bytes_without_a_log_file_exits_promptly_when_the_terminal_query_is_denied_and_the_log_fills_a_terminal`]
/// where the policy answers the terminal query with `ENOTTY`, exactly what a
/// descriptor that really is not a terminal answers. The stderr log sink used
/// to trust `ENOTTY` and write plainly, so this run was killed at the 45 s
/// bound.
#[test]
fn max_log_bytes_without_a_log_file_exits_promptly_when_the_terminal_query_answers_enotty_and_the_log_fills_a_terminal()
 {
    capped_run_exits_promptly_on_a_full_terminal_with_the_terminal_query_denied(libc::ENOTTY);
}

/// A capped run at debug, with the terminal query denied with `errno` and
/// stderr a full terminal that nobody reads, ends with 123 within
/// [`LOG_CAP_RUN_WAIT_BOUND`]; uncapped, with stderr read, it logs and exits 0.
fn capped_run_exits_promptly_on_a_full_terminal_with_the_terminal_query_denied(errno: i32) {
    let _lock = hermit_run_guard();
    // Premise: hermit runs, and logs to stderr, when the terminal query is
    // denied. Uncapped, so the log takes the plain write it always took.
    let mut premise = hermit_command_under_host_filter(&[
        "--log=debug",
        "run",
        "--timeout",
        "120",
        "--",
        "/bin/true",
    ]);
    premise.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_terminal_query(&mut premise, errno);
    let (status, elapsed, stderr) = stderr_when_read(&mut premise);
    assert!(
        stderr.contains(" DEBUG "),
        "the premise does not hold: with the terminal query denied and stderr read, the \
         uncapped run ({status:?} after {elapsed:?}) logged no debug line there; \
         stderr:\n{stderr}"
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "with the terminal query denied and stderr read: {status:?} after {elapsed:?}; \
         stderr:\n{stderr}"
    );
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut command = hermit_command_under_host_filter(&args);
    command.stdin(Stdio::null()).env("LC_ALL", "C");
    deny_terminal_query(&mut command, errno);
    let (master, slave) = full_unread_pseudo_terminal();
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::from(slave))
        .spawn()
        .unwrap();
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let (status, elapsed) = wait_at_most(&mut child, bound);
    // hermit is reaped, killed first if it outlived the bound.
    drop(master);
    eprintln!("capped run, stderr a full unread terminal: {status:?} after {elapsed:?}");
    let status = status.unwrap_or_else(|| {
        panic!(
            "hermit was still running after {elapsed:?} (bound {bound:?}) and was killed: \
             a diagnostic waited on the full terminal"
        )
    });
    assert_eq!(status.code(), Some(HERMIT_LOG_CAP_EXIT), "{status:?}");
}

/// Spawn `command`, whose stderr is already set, and wait at most
/// [`LOG_CAP_RUN_WAIT_BOUND`] for something to listen on TCP `port`; then kill
/// and reap hermit. Returns whether it listened, the time waited and, when
/// stderr is [`Stdio::piped`], everything hermit wrote there, which a thread
/// reads throughout. Fails if hermit exits first.
fn gdbserver_listens_promptly(command: &mut Command, port: u16) -> (bool, Duration, String) {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let reader = child.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
            bytes
        })
    });
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let start = Instant::now();
    let (listened, exited) = loop {
        if tcp_port_is_listening(port) {
            break (true, None);
        }
        if let Some(status) = child.try_wait().unwrap() {
            break (false, Some(status));
        }
        if start.elapsed() >= bound {
            break (false, None);
        }
        thread::sleep(Duration::from_millis(20));
    };
    let elapsed = start.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    let stderr = reader
        .map(|reader| String::from_utf8_lossy(&reader.join().unwrap()).into_owned())
        .unwrap_or_default();
    if let Some(status) = exited {
        panic!(
            "hermit run --gdbserver exited {status:?} before it listened on port {port}; \
             stderr:\n{stderr}"
        );
    }
    (listened, elapsed, stderr)
}

/// `--gdbserver` with the default local network warns on stderr that it
/// switches to host networking, while the outer process prepares the run.
/// The gdbserver then waits for a client and the cap is not reached, so the
/// test requires that it listens on its port within
/// [`LOG_CAP_RUN_WAIT_BOUND`], and kills it.
#[test]
fn max_log_bytes_gdbserver_listens_promptly_when_stderr_is_a_full_pipe() {
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log = directory.path().join("hermit.log");
    let command = |port: &str| {
        hermit_command(&[
            "--log=debug",
            "--max-log-bytes=64K",
            "--log-file",
            log.to_str().unwrap(),
            "run",
            "--timeout",
            "120",
            "--gdbserver",
            "--gdbserver-port",
            port,
            "--",
            "/bin/true",
        ])
    };

    // A pipe that is read, not a file: under the cap a diagnostic to a
    // regular file on btrfs or tmpfs is omitted (see
    // `hermit::nonwaiting_write::write_without_waiting`).
    let port = unused_local_port();
    let (listened, elapsed, stderr) =
        gdbserver_listens_promptly(command(&port.to_string()).stderr(Stdio::piped()), port);
    assert!(
        listened && stderr.contains("WARNING: --gdbserver requires host networking"),
        "the premise does not hold: with stderr read, listened={listened} after {elapsed:?}; \
         stderr:\n{stderr}"
    );

    let (reader, writer) = full_unread_stderr_pipe();
    let port = unused_local_port();
    let (listened, elapsed, _) =
        gdbserver_listens_promptly(command(&port.to_string()).stderr(writer), port);
    drop(reader);
    eprintln!(
        "capped gdbserver run, stderr a full unread pipe: listened={listened} after {elapsed:?}"
    );
    assert!(
        listened,
        "hermit run --gdbserver did not listen on port {port} within {elapsed:?} and was \
         killed: a diagnostic waited on the full stderr pipe"
    );
}

/// A capped run whose stderr pipe has no reader still exits exactly 123.
///
/// Round 2 of the review of https://github.com/rrnewton/hermit/pull/3686
/// (finding 4) ran this under `--no-namespace`, where the crossing process was
/// the Reverie tracer with SIGPIPE at its default disposition: its crossing
/// line to the reader-less pipe killed it, and the run exited 125. The cap is
/// refused with `--no-namespace` now (round-4c review, finding 1), so the test
/// runs in hermit's PID namespace. There the crossing process is the container
/// init, PID 1 of the namespace, and the kernel discards a signal at its
/// default disposition that the init raises in itself, so this test does not
/// detect a revert of that fix. The fix keeps its unit coverage in tracing.rs
/// (`a_cap_diagnostic_to_a_departed_reader_raises_no_signal` and
/// `the_diagnostic_signal_guard_consumes_only_the_signal_its_write_raised`).
/// This test still requires that nothing the outer process writes to the
/// reader-less stderr turns the cap's 123 into a panic (101) or a hang. Like
/// the test above, it waits at most [`LOG_CAP_RUN_WAIT_BOUND`], so that a hang
/// fails here, with the time waited, before nextest's own kill.
#[test]
fn max_log_bytes_keeps_the_cap_status_when_stderr_has_no_reader() {
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log = directory.path().join("hermit.log");
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: both descriptors were just created and are owned only here.
    let (reader, writer) = unsafe {
        use std::os::fd::FromRawFd;
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    };
    drop(reader);
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=64K",
        "--log-file",
        log.to_str().unwrap(),
        "run",
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let mut child = hermit_command(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer))
        .spawn()
        .unwrap();
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let (status, elapsed) = wait_at_most(&mut child, bound);
    eprintln!("capped run, stderr without a reader: {status:?} after {elapsed:?}");
    let status = status.unwrap_or_else(|| {
        panic!("hermit was still running after {elapsed:?} (bound {bound:?}) and was killed")
    });
    assert_eq!(
        status.code(),
        Some(HERMIT_LOG_CAP_EXIT),
        "{status:?}; the cap must end the run with 123 when stderr has no reader. Log \
         tail:\n{}",
        fs::read_to_string(&log)
            .map(|text| {
                text.get(text.len().saturating_sub(1500)..)
                    .unwrap_or(&text)
                    .to_string()
            })
            .unwrap_or_default()
    );
}

/// What a test below does to hermit's stderr once `run --verify` has announced
/// run 1, before that run crosses the cap.
#[derive(Clone, Copy, Debug)]
enum StderrAfterRun1 {
    /// Close the pipe's only read end. A blocking write then fails with EPIPE,
    /// which `eprintln!` turns into a panic and exit 101.
    LosesItsReader,
    /// Fill the pipe and never read it again. A blocking write then waits
    /// forever.
    FillsUp,
}

/// Round-3 review of https://github.com/rrnewton/hermit/pull/3686, finding 4.
/// After the cap ends run 1, `run --verify --keep-logs` keeps run 1's log and
/// reports that on stderr. The report must neither turn the cap's 123 into 101
/// nor wait on a stderr that nobody reads.
///
/// The run logs at debug level into run 1's per-run log, so stderr carries
/// nothing between the `Run1...` notice and the crossing. A 16M cap puts the
/// crossing seconds after that notice (a 4M cap crossed 2.0 s after spawn at a
/// load average of about 220), and the test changes stderr in between. It then
/// waits at most [`LOG_CAP_RUN_WAIT_BOUND`] from spawn, as the tests above do.
///
/// hermit's exit bound for a capped run (`bound_log_cap_exit` in
/// `tracing.rs`: a panic hook and a 750 ms timer, both exiting 123) also
/// covers this report, so this test passes whenever that bound works. The unit
/// tests `log_cap_retention_report_*` in `verify.rs` test the report without
/// it.
fn capped_verify_keeps_the_cap_status(after_run1: StderrAfterRun1) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: both descriptors were just created and are owned only here.
    let (reader, writer) = unsafe {
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
    };
    // Only this test reads the pipe, so this flag on the read end's open file
    // description changes nothing that hermit uses.
    assert_eq!(
        unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    let mut args = vec![
        "--log=debug",
        "--max-log-bytes=16M",
        "run",
        "--verify",
        "--keep-logs",
        "--verify-log-dir",
        directory.path().to_str().unwrap(),
        "--timeout",
        "120",
        "--",
    ];
    args.extend(LOG_CAP_NOISY_GUEST);
    let start = Instant::now();
    let mut child = hermit_command(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(writer))
        .spawn()
        .unwrap();
    let bound = LOG_CAP_RUN_WAIT_BOUND.mul_f64(dap_wall_timeout_multiplier());
    let mut reader = fs::File::from(reader);
    let run1_announced = |seen: &[u8]| {
        String::from_utf8_lossy(seen)
            .split_once("Run1...")
            .is_some_and(|(_, after)| after.contains('\n'))
    };
    let mut seen = Vec::new();
    while !run1_announced(&seen) {
        let mut chunk = [0; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => seen.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if child.try_wait().unwrap().is_some() || start.elapsed() >= bound {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("reading hermit's stderr: {error}"),
        }
    }
    let seen = String::from_utf8_lossy(&seen).into_owned();
    // The test shows something only if stderr changes before the crossing.
    assert!(
        run1_announced(seen.as_bytes())
            && !seen.contains("HERMIT_LOG_CAP")
            && child.try_wait().unwrap().is_none(),
        "{:?} after spawn hermit had not announced run 1, or had already crossed the cap \
         or exited; its stderr so far:\n{seen}",
        start.elapsed()
    );
    let reader = match after_run1 {
        StderrAfterRun1::LosesItsReader => {
            drop(reader);
            None
        }
        StderrAfterRun1::FillsUp => {
            // A second, non-blocking open file description for the same pipe,
            // so that hermit's own stays blocking.
            let mut filler = fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(format!("/proc/self/fd/{}", reader.as_raw_fd()))
                .unwrap();
            for chunk in [&[b'x'; 4096][..], &[b'x'; 1][..]] {
                loop {
                    match filler.write(chunk) {
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        Err(error) => panic!("filling hermit's stderr: {error}"),
                    }
                }
            }
            Some(reader)
        }
    };
    let (status, _) = wait_at_most(&mut child, bound.saturating_sub(start.elapsed()));
    let elapsed = start.elapsed();
    eprintln!(
        "capped run --verify, stderr {after_run1:?} after the Run1 notice: {status:?} \
         {elapsed:?} after spawn"
    );
    if let Some(mut reader) = reader {
        // What hermit wrote before the pipe filled is still at its head.
        let mut rest = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = reader.read(&mut chunk) {
            rest.extend_from_slice(&chunk[..read]);
        }
        let rest = String::from_utf8_lossy(&rest);
        assert!(
            !rest.contains("HERMIT_LOG_CAP") && !rest.contains("Verification logs retained"),
            "hermit reported the cap before the pipe was full, so this run shows nothing \
             about a full pipe:\n{}",
            rest.trim_end_matches('x')
        );
    }
    let status = status.unwrap_or_else(|| {
        panic!(
            "hermit was still running {elapsed:?} after spawn (bound {bound:?}) and was \
             killed: a report after the crossing waited on the stderr that {after_run1:?}"
        )
    });
    assert_eq!(
        status.code(),
        Some(HERMIT_LOG_CAP_EXIT),
        "{status:?}; 101 is a panic in a report after the crossing"
    );
    let kept: Vec<String> = fs::read_dir(directory.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("run1_log_"))
        .collect();
    assert_eq!(kept.len(), 1, "run 1's log was not kept: {kept:?}");
}

#[test]
fn max_log_bytes_verify_keeps_the_cap_status_when_stderr_loses_its_reader() {
    capped_verify_keeps_the_cap_status(StderrAfterRun1::LosesItsReader);
}

#[test]
fn max_log_bytes_verify_exits_promptly_when_stderr_fills_after_run1() {
    capped_verify_keeps_the_cap_status(StderrAfterRun1::FillsUp);
}

/// Where the cap could end hermit and leave the guest running, hermit refuses
/// the flag instead, with 122 (round-2 review of
/// https://github.com/rrnewton/hermit/pull/3686, finding 2). LiteInst, which
/// runs only in-guest, under `--no-namespace` spawns a guest that is not traced
/// and that no PID namespace contains, DBT's guest is a plain child of the outer process in
/// every namespace mode, and KVM's host processes under `--no-namespace` are not
/// shown to die with hermit. SaBRe under `--no-namespace` hands its guest to the
/// supervisor worker stopped and untraced, before `PTRACE_O_EXITKILL` binds it
/// (round-3 review of the same pull request, finding 2). The refusal comes
/// before backend availability and before any SaBRe artifact is resolved, so it
/// holds in builds where none of these backends is available. `analyze` and `bisect` trials skip
/// `run`'s own check, so both refuse `--no-namespace` before any trial, with
/// KVM and with the default ptrace backend, and bisect before it reads its
/// schedules (the paths below do not exist). ptrace and e9patch under
/// `--no-namespace` are refused too (round-4c review, finding 1); their cases,
/// with the guest's marker and the run without the flag, are in
/// `max_log_bytes_is_refused_under_no_namespace_on_the_ptrace_runtime`.
#[test]
fn max_log_bytes_is_refused_where_the_guest_could_outlive_hermit() {
    let _lock = hermit_run_guard();
    /// Arguments and the configuration the refusal must name.
    type Case<'a> = (&'a [&'a str], &'a str);
    let cases: [Case; 9] = [
        (
            &[
                "--max-log-bytes=64K",
                "--backend=liteinst",
                "run",
                "--no-namespace",
                "--max-timeslice=disabled",
                "--",
                "/bin/true",
            ],
            "--backend=liteinst and --no-namespace",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "--backend=dbt",
                "run",
                "--",
                "/bin/true",
            ],
            "--backend=dbt",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "--backend=kvm",
                "run",
                "--no-namespace",
                "--",
                "/bin/true",
            ],
            "--backend=kvm and --no-namespace",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "--backend=sabre",
                "run",
                "--no-namespace",
                "--",
                "/bin/true",
            ],
            "--backend=sabre and --no-namespace",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "--backend=kvm",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ],
            "--backend=kvm and --no-namespace",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "--backend=kvm",
                "bisect",
                "--good=/nonexistent/hermit-3686/good.json",
                "--bad=/nonexistent/hermit-3686/bad.json",
                "--",
                "--no-namespace",
                "/bin/true",
            ],
            "--backend=kvm and --no-namespace",
        ),
        // Round-4c review, finding 1: the ptrace tracer sets PTRACE_O_EXITKILL
        // only after it spawns the guest, and starts any GDB server in between.
        // The refusal comes before any GDB server is started, so nothing waits
        // for a client here.
        (
            &[
                "--max-log-bytes=64K",
                "run",
                "--no-namespace",
                "--gdbserver",
                "--",
                "/bin/true",
            ],
            "--no-namespace",
        ),
        // The default (ptrace) backend's trials under `--no-namespace`, the
        // same window as `run --no-namespace`.
        (
            &[
                "--max-log-bytes=64K",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ],
            "--no-namespace",
        ),
        (
            &[
                "--max-log-bytes=64K",
                "bisect",
                "--good=/nonexistent/hermit-3686/good.json",
                "--bad=/nonexistent/hermit-3686/bad.json",
                "--",
                "--no-namespace",
                "/bin/true",
            ],
            "--no-namespace",
        ),
    ];
    for (args, named) in cases {
        let output = hermit_command(args).stdin(Stdio::null()).output().unwrap();
        let text = stderr(&output);
        assert_eq!(
            output.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{args:?}: {output:?}"
        );
        assert!(
            text.contains(&format!(
                "--max-log-bytes cannot be enforced with {named}: "
            )),
            "{args:?}: {text}"
        );
        assert!(
            text.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
            "{args:?}: {text}"
        );
    }
}

/// Round-4c review of https://github.com/rrnewton/hermit/pull/3686, finding 1.
/// Reverie's ptrace tracer clones the guest and sets `PTRACE_O_EXITKILL` on it
/// only afterwards, and Linux clears the inherited parent-death signal in the
/// clone, so under `--no-namespace` the guest is bound to nothing in between.
/// A charged write can cross the cap there (the outer process's asynchronous
/// file appender draining a queued record), and exiting 123 would leave the
/// guest running. So ptrace, and e9patch, whose guest runs on the ptrace
/// runtime, refuse the flag under `--no-namespace` with 122, with or without
/// `--log-file`, before the guest exists: the marker it would print is absent.
/// Without the flag the same command is not refused: ptrace runs the guest,
/// which prints its marker, and e9patch fails as it always has under
/// `--no-namespace`, without naming the flag.
#[test]
fn max_log_bytes_is_refused_under_no_namespace_on_the_ptrace_runtime() {
    let _lock = hermit_run_guard();
    const MARKER: &str = "hermit-3686-guest-ran";
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let log = directory.path().join("hermit.log");
    let log = log.to_str().unwrap();
    let guest = ["--", "/bin/sh", "-c", "echo hermit-3686-guest-ran"];
    for backend in [None, Some("--backend=e9patch")] {
        for log_file in [None, Some(log)] {
            let mut without_flag: Vec<&str> = Vec::new();
            without_flag.extend(backend);
            if let Some(path) = log_file {
                without_flag.extend(["--log-file", path]);
            }
            without_flag.extend(["run", "--no-namespace"]);
            without_flag.extend(guest);
            let mut capped = vec!["--max-log-bytes=64K"];
            capped.extend(&without_flag);

            let output = hermit_command(&capped)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let (out, err) = (stdout(&output), stderr(&output));
            assert_eq!(
                output.status.code(),
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                "{capped:?}: {output:?}"
            );
            assert!(
                err.contains("--max-log-bytes cannot be enforced with --no-namespace: "),
                "{capped:?}: {err}"
            );
            assert!(
                err.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
                "{capped:?}: {err}"
            );
            assert!(
                !out.contains(MARKER) && !err.contains(MARKER),
                "{capped:?}: the guest ran: {out} {err}"
            );

            let output = hermit_command(&without_flag)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let (out, err) = (stdout(&output), stderr(&output));
            assert_ne!(
                output.status.code(),
                Some(HERMIT_POLICY_REFUSAL_EXIT),
                "{without_flag:?}: {output:?}"
            );
            assert!(!err.contains("--max-log-bytes"), "{without_flag:?}: {err}");
            if backend.is_none() {
                assert_eq!(
                    output.status.code(),
                    Some(0),
                    "{without_flag:?}: {output:?}"
                );
                assert!(out.contains(MARKER), "{without_flag:?}: {out} {err}");
            }
        }
    }
}

/// `hermit --backend=sabre strace` does not go through `run`. It launches the
/// SaBRe runner as a plain child of hermit, which no PID namespace,
/// parent-death signal or ptrace attachment binds to hermit, and the trace
/// reaches the inherited stderr without passing through hermit's charged
/// writers. The cap could neither count that trace nor stop the guest, so
/// hermit refuses the flag there with 122 (round-3 review of
/// https://github.com/rrnewton/hermit/pull/3686, finding 6). The refusal comes
/// before any SaBRe artifact is resolved: the HERMIT_SABRE_* variables are
/// removed below, and resolving them first would fail with "the sabre backend
/// needs HERMIT_SABRE_RUNNER" instead, in builds with and without SaBRe.
///
/// strace on any other backend is not refused. The M1 strace command runs only
/// on SaBRe, so there it fails exactly as it does without the flag, by asking
/// for `--backend sabre`, with the same status and the same stderr.
#[test]
fn max_log_bytes_is_refused_for_sabre_strace_and_leaves_other_strace_alone() {
    let strace = |args: &[&str]| {
        hermit_command(args)
            .env_remove("HERMIT_LOG")
            .env_remove("HERMIT_LOG_FILE")
            .env_remove("HERMIT_SABRE_RUNNER")
            .env_remove("HERMIT_SABRE_BINARY")
            .env_remove("HERMIT_SABRE_PLUGIN")
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
    };
    let args = [
        "--max-log-bytes=1M",
        "--backend=sabre",
        "strace",
        "/bin/true",
    ];
    let refused = strace(&args);
    let text = stderr(&refused);
    assert_eq!(
        refused.status.code(),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "{args:?}: {refused:?}"
    );
    assert!(
        text.contains(
            "--max-log-bytes cannot be enforced with strace --backend=sabre: the SaBRe \
             runner's trace output does not pass through hermit's charged writers, and the \
             runner is a plain child of hermit with no PID namespace, parent-death signal or \
             ptrace attachment binding it, so the cap could neither count the trace nor stop \
             the guest; drop --max-log-bytes"
        ),
        "{args:?}: {text}"
    );
    assert!(
        text.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "{args:?}: {text}"
    );

    for backend in [None, Some("--backend=ptrace")] {
        let plain: Vec<&str> = backend.into_iter().chain(["strace", "/bin/true"]).collect();
        let capped: Vec<&str> = ["--max-log-bytes=1M"]
            .into_iter()
            .chain(plain.iter().copied())
            .collect();
        let without = strace(&plain);
        let with = strace(&capped);
        assert_ne!(
            with.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{capped:?}: {with:?}"
        );
        assert_eq!(
            (with.status.code(), stderr(&with)),
            (without.status.code(), stderr(&without)),
            "{capped:?} must fail exactly as {plain:?} does"
        );
        assert!(
            stderr(&with).contains("the M1 strace command requires `--backend sabre`"),
            "{capped:?}: {}",
            stderr(&with)
        );
    }
}

/// The happy path: a run under its cap is unaffected, and a refused value
/// tells the user what to pass instead.
#[test]
fn max_log_bytes_leaves_a_run_under_the_cap_alone_and_refuses_zero() {
    let _lock = hermit_run_guard();
    let args = ["--log=info", "--max-log-bytes=1G", "run", "--", "/bin/true"];
    let output = hermit(&args);
    assert_success(&output, &args);
    assert!(!stderr(&output).contains("--max-log-bytes"));

    let output = hermit(&["--max-log-bytes=0", "run", "--", "/bin/true"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let refusal = stderr(&output);
    assert!(
        refusal.contains("Omit --max-log-bytes") && refusal.contains("8G"),
        "{refusal}"
    );
    let output = hermit(&["--max-log-bytes=eight", "run", "--", "/bin/true"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(stderr(&output).contains("--max-log-bytes=8G"), "{output:?}");
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

/// The implicit inputs a run config must capture, and the log variables a
/// harness may set, removed so the run samples its own.
fn without_implicit_hermit_inputs(command: &mut Command) -> &mut Command {
    for variable in [
        "HERMIT_EPOCH",
        "HERMIT_PRNG",
        "HERMIT_SCHED_SEED",
        "HERMIT_LOG",
        "HERMIT_LOG_FILE",
    ] {
        command.env_remove(variable);
    }
    command.stdin(Stdio::null())
}

/// `--save-config` writes a file that `--config` loads to reproduce the run:
/// the epoch sampled from the host clock and the seed `--seed-from` chose are
/// written out, so the reloaded guest prints the same time and the same
/// random bytes and Hermit's canonical INFO log matches. A command-line
/// `--log-file` replaces the file's. A third run with the same options but no
/// file samples new inputs and prints something else, so the match is the
/// file's doing.
#[test]
fn run_config_saved_by_one_run_reproduces_it() {
    let _lock = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let path = |name: &str| directory.path().join(name).to_str().unwrap().to_owned();
    let (config, first_log, second_log) = (path("run.yaml"), path("a.log"), path("b.log"));
    // `env -C /tmp` keeps the shell's startup `stat(".")` on the guest's
    // private /tmp rather than on a host directory that can change.
    let guest = [
        "--",
        "/usr/bin/env",
        "-C",
        "/tmp",
        "/bin/sh",
        "-c",
        "date +%s.%N; od -An -tx1 -N8 /dev/urandom",
    ];
    let save_config = format!("--save-config={config}");
    let first_log_arg = format!("--log-file={first_log}");
    let options = ["--max-timeslice=disabled", "--seed-from=SystemRandom"];
    let first_args: Vec<&str> = ["--log=info", first_log_arg.as_str(), "run"]
        .into_iter()
        .chain(options)
        .chain([save_config.as_str()])
        .chain(guest)
        .collect();
    let first = without_implicit_hermit_inputs(&mut hermit_command(&first_args))
        .output()
        .unwrap();
    assert_success(&first, &first_args);

    let saved: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
    assert_eq!(saved["schema"], "hermit-run-config/v1", "{saved:?}");
    assert_eq!(saved["global"]["log-file"], first_log.as_str(), "{saved:?}");
    assert!(saved["run"]["epoch"].is_string(), "{saved:?}");
    assert!(saved["run"]["seed"].is_u64(), "{saved:?}");
    assert!(saved["run"].get("seed-from").is_none(), "{saved:?}");
    assert!(saved["run"].get("save-config").is_none(), "{saved:?}");

    let second_log_arg = format!("--log-file={second_log}");
    let second_args = [second_log_arg.as_str(), "run", "--config", config.as_str()];
    let second = without_implicit_hermit_inputs(&mut hermit_command(&second_args))
        .output()
        .unwrap();
    assert_success(&second, &second_args);
    assert_eq!(stdout(&first).lines().count(), 2, "{}", stdout(&first));
    assert_eq!(stdout(&second), stdout(&first));
    let diff = log_diff(&["--canonical-info", &first_log, &second_log]);
    assert_eq!(diff.status.code(), Some(0), "{}", stderr(&diff));

    let third_args: Vec<&str> = ["run"].into_iter().chain(options).chain(guest).collect();
    let third = without_implicit_hermit_inputs(&mut hermit_command(&third_args))
        .output()
        .unwrap();
    assert_success(&third, &third_args);
    assert_ne!(stdout(&third), stdout(&first));
}

/// A run config with a key that names no option is refused before anything
/// runs, as a usage error that gives the spelling that works.
#[test]
fn run_config_refuses_an_unknown_key_with_the_working_spelling() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let config = directory.path().join("run.yaml");
    std::fs::write(
        &config,
        "schema: hermit-run-config/v1\nrun: {base_env: minimal}\nprogram: /bin/true\n",
    )
    .unwrap();
    let config = config.to_str().unwrap();
    let output = hermit(&["run", "--config", config]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let stderr = stderr(&output);
    assert!(
        stderr.contains(&format!(
            "error: cannot load run config {config}: `run.base_env` names no option: keys are \
             long option names, with hyphens: write `base-env`"
        )),
        "{stderr}"
    );
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
/// tracked files with git, so the LiteInst tests that staged the runtime
/// failed with "fatal: not a git repository". The test helper now hands the script
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

/// A verification report from a backend whose virtual clock is the
/// retired-branch counter repeats the binary's own `exact_branch_counter`
/// verdict (https://github.com/rrnewton/hermit/issues/3794), for `run --verify`
/// and for `record start --verify`, which ignores `--strict` and so still runs
/// where the counter is inexact. Those two run without `--strict`, so that a
/// host with an inexact counter writes a complete report too. A strict run
/// refused before it starts a guest, here for a missing program, still carries
/// the verdict in its no-result report: the strict counter refusal and every
/// later refusal come after the verdict is recorded.
#[test]
fn ptrace_verification_reports_carry_the_exact_branch_counter_verdict() {
    let _guard = hermit_run_guard();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the branch-counter report test directory");
    let capabilities = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["host-capabilities", "--json"])
        .output()
        .expect("failed to run hermit host-capabilities");
    assert!(capabilities.status.success(), "{capabilities:?}");
    let expected = serde_json::from_slice::<serde_json::Value>(&capabilities.stdout)
        .expect("host-capabilities --json is JSON")["exact_branch_counter"]
        .clone();
    assert!(expected.is_object(), "{expected}");
    for (label, file, prefix, program) in [
        ("run --verify", "run", &["run", "--verify"][..], "/bin/true"),
        (
            "record start --verify",
            "record",
            &["record", "start", "--verify"][..],
            "/bin/true",
        ),
        (
            "refused run --strict --verify",
            "refused",
            &["run", "--strict", "--verify"][..],
            "/nonexistent/hermit-missing-program",
        ),
    ] {
        let verdict_path = root.path().join(format!("{file}.json"));
        let mut args = prefix.to_vec();
        args.extend([
            "--verify-json",
            verdict_path.to_str().expect("verdict path should be UTF-8"),
            "--",
            program,
        ]);
        let output = hermit_command(&args)
            .output()
            .unwrap_or_else(|error| panic!("failed to run {label}: {error}"));
        let report: serde_json::Value = serde_json::from_slice(
            &fs::read(&verdict_path)
                .unwrap_or_else(|error| panic!("{label} wrote no report: {error}")),
        )
        .unwrap_or_else(|error| panic!("{label} report is not JSON: {error}"));
        assert_eq!(
            report["exact_branch_counter"],
            expected,
            "{label}: {report}\n{}",
            strip_ansi_sgr(&stderr(&output))
        );
        if file == "refused" {
            assert!(!output.status.success(), "{label} was not refused");
            assert_eq!(report["verdict"], "no_result", "{label}: {report}");
        }
    }
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
    let build_root = process_build_root("pipe-capacity-pin");
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
    let build_root = process_build_root("guest-fault-not-executable");
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
    let script_root = process_build_root("kill-gdbserver-replay-peer");
    fs::create_dir_all(&script_root).expect("failed to create the GDB script directory");
    let script = script_root.join("kill-gdbserver-replay-peer.py");
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

/// The REPLAY-stage classification site of `record --verify`, with the
/// container child killed from outside by a signal it did not choose.
///
/// THE FAULT DOES NOT HAVE TO COME FROM INSIDE THE CHILD (`agent(codex-rev-2628)`
/// in review): killing the replay container child from outside reaches the site.
/// It must be killed at a FIXED POINT, though. The first version waited for
/// `:: Replaying...` on stderr and then killed whatever child it found, which
/// raced the replay itself: a `/bin/sleep` replay takes about 0.2 s, and on
/// GitHub's hosted runner the replay finished first ("kill: No such process"),
/// matched, and the test failed. The injector's `block` mode now parks the
/// replay child at the `record_verify.replay` site and announces it on stderr;
/// the test kills it only then, so the kill always lands on a live child at the
/// site.
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

    let mut child = hermit_command(&["record", "--verify", "--", "/bin/true"])
        .env("HERMIT_DATA_DIR", data_dir.path())
        .env("HERMIT_TEST_CONTAINER_CHILD_FAULT", "block")
        .env(
            "HERMIT_TEST_CONTAINER_CHILD_FAULT_SITE",
            "record_verify.replay",
        )
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
            if !killed && line.contains("container child blocked at site record_verify.replay") {
                // The replay child waits at the site until it is killed, so it
                // is alive here; only a successful kill counts.
                for target in children_of(pid) {
                    // SAFETY: kill(2) on a pid read from /proc; no memory is shared.
                    if unsafe { libc::kill(target as libc::pid_t, libc::SIGKILL) } == 0 {
                        killed = true;
                    }
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
        "no replay container child was killed at the record_verify.replay site, so \
         this test never exercised the replay stage it exists for\nstderr:\n{stderr}"
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

/// In-guest LiteInst (https://github.com/rrnewton/hermit/issues/3520) cannot
/// deliver Detcore's preemption timer yet: the in-guest Tool host refuses
/// `set_timer` with `ENOSYS`, which Detcore treats as fatal. A run with the default maximum
/// timeslice must be REFUSED before dispatch, not fail inside the guest. The
/// refusal comes from argument validation, so it holds in builds without the
/// LiteInst runtime too. A build without the `liteinst` feature refuses the
/// backend itself instead, which
/// `run_liteinst_without_the_feature_refuses_and_names_the_flag` covers.
#[test]
#[cfg(feature = "liteinst")]
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

/// An in-guest LiteInst refusal of a `--verify` run must come before `--verify`
/// snapshots stdin. The snapshot reads stdin to its end, so a refusal checked
/// after it would wait for input that may never come instead of exiting. Stdin
/// here is a pipe that the test holds open and never writes to; the run is
/// refused for its maximum timeslice. A build without the `liteinst` feature
/// refuses the backend itself instead, and that refusal must not wait for input
/// either.
#[test]
fn liteinst_in_guest_refuses_verify_without_reading_stdin() {
    use std::io::Read;

    const DEADLINE: Duration = Duration::from_secs(20);

    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut child = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--verify",
        "--",
        "/bin/echo",
        "unreachable",
    ])
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

    #[cfg(feature = "liteinst")]
    let (exit, class, reason) = (
        HERMIT_POLICY_REFUSAL_EXIT,
        "HERMIT_POLICY_REFUSAL class=policy-refusal",
        "pass --max-timeslice=disabled",
    );
    #[cfg(not(feature = "liteinst"))]
    let (exit, class, reason) = (
        HERMIT_INTERNAL_FAILURE_EXIT,
        "HERMIT_INTERNAL_FAILURE class=backend-unavailable backend=liteinst",
        "compiled without the liteinst backend",
    );
    // EXIT-CLASS: hermit
    assert_eq!(
        status.code(),
        Some(exit),
        "status {status:?}, stderr:\n{stderr}"
    );
    assert!(stderr.contains(class), "stderr:\n{stderr}");
    assert!(
        stderr.contains(reason),
        "the refusal must give its reason. stderr:\n{stderr}"
    );
    assert!(!stdout.contains("unreachable"), "the guest must not run");
}

/// A `--backend=liteinst` replay whose explicit `--epoch` disagrees with the
/// epoch its recording was made under is refused by policy (122) in every
/// build. Replay-epoch reconciliation does not depend on the backend being
/// compiled in, so a build without the `liteinst` feature must not answer
/// "backend unavailable" (125) instead.
#[test]
fn liteinst_replay_with_a_conflicting_epoch_is_refused_in_every_build() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create a directory for the preemption record");
    let record = directory.path().join("record.json");
    fs::write(
        &record,
        r#"{"per_thread":{},"global":[],"epoch":"2000-12-31T23:59:59.123456789Z"}"#,
    )
    .expect("failed to write the preemption record");
    let replay = format!("--replay-preemptions-from={}", record.display());

    let output = hermit_command(&[
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--epoch=2001-01-01T00:00:00Z",
        &replay,
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
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a conflicting replay epoch must be refused by policy in every build. Got {:?}. \
         stderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("HERMIT_POLICY_REFUSAL class=policy-refusal"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("the explicit virtual-time epoch 2001-01-01T00:00:00+00:00"),
        "the refusal must name the conflicting epoch. stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("pass --epoch=2000-12-31T23:59:59.123456789+00:00"),
        "the refusal must name the recorded epoch. stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("compiled without the liteinst backend"),
        "stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "the guest must not run"
    );
}

/// `--verify` with an explicit log level that would hide the events it
/// compares is a command-line error in every build, `--backend=liteinst`
/// included. A build without the `liteinst` feature must report that error,
/// not the missing feature.
#[test]
fn liteinst_verify_with_a_quiet_log_level_is_refused_in_every_build() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let output = hermit_command(&[
        "--log=warn",
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--verify",
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
        Some(HERMIT_INTERNAL_FAILURE_EXIT),
        "status {:?}, stderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("HERMIT_INTERNAL_FAILURE class=cli-error"),
        "the run must be refused as a command-line error. stderr:\n{stderr}"
    );
    assert!(
        stderr
            .contains("--verify requires --log=info or a more verbose level; received --log=warn"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("compiled without the liteinst backend"),
        "stderr:\n{stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("unreachable"),
        "the guest must not run"
    );
}

/// What one in-guest LiteInst `--verify` run reported about its forwarded
/// DETLOG records.
#[cfg(feature = "liteinst")]
struct ForwardedVerify {
    stdout: String,
    stderr: String,
    /// The syscall records each run forwarded; both runs forwarded this many.
    syscall_records: usize,
    /// Run 1's retained log, which a matched run keeps as the golden log.
    golden_log: String,
}

/// Runs `guest` under in-guest LiteInst `--verify --keep-logs` and checks what
/// holds for every such run: it matches, both runs forward the same nonzero
/// number of syscall records, and run 1's log holds every one of them,
/// including the in-guest Tool's own `detcore::tool_local` records.
#[cfg(feature = "liteinst")]
fn liteinst_verify_with_forwarded_records(guest: &[&str]) -> ForwardedVerify {
    liteinst_verify_with_forwarded_records_under(guest, None)
}

/// [`liteinst_verify_with_forwarded_records`] with `RUST_LOG` set to `rust_log`
/// (unset when `None`).
#[cfg(feature = "liteinst")]
fn liteinst_verify_with_forwarded_records_under(
    guest: &[&str],
    rust_log: Option<&str>,
) -> ForwardedVerify {
    const FORWARDED: &str = "1970-01-01T00:00:00.000000Z INFO detcore";

    let logs = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let report = logs.path().join("verify.json");
    let log_dir = logs.path().to_str().expect("UTF-8 temporary path");
    let report_arg = report.to_str().expect("UTF-8 temporary path");
    let mut args = vec![
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--verify",
        "--keep-logs",
        "--verify-log-dir",
        log_dir,
        "--verify-json",
        report_arg,
        "--",
    ];
    args.extend_from_slice(guest);
    let mut command = hermit_command(&args);
    command
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .stdin(Stdio::null());
    if let Some(filter) = rust_log {
        command.env("RUST_LOG", filter);
    }
    let output = command.output().expect("failed to run hermit");
    assert_success(&output, &args);
    let stderr = stderr(&output);
    assert!(stderr.contains("Determinism verified"), "stderr:\n{stderr}");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("--verify-json report")).unwrap();
    assert!(
        report["verified"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["compare_logs"] == true,
        "{report}"
    );

    // Both runs forwarded the same number of syscall records.
    let counts = stderr
        .split_once(":: LiteInst syscall DETLOG records included: ")
        .and_then(|(_, rest)| rest.lines().next())
        .unwrap_or_else(|| panic!("no forwarded record counts in:\n{stderr}"));
    let [run1, run2] = ["run1=", "run2="].map(|key| -> usize {
        counts
            .split(", ")
            .find_map(|field| field.strip_prefix(key))
            .and_then(|count| count.parse().ok())
            .unwrap_or_else(|| panic!("no {key} count in {counts:?}"))
    });
    assert!(run1 > 0 && run1 == run2, "{counts}");

    // A matched run 2's log is deleted; run 1's is kept as the golden log.
    let golden: Vec<_> = fs::read_dir(logs.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect();
    let [golden] = &golden[..] else {
        panic!("expected one retained run 1 log in {log_dir}, found {golden:?}");
    };
    let golden_log = fs::read_to_string(golden).unwrap();
    let forwarded: Vec<&str> = golden_log
        .lines()
        .filter(|line| line.starts_with(FORWARDED))
        .collect();
    assert_eq!(
        forwarded
            .iter()
            .filter(|line| line.contains("DETLOG [syscall]"))
            .count(),
        run1,
        "run 1's log must hold every forwarded syscall record:\n{golden_log}"
    );
    assert!(
        forwarded
            .iter()
            .any(|line| line.contains("INFO detcore::tool_local: DETLOG ")),
        "run 1's log holds no record from the in-guest Tool's local state:\n{golden_log}"
    );
    ForwardedVerify {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr,
        syscall_records: run1,
        golden_log,
    }
}

/// In-guest LiteInst forwards each Detcore record by the CLI filter's INFO
/// answer for the record's module, which is what each in-process `detlog!`
/// callsite asks tracing under ptrace. Under `info,detcore::random=warn`
/// ptrace logs no `detcore::random` record; in-guest LiteInst once forwarded
/// the AT_RANDOM record anyway, because it forwarded every record whenever
/// INFO was on for `detcore`.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_verify_forwards_records_by_the_cli_filters_per_target_answer() {
    const RANDOM: &str = "INFO detcore::random: DETLOG ";
    const SCOPED: &str = "info,detcore::random=warn";

    let _lock = hermit_run_guard();
    let all = liteinst_verify_with_forwarded_records_under(&["/bin/true"], None);
    assert!(
        all.golden_log.contains(RANDOM),
        "without a scoped filter the guest forwards its detcore::random record:\n{}",
        all.golden_log
    );
    let scoped = liteinst_verify_with_forwarded_records_under(&["/bin/true"], Some(SCOPED));
    assert!(
        !scoped.golden_log.contains(RANDOM),
        "RUST_LOG={SCOPED} must keep detcore::random records out, as ptrace does:\n{}",
        scoped.golden_log
    );
    assert_eq!(
        scoped.syscall_records, all.syscall_records,
        "the scoped filter must not change the other modules' records"
    );
}

/// In-guest LiteInst forwards the Tool's DETLOG records to a descriptor Reverie
/// reserves and protects from the guest, and `--verify` reads them into each
/// run's log, so the two runs' records are compared
/// (https://github.com/rrnewton/hermit/issues/3520, C5).
///
/// The guest's stderr is never parsed. Here the guest writes a prompt with no
/// newline, a line in a forwarded record's exact shape, and the same line
/// behind the record marker an earlier revision of this change used: all must
/// reach the guest's stderr byte for byte, neither copy may enter the log, and
/// no real record may reach the guest's stderr.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_verify_compares_the_records_the_guest_forwards() {
    const LOOK_ALIKE: &str = "INFO detcore: DETLOG [syscall] guest look-alike\n\
        \u{1e}HERMIT_FORWARDED_DETLOG\u{1f}INFO detcore: DETLOG [syscall] guest look-alike\n";

    let _lock = hermit_run_guard();
    let run = liteinst_verify_with_forwarded_records(&[
        "/bin/sh",
        "-c",
        "printf 'prompt> ' >&2; \
         printf 'INFO detcore: DETLOG [syscall] guest look-alike\\n' >&2; \
         printf '\\036HERMIT_FORWARDED_DETLOG\\037INFO detcore: DETLOG [syscall] guest look-alike\\n' >&2; \
         echo forwarded",
    ]);
    assert_eq!(run.stdout, "forwarded\n");
    assert!(
        run.stderr.contains(&format!("prompt> {LOOK_ALIKE}")),
        "the guest's stderr bytes changed:\n{}",
        run.stderr
    );
    assert_eq!(
        run.stderr.matches("INFO detcore").count(),
        2,
        "a forwarded record reached the guest's stderr:\n{}",
        run.stderr
    );
    assert!(
        !run.golden_log.contains("guest look-alike"),
        "guest output entered the log:\n{}",
        run.golden_log
    );
}

/// Run 1's retained verify log of `/bin/true` under `backend`, with one fixed
/// epoch, kept in `logs`.
#[cfg(any(feature = "liteinst", feature = "dbt"))]
fn verify_log_of_true(backend: &str, logs: &Path) -> PathBuf {
    let log_dir = logs.to_str().expect("UTF-8 temporary path");
    // `--log info`, as the e2e harness runs every verify cell: at the default
    // verification level a ptrace record carries its tracing span.
    let args = [
        "--log",
        "info",
        "--backend",
        backend,
        "run",
        "--max-timeslice=disabled",
        "--epoch=2026-10-05T09:20:32+00:00",
        "--verify",
        "--keep-logs",
        "--verify-log-dir",
        log_dir,
        "--",
        "/bin/true",
    ];
    let output = hermit_command(&args)
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    assert_success(&output, &args);
    let golden: Vec<_> = fs::read_dir(logs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect();
    let [golden] = &golden[..] else {
        panic!("expected one retained run 1 log in {log_dir}, found {golden:?}");
    };
    golden.clone()
}

/// Under DBT each guest `rdtsc` is answered by Detcore, as under ptrace: the
/// read is charged and logged ("inbound rdtsc"), and the guest's TSC is
/// Detcore's virtual clock. Before, the DBT client answered every read from its
/// own fixed-stride counter, so DBT's run 1 log of `/bin/true` held none of the
/// 10 rdtsc records ptrace's does.
#[test]
#[cfg(feature = "dbt")]
fn dbt_rdtsc_is_answered_by_detcore_as_under_ptrace() {
    let _lock = hermit_run_guard();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (ptrace_dir, dbt_dir) = (dir.path().join("ptrace"), dir.path().join("dbt"));
    fs::create_dir_all(&ptrace_dir).unwrap();
    fs::create_dir_all(&dbt_dir).unwrap();
    let ptrace = fs::read_to_string(verify_log_of_true("ptrace", &ptrace_dir)).unwrap();
    let dbt = fs::read_to_string(verify_log_of_true("dbt", &dbt_dir)).unwrap();
    let reads = ptrace.matches("inbound rdtsc").count();
    assert!(reads > 0, "ptrace logged no rdtsc:\n{ptrace}");
    assert_eq!(
        dbt.matches("inbound rdtsc").count(),
        reads,
        "ptrace:\n{ptrace}\ndbt:\n{dbt}"
    );
}

/// The coordinator writes the records the in-guest Tool forwards into the log
/// as it handles the guest's requests, so backend parity's own comparison
/// (`hermit log-diff --record-envelope cross-backend-detcore-v1`) of the
/// ptrace and the in-guest LiteInst run 1 logs of `/bin/true` matches through
/// the root thread's seeding, the first scheduler commits and the post-exec
/// AT_RANDOM record. When the records were appended after the run, the log
/// held every coordinator record first, and the comparison diverged at the
/// third record (2 matched).
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_verify_log_keeps_records_in_ptraces_order() {
    let _lock = hermit_run_guard();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (ptrace_dir, liteinst_dir) = (dir.path().join("ptrace"), dir.path().join("liteinst"));
    fs::create_dir_all(&ptrace_dir).unwrap();
    fs::create_dir_all(&liteinst_dir).unwrap();
    let ptrace = verify_log_of_true("ptrace", &ptrace_dir);
    let liteinst = verify_log_of_true("liteinst", &liteinst_dir);
    let report = dir.path().join("log-diff.json");
    let args = [
        "log-diff",
        ptrace.to_str().unwrap(),
        liteinst.to_str().unwrap(),
        "--json",
        report.to_str().unwrap(),
        "--record-envelope",
        "cross-backend-detcore-v1",
    ];
    // /bin/true still differs later (its loader's system calls are not
    // intercepted in the guest), so the comparison exits nonzero; read its report.
    hermit_command(&args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit log-diff");
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("log-diff report")).unwrap();
    let matched = report["matched_prefix_records"].as_u64().unwrap_or(0);
    let first = report["first_divergent_left_message"]
        .as_str()
        .unwrap_or("");
    assert!(matched >= 6, "{report}");
    for early in [
        "USER RAND",
        "CHAOSRAND",
        "AT_RANDOM",
        "COMMIT turn 0",
        "COMMIT turn 1",
    ] {
        assert!(!first.contains(early), "diverged at {early}: {report}");
    }
}

/// A guest that points its stderr at a pipe nobody reads, with SIGPIPE at its
/// default action, that dup2s onto every descriptor from 1024 to 1039 (where
/// Reverie keeps, and keeps moving, the forwarding socket) and then closes
/// them all, still runs to the end under `--verify`: each dup2 succeeds as it
/// would without forwarding, and the records are still forwarded. When
/// records went to the guest's stderr, Detcore's own record of the first
/// `dup2` killed it with SIGPIPE under `--verify` only; with a socket that did
/// not move, the dup2 onto its number failed with EBADF.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_verify_survives_a_guest_stderr_without_a_reader() {
    const GUEST: &str = "import os, signal\n\
        signal.signal(signal.SIGPIPE, signal.SIG_DFL)\n\
        r, w = os.pipe()\n\
        os.close(r)\n\
        os.dup2(w, 2)\n\
        os.close(w)\n\
        for fd in range(1024, 1040):\n\
        \x20   assert os.dup2(1, fd) == fd, fd\n\
        os.closerange(3, 1100)\n\
        print('alive', flush=True)\n";

    let _lock = hermit_run_guard();
    // -I -B, as in sabre_examples' python3 guest: a writable bytecode cache
    // (PYTHONPYCACHEPREFIX) would make the two verify runs differ.
    let run =
        liteinst_verify_with_forwarded_records(&["/usr/bin/python3", "-I", "-B", "-c", GUEST]);
    assert_eq!(run.stdout, "alive\n");
    assert!(run.syscall_records > 0);
}

/// A guest whose own shared library's constructor, which the loader runs
/// before the in-guest runtime's constructor, closes the forwarding socket and
/// puts a socket pair of its own at that number. The runtime must not adopt the
/// guest's socket, which would send Tool records to the guest: it refuses to
/// start, before any record is sent. (Adopting it, the run was refused only
/// afterwards, as "forwarded records lost", with the records already in the
/// guest's socket.) An executable's own `.preinit_array` is refused earlier.
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_refuses_a_guest_socket_at_the_forwarding_number() {
    const LIBRARY: &str = r#"
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
extern char **environ;
__attribute__((constructor)) static void replace_tool_output(void) {
  for (char **entry = environ; *entry; entry++) {
    if (strncmp(*entry, "HERMIT_LITEINST_FORWARD_DETLOG=", 31) == 0) {
      int passed = atoi(*entry + 31);
      int pair[2];
      close(passed);
      if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, pair) != 0) _exit(3);
      if (pair[0] != passed && dup2(pair[0], passed) != passed) _exit(4);
    }
  }
}
"#;
    let _lock = hermit_run_guard();
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let compile = |args: &[&str]| {
        let build = Command::new("cc")
            .current_dir(dir.path())
            .args(args)
            .output()
            .expect("failed to run cc");
        assert!(
            build.status.success(),
            "guest compilation failed:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
    };
    fs::write(dir.path().join("replace.c"), LIBRARY).unwrap();
    fs::write(dir.path().join("main.c"), "int main(void) { return 0; }\n").unwrap();
    compile(&[
        "-shared",
        "-fPIC",
        "-O1",
        "-Wall",
        "-Werror",
        "-o",
        "libreplace.so",
        "replace.c",
    ]);
    let rpath = format!("-Wl,-rpath,{}", dir.path().display());
    compile(&["-O1", "-o", "guest", "main.c", "-L.", "-lreplace", &rpath]);
    let guest = dir.path().join("guest");
    let guest = guest.to_str().expect("UTF-8 temporary path");
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--verify",
        "--",
        guest,
    ];
    let output = hermit_command(&args)
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let stderr = stderr(&output);
    assert!(!output.status.success(), "the run was accepted:\n{stderr}");
    assert!(
        !stderr.contains("forwarded records lost"),
        "the runtime adopted the guest's socket and sent records to it:\n{stderr}"
    );
    assert!(
        stderr.contains("exited before connecting to the coordinator"),
        "the runtime did not refuse to start:\n{stderr}"
    );
}

/// Forwarded records count against the log's size bound
/// (`HERMIT_LOG_MAX_BYTES`), and a log they push past it still ends in the
/// truncation marker, so the comparison is refused (`no_result`) instead of
/// comparing records that were cut. The guest makes the volume, not the host:
/// a shell loop whose 200 iterations each record at least an open, a write
/// and a close forwards well over 100 KB of records (about 1.5 MB on the
/// development host for 300 iterations), far past the 50 000-byte bound,
/// while the log interleaves records in the order they happen, so forwarded
/// records are in it before the cut (the first one is near its start). The
/// retained log proves that. (`/bin/echo` alone logged about 76 KB on the
/// development host but under 50 000 bytes in the pinned validation root, so
/// it could not exercise the bound everywhere.)
#[test]
#[cfg(feature = "liteinst")]
fn liteinst_in_guest_verify_with_records_past_the_log_bound_is_no_result() {
    let _lock = hermit_run_guard();
    let logs = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let report = logs.path().join("verify.json");
    let log_dir = logs.path().to_str().expect("UTF-8 temporary path");
    let report_arg = report.to_str().expect("UTF-8 temporary path");
    let args = [
        "--backend",
        "liteinst",
        "run",
        "--max-timeslice=disabled",
        "--verify",
        "--keep-logs",
        "--verify-log-dir",
        log_dir,
        "--verify-json",
        report_arg,
        "--",
        "/bin/sh",
        "-c",
        "i=0; while [ $i -lt 200 ]; do echo x > /dev/null; i=$((i+1)); done; echo capped",
    ];
    let output = hermit_command(&args)
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .env("HERMIT_LOG_MAX_BYTES", "50000")
        .stdin(Stdio::null())
        .output()
        .expect("failed to run hermit");
    let stderr = stderr(&output);
    assert!(!output.status.success(), "stderr:\n{stderr}");
    assert!(
        stderr.contains("truncated at the configured size bound"),
        "stderr:\n{stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report).expect("--verify-json report")).unwrap();
    assert!(
        report["verdict"] == "no_result" && report["verified"] == false,
        "{report}"
    );
    let run1: Vec<_> = fs::read_dir(logs.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .collect();
    let [run1] = &run1[..] else {
        panic!("expected one retained run 1 log in {log_dir}, found {run1:?}");
    };
    let text = fs::read_to_string(run1).unwrap();
    assert!(
        text.lines()
            .any(|line| line.starts_with("1970-01-01T00:00:00.000000Z INFO detcore")),
        "the bound cut Hermit's own records before any forwarded one:\n{text}"
    );
    assert!(
        text.trim_end().ends_with("was NOT affected. ==="),
        "the log does not end in the truncation marker:\n{text}"
    );
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
        hermit_command(&args)
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

/// Signals that arrive while Detcore is blocked in a guest's `wait4`, under the
/// ptrace backend and in-guest LiteInst
/// (guest `hermit-cli/tests/fixtures/liteinst_in_guest_wait_signals.c`).
///
/// Around a blocking `wait4`, Detcore replaces the guest's signal mask with one
/// that blocks every signal except 16, 32 and 33, and restores the guest's mask
/// afterwards. Under in-guest LiteInst both are real `rt_sigprocmask` calls
/// made by the guest preload. Before
/// https://github.com/rrnewton/reverie/pull/913 the preload refused, with
/// EPERM, any set that blocked its own SIGSYS, so a guest that forked and
/// waited failed at its first `wait4`.
///
/// Each mode runs natively, once under the ptrace backend and three times
/// under in-guest LiteInst. Every run must print the mode's expected lines, end
/// with its expected status and write no line starting with `FAIL ` to stderr.
/// The native run shows that the expected lines are what Linux does. In
/// handler-after-reap, the native run's child also waits until Linux reports
/// the parent asleep in `wait4` (`wait-for-parent-asleep`, see the guest).
/// That report means asleep only on Linux 5.16 or later, so on an older kernel
/// the guest fails that run rather than relying on it.
/// When the guest is killed by a signal, `hermit` raises the same signal on
/// itself (`ExitStatus::raise_or_exit`), so every runner must end in that
/// signal death.
///
/// The guest cannot tell by itself whether a signal reached it inside
/// Detcore's wait, so each Hermit run also writes an INFO log, and the test
/// checks Detcore's scheduler records in it. The records name processes by
/// dettid. The parent's first commit is on `ParentContinue` naming itself, and
/// each `fork` it makes commits one naming the new child, in fork order, so the
/// test reads every dettid from those. Under ptrace the log also has a line for
/// each system call the parent finishes; in-guest logs have none.
///
/// The parent commits on `WaitidSignals([SigWrapper(N)])` when it is parked in
/// a wait with signal N pending. Detcore uses that resource for interrupted
/// writes as well, so the test binds it to the parent's dettid and orders it
/// against the children's `Exit` commits:
/// - sigchld-during-wait: the parent parks on the slow child, and its first
///   commit after that is on SIGCHLD. The fast child's `Exit` comes before that
///   commit and the slow child's after it, and the parent parks on the slow
///   child again before the slow child exits. Under ptrace the interrupted
///   `wait4` ends in ERESTARTSYS. So SIGCHLD reached the parent while the child
///   it waited for was not ready, and the wait went on.
/// - ignored-then-restart: the parent's `WaitidSignals` commit comes before the
///   waited child's `Exit`, and the parent parks on the waited child after the
///   signal commit, before that `Exit`. Under ptrace the interrupted `wait4`
///   ends in ERESTARTSYS.
/// - terminate-during-wait (ptrace only, see below): no `Exit` of the child
///   comes before the parent's `WaitidSignals` commit. The `wait4` ends in
///   ERESTARTSYS, the parent commits the delivery of SIGUSR1
///   (`InboundSignal(SigWrapper(10))`), and the default action kills it.
/// - ignored-during-wait, terminate-then-exit, guest-blocked and block-all:
///   the signal is pending when the reap succeeds. The child's `Exit` comes
///   before the parent's `WaitidSignals` commit. Under ptrace the parent's next
///   `wait4` returns that child, and for ignored-during-wait and
///   terminate-then-exit the parent's next commit is the delivery of SIGUSR1,
///   which kills terminate-then-exit's parent. In-guest, the guest's own
///   `child=5` line shows the reap. terminate-then-exit's parent dies before it
///   prints one, so its guest gives `wait4` a status word mapped from a file,
///   which outlives the parent, after setting the word to a value `wait4` never
///   stores. Under both Hermit runners the word must hold the child's exit
///   status 5, which shows the reap came before the parent's death. Natively the
///   parent usually dies first and the word keeps its first value (299 of 300
///   native runs on Linux 7.1.3), so either value is accepted there.
/// - handler-after-reap (ptrace only, see below): the parent parks on the
///   child; after the child's `Exit`, Detcore queues SIGCHLD for the parent
///   (its "Alarm fired" line), and the parent's first commit after parking is
///   the reap (`WaitChild`), after both, not SIGCHLD. Its `wait4` returns the
///   child, and its next commit is the delivery of SIGCHLD to the handler.
///
/// Every mode that prints `mask-after` or `mask-kept` reads the mask after the
/// wait, not during it: during the wait, Detcore unblocks signals 32 and 33 even
/// when the guest blocked them (https://github.com/rrnewton/hermit/issues/3697).
/// terminate-during-wait and terminate-then-exit print only their first line,
/// so they check no mask.
///
/// block-all prints the mask the guest gets when it asks to block all 64
/// signals. Linux never blocks SIGKILL or SIGSTOP, and each backend keeps a few
/// more signals for itself, so that one line is expected per backend. The masks
/// below name every signal each backend leaves unblocked.
///
/// Three modes do not yet pass in-guest. They still run natively and under
/// ptrace, so their expected lines stay checked:
/// - terminate-during-wait is not run in-guest: a guest process killed by a
///   signal is never reported to the Detcore scheduler, so instead of the
///   guest's SIGUSR1 death the run fails with an internal error after 30 s
///   (https://github.com/rrnewton/hermit/issues/3688).
/// - write-only-set runs in-guest and must fail exactly as
///   https://github.com/rrnewton/reverie/issues/921 describes: in-guest Detcore
///   cannot read a system-call argument from a page mapped write-only.
/// - handler-after-reap runs in-guest and must fail exactly as
///   https://github.com/rrnewton/reverie/issues/243 describes: the guest
///   runtime refuses, with EPERM, every guest signal handler other than
///   `SIG_DFL` and `SIG_IGN`.
///
/// "Exactly" means the run exits with status 1, prints nothing, and writes to
/// stderr only Hermit's in-guest selection line and the one `FAIL ` line the
/// issue describes. When either issue is fixed this test fails, and the mode
/// joins the others.
///
/// In-guest runs are compared by output, exit status and scheduler records
/// only; they do not pass `--verify`, so the schedule itself is not compared
/// here.
///
/// This test is ignored, and no validate node runs it, so it runs only when
/// someone runs it by hand, ptrace legs included
/// (https://github.com/rrnewton/hermit/issues/3698).
#[test]
#[cfg(feature = "liteinst")]
#[ignore = "needs the in-guest Detcore runtime from `cargo build -p detcore-liteinst`"]
fn liteinst_in_guest_signals_during_a_blocking_wait4_match_ptrace() {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;
    use std::sync::mpsc;

    const DEADLINE: Duration = Duration::from_secs(60);
    const IN_GUEST_RUNS: usize = 3;

    #[derive(Clone, Copy)]
    enum Exit {
        Code(i32),
        Signal(i32),
    }

    // How in-guest LiteInst is checked for a mode.
    #[derive(Clone, Copy)]
    enum InGuest {
        // Like the native and ptrace runs.
        Same,
        // Not run, until the linked issue is fixed.
        Skipped(&'static str),
        // Run, and must fail exactly as the linked issue describes: exit with
        // `code`, print nothing and write `stderr` to stderr.
        KnownBad {
            issue: &'static str,
            code: i32,
            stderr: &'static str,
        },
    }

    let bit = |signal: i32| 1_u64 << (signal - 1);
    let all_but = |unblocked: &[i32]| {
        unblocked
            .iter()
            .fold(u64::MAX, |mask, &signal| mask & !bit(signal))
    };
    // Linux never blocks these two.
    let native_mask = all_but(&[libc::SIGKILL, libc::SIGSTOP]);
    // The ptrace backend also leaves these two unblocked:
    // - SIGSTKFLT, reverie's perf-event timer signal (`reverie::PERF_EVENT_SIGNAL`).
    //   Detcore removes it from every mask a guest installs
    //   (`without_perf_event_signal` in `detcore/src/syscalls/signal.rs`).
    // - SIGTRAP. Detcore does not pass the guest's own blocking
    //   `rt_sigprocmask` through: it injects a copy with the adjusted set
    //   (`handle_rt_sigprocmask` in `detcore/src/syscalls/signal.rs`). Reverie's
    //   ptrace backend single-steps every injected call that is not the
    //   guest's original system call (`step_private_syscall`), and a step trap
    //   the guest has blocked is forced through, which unblocks SIGTRAP and
    //   resets its handler (https://github.com/rrnewton/reverie/issues/682,
    //   https://github.com/rrnewton/reverie/issues/879). Linux keeps it blocked.
    let ptrace_mask = all_but(&[libc::SIGKILL, libc::SIGSTOP, libc::SIGSTKFLT, libc::SIGTRAP]);
    // In-guest LiteInst has no tracer and leaves SIGTRAP blocked. It leaves
    // these three unblocked:
    // - SIGSTKFLT, for the same Detcore reason.
    // - SIGSYS and SIGSEGV. The guest runtime keeps both for itself and removes
    //   them from any set the guest installs
    //   (https://github.com/rrnewton/reverie/pull/913). SIGSYS is the seccomp
    //   trap for system calls that are not patched. SIGSEGV is how the kernel
    //   reports a trapped RDTSC or CPUID instruction. The runtime keeps it
    //   while either is trapped, and Detcore's default configuration traps
    //   RDTSC. So a guest that blocks either signal reads back a mask that
    //   differs from Linux (https://github.com/rrnewton/reverie/issues/915).
    let in_guest_mask = all_but(&[
        libc::SIGKILL,
        libc::SIGSTOP,
        libc::SIGSTKFLT,
        libc::SIGSYS,
        libc::SIGSEGV,
    ]);

    let expected_stdout = |mode: &str, block_all_mask: u64| -> String {
        match mode {
            "sigchld-during-wait" => "sigchld-during-wait slow=5 fast=3\nmask-after=0\n".to_owned(),
            "ignored-during-wait" => "ignored-during-wait child=5\nmask-after=0\n".to_owned(),
            "ignored-then-restart" => {
                "ignored-then-restart waited=5 signaller=7\nmask-after=0\n".to_owned()
            }
            "terminate-during-wait" => "terminate-during-wait waiting\n".to_owned(),
            "terminate-then-exit" => "terminate-then-exit waiting\n".to_owned(),
            "guest-blocked" => {
                "guest-blocked child=5 pending=1 taken=12 pending-after=0\nmask-after=0x800\n"
                    .to_owned()
            }
            "block-all" => format!(
                "how=-1 without a set: result=0 old-matches=1\n\
                 how=-1 with a set: result=-1 errno=EINVAL\n\
                 mask unchanged by the refused call=1\n\
                 block-all mask={block_all_mask:#x}\n\
                 block-all child=5 mask-kept=1\n\
                 block-all SIGUSR1 pending=1 SIGCHLD pending=1\n\
                 block-all taken=10\n"
            ),
            "write-only-set" => "write-only-set SIGUSR2 blocked=1\n".to_owned(),
            "handler-after-reap" => {
                "handler-after-reap child=5 before=0 handled=1\nmask-after=0\n".to_owned()
            }
            _ => panic!("no expected output for mode {mode}"),
        }
    };
    // Each mode, how it ends, and how in-guest LiteInst is checked.
    let modes = [
        ("sigchld-during-wait", Exit::Code(0), InGuest::Same),
        ("ignored-during-wait", Exit::Code(0), InGuest::Same),
        ("ignored-then-restart", Exit::Code(0), InGuest::Same),
        (
            "terminate-during-wait",
            Exit::Signal(libc::SIGUSR1),
            InGuest::Skipped("https://github.com/rrnewton/hermit/issues/3688"),
        ),
        (
            "terminate-then-exit",
            Exit::Signal(libc::SIGUSR1),
            InGuest::Same,
        ),
        ("guest-blocked", Exit::Code(0), InGuest::Same),
        ("block-all", Exit::Code(0), InGuest::Same),
        (
            "write-only-set",
            Exit::Code(0),
            InGuest::KnownBad {
                issue: "https://github.com/rrnewton/reverie/issues/921",
                code: 1,
                stderr: "FAIL block SIGUSR2 from a write-only page: Bad address\n",
            },
        ),
        (
            "handler-after-reap",
            Exit::Code(0),
            InGuest::KnownBad {
                issue: "https://github.com/rrnewton/reverie/issues/243",
                code: 1,
                stderr: "FAIL sigaction: Operation not permitted\n",
            },
        ),
    ];

    // One directory per test process, so two processes running this test at
    // once cannot overwrite each other's logs and status files; the Hermit
    // run lock serializes runs only within one process.
    let log_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "liteinst-in-guest-wait-signals/logs/{}",
        std::process::id()
    ));
    fs::create_dir_all(&log_root).expect("failed to create the wait-signals log directory");

    let remove_stale = |path: &Path| match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove {}: {error}", path.display()),
    };

    // One run of `mode`, with the Hermit INFO log written to `log` (Hermit
    // runners only) and, for terminate-then-exit, the guest's status word in
    // `status_file`.
    let run = |runner: &str,
               mode: &str,
               log: Option<&Path>,
               status_file: Option<&Path>|
     -> (ExitStatus, String, String) {
        let guest = liteinst_in_guest_wait_signals_guest()
            .to_str()
            .expect("guest path is not UTF-8");
        let mut guest_args = vec![mode];
        if let Some(status_file) = status_file {
            remove_stale(status_file);
            guest_args.push(status_file.to_str().expect("status path is not UTF-8"));
        }
        // Natively, nothing makes handler-after-reap's child exit after its
        // parent is asleep in `wait4`, so the child also waits until Linux
        // reports that (see the guest; it needs Linux 5.16 or later and fails
        // on an older kernel).
        if runner == "native" && mode == "handler-after-reap" {
            guest_args.push("wait-for-parent-asleep");
        }
        let log_args = log.map(|log| {
            remove_stale(log);
            [
                "--log=info".to_owned(),
                format!("--log-file={}", log.display()),
            ]
        });
        let hermit = |backend: &str| {
            let log_args = log_args
                .as_ref()
                .unwrap_or_else(|| panic!("{runner} {mode}: a Hermit run needs a log path"));
            let mut args: Vec<&str> = log_args.iter().map(String::as_str).collect();
            args.extend([
                "--backend",
                backend,
                "run",
                "--max-timeslice=disabled",
                "--",
                guest,
            ]);
            args.extend(&guest_args);
            hermit_command(&args)
        };
        let mut command = match runner {
            "native" => {
                let mut command = Command::new(guest);
                command.args(&guest_args);
                command
            }
            "ptrace" => hermit("ptrace"),
            "in-guest" => hermit("liteinst"),
            _ => panic!("unknown runner {runner}"),
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap_or_else(|error| panic!("{runner} {mode}: failed to spawn: {error}"));
        // Read both pipes on their own threads, so a full pipe cannot stall the
        // run and an open one cannot stall the test past its deadline.
        let (sender, receiver) = mpsc::channel();
        let pipes: [Box<dyn Read + Send>; 2] = [
            Box::new(child.stdout.take().expect("piped stdout")),
            Box::new(child.stderr.take().expect("piped stderr")),
        ];
        for (stream, mut pipe) in pipes.into_iter().enumerate() {
            let sender = sender.clone();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = pipe.read_to_end(&mut bytes).map(|_| bytes);
                let _ = sender.send((stream, result));
            });
        }
        drop(sender);
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            if let Some(status) = child.try_wait().expect("failed to poll the run") {
                break status;
            }
            if Instant::now() >= deadline {
                // SAFETY: `killpg` has no memory-safety preconditions; the group
                // was created for this run by `process_group(0)`, and its leader
                // has not been reaped.
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
                panic!(
                    "{runner} {mode}: still running after {DEADLINE:?}; killed its process group"
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        let mut outputs = [String::new(), String::new()];
        for _ in 0..2 {
            let (stream, result) = receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|_| {
                    panic!("{runner} {mode}: output still open 10 s after exit {status:?}")
                });
            let bytes = result.unwrap_or_else(|error| panic!("{runner} {mode}: read: {error}"));
            outputs[stream] = String::from_utf8_lossy(&bytes).into_owned();
        }
        let [stdout, stderr] = outputs;
        (status, stdout, stderr)
    };

    // The number that follows `marker` in `line`.
    let number_after = |line: &str, marker: &str| -> String {
        let start = line.find(marker).expect("marker is in the line") + marker.len();
        line[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect()
    };
    // Detcore's scheduler records that place each signal inside the parent's
    // wait. See the test's documentation.
    let check_log = |runner: &str, mode: &str, log: &str, context: &str| {
        let lines: Vec<&str> = log.lines().collect();
        // The first line at or after `from` that contains every one of `parts`.
        let find = |from: usize, parts: &[&str]| -> Option<usize> {
            (from..lines.len()).find(|&index| parts.iter().all(|part| lines[index].contains(part)))
        };
        // The first commit at or after `from` by `dettid` on a resource that
        // starts with `resource`; an empty `resource` matches any commit.
        let commit = |from: usize, dettid: &str, resource: &str| {
            let by = format!(", dettid {dettid} using resources {{{resource}");
            find(from, &[" COMMIT turn ", &by])
        };
        let parked = |from: usize, dettid: &str, child: &str| {
            let parking = format!(
                "parking dettid {dettid} for child ChildWaitSpec {{ selector: \
                 Exact(DetPid({child}))"
            );
            find(from, &[&parking])
        };
        let found = |line: Option<usize>, what: &str| -> usize {
            line.unwrap_or_else(|| panic!("{context}\n{what}"))
        };

        let first = found(
            find(
                0,
                &[
                    " COMMIT turn ",
                    "using resources {ParentContinue { parent: DetPid(",
                ],
            ),
            "no ParentContinue commit",
        );
        let parent = number_after(lines[first], ", dettid ");
        assert!(
            lines[first].contains(&format!(
                "ParentContinue {{ parent: DetPid({parent}), child: DetPid({parent}) }}"
            )),
            "{context}\nthe first ParentContinue commit is not the parent's own"
        );
        let forked =
            format!("using resources {{ParentContinue {{ parent: DetPid({parent}), child: DetPid(");
        let children: Vec<String> = lines[first + 1..]
            .iter()
            .filter(|line| line.contains(" COMMIT turn ") && line.contains(&forked))
            .map(|line| number_after(line, "child: DetPid("))
            .collect();
        let forks = |count: usize| {
            assert_eq!(
                children.len(),
                count,
                "{context}\nthe parent forks children {children:?}"
            );
        };
        let exited = |child: &str| {
            commit(
                0,
                child,
                &format!("Exit {{ group: true, process: DetPid({child}),"),
            )
        };
        // The parent's first finished `wait4` at or after `from`. Only ptrace
        // logs have system-call lines.
        let wait4_finished = |from: usize| {
            let by = format!("[detcore, dtid {parent}] finish syscall #");
            find(from, &[&by, ": wait4("])
        };
        let ended = |line: Option<usize>, child: &str, result: &str| {
            line.is_some_and(|line| {
                lines[line].contains(&format!(": wait4({child}, "))
                    && lines[line].contains(&format!(" = {result}"))
            })
        };
        let is_on = |line: Option<usize>, resource: &str| {
            line.is_some_and(|line| lines[line].contains(&format!("using resources {{{resource}")))
        };
        let died = format!("guest terminated by signal tid={parent} pid={parent} signal=SIGUSR1");

        match mode {
            // Neither waits nor signals.
            "write-only-set" => {}
            "sigchld-during-wait" => {
                forks(2);
                let (fast, slow) = (&children[0], &children[1]);
                let parked_on_slow = found(
                    parked(0, &parent, slow),
                    "the parent never parks on the slow child",
                );
                let sigchld = commit(parked_on_slow + 1, &parent, "");
                assert!(
                    is_on(sigchld, "InboundSignal(SigWrapper(17)): W}"),
                    "{context}\nafter parking on the slow child, the parent's first commit is \
                     not on SIGCHLD"
                );
                let sigchld = found(sigchld, "no SIGCHLD commit");
                let fast_exit = found(exited(fast), "the fast child never exits");
                let slow_exit = found(exited(slow), "the slow child never exits");
                assert!(
                    fast_exit < sigchld && sigchld < slow_exit,
                    "{context}\nthe SIGCHLD commit (line {sigchld}) is not after the fast \
                     child's Exit (line {fast_exit}) and before the slow child's (line \
                     {slow_exit})"
                );
                let parked_again = found(
                    parked(sigchld + 1, &parent, slow).filter(|&line| line < slow_exit),
                    "the parent does not park on the slow child again before it exits",
                );
                if runner == "ptrace" {
                    assert!(
                        ended(
                            wait4_finished(sigchld).filter(|&line| line < parked_again),
                            slow,
                            "Err(Errno(ERESTARTSYS))"
                        ),
                        "{context}\nthe interrupted wait4 does not end in ERESTARTSYS"
                    );
                }
            }
            "ignored-then-restart" => {
                forks(2);
                let waited = &children[0];
                let pending = found(
                    commit(0, &parent, "WaitidSignals([SigWrapper(10)]): W}"),
                    "no commit on SIGUSR1 pending in the wait",
                );
                let waited_exit = found(
                    exited(waited).filter(|&line| line > pending),
                    "the waited child does not exit after the parent's WaitidSignals commit",
                );
                let parked_after = found(
                    parked(pending + 1, &parent, waited).filter(|&line| line < waited_exit),
                    "the parent does not park on the waited child after the signal commit, \
                     before the child exits",
                );
                if runner == "ptrace" {
                    assert!(
                        ended(
                            wait4_finished(pending).filter(|&line| line < parked_after),
                            waited,
                            "Err(Errno(ERESTARTSYS))"
                        ),
                        "{context}\nthe interrupted wait4 does not end in ERESTARTSYS"
                    );
                }
            }
            "terminate-during-wait" => {
                forks(1);
                let child = &children[0];
                let pending = found(
                    commit(0, &parent, "WaitidSignals([SigWrapper(10)]): W}"),
                    "no commit on SIGUSR1 pending in the wait",
                );
                assert!(
                    exited(child).is_none_or(|line| line > pending),
                    "{context}\nthe child exits before the parent's WaitidSignals commit"
                );
                if runner == "ptrace" {
                    let finished = wait4_finished(pending);
                    assert!(
                        ended(finished, child, "Err(Errno(ERESTARTSYS))"),
                        "{context}\nthe interrupted wait4 does not end in ERESTARTSYS"
                    );
                    let delivered = finished.and_then(|line| {
                        commit(line, &parent, "InboundSignal(SigWrapper(10)): RW}")
                    });
                    assert!(
                        delivered.and_then(|line| find(line, &[&died])).is_some(),
                        "{context}\nthe parent is not delivered SIGUSR1 and killed by it after \
                         the wait4"
                    );
                }
            }
            "handler-after-reap" => {
                forks(1);
                let child = &children[0];
                let parked_on_child = found(
                    parked(0, &parent, child),
                    "the parent never parks on the child",
                );
                let child_exit = found(
                    exited(child).filter(|&line| line > parked_on_child),
                    "the child does not exit after the parent parks on it",
                );
                let queued = found(
                    find(
                        child_exit,
                        &[&format!(
                            "[dtid {parent}] Alarm fired, delivering signal SIGCHLD"
                        )],
                    ),
                    "SIGCHLD is never queued for the parent after the child's Exit",
                );
                let reaped = commit(parked_on_child + 1, &parent, "");
                assert!(
                    reaped.is_some_and(|line| line > queued)
                        && is_on(
                            reaped,
                            &format!(
                                "WaitChild {{ parent: DetPid({parent}), spec: ChildWaitSpec {{ \
                                 selector: Exact(DetPid({child})),"
                            )
                        ),
                    "{context}\nafter parking, the parent's first commit is not the reap after \
                     the child's Exit and the queued SIGCHLD"
                );
                if runner == "ptrace" {
                    let finished = wait4_finished(found(reaped, "no reap"));
                    assert!(
                        ended(finished, child, &format!("Ok({child})")),
                        "{context}\nthe wait4 does not return the child"
                    );
                    assert!(
                        is_on(
                            finished.and_then(|line| commit(line, &parent, "")),
                            "InboundSignal(SigWrapper(17)): RW}"
                        ),
                        "{context}\nafter the wait4, the parent's next commit is not the delivery \
                         of SIGCHLD"
                    );
                }
            }
            "ignored-during-wait" | "terminate-then-exit" | "guest-blocked" | "block-all" => {
                forks(1);
                let child = &children[0];
                let signal = if mode == "guest-blocked" {
                    libc::SIGUSR2
                } else {
                    libc::SIGUSR1
                };
                let pending = found(
                    commit(
                        0,
                        &parent,
                        &format!("WaitidSignals([SigWrapper({signal})]): W}}"),
                    ),
                    &format!("no commit on signal {signal} pending in the wait"),
                );
                assert!(
                    exited(child).is_some_and(|line| line < pending),
                    "{context}\nthe child does not exit before the parent's commit on signal \
                     {signal} pending in the wait"
                );
                if runner == "ptrace" {
                    let finished = wait4_finished(pending);
                    assert!(
                        ended(finished, child, &format!("Ok({child})")),
                        "{context}\nthe wait4 does not return the child"
                    );
                    if matches!(mode, "ignored-during-wait" | "terminate-then-exit") {
                        let delivered = finished.and_then(|line| commit(line, &parent, ""));
                        assert!(
                            is_on(
                                delivered,
                                &format!("InboundSignal(SigWrapper({signal})): RW}}")
                            ),
                            "{context}\nafter the wait4, the parent's next commit is not the \
                             delivery of signal {signal}"
                        );
                        if mode == "terminate-then-exit" {
                            assert!(
                                delivered.and_then(|line| find(line, &[&died])).is_some(),
                                "{context}\nthe parent is not killed by SIGUSR1"
                            );
                        }
                    }
                }
            }
            _ => panic!("no scheduler records to check for mode {mode}"),
        }
    };

    // An in-guest run's stderr must show that the guest preload hosted Detcore.
    const IN_GUEST_SELECTED: &str =
        "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";
    let check_in_guest_selected = |stderr: &str, context: &str| {
        assert!(
            stderr.lines().any(|line| line == IN_GUEST_SELECTED),
            "{context}\nno line of stderr is Hermit's in-guest selection line"
        );
        assert!(!stderr.contains("[liteinst host hybrid]"), "{context}");
    };

    // terminate-then-exit's guest sets its status word to this before its
    // `wait4`; `wait4` never stores it.
    const STATUS_UNWRITTEN: i32 = 0x5a5a_5a5a;
    // The status `wait4` stores for a child that exited with 5.
    const EXITED_5: i32 = 5 << 8;
    let check_reaped = |runner: &str, status_file: &Path, context: &str| {
        let bytes = fs::read(status_file).unwrap_or_else(|error| {
            panic!(
                "{context}\nfailed to read {}: {error}",
                status_file.display()
            )
        });
        let word = i32::from_ne_bytes(bytes.as_slice().try_into().unwrap_or_else(|_| {
            panic!(
                "{context}\n{} holds {} bytes, not 4",
                status_file.display(),
                bytes.len()
            )
        }));
        if runner == "native" {
            assert!(
                word == EXITED_5 || word == STATUS_UNWRITTEN,
                "{context}\nthe status word is {word:#x}"
            );
        } else {
            assert_eq!(
                word, EXITED_5,
                "{context}\nthe parent died before its wait4 stored the child's status \
                 ({STATUS_UNWRITTEN:#x} means wait4 never wrote the word)"
            );
        }
    };

    let check = |runner: &str, mode: &str, exit: Exit, block_all_mask: u64, attempt: usize| {
        let log =
            (runner != "native").then(|| log_root.join(format!("{runner}-{mode}-{attempt}.log")));
        let status_file = (mode == "terminate-then-exit")
            .then(|| log_root.join(format!("{runner}-{mode}-{attempt}.status")));
        let (status, stdout, stderr) = run(runner, mode, log.as_deref(), status_file.as_deref());
        let mut context = format!(
            "{runner} run {attempt} of {mode}: status {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        if let Some(log) = &log {
            context.push_str(&format!("\nHermit log: {}", log.display()));
        }
        match exit {
            Exit::Signal(signal) => assert_eq!(status.signal(), Some(signal), "{context}"),
            Exit::Code(code) => assert_eq!(status.code(), Some(code), "{context}"),
        }
        assert_eq!(stdout, expected_stdout(mode, block_all_mask), "{context}");
        assert!(
            !stderr.lines().any(|line| line.starts_with("FAIL ")),
            "{context}\nthe guest reported a failure"
        );
        if runner == "in-guest" {
            check_in_guest_selected(&stderr, &context);
        }
        if let Some(status_file) = &status_file {
            check_reaped(runner, status_file, &context);
        }
        if let Some(log) = &log {
            let text = fs::read_to_string(log)
                .unwrap_or_else(|error| panic!("{context}\nfailed to read the log: {error}"));
            check_log(runner, mode, &text, &context);
        }
    };

    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    for (mode, exit, in_guest) in modes {
        check("native", mode, exit, native_mask, 1);
        check("ptrace", mode, exit, ptrace_mask, 1);
        match in_guest {
            InGuest::Same => {
                for attempt in 1..=IN_GUEST_RUNS {
                    check("in-guest", mode, exit, in_guest_mask, attempt);
                }
            }
            InGuest::Skipped(issue) => eprintln!("{mode}: not run in-guest until {issue} is fixed"),
            InGuest::KnownBad {
                issue,
                code,
                stderr: expected_stderr,
            } => {
                for attempt in 1..=IN_GUEST_RUNS {
                    let log = log_root.join(format!("in-guest-{mode}-{attempt}.log"));
                    let (status, stdout, stderr) = run("in-guest", mode, Some(&log), None);
                    let context = format!(
                        "in-guest run {attempt} of {mode}, expected to fail as {issue} \
                         describes; if it is fixed, check this mode like the others: status \
                         {status:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                    );
                    assert_eq!(status.code(), Some(code), "{context}");
                    assert_eq!(stdout, "", "{context}");
                    check_in_guest_selected(&stderr, &context);
                    // Hermit's selection line, then only the failure the issue
                    // describes.
                    let (selected, failure) = stderr.split_once('\n').unwrap_or((&stderr, ""));
                    assert_eq!(
                        selected, IN_GUEST_SELECTED,
                        "{context}\nthe first stderr line is not Hermit's selection line"
                    );
                    assert_eq!(failure, expected_stderr, "{context}");
                }
            }
        }
    }

    // Every run passed; a failing one panics above and keeps its files. Remove
    // this process's logs and guest build so passing runs leave nothing behind.
    fs::remove_dir_all(&log_root).expect("failed to remove the wait-signals log directory");
    let build_root = liteinst_in_guest_wait_signals_guest()
        .parent()
        .expect("the wait-signals guest has no directory");
    fs::remove_dir_all(build_root).expect("failed to remove the wait-signals guest directory");
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

#[test]
fn run_kvm_waitid_terminal_copyout_preserves_arenas_and_child_lifecycle() {
    kvm_waitid_copyout::run("terminal");
}

#[test]
fn run_kvm_waitid_error_copyout_preserves_arenas_and_interrupt_precedence() {
    kvm_waitid_copyout::run("errors");
}

#[test]
fn run_kvm_waitid_sibling_leader_child_pid_success() {
    kvm_waitid_copyout::run_sibling("leader-child", "pid", "success");
}

#[test]
fn run_kvm_waitid_sibling_leader_child_pid_efault() {
    kvm_waitid_copyout::run_sibling("leader-child", "pid", "efault");
}

#[test]
fn run_kvm_waitid_sibling_leader_child_all_success() {
    kvm_waitid_copyout::run_sibling("leader-child", "all", "success");
}

#[test]
fn run_kvm_waitid_sibling_leader_child_all_efault() {
    kvm_waitid_copyout::run_sibling("leader-child", "all", "efault");
}

#[test]
fn run_kvm_waitid_sibling_worker_child_pid_success() {
    kvm_waitid_copyout::run_sibling("worker-child", "pid", "success");
}

#[test]
fn run_kvm_waitid_sibling_worker_child_pid_efault() {
    kvm_waitid_copyout::run_sibling("worker-child", "pid", "efault");
}

#[test]
fn run_kvm_waitid_sibling_worker_child_all_success() {
    kvm_waitid_copyout::run_sibling("worker-child", "all", "success");
}

#[test]
fn run_kvm_waitid_sibling_worker_child_all_efault() {
    kvm_waitid_copyout::run_sibling("worker-child", "all", "efault");
}

#[test]
fn run_kvm_wait4_fault_consumption_preserves_waitid_and_children_cpu() {
    kvm_waitid_copyout::run_wait4_fault();
}

#[test]
fn run_kvm_wait4_int_min_preserves_errno_and_arenas() {
    kvm_waitid_copyout::run_wait4_int_min();
}

#[test]
fn run_kvm_wait4_nothread_admits_only_the_creating_thread() {
    kvm_waitid_copyout::run_wait4_nothread();
}

#[test]
fn run_kvm_waitid_parked_foreign_waiters_contend_for_one_child() {
    kvm_waitid_copyout::run_parked_foreign_waiters("waitid");
}

#[test]
fn run_kvm_wait4_parked_foreign_waiters_contend_for_one_child() {
    kvm_waitid_copyout::run_parked_foreign_waiters("wait4");
}

/// A `ppoll` with a timeout returns on time while another thread spins without
/// syscalls (https://github.com/rrnewton/hermit/issues/3952). The guest waits
/// five times for 50 ms each; every wait must return 0 after at least 50 ms and
/// less than 100 ms of virtual time. Before the poll-deadline wake each took
/// about 1.05 s: the poller was backed off after its first retry and waited up
/// to 200 of the spinner's timeslices for a poll upgrade.
///
/// PMU SUBJECT: the spinner makes no syscall and is preempted only by the PMU
/// timer (`--max-timeslice`), so without a PMU nothing interleaves the two
/// threads and this case proves nothing. test.cli and test.cli_on_host skip it
/// by exact name; privileged-test.pmu_cli_cases runs it.
///
/// Because the PMU timer is its subject, a run Hermit refuses for skid
/// overshoot (exit 122 with the typed refusal line) is run again, at most
/// [`SKID_RETRY_MAX_ATTEMPTS`] times in all, and every retry is printed
/// (https://github.com/rrnewton/hermit/issues/1845); see
/// [`run_pmu_spinner_retrying_skid`]. Any other outcome is asserted as before.
#[test]
fn poll_timeout_returns_on_time_while_another_thread_spins() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let guest = poll_timeout_vs_spinner_guest().to_str().unwrap().to_owned();
    // The guest's five waits, each of which must time out within 50-100 ms of
    // virtual time.
    fn check_waits(stdout: &str) {
        let waits: Vec<(i32, u64)> = stdout
            .lines()
            .map(|line| {
                let (rc, elapsed) = line
                    .strip_prefix("ppoll rc=")
                    .and_then(|rest| rest.split_once(" elapsed_ns="))
                    .unwrap_or_else(|| panic!("unexpected guest output line {line:?}"));
                (rc.parse().unwrap(), elapsed.parse().unwrap())
            })
            .collect();
        assert_eq!(waits.len(), 5, "five waits expected:\n{stdout}");
        for (rc, elapsed_ns) in waits {
            assert_eq!(rc, 0, "every wait must time out:\n{stdout}");
            assert!(
                (50_000_000..100_000_000).contains(&elapsed_ns),
                "a 50 ms ppoll returned after {elapsed_ns} ns of virtual time:\n{stdout}"
            );
        }
    }
    // A deadline, so the old behaviour (about 5 s of virtual time and minutes
    // of wall) fails with a message instead of stalling the suite.
    let SpinnerRun {
        status,
        stdout,
        stderr: log,
    } = run_pmu_spinner_retrying_skid(
        "poll_timeout_returns_on_time_while_another_thread_spins",
        &[
            "run",
            "--strict",
            "--max-timeslice",
            "5000000",
            "--",
            guest.as_str(),
        ],
        Duration::from_secs(120),
        check_waits,
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish within 120 s and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    check_waits(&stdout);
}

/// A vfork parent killed while its vfork child runs no longer stops the
/// schedule (https://github.com/rrnewton/hermit/issues/3984). The child kills
/// its parent and exits; main waits for the parent and must report
/// `A killed by signal 9`, as natively. Before the fix the killed parent left
/// its vfork barrier behind and the run hung until Hermit's `--timeout`.
/// Run mode only: `--verify-strict` on this guest also meets the separate,
/// pre-existing host timing of a cross-process SIGKILL on ptrace.
#[test]
fn a_killed_vfork_parent_does_not_stop_the_schedule() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = vfork_parent_killed_guest().to_str().unwrap().to_owned();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "30", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(60),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(stdout, "A killed by signal 9\n", "stderr:\n{log}");
}

/// A vfork child that kills its parent and then execs into a program that
/// forks and waits must not keep the vfork serialization after its exec
/// (https://github.com/rrnewton/hermit/issues/3984): its own child has to run.
/// main must report `A killed by signal 9`, then the exec'd image's
/// `forkwait ok`, as natively.
#[test]
fn a_vfork_child_that_kills_its_parent_and_execs_lets_its_own_child_run() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = vfork_kill_exec_guest().to_str().unwrap().to_owned();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "30", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(60),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout, "A killed by signal 9\nforkwait ok\n",
        "stderr:\n{log}"
    );
}

/// A `FUTEX_WAKE` count of 0, and a count of 0xffffffff (negative as the
/// kernel's `int nr_wake`), each wake one waiter, as Linux's `futex_wake`
/// does; a count of 0 with nobody waiting wakes nobody
/// (https://github.com/rrnewton/hermit/issues/3957). Before the fix the
/// count-0 wake never woke the waiter, so the guest gave up and exited 1, and
/// the 0xffffffff wake panicked Detcore's scheduler.
#[test]
fn a_futex_wake_count_of_zero_or_negative_wakes_one_waiter() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = futex_wake_count_zero_guest().to_str().unwrap().to_owned();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "60", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout, "count 0 woke 1\ncount 0xffffffff woke 1\ncount 0 with no waiter woke 0\n",
        "stderr:\n{log}"
    );
}

/// A process-directed signal to a process with two live threads is refused by
/// name (https://github.com/rrnewton/hermit/issues/4034): Linux's
/// `complete_signal` would give it to any thread that wants it, and Detcore
/// does not model that choice. Before the change `kill` and `rt_sigqueueinfo`
/// returned ENOSYS as an ordinary errno, so a fail-closed run went on with a
/// failure Linux never returns. A fail-closed run now stops with the
/// policy-refusal status after the one-thread block, which still delivers; with
/// `--allow-unsupported-syscalls` both calls return ENOSYS, as before.
#[test]
fn a_process_directed_signal_to_several_threads_is_refused_by_name() {
    let guest = kill_multithreaded_process_guest()
        .to_str()
        .unwrap()
        .to_owned();
    let one_thread =
        "one thread, kill: 0\none thread, rt_sigqueueinfo: 0\none thread, handled: 2\n";
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--log=error",
            "run",
            "--strict",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a fail-closed run must stop with the policy-refusal status:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert!(
        log.contains("process-directed signal 10 to process")
            && log.contains("by kill is not supported: the process has 2 live threads"),
        "the refusal must name the call and the reason:\nstderr:\n{log}"
    );
    assert_eq!(stdout, one_thread, "stderr:\n{log}");
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        // `--strict` refuses the opt-out, so the compatibility run is not strict.
        hermit_command(&[
            "run",
            "--allow-unsupported-syscalls",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        format!(
            "{one_thread}two threads, kill: -1 ENOSYS\ntwo threads, rt_sigqueueinfo: -1 ENOSYS\n"
        ),
        "stderr:\n{log}"
    );
    assert!(
        log.contains("by rt_sigqueueinfo is not supported"),
        "the compatibility run must still name each refusal:\nstderr:\n{log}"
    );
}

/// A signal addressed to a process group
/// (https://github.com/rrnewton/hermit/issues/4046). The guest first leads its
/// own process group (3 in Hermit's PID namespace), so its negative pids are
/// real ones. `rt_sigqueueinfo` has no process groups: for pid 0, `-pgrp` or -1
/// Linux returns ESRCH and sends nothing, and Hermit forwards the call and gets
/// the same answer (it returned ENOSYS before). `kill(INT_MIN, sig)` is ESRCH
/// in Linux, SIGKILL included, and Hermit forwards it rather than overflowing
/// on its negation (the process-group SIGKILL arm negates the pid too).
/// `kill(0, sig)`, `kill(-pgrp, sig)` and `kill(-1, sig)` signal every process
/// in a set, which Detcore does not model:
/// - a fail-closed run stops with the policy-refusal status at `kill(0)` and
///   names it (it returned ENOSYS as a plain errno before);
/// - `--allow-unsupported-syscalls` returns ENOSYS for each and names each
///   refusal with its target.
#[test]
fn a_signal_to_a_process_group_is_refused_by_name() {
    let guest = kill_process_group_guest().to_str().unwrap().to_owned();
    let answered = "own group: yes\n\
                    rt_sigqueueinfo(0): -1 ESRCH, handled 0\n\
                    rt_sigqueueinfo(-pgrp): -1 ESRCH, handled 0\n\
                    rt_sigqueueinfo(-1): -1 ESRCH, handled 0\n\
                    kill(INT_MIN): -1 ESRCH, handled 0\n\
                    kill(INT_MIN, SIGKILL): -1 ESRCH, handled 0\n";
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--log=error",
            "run",
            "--strict",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
            "broadcast",
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a fail-closed run must stop with the policy-refusal status:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert!(
        log.contains("signal 10 to its own process group by kill is not supported"),
        "the refusal must name the call and its target:\nstderr:\n{log}"
    );
    assert_eq!(stdout, answered, "stderr:\n{log}");
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        // `--strict` refuses the opt-out, so the compatibility run is not strict.
        hermit_command(&[
            "run",
            "--allow-unsupported-syscalls",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
            "broadcast",
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        format!(
            "{answered}kill(0): -1 ENOSYS, handled 0\n\
             kill(-pgrp): -1 ENOSYS, handled 0\n\
             kill(-1): -1 ENOSYS, handled 0\n"
        ),
        "stderr:\n{log}"
    );
    for target in [
        "its own process group",
        "process group 3",
        "every process it may signal",
    ] {
        assert_eq!(
            log.matches(&format!("signal 10 to {target} by kill is not supported"))
                .count(),
            1,
            "the compatibility run must name the refusal for {target}:\nstderr:\n{log}"
        );
    }
}

/// Under `--no-sequentialize-threads` a `kill` to a process group or to every
/// process never reaches the host kernel: the guest's process group is
/// inherited from Hermit's, so `kill(0)` would signal Hermit and the processes
/// that started it (https://github.com/rrnewton/hermit/issues/4057). Two modes
/// of the guest:
/// - "inherited" keeps that group. Signal 0 to it sends nothing and is
///   forwarded with Linux's answer; then `kill(0)` is refused (inside the PID
///   namespace the inherited group reads as 0, so `kill(-pgrp)` is `kill(0)`
///   again), and so is a last `kill(0, SIGKILL)`.
/// - "broadcast" first leads its own group (3 in the namespace), so
///   `kill(-pgrp)` sends a real negative pid, and it also sends `kill(-1)`
///   (the second Claude review of https://github.com/rrnewton/hermit/pull/4062:
///   a refusal of pid 0 alone passed "inherited").
///
/// A fail-closed run stops with the policy-refusal status at the first group
/// kill and names the reason; `--allow-unsupported-syscalls` returns ENOSYS
/// for each and names each refusal with its target, and Hermit survives to exit
/// 0 (before the fix, the forwarded `kill(0, SIGUSR1)` killed Hermit itself,
/// exit 138). Hermit runs in a session of its own, so even a regression cannot
/// signal the test runner.
#[test]
fn group_kills_never_reach_the_host_without_sequentialized_threads() {
    use std::os::unix::process::CommandExt;
    let guest = kill_process_group_guest().to_str().unwrap().to_owned();
    let answered = |own: &str| {
        format!(
            "own group: {own}\n\
             rt_sigqueueinfo(0): -1 ESRCH, handled 0\n\
             rt_sigqueueinfo(-pgrp): -1 ESRCH, handled 0\n\
             rt_sigqueueinfo(-1): -1 ESRCH, handled 0\n\
             kill(INT_MIN): -1 ESRCH, handled 0\n\
             kill(INT_MIN, SIGKILL): -1 ESRCH, handled 0\n"
        )
    };
    let inherited = answered("no").replacen(
        "own group: no\n",
        "own group: no\nkill(0, 0): 0, handled 0\n",
        1,
    );
    let broadcast = answered("yes");
    let in_own_session = |args: &[&str]| {
        let mut command = hermit_command(args);
        // SAFETY: setsid is async-signal-safe and touches only the child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        command
    };
    let refused = |target: &str| {
        format!(
            "signal 10 to {target} by kill is not supported: with threads not sequentialized \
             (--no-sequentialize-threads, --strace-only)"
        )
    };
    let cases = [
        (
            "inherited",
            false,
            HERMIT_POLICY_REFUSAL_EXIT,
            inherited.clone(),
            vec!["its own process group"],
        ),
        (
            "inherited",
            true,
            0,
            format!(
                "{inherited}kill(0): -1 ENOSYS, handled 0\nkill(-pgrp): -1 ENOSYS, handled 0\n\
                 kill(0, SIGKILL): -1 ENOSYS, handled 0\n"
            ),
            vec!["its own process group", "its own process group"],
        ),
        (
            "broadcast",
            false,
            HERMIT_POLICY_REFUSAL_EXIT,
            broadcast.clone(),
            vec!["its own process group"],
        ),
        (
            "broadcast",
            true,
            0,
            format!(
                "{broadcast}kill(0): -1 ENOSYS, handled 0\nkill(-pgrp): -1 ENOSYS, handled 0\n\
                 kill(-1): -1 ENOSYS, handled 0\n"
            ),
            vec![
                "its own process group",
                "process group 3",
                "every process it may signal",
            ],
        ),
    ];
    for (mode, allow, expected_exit, expected_stdout, targets) in cases {
        let mut args = vec!["run", "--no-sequentialize-threads", "--timeout", "60"];
        if allow {
            args.push("--allow-unsupported-syscalls");
        }
        args.extend(["--", guest.as_str(), mode]);
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let (status, log) = run_with_deadline(
            in_own_session(&args),
            directory.path(),
            Duration::from_secs(120),
            false,
        );
        let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
            .expect("failed to read the guest's stdout");
        let case = format!("{mode} allow={allow}");
        assert_eq!(
            status.and_then(|status| status.code()),
            Some(expected_exit),
            "{case}: {status:?}\nstdout:\n{stdout}\nstderr:\n{log}"
        );
        assert_eq!(stdout, expected_stdout, "{case}: stderr:\n{log}");
        for target in [
            "its own process group",
            "process group 3",
            "every process it may signal",
        ] {
            let expected = targets.iter().filter(|t| **t == target).count();
            assert_eq!(
                log.matches(&refused(target)).count(),
                expected,
                "{case}: refusals naming {target}:\nstderr:\n{log}"
            );
        }
        // "inherited" ends with a SIGKILL to its group, refused like the rest.
        let sigkill = "signal 9 to its own process group by kill is not supported: with threads \
                       not sequentialized";
        assert_eq!(
            log.matches(sigkill).count(),
            usize::from(mode == "inherited" && allow),
            "{case}: the SIGKILL group kill:\nstderr:\n{log}"
        );
    }
}

/// A POSIX timer that notifies with a real-time signal
/// (https://github.com/rrnewton/hermit/issues/3893). Detcore cannot deliver
/// real-time signals, and such a timer used to be armed and then silently never
/// fire. Creating, arming, reading and deleting one before it expires behave as
/// on Linux, as do one in a process that exits or execs and one whose signal is
/// ignored and unblocked (Linux discards it); when one expires, a fail-closed
/// run stops with the policy-refusal
/// status and names the signal. A run with `--allow-unsupported-syscalls` keeps
/// the expiration undelivered, and says so.
#[test]
fn a_real_time_signal_timer_is_refused_by_name_when_it_expires() {
    let guest = timer_create_rt_signal_refused_guest()
        .to_str()
        .unwrap()
        .to_owned();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "60", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a fail-closed run must stop with the policy-refusal status:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert!(
        log.contains("a POSIX timer expired that notifies with real-time signal 35"),
        "the refusal must name the signal:\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        "unexpired SIGRTMIN+1 timer: armed 1, delete 0\n\
         child exited with a SIGRTMIN+1 timer armed: status 0\n\
         child execed with a SIGRTMIN+1 timer armed: status 0\n\
         ignored SIGRTMIN+2 timer expired: the run goes on\n",
        "unexpired, exited, execed and ignored timers must work as on Linux, and the \
         run must stop at the first delivered expiry:\nstderr:\n{log}"
    );
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        // `--strict` refuses the opt-out, so the compatibility run is not strict.
        hermit_command(&[
            "run",
            "--allow-unsupported-syscalls",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        "unexpired SIGRTMIN+1 timer: armed 1, delete 0\n\
         child exited with a SIGRTMIN+1 timer armed: status 0\n\
         child execed with a SIGRTMIN+1 timer armed: status 0\n\
         ignored SIGRTMIN+2 timer expired: the run goes on\n\
         SIGRTMIN+1 timer expired: -1 EAGAIN\n\
         SIGEV_THREAD timer fired: 0\n",
        "stderr:\n{log}"
    );
    assert!(
        log.contains("which Detcore cannot deliver; it is armed but will never fire"),
        "the permissive run must say the timer will never fire:\nstderr:\n{log}"
    );
}

/// FUTEX_REQUEUE, FUTEX_CMP_REQUEUE and FUTEX_WAKE_OP give Linux's results under
/// precise futex mode, and so do the commands Linux answers with an errno
/// (https://github.com/rrnewton/hermit/issues/3958). The expected output is the
/// guest's native output on Linux; before the fix the first FUTEX_REQUEUE
/// panicked Detcore ("futex op not handled yet").
#[test]
fn futex_requeue_and_wake_op_match_linux() {
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let guest = futex_c_guest(&FUTEX_REQUEUE_WAKE_OP_GUEST, "futex_requeue_wake_op")
        .to_str()
        .unwrap()
        .to_owned();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "60", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        "requeue nr_requeue -1: -1 EINVAL\n\
         requeue nr_wake -1: -1 EINVAL\n\
         cmp_requeue val3 mismatch: -1 EAGAIN\n\
         cmp_requeue nr_requeue -1, val3 mismatch: -1 EINVAL\n\
         requeue to misaligned: -1 EINVAL\n\
         wake misaligned: -1 EINVAL\n\
         futex_fd: -1 ENOSYS\n\
         unknown command 99: -1 ENOSYS\n\
         wake with clock_realtime: -1 ENOSYS\n\
         requeue with no waiters: 0\n\
         cmp_requeue A->B wake 0 move all: 3\n\
         wake A after the move: 0\n\
         wake B 1: 1\n\
         wake B all: 2\n\
         requeue A->B wake 1 move 1: 2\n\
         wake B after requeue: 1\n\
         wake_op set B=5 if old==0: 2\n\
         B is 5\n\
         wake_op B+=3 if old>100: 0\n\
         B is 8\n\
         wake_op B|=1<<4 if old!=8: 0\n\
         B is 24\n\
         wake_op unknown op 7: -1 ENOSYS\n\
         B is 24\n\
         wake_op unknown cmp 6: -1 ENOSYS\n\
         B is 1\n\
         wake_op uaddr2 NULL: -1 EFAULT\n\
         wake_op uaddr2 NULL unknown op 7: -1 ENOSYS\n\
         requeue A->B wake 0 move 0: 0\n\
         queued on A after it: 2\n\
         wake_op nr_wake 0: 1\n\
         wake_op nr_wake -1: 1\n\
         wake_op nr_wake2 0: 1\n\
         wake_op nr_wake2 -1: 1\n\
         cmp_requeue A->B wake 0 move 2: 2\n\
         requeue A->B wake 0 move 1: 1\n\
         wake B 1 released waiter 0\n\
         wake B 1 released waiter 1\n\
         wake B 1 released waiter 2\n\
         wake B 1 released waiter 3\n",
        "stderr:\n{log}"
    );
}

/// A futex waiter whose word changed while it was parked answers EAGAIN when a
/// signal ends its wait, as Linux does
/// (https://github.com/rrnewton/hermit/issues/4033). The guest changes the word
/// under the parked waiter without waking it, then arms a one-shot SIGALRM.
/// - By default, as natively, the signal goes to the spinning main thread and
///   the waiter is woken only by the final FUTEX_WAKE: 0.
/// - When the handler runs on the main thread after the scheduler woke the
///   waiter for the signal, Linux's woken waiter keeps TIF_SIGPENDING, returns
///   -ERESTARTSYS, finds nothing to deliver, and its restarted FUTEX_WAIT finds
///   the changed word: EAGAIN. (Detcore reaches the same answer by re-reading
///   the word when nothing is pending, as `__futex_wait`'s retry would.)
/// - When the handler runs on the waiter, its wait is interrupted and restarted
///   (`SA_RESTART`), and the restarted FUTEX_WAIT finds the changed word:
///   EAGAIN. Natively, with the signal forced onto the waiter, that is 30/30.
///
/// Before the fix both chaos classes answered 0, which Linux cannot produce.
/// The listed targeted-chaos seeds must each give a legal line, and both
/// EAGAIN classes must be reached, so a scheduler change that moves the seeds
/// fails loudly rather than passing without the paths.
#[test]
fn a_futex_waiter_woken_for_a_signal_another_thread_takes_rereads_its_word() {
    let guest = futex_c_guest(
        &FUTEX_REWAIT_CHANGED_WORD_GUEST,
        "futex_rewait_changed_word",
    )
    .to_str()
    .unwrap()
    .to_owned();
    let run = |extra: &[&str]| {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let mut args = vec!["run", "--strict", "--timeout", "60"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", guest.as_str()]);
        let (status, log) = run_with_deadline(
            hermit_command(&args),
            directory.path(),
            Duration::from_secs(120),
            false,
        );
        let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
            .expect("failed to read the guest's stdout");
        assert_eq!(
            status.and_then(|status| status.code()),
            Some(0),
            "{extra:?}: the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
        );
        stdout
    };
    assert_eq!(run(&[]), "handler on main, waiter woken (0)\n");
    let (mut retried_on_main, mut restarted_on_waiter) = (0, 0);
    for seed in ["1", "2", "19", "34"] {
        let stdout = run(&["--chaos", "--chaos-target-races", "--seed", seed]);
        match stdout.as_str() {
            "handler on main, waiter EAGAIN\n" => retried_on_main += 1,
            "handler on waiter, waiter EAGAIN\n" => restarted_on_waiter += 1,
            "handler on main, waiter woken (0)\n" => {}
            other => panic!("seed {seed}: a line Linux cannot produce here: {other:?}"),
        }
    }
    assert!(
        retried_on_main > 0 && restarted_on_waiter > 0,
        "the listed chaos seeds no longer reach both EAGAIN paths \
         (main: {retried_on_main}, waiter: {restarted_on_waiter}); pick new seeds"
    );
    // Record and replay recompute futex results; a recording made now replays
    // to a match, and RECORD_VERSION refuses one made before the change.
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&["record", "--verify", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(240),
        false,
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "record --verify must succeed:\nstderr:\n{log}"
    );
    assert!(
        log.contains("Success: replay matched recording."),
        "the replay must match the recording:\nstderr:\n{log}"
    );
}

/// A priority-inheritance futex command is refused by name, never a panic
/// (https://github.com/rrnewton/hermit/issues/3958): a fail-closed run stops with
/// the policy-refusal status, and with `--allow-unsupported-syscalls` the guest
/// gets ENOSYS, Linux's answer when built without CONFIG_FUTEX_PI.
#[test]
fn futex_lock_pi_is_refused_by_name() {
    let guest = futex_c_guest(&FUTEX_LOCK_PI_REFUSED_GUEST, "futex_lock_pi_refused")
        .to_str()
        .unwrap()
        .to_owned();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--log=error",
            "run",
            "--strict",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a fail-closed run must stop with the policy-refusal status:\nstderr:\n{log}"
    );
    assert!(
        log.contains("futex command FUTEX_LOCK_PI is not supported"),
        "the refusal must name the command:\nstderr:\n{log}"
    );
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        // `--strict` refuses the opt-out, so the compatibility run is not strict.
        hermit_command(&[
            "run",
            "--allow-unsupported-syscalls",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(stdout, "lock_pi: -1 ENOSYS\n", "stderr:\n{log}");
}

/// FUTEX_REQUEUE, FUTEX_CMP_REQUEUE and FUTEX_WAKE_OP key and access their
/// words as Linux does (https://github.com/rrnewton/hermit/pull/4020): counts
/// before addresses, keys without reading key-only words (NULL is a valid
/// private key), CMP_REQUEUE's compare with the guest's permissions, and
/// WAKE_OP's word changed in place as exactly four bytes (end of a page, a
/// write-only page) or refused with EFAULT (read-only, PROT_NONE), and shared
/// keys admitted by the page backing the word (a copied-on-write page made
/// read-only and a page past a truncation are EFAULT). Every line matches
/// native Linux.
#[test]
fn futex_requeue_and_wake_op_key_and_access_their_words_as_linux() {
    let guest = futex_c_guest(&FUTEX_KEYED_ACCESS_GUEST, "futex_keyed_access")
        .to_str()
        .unwrap()
        .to_owned();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&["run", "--strict", "--timeout", "60", "--", guest.as_str()]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "the run must finish and exit 0:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        "requeue, unmapped private source, counts 0: 0, word 0\n\
         requeue, negative nr_wake, unmapped source: -1 EINVAL, word 0\n\
         cmp_requeue, negative nr_requeue, unmapped source: -1 EINVAL, word 0\n\
         requeue, target at the top of the address space: -1 EFAULT, word 0\n\
         requeue, target in the kernel half: -1 EFAULT, word 0\n\
         requeue, private target in the low half above 2^62: -1 EFAULT, word 0\n\
         requeue, private target just below the 4-level user limit: 0, word 0\n\
         shared requeue, read-only anonymous target: -1 EFAULT, word 17\n\
         shared requeue, read-only shmem target: 0, word 17\n\
         shared wake_op, read-only shmem word, unknown op: -1 EFAULT, word 17\n\
         private wake_op, read-only word, unknown op: -1 ENOSYS, word 17\n\
         shared requeue, untouched read-only private file page: 0, word 42\n\
         shared requeue, writable copied-on-write page: 0, word 43\n\
         shared requeue, copied-on-write page made read-only: -1 EFAULT, word 43\n\
         private requeue, copied-on-write page made read-only: 0, word 43\n\
         shared requeue, writable shared file page: 0, word 42\n\
         shared requeue, shared file page past a truncation: -1 EFAULT, word 0\n\
         private requeue, shared file page past a truncation: 0, word 0\n\
         shared requeue, PROT_NONE target: -1 EFAULT, word 0\n\
         cmp_requeue, PROT_NONE source: -1 EFAULT, word 0\n\
         wake_op, unmapped private first word, set B=7: 0, word 7\n\
         wake_op, NULL second word, unknown op: -1 ENOSYS, word 7\n\
         wake_op +1, word at the end of a page before a hole: 0, word 43\n\
         wake_op +1, write-only page: 0, word 43\n\
         wake_op +1, read-only page: -1 EFAULT, word 42\n\
         wake_op +1, PROT_NONE page: -1 EFAULT, word 42\n\
         wake_op, NULL first word, wake 1 on B: 1, word 0\n\
         waiter on B: woken\n\
         requeue B->NULL: 1, word 0\n\
         wake NULL 1: 1, word 0\n\
         waiter: woken\n\
         requeue B->NULL: 1, word 0\n\
         requeue NULL->B: 1, word 0\n\
         wake B 1: 1, word 0\n\
         waiter: woken\n",
        "stderr:\n{log}"
    );
}

/// FUTEX_WAKE_OP on a word in a file-backed shared mapping is refused by name
/// (https://github.com/rrnewton/hermit/pull/4020): a process outside Hermit
/// could update that word concurrently, and Detcore's read and write would lose
/// its update where Linux's locked instruction does not. An anonymous shared
/// word, which only guest processes reach, still works. A fail-closed run stops
/// with the policy-refusal status; with `--allow-unsupported-syscalls` the
/// guest gets ENOSYS and the word is unchanged.
#[test]
fn a_wake_op_on_a_file_backed_shared_word_is_refused_by_name() {
    let guest = futex_c_guest(
        &FUTEX_WAKE_OP_FILE_SHARED_GUEST,
        "futex_wake_op_file_shared",
    )
    .to_str()
    .unwrap()
    .to_owned();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        hermit_command(&[
            "--log=error",
            "run",
            "--strict",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(HERMIT_POLICY_REFUSAL_EXIT),
        "a fail-closed run must stop with the policy-refusal status:\nstdout:\n{stdout}\nstderr:\n{log}"
    );
    assert!(
        log.contains("futex command FUTEX_WAKE_OP is not supported: its word is in a file-backed shared mapping"),
        "the refusal must name the command and the reason:\nstderr:\n{log}"
    );
    assert_eq!(
        stdout, "wake_op +1, anonymous shared word: 0, word 1\n",
        "stderr:\n{log}"
    );
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let (status, log) = run_with_deadline(
        // `--strict` refuses the opt-out, so the compatibility run is not strict.
        hermit_command(&[
            "run",
            "--allow-unsupported-syscalls",
            "--timeout",
            "60",
            "--",
            guest.as_str(),
        ]),
        directory.path(),
        Duration::from_secs(120),
        false,
    );
    let stdout = fs::read_to_string(directory.path().join("deadline-run.stdout"))
        .expect("failed to read the guest's stdout");
    assert_eq!(
        status.and_then(|status| status.code()),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{log}"
    );
    assert_eq!(
        stdout,
        "wake_op +1, anonymous shared word: 0, word 1\n\
         wake_op +1, file-backed shared word: -1 ENOSYS, word 0\n\
         wake_op +1, old alias after MREMAP_DONTUNMAP: -1 ENOSYS, word 0\n\
         wake_op +1, new alias after MREMAP_DONTUNMAP: -1 ENOSYS, word 0\n",
        "stderr:\n{log}"
    );
}

/// `HERMIT_VERIFY_TMPDIR` is Hermit's setting, not the guest's: a guest that
/// inherits Hermit's environment (`--base-env=host`) prints the same TMPDIR
/// and the same environment whether or not it is set.
#[test]
fn verify_tmpdir_does_not_reach_the_guest_environment() {
    let _guard = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let script = "printf 'TMPDIR=[%s]\\n' \"${TMPDIR-unset}\"; env | grep -c '^HERMIT_VERIFY_TMPDIR=' || true";
    let run = |verify_tmpdir: Option<&Path>| {
        let mut command =
            hermit_command(&["run", "--base-env=host", "--", "/bin/sh", "-c", script]);
        command.env("TMPDIR", directory.path());
        match verify_tmpdir {
            Some(path) => command.env("HERMIT_VERIFY_TMPDIR", path),
            None => command.env_remove("HERMIT_VERIFY_TMPDIR"),
        };
        let output = command.output().expect("run hermit");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let without = run(None);
    let with = run(Some(directory.path()));
    assert_eq!(with, without);
    assert_eq!(
        without,
        format!("TMPDIR=[{}]\n0\n", directory.path().display())
    );
}

/// A verify killed mid-run leaves its two temporary logs behind. With
/// `HERMIT_VERIFY_TMPDIR` set they are created there, not in TMPDIR, so a
/// caller that kills runs can clean them up.
#[test]
fn a_killed_verify_leaves_its_logs_in_verify_tmpdir_not_tmpdir() {
    let _guard = hermit_run_guard();
    let tmpdir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let verify_tmpdir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    // A guest that never finishes on its own: `sleep` runs in virtual time and
    // would let the verify finish, and delete both logs, within about a second.
    let mut child = hermit_command(&[
        "run",
        "--verify",
        "--",
        "/bin/sh",
        "-c",
        "while :; do :; done",
    ])
    .env("TMPDIR", tmpdir.path())
    .env("HERMIT_VERIFY_TMPDIR", verify_tmpdir.path())
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(fs::File::create(tmpdir.path().join("hermit.stderr")).unwrap())
    .process_group(0)
    .spawn()
    .expect("spawn hermit");
    let logs = |directory: &Path| {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("run1_log_") || name.starts_with("run2_log_"))
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while logs(verify_tmpdir.path()).len() < 2 && std::time::Instant::now() < deadline {
        thread::sleep(std::time::Duration::from_millis(50));
    }
    let group = child.id() as libc::pid_t;
    assert_eq!(unsafe { libc::kill(-group, libc::SIGKILL) }, 0);
    child.wait().unwrap();
    let kept = logs(verify_tmpdir.path());
    let stderr = fs::read_to_string(tmpdir.path().join("hermit.stderr")).unwrap_or_default();
    assert_eq!(
        kept.len(),
        2,
        "logs in HERMIT_VERIFY_TMPDIR: {kept:?}; hermit stderr:\n{stderr}"
    );
    assert!(kept[0].starts_with("run1_log_") && kept[1].starts_with("run2_log_"));
    assert_eq!(logs(tmpdir.path()), Vec::<String>::new());
}

/// A HERMIT_VERIFY_TMPDIR that cannot hold the logs is a clean error naming
/// the variable, for `run --verify` and `record start --verify` alike, not a
/// panic.
#[test]
fn an_unusable_verify_tmpdir_is_a_clean_error_naming_the_variable() {
    let _guard = hermit_run_guard();
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let missing = directory.path().join("missing");
    let data_dir = format!("--data-dir={}", directory.path().join("data").display());
    for args in [
        &["run", "--verify", "--", "/bin/true"][..],
        &["record", "start", "--verify", &data_dir, "--", "/bin/true"][..],
    ] {
        let output = hermit_command(args)
            .env("HERMIT_VERIFY_TMPDIR", &missing)
            .stdin(Stdio::null())
            .output()
            .expect("run hermit");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{args:?} succeeded: {stderr}");
        assert_ne!(
            output.status.code(),
            Some(101),
            "{args:?} panicked: {stderr}"
        );
        assert!(
            stderr.contains(&format!("HERMIT_VERIFY_TMPDIR={}", missing.display())),
            "{args:?}: {stderr}"
        );
    }
}
