/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT;
use hermit::Verdict;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";

#[derive(Debug)]
struct Workload {
    name: &'static str,
    path: PathBuf,
    args: &'static [&'static str],
}

#[derive(Clone, Copy)]
enum RunMode {
    Default,
    Strict,
    Chaos,
    Verify,
}

impl RunMode {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Strict => "strict",
            Self::Chaos => "chaos",
            Self::Verify => "verify",
        }
    }
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
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

fn configure_execution_root(command: &mut Command) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    let args = execution_root_args(requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
    command.args(args);
}

fn minimal_execution_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
    let mut args = execution_root_args(requested)?;
    if requested.is_some() {
        args.insert(0, "--base-env=minimal".into());
    }
    Ok(args)
}

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread", "-D_GNU_SOURCE"])
        .arg("-I")
        .arg(
            source
                .parent()
                .expect("C workload source should have a parent directory"),
        )
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation");
}

fn compile_c_without_libc(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-g", "-nostdlib"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation without libc");
}

fn compile_rust(source: &Path, output: &Path) {
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "Rust workload compilation");
}

#[path = "../../ci/cargo-guest-binaries.rs"]
mod cargo_guests;
use cargo_guests::CARGO_GUEST_BINARIES;

fn cargo_guest_workloads(_repository: &Path) -> Vec<Workload> {
    let raw = std::env::var("HERMIT_PREPARED_CARGO_GUESTS").expect(
        "Cargo guests must be prepared and verified by ci/nextest-binaries.rs before hermit_modes runs",
    );
    let guests: std::collections::BTreeMap<String, PathBuf> =
        serde_json::from_str(&raw).expect("prepared Cargo guest paths must be a JSON object");
    assert_eq!(
        guests
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        CARGO_GUEST_BINARIES.into_iter().collect(),
        "prepared Cargo guest identities must match every hermit_modes fixture",
    );
    CARGO_GUEST_BINARIES
        .iter()
        .map(|&name| {
            let path = guests[name].clone();
            assert!(
                path.is_file(),
                "missing prepared Cargo guest: {}",
                path.display()
            );
            workload(name, path)
        })
        .collect()
}

fn workload(name: &'static str, path: PathBuf) -> Workload {
    Workload {
        name,
        path,
        args: &[],
    }
}

/// How a workload is built. Each is built the first time a test asks for it,
/// with the same compiler, flags and source as before: the 2026-10-17 test
/// audit measured about 8.7 s per test in the `-j 1` test.hermit_modes node
/// spent compiling all ~30 workloads in every test process, most of them for
/// `default_*` tests that the node skips.
#[derive(Clone, Copy)]
enum Build {
    /// A repository-relative C source, built by [`compile_c`].
    C(&'static str),
    /// A repository-relative C source, built by [`compile_c_without_libc`].
    CWithoutLibc(&'static str),
    /// A repository-relative Rust source, built by [`compile_rust`].
    Rust(&'static str),
    /// A repository-relative script, run in place.
    Script(&'static str),
}

struct Spec {
    name: &'static str,
    /// The build output's file name; workloads sharing one binary name it.
    binary: &'static str,
    build: Build,
    args: &'static [&'static str],
}

const fn spec(name: &'static str, build: Build) -> Spec {
    Spec {
        name,
        binary: name,
        build,
        args: &[],
    }
}

const STABLE_WORKLOADS: [Spec; 6] = [
    spec("getpid", Build::C("tests/c/getpid.c")),
    spec("io_uring_fallback", Build::C("tests/c/io_uring_fallback.c")),
    spec("uname", Build::C("tests/c/uname.c")),
    spec("sysinfo", Build::C("tests/c/sysinfo.c")),
    spec("wait_on_child", Build::C("tests/c/wait_on_child.c")),
    spec("nanosleep_parallel", Build::C("tests/c/nanosleep-par.c")),
];

const DEFAULT_ONLY_WORKLOADS: [Spec; 29] = [
    spec("clone", Build::C("tests/c/clone.c")),
    spec("getcpu", Build::C("tests/c/getCpu.c")),
    spec("hello_alarm", Build::C("tests/c/hello_alarm.c")),
    spec("hello_signals", Build::C("tests/c/hello_signals.c")),
    spec("just_spin", Build::C("tests/c/just_spin.c")),
    spec("memory_pressure", Build::C("tests/c/memoryPress.c")),
    spec("print_memaddrs", Build::C("tests/c/print_memaddrs.c")),
    spec(
        "printf_with_threads",
        Build::C("tests/c/printf_with_threads.c"),
    ),
    spec(
        "sigtimedwait_no_timeout",
        Build::C("tests/c/sigtimedwait-no-timeout.c"),
    ),
    spec(
        "sigtimedwait_timeout_0s",
        Build::C("tests/c/sigtimedwait-timeout-0s.c"),
    ),
    spec(
        "sigtimedwait_timeout_1s",
        Build::C("tests/c/sigtimedwait-timeout-1s.c"),
    ),
    spec("sysinfo_uptime", Build::C("tests/c/sysinfo_uptime.c")),
    spec("thread_exhaustion", Build::C("tests/c/threadExhaustion.c")),
    spec(
        "lit_hello_world_c",
        Build::C("detcore/tests/lit/hello_world_c/main.c"),
    ),
    spec(
        "lit_rt_sigaction",
        Build::C("detcore/tests/lit/rt_sigaction/main.c"),
    ),
    spec(
        "lit_rt_sigprocmask",
        Build::C("detcore/tests/lit/rt_sigprocmask/main.c"),
    ),
    spec(
        "lit_networking",
        Build::C("detcore/tests/lit/networking/main.c"),
    ),
    spec(
        "network_bind",
        Build::Rust("tests/standalone/network_bind.rs"),
    ),
    spec(
        "lit_hello_world_rust",
        Build::Rust("detcore/tests/lit/hello_world_rs/main.rs"),
    ),
    spec("rust_stack_ptr", Build::Rust("tests/rust/stack_ptr.rs")),
    spec("rust_heap_ptrs", Build::Rust("tests/rust/heap_ptrs.rs")),
    spec("rust_rdtsc", Build::Rust("tests/rust/rdtsc.rs")),
    spec("rust_mem_race", Build::Rust("tests/rust/mem_race.rs")),
    spec(
        "minimal_hello",
        Build::CWithoutLibc("tests/c/simple/hello_nostdlib.c"),
    ),
    spec(
        "pread64_nostdlib",
        Build::CWithoutLibc("tests/c/simple/pread64_nostdlib.c"),
    ),
    Spec {
        name: "lit_rt_sigprocmask_mask",
        binary: "lit_rt_sigprocmask",
        build: Build::C("detcore/tests/lit/rt_sigprocmask/main.c"),
        args: &["mask"],
    },
    Spec {
        name: "lit_rt_sigprocmask_block",
        binary: "lit_rt_sigprocmask",
        build: Build::C("detcore/tests/lit/rt_sigprocmask/main.c"),
        args: &["block"],
    },
    spec(
        "shell_parallel_work",
        Build::Script("tests/shell/par_work.sh"),
    ),
    spec("shell_taskset", Build::Script("tests/shell/taskset.sh")),
];

const RESOURCE_DETERMINISM: Spec = spec(
    "resource_determinism",
    Build::C("tests/c/resource_determinism.c"),
);

const HELLO_RACE: Spec = spec("hello_race", Build::Rust("flaky-tests/hello_race.rs"));

const SABRE_EXIT_GROUP_PARKED: Spec = spec(
    "sabre_exit_group_parked",
    Build::C("tests/c/sabre_exit_group_parked.c"),
);

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// The prepared Cargo guests, checked once per test process: their identities
/// must match every hermit_modes fixture, and every path must exist.
fn cargo_guests() -> &'static [Workload] {
    static GUESTS: OnceLock<Vec<Workload>> = OnceLock::new();
    GUESTS.get_or_init(|| cargo_guest_workloads(repository()))
}

/// Builds `spec` on first use and returns its workload. A build writes a
/// per-process temporary file and renames it into place, so test processes
/// that build the same workload at once never see a partly written binary.
fn built(spec: &Spec) -> Workload {
    static BUILT: Mutex<std::collections::BTreeMap<&'static str, PathBuf>> =
        Mutex::new(std::collections::BTreeMap::new());
    // Every workload request checks the prepared Cargo guests, as building
    // the whole set used to.
    cargo_guests();
    let source = |relative: &str| repository().join(relative);
    let path = match spec.build {
        Build::Script(script) => source(script),
        build => {
            let mut built = BUILT.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(path) = built.get(spec.binary) {
                path.clone()
            } else {
                let build_root =
                    Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-wave1-workloads");
                fs::create_dir_all(&build_root).expect("failed to create workload build directory");
                let path = build_root.join(spec.binary);
                let partial =
                    build_root.join(format!(".{}.{}.partial", spec.binary, std::process::id()));
                match build {
                    Build::C(relative) => compile_c(&source(relative), &partial),
                    Build::CWithoutLibc(relative) => {
                        compile_c_without_libc(&source(relative), &partial)
                    }
                    Build::Rust(relative) => compile_rust(&source(relative), &partial),
                    Build::Script(_) => unreachable!("scripts are not built"),
                }
                fs::rename(&partial, &path).unwrap_or_else(|error| {
                    panic!("failed to install workload {}: {error}", path.display())
                });
                built.insert(spec.binary, path.clone());
                path
            }
        }
    };
    Workload {
        name: spec.name,
        path,
        args: spec.args,
    }
}

fn stable_workloads() -> Vec<Workload> {
    STABLE_WORKLOADS.iter().map(built).collect()
}

/// A default-mode workload by name: a built one, or a prepared Cargo guest.
fn default_workload(name: &str) -> Workload {
    if let Some(spec) = DEFAULT_ONLY_WORKLOADS.iter().find(|spec| spec.name == name) {
        return built(spec);
    }
    let guest = cargo_guests()
        .iter()
        .find(|workload| workload.name == name)
        .unwrap_or_else(|| panic!("unknown default-mode workload: {name}"));
    Workload {
        name: guest.name,
        path: guest.path.clone(),
        args: guest.args,
    }
}

fn hermit_command(base_env: &str) -> Command {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    hermit_command_with_execution_root(base_env, requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"))
}

fn hermit_command_with_execution_root(
    base_env: &str,
    requested_workdir: Option<&OsStr>,
) -> Result<Command, String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .arg("run")
        .arg(format!("--base-env={base_env}"))
        .args(["--no-virtualize-cpuid", "--max-timeslice=disabled"])
        .args(execution_root_args(requested_workdir)?);
    Ok(command)
}

fn verify_guest_command(
    tmp: &Path,
    report: &Path,
    script: &str,
    extra_options: &[&str],
) -> Command {
    let guest = tmp.join("guest");
    fs::write(&guest, script).expect("failed to write verify guest");
    let mut permissions = fs::metadata(&guest)
        .expect("failed to stat verify guest")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&guest, permissions).expect("failed to make verify guest executable");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["run", "--verify"])
        .arg("--verify-json")
        .arg(report)
        .args(extra_options)
        .args([
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
        ]);
    configure_execution_root(&mut command);
    command
        .arg(format!("--tmp={}", tmp.display()))
        .arg("/tmp/guest");
    command
}

fn read_verification_report(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap_or_else(|error| {
        panic!(
            "verification did not publish report {}: {error}",
            path.display()
        )
    }))
    .unwrap_or_else(|error| panic!("verification report was not valid JSON: {error}"))
}

fn verification_verdict(report: &serde_json::Value) -> Verdict {
    serde_json::from_value(
        report
            .get("verdict")
            .cloned()
            .expect("verification report has no verdict field"),
    )
    .expect("verification report verdict is not a known Verdict")
}

fn remove_retained_verify_logs(stderr: &str) {
    for word in stderr.split_whitespace() {
        if word.starts_with("/tmp/run") && word.contains("_log_") {
            let _ = fs::remove_file(word);
        }
    }
}

fn default_hermit_command(base_env: &str) -> Command {
    let mut command = hermit_command(base_env);
    command.args(["--no-sequentialize-threads", "--no-deterministic-io"]);
    command
}

fn hermit_run(mode: RunMode, workload: &Workload) {
    let mut command = hermit_command("minimal");
    match mode {
        RunMode::Default => {
            command.args(["--no-sequentialize-threads", "--no-deterministic-io"]);
        }
        RunMode::Strict => {}
        RunMode::Chaos => {
            command.arg("--chaos");
        }
        RunMode::Verify => {
            command.arg("--verify");
        }
    }
    command
        .arg(format!("--env=HERMIT_MODE={}", mode.name()))
        .arg("--")
        .arg(&workload.path)
        .args(workload.args);
    command_output(
        command,
        &format!("{} mode for {}", mode.name(), workload.name),
    );
}

fn run_stable_matrix(mode: RunMode) {
    let _guard = hermit_run_lock();
    for workload in &stable_workloads() {
        hermit_run(mode, workload);
    }
}

fn run_buck_chaos_workload(name: &str) {
    let _guard = hermit_run_lock();
    let workload = STABLE_WORKLOADS
        .iter()
        .find(|spec| spec.name == name)
        .map(built)
        .or_else(|| {
            let known = DEFAULT_ONLY_WORKLOADS.iter().any(|spec| spec.name == name)
                || cargo_guests().iter().any(|workload| workload.name == name);
            known.then(|| default_workload(name))
        })
        .unwrap_or_else(|| panic!("unknown Buck chaos workload: {name}"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .current_dir(
            workload
                .path
                .parent()
                .expect("Buck chaos workload should have a build directory"),
        )
        .args([
            "run",
            "--verify",
            "--chaos",
            "--base-env=empty",
            "--max-timeslice=1000000",
            "--env=HERMIT_MODE=chaos",
        ]);
    configure_execution_root(&mut command);
    command.arg("--").arg(&workload.path).args(workload.args);
    command_output(command, &format!("Buck chaos mode for {}", workload.name));
}

#[test]
fn pinned_root_arguments_are_exact_and_fail_closed() {
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

    assert!(minimal_execution_root_args(None).unwrap().is_empty());
    assert_eq!(
        minimal_execution_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );

    let command = hermit_command_with_execution_root("minimal", Some(OsStr::new("/test")))
        .expect("the documented workdir request should build a command");
    let args: Vec<_> = command.get_args().collect();
    assert!(args.windows(2).any(|args| {
        args == [
            OsStr::new("--mount=type=tmpfs,target=/test"),
            OsStr::new("--workdir=/test"),
        ]
    }));
    assert!(
        hermit_command_with_execution_root("minimal", Some(OsStr::new("/tmp")))
            .unwrap_err()
            .contains("HERMIT_E2E_EMPTY_WORKDIR must be /test")
    );
}

macro_rules! buck_chaos_tests {
    ($($test_name:ident => $workload_name:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                run_buck_chaos_workload($workload_name);
            }
        )+
    };
}

buck_chaos_tests! {
    chaos_buck_getpid => "getpid",
    chaos_buck_uname => "uname",
    chaos_buck_sysinfo => "sysinfo",
    chaos_buck_wait_on_child => "wait_on_child",
    chaos_buck_clone => "clone",
    chaos_buck_hello_alarm => "hello_alarm",
}

// TODO(#2792): Replace this static PMU partition with shared RCB/PMU
// prerequisite selection once that policy is decided.
#[test]
#[ignore = "known PMU chaos failure; tracked by #2791"]
// TODO(#2791): The workload fails after an interrupted nanosleep reports EINTR.
fn chaos_buck_nanosleep_parallel() {
    run_buck_chaos_workload("nanosleep_parallel");
}

#[test]
#[ignore = "known PMU chaos no-verdict; tracked by #2791"]
// TODO(#2791): The workload produced no verdict within its 300-second bound.
fn chaos_buck_mem_race() {
    run_buck_chaos_workload("rust_mem_race");
}

#[test]
fn default_mode_matrix() {
    run_stable_matrix(RunMode::Default);
}

#[test]
fn resource_syscalls_are_deterministic_across_five_runs() {
    let _guard = hermit_run_lock();
    let workload = &built(&RESOURCE_DETERMINISM);
    let mut baseline = None;

    for run in 1..=5 {
        let mut command = hermit_command("minimal");
        command.args(["--strict", "--"]).arg(&workload.path);
        let output = command_output(command, &format!("resource determinism run {run}"));
        let stdout = String::from_utf8(output.stdout).expect("resource output should be UTF-8");

        for expected in [
            "limit CPU",
            "limit RTTIME",
            "setrlimit libc",
            "setrlimit syscall",
            "prlimit64",
            "prlimit64 refusals deterministic",
            "prlimit64 fork inheritance deterministic",
            "rusage self before modeled-cpu",
            "rusage thread before modeled-cpu",
            "rusage self and thread logical CPU advances",
            "rusage children zero",
            "rusage children before reap modeled-cpu",
            "rusage children after reap modeled-cpu",
            "sysinfo",
            "sysinfo memory matches configured memory",
            "times logical process and child CPU ticks",
        ] {
            assert!(
                stdout.contains(expected),
                "run {run} missing {expected:?}:\n{stdout}"
            );
        }

        if let Some(expected) = &baseline {
            assert_eq!(
                &stdout, expected,
                "resource syscall output changed on run {run}"
            );
        } else {
            baseline = Some(stdout);
        }
    }
}

#[test]
fn resource_syscalls_are_independent_of_epoch_fraction() {
    let _guard = hermit_run_lock();
    let workload = &built(&RESOURCE_DETERMINISM);
    let mut baseline = None;

    for epoch in [
        "2026-01-01T00:00:00.000001000Z",
        "2026-01-01T00:00:00.999999000Z",
    ] {
        for run in 1..=2 {
            let mut command = hermit_command("minimal");
            command
                .args(["--strict", "--seed=0", "--epoch", epoch, "--"])
                .arg(&workload.path);
            let output = command_output(
                command,
                &format!("resource determinism epoch {epoch} run {run}"),
            );
            assert!(
                String::from_utf8_lossy(&output.stdout)
                    .contains("sysinfo memory matches configured memory"),
                "the resource workload did not complete its sysinfo checks"
            );
            if let Some(expected) = &baseline {
                assert_eq!(
                    &output.stdout, expected,
                    "complete resource output changed at epoch {epoch} run {run}"
                );
            } else {
                baseline = Some(output.stdout);
            }
        }
    }
}

#[test]
fn sysinfo_uptime_follows_thread_join_and_exec() {
    let _guard = hermit_run_lock();
    let workload = &default_workload("sysinfo_uptime");
    let mut baseline = None;
    for run in 1..=2 {
        let mut command = hermit_command("minimal");
        command
            .args([
                "--strict",
                "--seed=0",
                "--epoch=2026-01-01T00:00:00.000001000Z",
                "--",
            ])
            .arg(&workload.path)
            .arg("--observe-thread-exec");
        let output = command_output(command, &format!("sysinfo thread/exec run {run}"));
        let stdout = String::from_utf8(output.stdout).expect("sysinfo output should be UTF-8");
        for expected in [
            "sysinfo observation parent before thread uptime=",
            "sysinfo observation worker before work uptime=",
            "sysinfo observation worker after work uptime=",
            "sysinfo observation parent after join uptime=",
            "sysinfo observation after exec uptime=",
            "sysinfo thread join and exec continuity checked",
        ] {
            assert!(stdout.contains(expected), "missing {expected:?}:\n{stdout}");
        }
        if let Some(expected) = &baseline {
            assert_eq!(
                &stdout, expected,
                "complete sysinfo thread/exec output changed on run {run}"
            );
        } else {
            baseline = Some(stdout);
        }
    }
}

fn run_default_workload(name: &str) {
    let _guard = hermit_run_lock();
    let workload = &default_workload(name);
    hermit_run(RunMode::Default, workload);
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1023): Review the SaBRe exit-group teardown regression.
fn run_bounded_sabre_strict_verify(program: &Path, args: &[&str], label: &str) {
    let hermit_binary = Path::new(env!("CARGO_BIN_EXE_hermit"));
    let executable_dir = hermit_binary.parent().unwrap();
    let target_dir = executable_dir.parent().unwrap();
    let configured_loader = std::env::var_os("HERMIT_SABRE_BINARY").map(PathBuf::from);
    let loader = configured_loader
        .clone()
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = executable_dir.join("libdetcore_sabre.so");
    if !loader.is_file() || !plugin.is_file() {
        if configured_loader.is_some() {
            panic!(
                "configured SaBRe regression artifacts are unavailable: loader={}, plugin={}",
                loader.display(),
                plugin.display(),
            );
        }
        eprintln!(
            "skipping {label}: SaBRe regression artifacts are unavailable: loader={}, plugin={}",
            loader.display(),
            plugin.display(),
        );
        return;
    }

    let _guard = hermit_run_lock();
    let mut command = Command::new(hermit_binary);
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    command.env("HERMIT_SABRE_BINARY", &loader).args([
        "--backend",
        "sabre",
        "run",
        "--strict",
        "--verify",
    ]);
    command.args(
        minimal_execution_root_args(requested.as_deref())
            .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}")),
    );
    command
        .arg("--")
        .arg(program)
        .args(args)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if started.elapsed() >= Duration::from_secs(30) => {
                let process_group = -(child.id() as libc::pid_t);
                unsafe {
                    libc::kill(process_group, libc::SIGKILL);
                }
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("failed to poll {label}: {rendered}: {error}"),
        }
    };
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to collect {label}: {rendered}: {error}"));
    assert!(
        !timed_out && output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\ntimed out: {timed_out}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn sabre_exit_group_cancels_parked_futex_thread() {
    let workload = &built(&SABRE_EXIT_GROUP_PARKED);
    run_bounded_sabre_strict_verify(
        &workload.path,
        workload.args,
        "SaBRe exit-group teardown regression",
    );
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-1154): Review the SaBRe fork/exec pipe-state regression.
#[test]
fn sabre_exec_pipeline_preserves_blocking_pipe_semantics() {
    run_bounded_sabre_strict_verify(
        Path::new("/usr/bin/bash"),
        &[
            "-c",
            "set -euo pipefail; paste -d: <(printf 'alpha\\nbeta\\n') <(printf '1\\n2\\n') | diff -u <(printf 'alpha:1\\nbeta:2\\n') -; printf 'paste-ok\\n'",
        ],
        "SaBRe exec pipe-metadata regression",
    );
}

#[test]
fn sabre_timeout_observes_child_exit_before_virtual_alarm() {
    run_bounded_sabre_strict_verify(
        Path::new("/usr/bin/timeout"),
        &["1", "/usr/bin/true"],
        "SaBRe timeout physical-exit regression",
    );
}

#[test]
fn sabre_root_exit_with_orphan_child_is_bounded() {
    run_bounded_sabre_strict_verify(
        Path::new("/usr/bin/bash"),
        &["-c", "/usr/bin/sleep 0.01 & exit 0"],
        "SaBRe root exit with orphan child regression",
    );
}

#[test]
fn sabre_ignored_sigchld_does_not_block_parent_timer() {
    run_bounded_sabre_strict_verify(
        Path::new("/usr/bin/bash"),
        &[
            "-c",
            "trap \"\" CHLD; /usr/bin/true & /usr/bin/sleep 0.01; exit 0",
        ],
        "SaBRe ignored SIGCHLD timer regression",
    );
}

#[test]
fn sabre_ignored_blocked_sigchld_external_wait_does_not_block_alarm() {
    run_bounded_sabre_strict_verify(
        Path::new("/usr/bin/python3"),
        &[
            "-c",
            "import ctypes, os, signal, time; signal.signal(signal.SIGCHLD, signal.SIG_IGN); signal.signal(signal.SIGALRM, lambda s,f: None); libc=ctypes.CDLL(None, use_errno=True); Mask=ctypes.c_ulong*16; mask=Mask(); assert libc.sigemptyset(ctypes.byref(mask)) == 0; assert libc.sigaddset(ctypes.byref(mask), signal.SIGCHLD) == 0; pid=os.fork(); (time.sleep(0.005), os._exit(0)) if pid == 0 else None; signal.alarm(1); rc=libc.sigsuspend(ctypes.byref(mask)); print('done', rc, ctypes.get_errno())",
        ],
        "SaBRe ignored and blocked SIGCHLD external-wait alarm regression",
    );
}

macro_rules! default_workload_tests {
    ($($test_name:ident => $workload_name:literal),+ $(,)?) => {
        $(
            #[test]
            fn $test_name() {
                run_default_workload($workload_name);
            }
        )+
    };
}

default_workload_tests! {
    default_clone => "clone",
    default_getcpu => "getcpu",
    default_hello_alarm => "hello_alarm",
    default_hello_signals => "hello_signals",
    default_just_spin => "just_spin",
    default_memory_pressure => "memory_pressure",
    default_print_memaddrs => "print_memaddrs",
    default_printf_with_threads => "printf_with_threads",
    default_sigtimedwait_no_timeout => "sigtimedwait_no_timeout",
    default_sigtimedwait_timeout_0s => "sigtimedwait_timeout_0s",
    default_sigtimedwait_timeout_1s => "sigtimedwait_timeout_1s",
    default_sysinfo_uptime => "sysinfo_uptime",
    default_thread_exhaustion => "thread_exhaustion",
    default_lit_hello_world_c => "lit_hello_world_c",
    default_lit_hello_world_rust => "lit_hello_world_rust",
    default_lit_rt_sigaction => "lit_rt_sigaction",
    default_lit_rt_sigprocmask_mask => "lit_rt_sigprocmask_mask",
    default_lit_rt_sigprocmask_block => "lit_rt_sigprocmask_block",
    default_network_bind => "network_bind",
    default_rust_stack_ptr => "rust_stack_ptr",
    default_rust_heap_ptrs => "rust_heap_ptrs",
    default_rust_rdtsc => "rust_rdtsc",
    default_rust_mem_race => "rust_mem_race",
    default_shell_parallel_work => "shell_parallel_work",
    default_shell_taskset => "shell_taskset",
    default_cargo_clock_gettime => "rustbin_clock_gettime",
    default_cargo_exit_group => "rustbin_exit_group",
    default_cargo_futex_and_print => "rustbin_futex_and_print",
    default_cargo_futex_timeout => "rustbin_futex_timeout",
    default_cargo_futex_wait_child => "rustbin_futex_wait_child",
    default_cargo_futex_wake_some => "rustbin_futex_wake_some",
    default_cargo_interrogate_tty => "rustbin_interrogate_tty",
    default_cargo_nanosleep => "rustbin_nanosleep",
    default_cargo_network_hello_world => "rustbin_network_hello_world",
    default_cargo_pipe_basics => "rustbin_pipe_basics",
    default_cargo_poll => "rustbin_poll",
    default_cargo_poll_spin => "rustbin_poll_spin",
    default_cargo_clock_nanosleep_monotonic_abs => "rustbin_print_clock_nanosleep_monotonic_abs_race",
    default_cargo_clock_nanosleep_monotonic => "rustbin_print_clock_nanosleep_monotonic_race",
    default_cargo_clock_nanosleep_realtime_abs => "rustbin_print_clock_nanosleep_realtime_abs_race",
    default_cargo_print_nanosleep => "rustbin_print_nanosleep_race",
    default_cargo_sched_yield => "rustbin_sched_yield",
    default_cargo_socketpair => "rustbin_socketpair",
    default_cargo_thread_random => "rustbin_thread_random",
}

#[test]
#[ignore = "racy default mode can block in Hermit's connect emulation"]
fn default_cargo_bind_connect_race() {
    run_default_workload("rustbin_bind_connect_race");
}

#[test]
#[ignore = "default mode can block in clock total-order scheduling"]
fn default_cargo_clock_total_order() {
    run_default_workload("rustbin_clock_total_order");
}

#[test]
fn default_minimal_hello() {
    run_default_workload("minimal_hello");
    run_default_workload("pread64_nostdlib");
}

#[test]
fn default_lit_networking() {
    let _guard = hermit_run_lock();
    let workload = &default_workload("lit_networking");
    let mut command = default_hermit_command("minimal");
    command
        .args(["--analyze-networking", "--"])
        .arg(&workload.path);
    let output = command_output(command, "lit networking diagnostics");
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(diagnostics.contains("0.0.0.0:1299"));
    assert!(diagnostics.contains(":::1299"));
}

#[test]
fn default_exit_codes() {
    let _guard = hermit_run_lock();
    for (program, args, expected) in [
        ("/usr/bin/true", &[][..], 0),
        ("/usr/bin/false", &[][..], 1),
        ("/bin/sh", &["-c", "exit 42"][..], 42),
    ] {
        let mut command = default_hermit_command("minimal");
        command.arg("--").arg(program).args(args);
        let rendered = format!("{command:?}");
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "wrong propagated exit code: {rendered}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn default_virtualized_uname() {
    let _guard = hermit_run_lock();
    let mut command = default_hermit_command("minimal");
    command.args(["--", "/usr/bin/uname", "-nr"]);
    let output = command_output(command, "virtualized uname");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "hermetic-container.local 5.2.0\n"
    );
}

#[test]
fn default_cat_issue() {
    let _guard = hermit_run_lock();
    let mut command = default_hermit_command("minimal");
    command.args(["--", "/usr/bin/cat", "/etc/issue"]);
    let output = command_output(command, "cat /etc/issue");
    assert!(!output.stdout.is_empty());
}

#[test]
fn default_bind_mounts() {
    let _guard = hermit_run_lock();
    let root = tempfile::tempdir().expect("failed to create bind-mount test directory");
    let foo = root.path().join("foo");
    let bar = root.path().join("bar");
    fs::create_dir_all(&foo).expect("failed to create foo bind source");
    fs::create_dir_all(&bar).expect("failed to create bar bind source");
    fs::write(foo.join("one.txt"), b"one").expect("failed to write foo fixture");
    fs::write(bar.join("two.txt"), b"two").expect("failed to write bar fixture");

    let mut command = default_hermit_command("minimal");
    command
        .arg(format!("--bind={}:/tmp/foo", foo.display()))
        .arg(format!("--bind={}:/tmp/bar", bar.display()))
        .arg("--")
        .args([
            "/bin/sh",
            "-c",
            "test -f /tmp/foo/one.txt && test -f /tmp/bar/two.txt",
        ]);
    command_output(command, "tmpfs bind mounts");
}

#[test]
fn default_preserved_tmpfs() {
    let _guard = hermit_run_lock();
    let root = tempfile::tempdir().expect("failed to create tmpfs test directory");
    let guest_tmp = root.path().join("guest-tmp");
    let mut command = default_hermit_command("minimal");
    command
        .arg(format!("--tmp={}", guest_tmp.display()))
        .arg("--")
        .args(["/usr/bin/touch", "/tmp/one.txt", "/tmp/two.txt"]);
    command_output(command, "preserved tmpfs");
    assert!(guest_tmp.join("one.txt").is_file());
    assert!(guest_tmp.join("two.txt").is_file());
}

#[test]
fn default_environment_selection() {
    let _guard = hermit_run_lock();

    let mut empty = default_hermit_command("empty");
    empty.args(["--", "/usr/bin/env"]);
    let empty_output = command_output(empty, "empty guest environment");
    let empty_stdout = String::from_utf8_lossy(&empty_output.stdout);
    assert!(!empty_stdout.contains("HOST_ONLY_VALUE="));

    let mut selected = default_hermit_command("empty");
    selected.env("HOST_ONLY_VALUE", "from-host").args([
        "--env=HOST_ONLY_VALUE",
        "--env=FIXED_VALUE=33",
        "--",
        "/usr/bin/env",
    ]);
    let selected_output = command_output(selected, "selected guest environment");
    let selected_stdout = String::from_utf8_lossy(&selected_output.stdout);
    assert!(
        selected_stdout
            .lines()
            .any(|line| line == "HOST_ONLY_VALUE=from-host")
    );
    assert!(selected_stdout.lines().any(|line| line == "FIXED_VALUE=33"));
}

#[test]
fn no_hardware_minimal_hello_backtraces() {
    let _guard = hermit_run_lock();
    let workload = &default_workload("minimal_hello");

    let mut first = hermit_command("minimal");
    first
        .args(["--record-preemptions", "--summary", "--"])
        .arg(&workload.path);
    let first_output = command_output(first, "minimal hello event count");
    let first_stderr = String::from_utf8_lossy(&first_output.stderr);
    let event_count = first_stderr
        .lines()
        .find_map(|line| {
            let (_, suffix) = line.split_once("recorded ")?;
            let (count, _) = suffix.split_once(" events")?;
            count.parse::<usize>().ok()
        })
        .unwrap_or_else(|| panic!("missing recorded event count in stderr:\n{first_stderr}"));
    assert!(event_count > 0);

    let mut second = hermit_command("minimal");
    second.args(["--record-preemptions", "--summary"]);
    for index in 0..event_count {
        second.arg(format!("--stacktrace-event={index}"));
    }
    second.arg("--").arg(&workload.path);
    let second_output = command_output(second, "minimal hello event stacktraces");
    let second_stderr = String::from_utf8_lossy(&second_output.stderr);
    assert_eq!(
        second_stderr
            .matches("Printing stack trace for scheduled event")
            .count(),
        event_count,
        "wrong stacktrace count in stderr:\n{second_stderr}"
    );
}

#[test]
fn no_hardware_stacktrace_signal() {
    let _guard = hermit_run_lock();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=info",
        "run",
        "-u",
        "--stacktrace-signal=SIGQUIT",
        "--stacktrace-event=10",
        "--record-preemptions",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    configure_execution_root(&mut command);
    command.args(["--", "/bin/date"]);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGQUIT),
        "guest did not propagate SIGQUIT: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
    assert!(
        stderr.contains("SIGQUIT"),
        "stacktrace log did not mention SIGQUIT:\n{stderr}"
    );
}

#[test]
fn strict_mode_matrix() {
    run_stable_matrix(RunMode::Strict);
}

#[test]
fn chaos_mode_matrix() {
    run_stable_matrix(RunMode::Chaos);
}

#[test]
fn verify_mode_matrix() {
    run_stable_matrix(RunMode::Verify);
}

#[test]
fn verify_rejects_explicit_log_levels_below_info() {
    let _guard = hermit_run_lock();
    for level in ["off", "error", "warn"] {
        let output = hermit_test::hermit_output(
            "hermit run",
            Command::new(env!("CARGO_BIN_EXE_hermit")).args([
                &format!("--log={level}"),
                "run",
                "--verify",
                "--",
                "/bin/true",
            ]),
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "--log={level} unexpectedly passed"
        );
        assert!(
            stderr.contains("--verify requires --log=info"),
            "unexpected --log={level} error:\n{stderr}"
        );
    }
}

#[test]
fn verify_reports_stdout_divergence() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let report = tempfile::NamedTempFile::new().expect("failed to create verification report");
    let mut command = verify_guest_command(
        tmp.path(),
        report.path(),
        "#!/bin/sh\nif [ -e /tmp/state ]; then printf second; else printf first; fi\n: > /tmp/state\n",
        &[],
    );

    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "divergent verification unexpectedly exited successfully:\n{stderr}"
    );
    let report = read_verification_report(report.path());
    assert_eq!(
        verification_verdict(&report),
        Verdict::Diverged,
        "unexpected verification report: {report}"
    );
    assert_eq!(
        output.status.code(),
        Some(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "unexpected status:\n{stderr}"
    );
    assert!(!stderr.contains("HERMIT_INTERNAL_FAILURE"), "{stderr}");
    assert_eq!(report["guest_exit_code"], serde_json::json!(0));
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_reports_exit_status_divergence() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let report = tempfile::NamedTempFile::new().expect("failed to create verification report");
    let mut command = verify_guest_command(
        tmp.path(),
        report.path(),
        "#!/bin/sh\nif [ -e /tmp/state ]; then exit 17; fi\n: > /tmp/state\nexit 0\n",
        &["--verify-allow=both"],
    );

    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "divergent verification unexpectedly exited successfully:\n{stderr}"
    );
    let report = read_verification_report(report.path());
    assert_eq!(
        verification_verdict(&report),
        Verdict::Diverged,
        "unexpected verification report: {report}"
    );
    assert_eq!(
        output.status.code(),
        Some(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "unexpected status:\n{stderr}"
    );
    assert_eq!(report["guest_exit_code"], serde_json::json!(17));
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_verbose_compares_the_full_trace() {
    let _guard = hermit_run_lock();
    let report = tempfile::NamedTempFile::new().expect("failed to create verification report");
    let mut command = hermit_command("minimal");
    command
        .args(["--verify", "--verify-verbose"])
        .arg("--verify-json")
        .arg(report.path())
        .args(["--", "/bin/true"]);

    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "divergent verification unexpectedly exited successfully:\n{stderr}"
    );
    let report = read_verification_report(report.path());
    assert_eq!(
        verification_verdict(&report),
        Verdict::Diverged,
        "unexpected verification report: {report}"
    );
    assert_eq!(
        output.status.code(),
        Some(HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "unexpected status:\n{stderr}"
    );
    assert_eq!(
        report["comparison"]["log_scope"],
        serde_json::json!("full_trace"),
        "verbose verification did not report a full-trace comparison: {report}"
    );
    assert_eq!(report["guest_exit_code"], serde_json::json!(0));
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_strict_info_reports_typed_memory_parity_on_landed_fixture() {
    let _guard = hermit_run_lock();
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let tmp = tempfile::Builder::new()
        .prefix("verify-memory-regions-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create strict verification directory");
    // This existing fixture intentionally materializes both a C stack value and
    // several malloc allocations. A no-libc hello fixture has no [heap] mapping
    // and cannot witness --detlog-heap even when aggregate INFO counts are nonzero.
    let guest = tmp.path().join("print_memaddrs");
    compile_c(&repository.join("tests/c/print_memaddrs.c"), &guest);
    let report = tmp.path().join("verify.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "--log=info",
            "run",
            "--verify",
            "--verify-strict",
            "--verify-logs",
        ])
        .arg("--verify-json")
        .arg(&report)
        .args([
            "--detlog-heap",
            "--detlog-stack",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
        ]);
    configure_execution_root(&mut command);
    let output = hermit_test::hermit_output("hermit run", command.arg("--").arg(&guest));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "strict INFO verification failed:\n{stderr}"
    );
    assert!(
        stderr.contains("Comparing INFO messages"),
        "strict comparison did not name its INFO envelope:\n{stderr}"
    );
    assert!(
        !stderr.contains("Comparing full trace messages"),
        "ordinary strict verification unexpectedly promoted diagnostics:\n{stderr}"
    );
    let memory_region_count = |region: &str| {
        let marker = format!("[{region}]->");
        stderr
            .lines()
            .filter(|line| {
                line.contains(" INFO ")
                    && line.contains("DETLOG [memory]")
                    && line.contains(&marker)
            })
            .count()
    };
    let heap_info_messages = memory_region_count("heap");
    let stack_info_messages = memory_region_count("stack");
    assert!(
        heap_info_messages > 0,
        "fixture produced no compared INFO heap evidence:\n{stderr}"
    );
    assert!(
        stack_info_messages > 0,
        "fixture produced no compared INFO stack evidence:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("strict verification did not publish its report"),
    )
    .expect("strict verification report was not valid JSON");
    assert_eq!(report["verified"], serde_json::json!(true));
    assert_eq!(report["bitwise_parity"], serde_json::json!(true));
    assert_eq!(report["comparison"]["log_scope"], serde_json::json!("info"));
    assert!(
        report["compared_log_messages"]["left"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
    assert!(
        report["compared_log_messages"]["right"]
            .as_u64()
            .is_some_and(|count| count > 0)
    );
}

#[test]
fn verify_honors_tmp_and_environment() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let source = tmp.path().join("guest.c");
    let guest = tmp.path().join("guest");
    fs::write(
        &source,
        r#"#include <stdlib.h>
#include <string.h>

int main(void) {
    const char *configured = getenv("VERIFY_CONFIGURED");
    if (configured == NULL || strcmp(configured, "expected") != 0) {
        return 11;
    }
    if (getenv("VERIFY_HOST_ONLY") != NULL) {
        return 12;
    }
    return 0;
}
"#,
    )
    .expect("failed to write verify guest source");
    compile_c(&source, &guest);

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--verify",
            "--base-env=empty",
            "--env=VERIFY_CONFIGURED=expected",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
        ])
        .arg(format!("--tmp={}", tmp.path().display()));
    configure_execution_root(&mut command);
    command
        .arg("/tmp/guest")
        .env("VERIFY_HOST_ONLY", "unexpected");
    command_output(command, "verify configuration");
}

#[test]
fn hello_race_chaos_verify() {
    let _guard = hermit_run_lock();
    let workload = &built(&HELLO_RACE);
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "run",
        "--verify",
        "--verify-allow=both",
        "--chaos",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--env=HERMIT_MODE=chaos",
    ]);
    configure_execution_root(&mut command);
    command.arg("--").arg(&workload.path);
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Hermit propagates the guest status even when --verify-allow=both accepts it.
    assert!(
        output.status.code().is_some() && stderr.contains("Success: deterministic."),
        "chaos verification for hello_race failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
}
