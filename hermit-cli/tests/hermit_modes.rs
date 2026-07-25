/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<Workloads> = OnceLock::new();

#[derive(Debug)]
struct Workload {
    name: &'static str,
    path: PathBuf,
    args: &'static [&'static str],
}

struct Workloads {
    stable: Vec<Workload>,
    default_only: Vec<Workload>,
    hello_race: Workload,
    resource_determinism: Workload,
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

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

const CARGO_GUEST_BINARIES: [&str; 21] = [
    "rustbin_bind_connect_race",
    "rustbin_clock_gettime",
    "rustbin_clock_total_order",
    "rustbin_exit_group",
    "rustbin_futex_and_print",
    "rustbin_futex_timeout",
    "rustbin_futex_wait_child",
    "rustbin_futex_wake_some",
    "rustbin_interrogate_tty",
    "rustbin_nanosleep",
    "rustbin_network_hello_world",
    "rustbin_pipe_basics",
    "rustbin_poll",
    "rustbin_poll_spin",
    "rustbin_print_clock_nanosleep_monotonic_abs_race",
    "rustbin_print_clock_nanosleep_monotonic_race",
    "rustbin_print_clock_nanosleep_realtime_abs_race",
    "rustbin_print_nanosleep_race",
    "rustbin_sched_yield",
    "rustbin_socketpair",
    "rustbin_thread_random",
];

fn cargo_guest_workloads(repository: &Path) -> Vec<Workload> {
    let binary_directory = Path::new(env!("CARGO_BIN_EXE_hermit"))
        .parent()
        .expect("Hermit binary should have a parent directory");
    if CARGO_GUEST_BINARIES
        .iter()
        .any(|name| !binary_directory.join(name).is_file())
    {
        let mut command = Command::new(env!("CARGO"));
        command.current_dir(repository).args([
            "build",
            "-p",
            "hermetic_infra_hermit_tests",
            "--bins",
        ]);
        command_output(command, "Cargo guest workload compilation");
    }

    CARGO_GUEST_BINARIES
        .iter()
        .map(|&name| {
            let path = binary_directory.join(name);
            assert!(
                path.is_file(),
                "missing Cargo guest binary: {}",
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

fn workloads() -> &'static Workloads {
    WORKLOADS.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-wave1-workloads");
        fs::create_dir_all(&build_root).expect("failed to create workload build directory");

        let stable_c_sources = [
            ("getpid", "getpid.c"),
            ("io_uring_fallback", "io_uring_fallback.c"),
            ("uname", "uname.c"),
            ("sysinfo", "sysinfo.c"),
            ("wait_on_child", "wait_on_child.c"),
            ("nanosleep_parallel", "nanosleep-par.c"),
        ];
        let stable = stable_c_sources
            .into_iter()
            .map(|(name, source_name)| {
                let path = build_root.join(name);
                compile_c(&repository.join("tests/c").join(source_name), &path);
                workload(name, path)
            })
            .collect();

        let default_c_sources = [
            ("clone", "tests/c/clone.c"),
            ("getcpu", "tests/c/getCpu.c"),
            ("hello_alarm", "tests/c/hello_alarm.c"),
            ("hello_signals", "tests/c/hello_signals.c"),
            ("just_spin", "tests/c/just_spin.c"),
            ("memory_pressure", "tests/c/memoryPress.c"),
            ("print_memaddrs", "tests/c/print_memaddrs.c"),
            ("printf_with_threads", "tests/c/printf_with_threads.c"),
            (
                "sigtimedwait_no_timeout",
                "tests/c/sigtimedwait-no-timeout.c",
            ),
            (
                "sigtimedwait_timeout_0s",
                "tests/c/sigtimedwait-timeout-0s.c",
            ),
            (
                "sigtimedwait_timeout_1s",
                "tests/c/sigtimedwait-timeout-1s.c",
            ),
            ("sysinfo_uptime", "tests/c/sysinfo_uptime.c"),
            ("thread_exhaustion", "tests/c/threadExhaustion.c"),
            (
                "lit_hello_world_c",
                "detcore/tests/lit/hello_world_c/main.c",
            ),
            ("lit_rt_sigaction", "detcore/tests/lit/rt_sigaction/main.c"),
            (
                "lit_rt_sigprocmask",
                "detcore/tests/lit/rt_sigprocmask/main.c",
            ),
            ("lit_networking", "detcore/tests/lit/networking/main.c"),
        ];
        let mut default_only: Vec<_> = default_c_sources
            .into_iter()
            .map(|(name, source)| {
                let path = build_root.join(name);
                compile_c(&repository.join(source), &path);
                workload(name, path)
            })
            .collect();

        let rust_sources = [
            ("network_bind", "tests/standalone/network_bind.rs"),
            (
                "lit_hello_world_rust",
                "detcore/tests/lit/hello_world_rs/main.rs",
            ),
            ("rust_stack_ptr", "tests/rust/stack_ptr.rs"),
            ("rust_heap_ptrs", "tests/rust/heap_ptrs.rs"),
            ("rust_rdtsc", "tests/rust/rdtsc.rs"),
            ("rust_mem_race", "tests/rust/mem_race.rs"),
        ];
        default_only.extend(rust_sources.into_iter().map(|(name, source)| {
            let path = build_root.join(name);
            compile_rust(&repository.join(source), &path);
            workload(name, path)
        }));

        let minimal_hello = build_root.join("minimal_hello");
        compile_c_without_libc(
            &repository.join("tests/c/simple/hello_nostdlib.c"),
            &minimal_hello,
        );
        default_only.push(workload("minimal_hello", minimal_hello));

        let pread64_nostdlib = build_root.join("pread64_nostdlib");
        compile_c_without_libc(
            &repository.join("tests/c/simple/pread64_nostdlib.c"),
            &pread64_nostdlib,
        );
        default_only.push(workload("pread64_nostdlib", pread64_nostdlib));

        let lit_sigprocmask = build_root.join("lit_rt_sigprocmask");
        default_only.extend([
            Workload {
                name: "lit_rt_sigprocmask_mask",
                path: lit_sigprocmask.clone(),
                args: &["mask"],
            },
            Workload {
                name: "lit_rt_sigprocmask_block",
                path: lit_sigprocmask,
                args: &["block"],
            },
        ]);

        let script_sources = [
            ("shell_parallel_work", "tests/shell/par_work.sh"),
            ("shell_taskset", "tests/shell/taskset.sh"),
        ];
        default_only.extend(
            script_sources
                .into_iter()
                .map(|(name, source)| workload(name, repository.join(source))),
        );

        default_only.extend(cargo_guest_workloads(repository));

        let resource_determinism = workload(
            "resource_determinism",
            build_root.join("resource_determinism"),
        );
        compile_c(
            &repository.join("tests/c/resource_determinism.c"),
            &resource_determinism.path,
        );

        let hello_race = workload("hello_race", build_root.join("hello_race"));
        compile_rust(
            &repository.join("flaky-tests/hello_race.rs"),
            &hello_race.path,
        );

        Workloads {
            stable,
            default_only,
            hello_race,
            resource_determinism,
        }
    })
}

fn hermit_command(base_env: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .arg("run")
        .arg(format!("--base-env={base_env}"))
        .args(["--no-virtualize-cpuid", "--preemption-timeout=disabled"]);
    command
}

fn verify_guest_command(tmp: &Path, script: &str, extra_options: &[&str]) -> Command {
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
        .args(extra_options)
        .args([
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
        ])
        .arg(format!("--tmp={}", tmp.display()))
        .arg("/tmp/guest");
    command
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
    for workload in &workloads().stable {
        hermit_run(mode, workload);
    }
}

fn run_buck_chaos_workload(name: &str) {
    let _guard = hermit_run_lock();
    let workload = workloads()
        .stable
        .iter()
        .chain(&workloads().default_only)
        .find(|workload| workload.name == name)
        .unwrap_or_else(|| panic!("unknown Buck chaos workload: {name}"));
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--verify",
            "--chaos",
            "--base-env=empty",
            "--preemption-timeout=1000000",
            "--env=HERMIT_MODE=chaos",
            "--",
        ])
        .arg(&workload.path)
        .args(workload.args);
    command_output(command, &format!("Buck chaos mode for {}", workload.name));
}

macro_rules! buck_chaos_tests {
    ($($test_name:ident => $workload_name:literal),+ $(,)?) => {
        $(
            #[test]
            #[ignore = "requires PMU branch counters and working mount namespaces"]
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
    chaos_buck_nanosleep_parallel => "nanosleep_parallel",
    chaos_buck_clone => "clone",
    chaos_buck_hello_alarm => "hello_alarm",
    chaos_buck_mem_race => "rust_mem_race",
}

#[test]
fn default_mode_matrix() {
    run_stable_matrix(RunMode::Default);
}

#[test]
fn resource_syscalls_are_deterministic_across_five_runs() {
    let _guard = hermit_run_lock();
    let workload = &workloads().resource_determinism;
    let mut baseline = None;

    for run in 1..=5 {
        let mut command = hermit_command("minimal");
        command.arg("--").arg(&workload.path);
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
            "rusage self maxrss",
            "rusage thread maxrss",
            "rusage children zero",
            "sysinfo",
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

fn run_default_workload(name: &str) {
    let _guard = hermit_run_lock();
    let workload = workloads()
        .default_only
        .iter()
        .find(|workload| workload.name == name)
        .unwrap_or_else(|| panic!("unknown default-mode workload: {name}"));
    hermit_run(RunMode::Default, workload);
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
    let workload = workloads()
        .default_only
        .iter()
        .find(|workload| workload.name == "lit_networking")
        .expect("missing lit networking workload");
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
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start exit-code check: {rendered}: {error}"));
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
    let workload = workloads()
        .default_only
        .iter()
        .find(|workload| workload.name == "minimal_hello")
        .expect("missing minimal hello workload");

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
        "--preemption-timeout=disabled",
        "--",
        "/bin/date",
    ]);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start stacktrace signal: {rendered}: {error}"));
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
fn verify_captures_debug_logs_when_a_lower_level_is_requested() {
    let _guard = hermit_run_lock();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=error",
        "run",
        "--verify",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--preemption-timeout=disabled",
        "--",
        "/bin/true",
    ]);

    let output = command_output(command, "verify minimum log level");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Logs contain "),
        "missing log counts:\n{stderr}"
    );
    assert!(
        !stderr.contains("Logs contain 0 | 0 messages total"),
        "verify compared empty logs:\n{stderr}"
    );
}

#[test]
fn verify_reports_stdout_divergence() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let mut command = verify_guest_command(
        tmp.path(),
        "#!/bin/sh\nif [ -e /tmp/state ]; then printf second; else printf first; fi\n: > /tmp/state\n",
        &[],
    );

    let output = command.output().expect("failed to run stdout verification");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status:\n{stderr}"
    );
    assert!(
        stderr.contains("Mismatch in stdout between run 1 and run 2"),
        "missing stdout diagnostic:\n{stderr}"
    );
    assert!(
        stderr.contains("Failure: nondeterministic."),
        "missing verification failure:\n{stderr}"
    );
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_reports_exit_status_divergence() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let mut command = verify_guest_command(
        tmp.path(),
        "#!/bin/sh\nif [ -e /tmp/state ]; then exit 17; fi\n: > /tmp/state\nexit 0\n",
        &["--verify-allow=both"],
    );

    let output = command
        .output()
        .expect("failed to run exit-status verification");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status:\n{stderr}"
    );
    assert!(
        stderr.contains("Mismatch in exit status between run 1 and run 2"),
        "missing exit-status diagnostic:\n{stderr}"
    );
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_verbose_compares_the_full_trace() {
    let _guard = hermit_run_lock();
    let mut command = hermit_command("minimal");
    command.args(["--verify", "--verify-verbose", "--", "/bin/true"]);

    let output = command
        .output()
        .expect("failed to run verbose verification");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "unexpected status:\n{stderr}"
    );
    assert!(
        stderr.contains("Comparing full trace messages"),
        "missing full-trace comparison:\n{stderr}"
    );
    assert!(
        stderr.contains("Failure: nondeterministic."),
        "missing verification failure:\n{stderr}"
    );
    remove_retained_verify_logs(&stderr);
}

#[test]
fn verify_honors_tmp_and_environment() {
    let _guard = hermit_run_lock();
    let tmp = tempfile::tempdir().expect("failed to create verify tmp directory");
    let guest = tmp.path().join("guest");
    fs::write(
        &guest,
        r#"#!/bin/sh
[ "${VERIFY_CONFIGURED-}" = expected ] || exit 11
[ "${VERIFY_HOST_ONLY+set}" != set ] || exit 12
printf 'configured\n'
"#,
    )
    .expect("failed to write verify guest");
    let mut permissions = fs::metadata(&guest)
        .expect("failed to stat verify guest")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&guest, permissions).expect("failed to make verify guest executable");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--verify",
            "--base-env=empty",
            "--env=VERIFY_CONFIGURED=expected",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
        ])
        .arg(format!("--tmp={}", tmp.path().display()))
        .arg("/tmp/guest")
        .env("VERIFY_HOST_ONLY", "unexpected");
    command_output(command, "verify configuration");
}

#[test]
fn hello_race_chaos_verify() {
    let _guard = hermit_run_lock();
    let workload = &workloads().hello_race;
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--verify",
            "--verify-allow=both",
            "--chaos",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--env=HERMIT_MODE=chaos",
        ])
        .arg(&workload.path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Hermit propagates the guest status even when --verify-allow=both accepts it.
    assert!(
        output.status.code().is_some() && stderr.contains("Success: deterministic."),
        "chaos verification for hello_race failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
}
