/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Cargo runner for the Detcore LLVM lit tests.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use litcheck_filecheck::Config;
use litcheck_filecheck::Test;
use litcheck_filecheck::litcheck::Symbol;

static LIT_LOCK: Mutex<()> = Mutex::new(());
static NATIVE_FIXTURES: OnceLock<NativeFixtures> = OnceLock::new();

struct NativeFixtures {
    hello_world_c: PathBuf,
    hello_world_go: PathBuf,
    networking: PathBuf,
    rt_sigaction: PathBuf,
    rt_sigprocmask: PathBuf,
}

fn lit_lock() -> MutexGuard<'static, ()> {
    LIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli must be inside the repository")
        .to_path_buf()
}

fn lit_root() -> PathBuf {
    repository().join("detcore/tests/lit")
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

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args([
            "-D_POSIX_C_SOURCE=20180920",
            "-D_GNU_SOURCE=1",
            "-UNDEBUG",
            "-pthread",
        ])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C lit fixture compilation");
}

fn compile_go(source: &Path, output: &Path) {
    let mut command = Command::new("go");
    command.arg("build").arg("-o").arg(output).arg(source);
    command_output(command, "Go lit fixture compilation");
}

fn native_fixtures() -> &'static NativeFixtures {
    NATIVE_FIXTURES.get_or_init(|| {
        let root = lit_root();
        let output = Path::new(env!("CARGO_TARGET_TMPDIR")).join("detcore-lit-fixtures");
        fs::create_dir_all(&output).expect("failed to create native fixture directory");

        let hello_world_c = output.join("hello_world_c");
        let networking = output.join("networking");
        let rt_sigaction = output.join("rt_sigaction");
        let rt_sigprocmask = output.join("rt_sigprocmask");
        let hello_world_go = output.join("hello_world_go");

        compile_c(&root.join("hello_world_c/main.c"), &hello_world_c);
        compile_c(&root.join("networking/main.c"), &networking);
        compile_c(&root.join("rt_sigaction/main.c"), &rt_sigaction);
        compile_c(&root.join("rt_sigprocmask/main.c"), &rt_sigprocmask);
        compile_go(&root.join("hello_world_go/main.go"), &hello_world_go);

        NativeFixtures {
            hello_world_c,
            hello_world_go,
            networking,
            rt_sigaction,
            rt_sigprocmask,
        }
    })
}

fn fixture_path(name: &str) -> PathBuf {
    let native = native_fixtures();
    match name {
        "hello_world_c" => native.hello_world_c.clone(),
        "hello_world_go" => native.hello_world_go.clone(),
        "networking" => native.networking.clone(),
        "rt_sigaction" => native.rt_sigaction.clone(),
        "rt_sigprocmask" => native.rt_sigprocmask.clone(),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_detcore-lit-fixture")),
    }
}

fn shell_quote(path: &Path) -> String {
    shell_words::quote(&path.to_string_lossy()).into_owned()
}

fn run_directive(directive: &str, source: &Path, fixture: Option<&str>, workdir: &Path) {
    let hermit = shell_quote(Path::new(env!("CARGO_BIN_EXE_hermit")));
    let uses_hermit = directive.contains("%hermit");
    let source_is_script = source.extension().is_some_and(|ext| ext == "sh");
    let fixture_path = fixture.map(fixture_path);
    let local_fixture = fixture_path.as_ref().map(|fixture| {
        let local = workdir.join("fixture");
        fs::copy(fixture, &local).unwrap_or_else(|error| {
            panic!(
                "failed to copy lit fixture {} to {}: {error}",
                fixture.display(),
                local.display()
            )
        });
        local
    });
    let fixture_arg = local_fixture.as_ref().map(|fixture| {
        if uses_hermit {
            "/tmp/fixture".to_owned()
        } else {
            shell_quote(fixture)
        }
    });
    let local_script = if uses_hermit && source_is_script {
        let local = workdir.join("test.sh");
        fs::copy(source, &local).unwrap_or_else(|error| {
            panic!(
                "failed to copy lit source {} to {}: {error}",
                source.display(),
                local.display()
            )
        });
        Some(local)
    } else {
        None
    };
    let source_arg = if local_script.is_some() {
        "/tmp/test.sh".to_owned()
    } else {
        shell_quote(source)
    };
    let guest_payload = local_fixture
        .as_ref()
        .map(|path| (path, "/tmp/fixture"))
        .or_else(|| local_script.as_ref().map(|path| (path, "/tmp/test.sh")));
    let run_args = if let Some((source, target)) = guest_payload {
        let mount = format!(
            "type=bind,source={},target={target}",
            source.to_string_lossy()
        );
        format!(
            " run --mount={} --env=TMPDIR=/tmp ",
            shell_words::quote(&mount)
        )
    } else {
        " run ".to_owned()
    };

    let mut command = directive
        .replace("%hermit", &hermit)
        .replace("%me", fixture_arg.as_deref().unwrap_or("%me"))
        .replace("%s", &source_arg);
    command = command.replacen(" run ", &run_args, 1);
    if source_is_script {
        command = command.replace(" --bind /tmp", "");
    }

    let (command, filecheck) = if let Some((command, args)) = command.split_once(" |& FileCheck") {
        (format!("{command} 2>&1"), Some(args))
    } else if let Some((command, args)) = command.split_once(" | FileCheck") {
        (command.to_owned(), Some(args))
    } else {
        (command, None)
    };

    let mut process = Command::new("bash");
    process.args(["-c", &command]).current_dir(workdir).env(
        "TMPDIR",
        if uses_hermit {
            Path::new("/tmp")
        } else {
            workdir
        },
    );
    if let Some(fixture) = fixture {
        process.env("HERMIT_LIT_FIXTURE", fixture);
    }

    let rendered = format!("{process:?}");
    let output = process
        .output()
        .unwrap_or_else(|error| panic!("failed to run lit command {rendered}: {error}"));
    assert!(
        output.status.success(),
        "lit command failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    if let Some(filecheck_args) = filecheck {
        let prefix = filecheck_args
            .split_whitespace()
            .find_map(|arg| arg.strip_prefix("--check-prefix="))
            .unwrap_or("CHECK");
        let mut config = Config::default();
        config.options.check_prefixes = vec![Symbol::intern(prefix)];

        let mut input =
            tempfile::NamedTempFile::new_in(workdir).expect("failed to create FileCheck input");
        input
            .write_all(&output.stdout)
            .expect("failed to write FileCheck input");

        Test::from_file(source, &config)
            .verify_file(input.path())
            .unwrap_or_else(|error| {
                panic!(
                    "FileCheck failed for {rendered}: {error:?}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                )
            });
    }
}

fn run_lit(relative_path: &str, fixture: Option<&str>) {
    let _guard = lit_lock();
    let source = lit_root().join(relative_path);
    let contents = fs::read_to_string(&source)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", source.display()));
    let workdir = tempfile::Builder::new()
        .prefix("hermit-detcore-lit-")
        .tempdir()
        .expect("failed to create lit working directory");

    let directives: Vec<_> = contents
        .lines()
        .filter_map(|line| line.find("RUN:").map(|index| line[index + 4..].trim()))
        .collect();
    assert!(
        !directives.is_empty(),
        "{} contains no RUN directives",
        source.display()
    );

    for directive in directives {
        run_directive(directive, &source, fixture, workdir.path());
    }
}

macro_rules! lit_test {
    ($name:ident, $path:literal, $fixture:expr) => {
        #[test]
        fn $name() {
            run_lit($path, $fixture);
        }
    };
}

macro_rules! pmu_lit_test {
    ($name:ident, $path:literal, $fixture:expr) => {
        #[test]
        #[ignore = "requires PMU support for RCB counting"]
        fn $name() {
            run_lit($path, $fixture);
        }
    };
}

lit_test!(
    child_should_inherit_fds,
    "child_should_inherit_fds/main.rs",
    Some("child_should_inherit_fds")
);
lit_test!(
    child_should_inherit_fds_hermit_run,
    "child_should_inherit_fds/hermit-run.lit",
    Some("child_should_inherit_fds")
);
lit_test!(
    close_on_exec,
    "close_on_exec/main.rs",
    Some("close_on_exec")
);
lit_test!(
    close_on_exec_hermit_run,
    "close_on_exec/hermit-run.lit",
    Some("close_on_exec")
);
lit_test!(
    dup2_existing_fd,
    "dup2_existing_fd/main.rs",
    Some("dup2_existing_fd")
);
lit_test!(
    dup2_existing_fd_hermit_run,
    "dup2_existing_fd/hermit-run.lit",
    Some("dup2_existing_fd")
);
lit_test!(
    dup_should_create_a_valid_fd,
    "dup_should_create_a_valid_fd/main.rs",
    Some("dup_should_create_a_valid_fd")
);
lit_test!(
    dup_should_create_a_valid_fd_hermit_run,
    "dup_should_create_a_valid_fd/hermit-run.lit",
    Some("dup_should_create_a_valid_fd")
);
lit_test!(fcntl_dupfd, "fcntl_dupfd/main.rs", Some("fcntl_dupfd"));
lit_test!(
    fcntl_dupfd_hermit_run,
    "fcntl_dupfd/hermit-run.lit",
    Some("fcntl_dupfd")
);
lit_test!(
    fcntl_getfd_setfd,
    "fcntl_getfd_setfd/main.rs",
    Some("fcntl_getfd_setfd")
);
lit_test!(
    fcntl_getfd_setfd_hermit_run,
    "fcntl_getfd_setfd/hermit-run.lit",
    Some("fcntl_getfd_setfd")
);
lit_test!(
    file_race_openwrite,
    "file_race_openwrite/main.rs",
    Some("file_race_openwrite")
);
lit_test!(
    file_race_openwrite_hermit_run,
    "file_race_openwrite/hermit-run.lit",
    Some("file_race_openwrite")
);
lit_test!(
    file_write_race,
    "file_write_race/main.rs",
    Some("file_write_race")
);
lit_test!(
    file_write_race_hermit_run,
    "file_write_race/hermit-run.lit",
    Some("file_write_race")
);
lit_test!(fstat, "fstat/main.rs", Some("fstat"));
lit_test!(fstat_hermit_run, "fstat/hermit-run.lit", Some("fstat"));
lit_test!(hello_world_c, "hello_world_c/main.c", Some("hello_world_c"));
lit_test!(
    hello_world_c_hermit_run,
    "hello_world_c/hermit-run.lit",
    Some("hello_world_c")
);
lit_test!(
    hello_world_go,
    "hello_world_go/main.go",
    Some("hello_world_go")
);
lit_test!(
    hello_world_go_hermit_run,
    "hello_world_go/hermit-run.lit",
    Some("hello_world_go")
);
lit_test!(
    hello_world_rs_hermit_run,
    "hello_world_rs/hermit-run.lit",
    Some("hello_world_rs")
);
lit_test!(networking, "networking/main.c", Some("networking"));
lit_test!(
    networking_hermit_run,
    "networking/hermit-run.lit",
    Some("networking")
);
lit_test!(
    no_close_on_exec,
    "no_close_on_exec/main.rs",
    Some("no_close_on_exec")
);
lit_test!(
    no_close_on_exec_hermit_run,
    "no_close_on_exec/hermit-run.lit",
    Some("no_close_on_exec")
);
lit_test!(
    open_null_ptr,
    "open_null_ptr/main.rs",
    Some("open_null_ptr")
);
lit_test!(
    open_null_ptr_hermit_run,
    "open_null_ptr/hermit-run.lit",
    Some("open_null_ptr")
);
lit_test!(
    openat_lowest_fd,
    "openat_lowest_fd/main.rs",
    Some("openat_lowest_fd")
);
lit_test!(
    openat_lowest_fd_hermit_run,
    "openat_lowest_fd/hermit-run.lit",
    Some("openat_lowest_fd")
);
lit_test!(
    openat_next_lowest_fd,
    "openat_next_lowest_fd/main.rs",
    Some("openat_next_lowest_fd")
);
lit_test!(
    openat_next_lowest_fd_hermit_run,
    "openat_next_lowest_fd/hermit-run.lit",
    Some("openat_next_lowest_fd")
);
lit_test!(
    pipe_creates_valid_fds,
    "pipe_creates_valid_fds/main.rs",
    Some("pipe_creates_valid_fds")
);
lit_test!(
    pipe_creates_valid_fds_hermit_run,
    "pipe_creates_valid_fds/hermit-run.lit",
    Some("pipe_creates_valid_fds")
);
lit_test!(print_race, "print_race/main.rs", Some("print_race"));
lit_test!(read_badfd, "read_badfd/main.rs", Some("read_badfd"));
lit_test!(
    read_badfd_hermit_run,
    "read_badfd/hermit-run.lit",
    Some("read_badfd")
);
lit_test!(rt_sigaction, "rt_sigaction/main.c", Some("rt_sigaction"));
lit_test!(
    rt_sigaction_hermit_run,
    "rt_sigaction/hermit-run.lit",
    Some("rt_sigaction")
);
lit_test!(
    rt_sigprocmask,
    "rt_sigprocmask/main.c",
    Some("rt_sigprocmask")
);
lit_test!(
    rt_sigprocmask_hermit_run,
    "rt_sigprocmask/hermit-run.lit",
    Some("rt_sigprocmask")
);
lit_test!(
    sched_getaffinity,
    "sched_getaffinity/main.rs",
    Some("sched_getaffinity")
);
lit_test!(tmpfs, "tmpfs.test", None);
lit_test!(tmpfs_preserved, "tmpfs_preserved.test", None);
lit_test!(uname, "uname.test", None);
lit_test!(utime, "utime/main.rs", Some("utime"));
lit_test!(utime_hermit_run, "utime/hermit-run.lit", Some("utime"));
lit_test!(utimes, "utimes/main.rs", Some("utimes"));
lit_test!(utimes_hermit_run, "utimes/hermit-run.lit", Some("utimes"));
lit_test!(exit_code, "exit_code.test", None);

pmu_lit_test!(cat, "cat.test", None);
pmu_lit_test!(
    dup2_existing_fd_hermit_run_strict,
    "dup2_existing_fd/hermit-run-strict.lit",
    Some("dup2_existing_fd")
);
pmu_lit_test!(
    dup_should_create_a_valid_fd_hermit_run_strict,
    "dup_should_create_a_valid_fd/hermit-run-strict.lit",
    Some("dup_should_create_a_valid_fd")
);
pmu_lit_test!(
    fcntl_dupfd_hermit_run_strict,
    "fcntl_dupfd/hermit-run-strict.lit",
    Some("fcntl_dupfd")
);
pmu_lit_test!(
    fcntl_getfd_setfd_hermit_run_strict,
    "fcntl_getfd_setfd/hermit-run-strict.lit",
    Some("fcntl_getfd_setfd")
);
pmu_lit_test!(
    file_race_openwrite_hermit_run_strict,
    "file_race_openwrite/hermit-run-strict.lit",
    Some("file_race_openwrite")
);
pmu_lit_test!(
    file_write_race_hermit_run_strict,
    "file_write_race/hermit-run-strict.lit",
    Some("file_write_race")
);
pmu_lit_test!(
    fstat_hermit_run_strict,
    "fstat/hermit-run-strict.lit",
    Some("fstat")
);
pmu_lit_test!(
    hello_world_c_hermit_run_strict,
    "hello_world_c/hermit-run-strict.lit",
    Some("hello_world_c")
);
pmu_lit_test!(
    hello_world_go_hermit_run_strict,
    "hello_world_go/hermit-run-strict.lit",
    Some("hello_world_go")
);
pmu_lit_test!(
    hello_world_go_hermit_run_strict_verify,
    "hello_world_go/hermit-run-strict-verify.lit",
    Some("hello_world_go")
);
pmu_lit_test!(
    hello_world_rs,
    "hello_world_rs/main.rs",
    Some("hello_world_rs")
);
pmu_lit_test!(
    hello_world_rs_hermit_run_strict,
    "hello_world_rs/hermit-run-strict.lit",
    Some("hello_world_rs")
);
pmu_lit_test!(hostname, "hostname.test", None);
pmu_lit_test!(
    open_null_ptr_hermit_run_strict,
    "open_null_ptr/hermit-run-strict.lit",
    Some("open_null_ptr")
);
pmu_lit_test!(
    openat_lowest_fd_hermit_run_strict,
    "openat_lowest_fd/hermit-run-strict.lit",
    Some("openat_lowest_fd")
);
pmu_lit_test!(
    openat_next_lowest_fd_hermit_run_strict,
    "openat_next_lowest_fd/hermit-run-strict.lit",
    Some("openat_next_lowest_fd")
);
pmu_lit_test!(
    pipe_creates_valid_fds_hermit_run_strict,
    "pipe_creates_valid_fds/hermit-run-strict.lit",
    Some("pipe_creates_valid_fds")
);
pmu_lit_test!(
    print_race_hermit_run_strict,
    "print_race/hermit-run-strict.lit",
    Some("print_race")
);
pmu_lit_test!(
    print_race_hermit_run_strict_verify,
    "print_race/hermit-run-strict-verify.lit",
    Some("print_race")
);
pmu_lit_test!(
    read_badfd_hermit_run_strict,
    "read_badfd/hermit-run-strict.lit",
    Some("read_badfd")
);
pmu_lit_test!(
    sched_getaffinity_hermit_run_strict,
    "sched_getaffinity/hermit-run-strict.lit",
    Some("sched_getaffinity")
);
pmu_lit_test!(
    sched_getaffinity_hermit_run_strict_verify,
    "sched_getaffinity/hermit-run-strict-verify.lit",
    Some("sched_getaffinity")
);
pmu_lit_test!(
    scheduler_strategies_chaos_verify,
    "scheduler_strategies/hermit-run-shedulers-chaos-verify.sh",
    None
);
pmu_lit_test!(
    scheduler_strategies_strict_verify,
    "scheduler_strategies/hermit-run-shedulers-strict-verify.sh",
    None
);
pmu_lit_test!(
    utime_hermit_run_strict,
    "utime/hermit-run-strict.lit",
    Some("utime")
);
pmu_lit_test!(
    utimes_hermit_run_strict,
    "utimes/hermit-run-strict.lit",
    Some("utimes")
);
