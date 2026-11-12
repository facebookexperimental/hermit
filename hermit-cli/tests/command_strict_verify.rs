/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end L2 coverage for standard command-line tools that are expected on
//! the portable CI runner.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const HERMIT_VERIFY_TIMEOUT: &str = "60s";
const HERMIT_VERIFY_KILL_AFTER: &str = "10s";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";

struct StrictCommandCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
    stdin: Option<&'static [u8]>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_command(case: &StrictCommandCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "ERROR: required command {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn pinned_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
    match requested {
        None => Ok(Vec::new()),
        Some(value) if value == OsStr::new(HERMETIC_TEST_WORKDIR) => Ok(vec![
            "--base-env=minimal".into(),
            "--mount=type=tmpfs,target=/test".into(),
            "--workdir=/test".into(),
        ]),
        Some(value) => Err(format!(
            "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {value:?}"
        )),
    }
}

fn configure_pinned_root(command: &mut Command) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    let args = pinned_root_args(requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
    command.args(args);
}

fn assert_l2_under_strict_verify(case: &StrictCommandCase) {
    assert_l2_under_strict_verify_with(case, &[]);
}

/// As [`assert_l2_under_strict_verify`], at the `standard` `/proc` level, for
/// a command that reads a name only that level has.
fn assert_l2_under_strict_verify_at_standard(case: &StrictCommandCase) {
    assert_l2_under_strict_verify_with(case, &["--proc=standard"]);
}

fn assert_l2_under_strict_verify_with(case: &StrictCommandCase, run_args: &[&str]) {
    let program = required_command(case);
    let (output, rendered) = strict_verify_output(&program, case.args, case.stdin, run_args);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} did not reach L2 under strict verification ({rendered})\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "{} exited 0 without Hermit's determinism marker ({rendered})\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

/// Runs `program args` under `hermit --log=info run --strict --verify`, with
/// `run_args` after `--verify`, in a fresh HOME and working directory, and
/// returns its output and the rendered command line.
fn strict_verify_output(
    program: &Path,
    args: &[&str],
    stdin: Option<&[u8]>,
    run_args: &[&str],
) -> (Output, String) {
    let home = tempfile::tempdir().expect("failed to create isolated command HOME");
    std::fs::create_dir_all(home.path().join(".config/procps"))
        .expect("failed to preseed the isolated procps HOME");
    let working_directory = tempfile::Builder::new()
        .prefix("command-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create isolated command working directory");
    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--strict",
            detcore_model::config::CANONICAL_TEST_EPOCH_ARG,
            "--verify",
        ])
        .args(run_args)
        .arg(format!("--env=HOME={}", home.path().display()))
        .arg(format!(
            "--env=XDG_CONFIG_HOME={}",
            home.path().join(".config").display()
        ));
    configure_pinned_root(&mut command);
    command
        .arg("--")
        .arg(program)
        .args(args)
        .env("HOME", home.path())
        .current_dir(working_directory.path())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    // A typed PMU skid-overshoot refusal, and nothing else, runs it again.
    let (output, ()) = hermit_test::hermit_output_with(&rendered, || {
        let mut child = command
            .spawn()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("piped stdin should be available")
                .write_all(input)
                .unwrap_or_else(|error| panic!("failed to write stdin for {rendered}: {error}"));
        }
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("failed to collect {rendered}: {error}"));
        (output, ())
    });
    (output, rendered)
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + standard Unix command tools"]
fn common_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "ls",
            candidates: &["/usr/bin/ls", "/bin/ls"],
            args: &["-1", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "cat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "wc",
            candidates: &["/usr/bin/wc", "/bin/wc"],
            args: &["-l", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "head",
            candidates: &["/usr/bin/head", "/bin/head"],
            args: &["-n", "3", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sort",
            candidates: &["/usr/bin/sort", "/bin/sort"],
            args: &[],
            stdin: Some(b"gamma\nalpha\nbeta\n"),
        },
        StrictCommandCase {
            name: "uniq",
            candidates: &["/usr/bin/uniq", "/bin/uniq"],
            args: &["/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tail",
            candidates: &["/usr/bin/tail", "/bin/tail"],
            args: &["-n", "3", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "env",
            candidates: &["/usr/bin/env", "/bin/env"],
            args: &["-i", "HERMIT_COMMAND_COMPAT=1"],
            stdin: None,
        },
        StrictCommandCase {
            name: "date",
            candidates: &["/usr/bin/date", "/bin/date"],
            args: &["-u", "+%s"],
            stdin: None,
        },
        StrictCommandCase {
            name: "id",
            candidates: &["/usr/bin/id", "/bin/id"],
            args: &["-u"],
            stdin: None,
        },
        StrictCommandCase {
            name: "hostname",
            candidates: &["/usr/bin/hostname", "/bin/hostname"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "uname",
            candidates: &["/usr/bin/uname", "/bin/uname"],
            args: &["-a"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tr",
            candidates: &["/usr/bin/tr", "/bin/tr"],
            args: &["a-z", "A-Z"],
            stdin: Some(b"hello hermit\n"),
        },
        StrictCommandCase {
            name: "cut",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-d:", "-f1", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tee",
            candidates: &["/usr/bin/tee", "/bin/tee"],
            args: &["/dev/null"],
            stdin: Some(b"tee-through-hermit\n"),
        },
        StrictCommandCase {
            name: "diff",
            candidates: &["/usr/bin/diff", "/bin/diff"],
            args: &["/etc/hostname", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "grep",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: &["-m", "1", "root", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1,3p", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "find",
            candidates: &["/usr/bin/find", "/bin/find"],
            args: &[
                "/etc",
                "-maxdepth",
                "1",
                "-type",
                "f",
                "-name",
                "hostname",
                "-print",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "xargs",
            candidates: &["/usr/bin/xargs", "/bin/xargs"],
            args: &["echo"],
            stdin: Some(b"one two three\n"),
        },
        StrictCommandCase {
            name: "basename",
            candidates: &["/usr/bin/basename", "/bin/basename"],
            args: &["/tmp/hermit-example.txt", ".txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "dirname",
            candidates: &["/usr/bin/dirname", "/bin/dirname"],
            args: &["/tmp/hermit-example.txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "realpath",
            candidates: &["/usr/bin/realpath", "/bin/realpath"],
            args: &["/etc/../etc/passwd"],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#877)
        StrictCommandCase {
            name: "readlink mount namespace",
            candidates: &["/usr/bin/readlink", "/bin/readlink"],
            args: &["/proc/self/ns/mnt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "readlink executable control",
            candidates: &["/usr/bin/readlink", "/bin/readlink"],
            args: &["/proc/self/exe"],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        StrictCommandCase {
            name: "Python PID namespace readlink",
            candidates: &["/usr/bin/python3"],
            args: &[
                "-c",
                "import os; d=os.open('/', os.O_RDONLY); \
                 print(os.readlink('/proc/self/ns/pid', dir_fd=d))",
            ],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        StrictCommandCase {
            name: "Perl user namespace readlink",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &["-e", "print readlink('/proc/self/ns/user'), qq(\\n)"],
            stdin: None,
        },
        StrictCommandCase {
            name: "md5sum",
            candidates: &["/usr/bin/md5sum", "/bin/md5sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sha256sum",
            candidates: &["/usr/bin/sha256sum", "/bin/sha256sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "du",
            candidates: &["/usr/bin/du", "/bin/du"],
            args: &["-b", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sqlite3",
            candidates: &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"],
            args: &[
                ":memory:",
                "CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); \
                 SELECT group_concat(v, ',') FROM (SELECT v FROM t ORDER BY v);",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
            stdin: None,
        },
        StrictCommandCase {
            name: "perl",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &["-e", "print join(',', map { $_ * $_ } 1..5), qq(\n)"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

#[test]
fn pinned_root_arguments_are_exact_and_fail_closed() {
    assert!(pinned_root_args(None).unwrap().is_empty());
    assert_eq!(
        pinned_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );
    let error = pinned_root_args(Some(OsStr::new("/tmp"))).unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces + whoami/groups"]
fn identity_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "whoami",
            candidates: &["/usr/bin/whoami", "/bin/whoami"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "groups",
            candidates: &["/usr/bin/groups", "/bin/groups"],
            args: &[],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-843): Review strict process-accounting command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + procps-ng tools"]
fn process_accounting_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "ps aux",
            candidates: &["/usr/bin/ps", "/bin/ps"],
            args: &["aux"],
            stdin: None,
        },
        StrictCommandCase {
            name: "free -m",
            candidates: &["/usr/bin/free", "/bin/free"],
            args: &["-m"],
            stdin: None,
        },
        StrictCommandCase {
            name: "top batch",
            candidates: &["/usr/bin/top", "/bin/top"],
            args: &["-b", "-n", "1", "-p", "1", "-w", "80"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }

    // These read names only the `standard` /proc level has.
    let standard_cases = [StrictCommandCase {
        name: "vmstat -s",
        candidates: &["/usr/bin/vmstat", "/bin/vmstat"],
        args: &["-s"],
        stdin: None,
    }];
    for case in &standard_cases {
        assert_l2_under_strict_verify_at_standard(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review kernel pseudo-file command coverage.
#[test]
#[ignore = "e2e: requires hermit + mount namespaces + util-linux/procps/sysstat"]
// TODO(#2791): Remove the portable-DAG exclusion after #2801 determinizes mountinfo.
fn kernel_pseudofile_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "findmnt",
            candidates: &["/usr/bin/findmnt", "/bin/findmnt"],
            args: &[
                "--kernel",
                "--list",
                "--output",
                "TARGET,SOURCE,FSTYPE,OPTIONS",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "sysctl random UUID",
            candidates: &["/usr/sbin/sysctl", "/usr/bin/sysctl"],
            args: &["kernel.random.uuid"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }

    // These read names only the `standard` /proc level has.
    let standard_cases = [StrictCommandCase {
        name: "sar resource tables",
        candidates: &["/usr/bin/sar"],
        args: &["-v", "1", "1"],
        stdin: None,
    }];
    for case in &standard_cases {
        assert_l2_under_strict_verify_at_standard(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-881)
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + util-linux ionice"]
fn ionice_query_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "ionice current-process query",
        candidates: &["/usr/bin/ionice", "/bin/ionice"],
        args: &["-p", "0"],
        stdin: None,
    };
    assert_l2_under_strict_verify(&case);
}

/// The names iostat and lsmod read at the default `/proc` level, where the
/// removed io_accounting and kernel_activity cases ran them.
const DEFAULT_LEVEL_ACTIVITY_FILES: &[&str] = &[
    "/proc/uptime",
    "/proc/stat",
    "/proc/cmdline",
    "/proc/modules",
];

/// The names iostat -d -x, vmstat -d, pidstat -d, lsmod, lsirq and mpstat -I
/// SCPU read at the `standard` level, where those cases ran the other four.
const STANDARD_LEVEL_ACTIVITY_FILES: &[&str] = &[
    "/proc/uptime",
    "/proc/stat",
    "/proc/cmdline",
    "/proc/modules",
    "/proc/diskstats",
    "/proc/softirqs",
    "/proc/interrupts",
    "/proc/self/io",
    "/proc/1/io",
    "/proc/1/stat",
    "/proc/1/schedstat",
    "/proc/1/status",
    "/proc/sys/kernel/osrelease",
    "/sys/devices/system/cpu/online",
];

/// The `/proc` and `/sys` reads that iostat -d -x, vmstat -d and pidstat -d
/// (the old io_accounting case) and lsmod, lsirq and mpstat -I SCPU (the old
/// kernel_activity case) made, made instead by one in-repo guest,
/// tests/c/procfs_activity_sampler.c. sysstat is in neither fbsource's
/// third-party tree nor its CI hosts, so fbsource skipped both cases. The
/// guest reads the tools' files the way they do: stdio reads (openat, fstat,
/// read, close) of each counter file before and after a one-second interval,
/// waited out once with alarm(2) and pause(2) as sysstat does and once with
/// clock_nanosleep(2) as procps does; getdents64 listings of
/// /sys/devices/system/cpu and /sys/block with newfstatat(2) and access(2) of
/// each entry and a rewind (lseek) of each device's stat file; and uname(2),
/// time(2), /etc/localtime and /proc/self/auxv. It prints every line it reads
/// and fails if the uptime does not advance across a wait. It runs at the
/// `standard` level and at the default level, each to canonical bitwise
/// parity (`--verify-strict`), and each level must present the names the
/// tools read there.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-861): Review strict I/O-accounting command coverage.
// TODO-HUMAN-REVIEW(PR-883): Review interrupt and module command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + cc"]
fn activity_counter_reads_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let build = tempfile::Builder::new()
        .prefix("procfs-activity-sampler-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest build directory");
    let guest = compile_c_guest("procfs_activity_sampler", build.path());

    let standard = assert_bitwise_parity_under_strict_verify(&guest, &["--proc=standard"]);
    for path in STANDARD_LEVEL_ACTIVITY_FILES {
        assert_read(&standard, path, "--proc=standard");
    }
    let default = assert_bitwise_parity_under_strict_verify(&guest, &[]);
    for path in DEFAULT_LEVEL_ACTIVITY_FILES {
        assert_read(&default, path, "the default level");
    }
}

/// Compiles tests/c/`name`.c into `directory`.
fn compile_c_guest(name: &str, directory: &Path) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let source = repository.join("tests/c").join(format!("{name}.c"));
    let guest = directory.join(name);
    let output = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap_or_else(|error| panic!("failed to run cc on {}: {error}", source.display()));
    assert!(
        output.status.success(),
        "cc failed on {}\nstdout:\n{}\nstderr:\n{}",
        source.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    guest
}

/// Runs `guest` under `hermit run --strict --verify --verify-strict`, with
/// `run_args`, requires canonical bitwise parity over a nonempty INFO log, and
/// returns the guest's stdout.
fn assert_bitwise_parity_under_strict_verify(guest: &Path, run_args: &[&str]) -> String {
    let report_directory =
        tempfile::tempdir().expect("failed to create the verification report directory");
    let report_path = report_directory.path().join("verify.json");
    let report_arg = format!("--verify-json={}", report_path.display());
    let mut verify_args = vec!["--verify-strict", report_arg.as_str()];
    verify_args.extend_from_slice(run_args);
    let (output, rendered) = strict_verify_output(guest, &[], None, &verify_args);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{rendered} failed\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    let report_bytes = std::fs::read(&report_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", report_path.display()));
    let report: serde_json::Value =
        serde_json::from_slice(&report_bytes).expect("Hermit's verification report is JSON");
    let compared = |side: &str| report["compared_log_messages"][side].as_u64().unwrap_or(0);
    assert!(
        report["verdict"] == "matched"
            && report["verified"] == true
            && report["bitwise_parity"] == true
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["log_scope"] == "info"
            && compared("left") > 0
            && compared("left") == compared("right"),
        "{rendered}: the report is not canonical bitwise parity over a nonempty log: {report}",
    );
    stdout
}

/// Requires the guest's "`path`: N lines" line, which it prints after reading
/// a file it could open.
fn assert_read(stdout: &str, path: &str, level: &str) {
    let read = stdout.lines().any(|line| {
        line.strip_prefix(path)
            .and_then(|rest| rest.strip_prefix(": "))
            .and_then(|rest| rest.strip_suffix(" lines"))
            .is_some_and(|count| count.parse::<u64>().is_ok())
    });
    assert!(read, "at {level}, the guest did not read {path}:\n{stdout}");
}

/// The NUMA tools on the kernel without NUMA that Hermit presents
/// (https://github.com/rrnewton/hermit/issues/5269): libnuma's probe
/// answers ENOSYS and the sysfs node directory does not exist, so each
/// tool prints its no-NUMA line and exits 1, deterministically. The
/// shell wrapper requires exactly that answer, as the compat cells do.
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-865): Review NUMA and sensor command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + numactl/lm_sensors tools"]
fn hardware_accounting_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "numastat",
            candidates: &["/bin/bash", "/usr/bin/bash"],
            args: &[
                "-c",
                "output=$(/usr/bin/numastat 2>&1); rc=$?; test \"$rc\" -eq 1 && \
                 test \"$output\" = \"sysfs not mounted or system not NUMA aware: \
                 No such file or directory\" && printf \"numastat-no-numa\\n\"",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "numactl hardware",
            candidates: &["/bin/bash", "/usr/bin/bash"],
            args: &[
                "-c",
                "output=$(/usr/bin/numactl --hardware 2>&1); rc=$?; test \"$rc\" -eq 1 && \
                 test \"$output\" = \"No NUMA available on this system\" && \
                 printf \"numactl-no-numa\\n\"",
            ],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_prlimit64_query_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 prlimit64 query",
        candidates: &["/usr/bin/python3"],
        args: &[],
        stdin: None,
    };
    let python = required_command(&case);
    let query = "import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE))";
    let working_directory = tempfile::Builder::new()
        .prefix("python-prlimit64-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create isolated Python working directory");

    let mut strict_command = Command::new("timeout");
    strict_command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "run", "--strict"]);
    configure_pinned_root(&mut strict_command);
    let strict_output = hermit_test::hermit_output(
        "hermit run",
        strict_command
            .arg("--")
            .arg(&python)
            .args(["-c", query])
            .current_dir(working_directory.path()),
    );
    let strict_stdout = String::from_utf8_lossy(&strict_output.stdout);
    let strict_stderr = String::from_utf8_lossy(&strict_output.stderr);
    assert!(
        strict_output.status.success(),
        "Python prlimit64 strict-mode query failed: {}\n\
         stdout:\n{strict_stdout}\nstderr:\n{strict_stderr}",
        strict_output.status
    );
    assert_eq!(
        strict_stdout.trim(),
        "(1048576, 1048576)",
        "Python observed a non-deterministic RLIMIT_NOFILE value"
    );

    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=info", "run", "--strict", "--verify"]);
    configure_pinned_root(&mut command);
    let output = hermit_test::hermit_output(
        "hermit run",
        command
            .arg("--")
            .arg(&python)
            .args(["-c", query])
            .current_dir(working_directory.path()),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Python prlimit64 query did not reach L2 under strict verification: {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "Hermit exited 0 without its determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_getrandom_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 getrandom",
        candidates: &["/usr/bin/python3"],
        args: &["-c", "import os; print(os.urandom(16).hex())"],
        stdin: None,
    };
    assert_l2_under_strict_verify(&case);
}
