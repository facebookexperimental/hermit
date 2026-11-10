/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A pinned run that cannot keep other Hermit runs off its CPU says so by
//! name on stderr (`hermit run --pinned-cpu`), and the CPU it lands on never
//! reaches the guest. Each test forces the case it names without load: the
//! run may use one CPU, and the test holds that CPU's lock file in a private
//! lock directory (`--pinned-cpu-dir`), as another run would. Nothing
//! here spins; the guests are `echo` and shell builtins.

use std::fs;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::process::Output;

/// The first CPU this test process may run on.
fn one_cpu() -> usize {
    let mask = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0))
        .expect("sched_getaffinity of the test process");
    (0..nix::sched::CpuSet::count())
        .find(|cpu| mask.is_set(*cpu).unwrap_or(false))
        .expect("an allowed CPU")
}

/// Hold `cpu`'s lock file in `dir` while the returned file is open, as a
/// Hermit run that reserved it does.
fn hold(dir: &Path, cpu: usize) -> fs::File {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(cpu.to_string()))
        .expect("create the lock file");
    // SAFETY: flock on a descriptor this test owns.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "lock CPU {cpu}"
    );
    file
}

/// `hermit` with `args` (a `run` command), allowed only `cpu`, given the lock
/// directory `dir` with `--pinned-cpu-dir`, with the notice left on (no
/// `--pinned-cpu-notice`, and neither environment fallback set). Hermit
/// starts in a new user and network namespace (`unshare -rn`), so the only
/// runs that can hold its CPU are the ones this test arranges: the abstract
/// socket names other tests' runs hold live in the test's namespace, which
/// it cannot see, and the lock directory is private to the test.
fn pinned_hermit(cpu: usize, dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("unshare");
    command.arg("-rn").arg(env!("CARGO_BIN_EXE_hermit"));
    assert_eq!(args.first(), Some(&"run"));
    let dir_arg = format!("--pinned-cpu-dir={}", dir.display());
    let mut with_dir = vec!["run", dir_arg.as_str()];
    with_dir.extend_from_slice(&args[1..]);
    super::append_hermit_args(&mut command, &with_dir);
    command
        .env_remove("HERMIT_PINNED_CPU_DIR")
        .env_remove("HERMIT_PINNED_CPU_NOTICE");
    // SAFETY: sched_setaffinity only, between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(move || {
            let mut set = nix::sched::CpuSet::new();
            set.set(cpu).map_err(std::io::Error::from)?;
            nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set)
                .map_err(std::io::Error::from)
        });
    }
    command.output().expect("start hermit")
}

/// A private lock directory, owned by this user.
fn lock_dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("lock directory")
}

const ECHO: [&str; 6] = ["run", "--pin-threads", "--", "/bin/echo", "pinned", "guest"];

/// (a) With the run's only CPU held by another run, the default
/// `--pinned-cpu=share` runs the guest on it and prints the cpu-shared notice,
/// naming the CPU; with the CPU free it prints none. Not opted out of the
/// notice.
#[test]
fn a_run_that_shares_its_cpu_prints_the_cpu_shared_notice() {
    let cpu = one_cpu();
    let dir = lock_dir();
    let free = pinned_hermit(cpu, dir.path(), &ECHO);
    assert!(free.status.success(), "{free:?}");
    assert_eq!(super::stdout(&free), "pinned guest\n");
    assert!(!super::stderr(&free).contains("cpu-"), "{free:?}");

    let _other_run = hold(dir.path(), cpu);
    let shared = pinned_hermit(cpu, dir.path(), &ECHO);
    assert!(shared.status.success(), "{shared:?}");
    assert_eq!(super::stdout(&shared), "pinned guest\n");
    let stderr = super::stderr(&shared);
    assert!(
        stderr.contains(&format!(
            "hermit: warning: cpu-shared: every allowed CPU ({cpu}) is held by another Hermit run; \
             this run shares CPU {cpu}"
        )),
        "{stderr}"
    );
}

/// `--pinned-cpu=refuse` refuses that run before the guest starts, naming
/// the CPU set; `--pinned-cpu=wait:1` sleeps for a second of retries and then
/// refuses, naming the wait.
#[test]
fn refuse_and_wait_name_the_cpu_set_instead_of_sharing() {
    let cpu = one_cpu();
    let dir = lock_dir();
    let _other_run = hold(dir.path(), cpu);
    for (policy, expected) in [
        (
            "--pinned-cpu=refuse",
            format!(
                "--pinned-cpu=refuse: every allowed CPU ({cpu}) is held by another Hermit run, so \
                 the run was not started. Retry when one is free, wait for one with \
                 --pinned-cpu=wait[:SECONDS], allow more CPUs, or run on a shared CPU with \
                 --pinned-cpu=share."
            ),
        ),
        (
            "--pinned-cpu=wait:1",
            format!(
                "--pinned-cpu=wait:1: every allowed CPU ({cpu}) stayed held by another Hermit run \
                 for the whole 1 s wait, so the run was not started. Retry later, wait longer \
                 with --pinned-cpu=wait:SECONDS, allow more CPUs, or run on a shared CPU with \
                 --pinned-cpu=share."
            ),
        ),
    ] {
        let args = ["run", "--pin-threads", policy, "--", "/bin/echo", "never"];
        let output = pinned_hermit(cpu, dir.path(), &args);
        assert_eq!(output.status.code(), Some(122), "{policy}: {output:?}");
        assert_eq!(super::stdout(&output), "", "{policy}: the guest never ran");
        assert!(
            super::stderr(&output).contains(&expected),
            "{policy}: {output:?}"
        );
    }
}

/// (c) The abstract socket names stop at the network namespace: a CPU whose
/// name the test holds in its own namespace is free to a run in another one.
/// The lock file does not stop there: once the test also holds the CPU's lock
/// file, that run sees the CPU held and says so. With the lock directory
/// unusable (here a symlink) the run is reserved only within its namespace
/// and prints the cpu-netns-only notice.
#[test]
fn a_run_in_another_network_namespace_still_sees_a_held_cpu() {
    use std::os::linux::net::SocketAddrExt;
    let cpu = one_cpu();
    let dir = lock_dir();
    // The name in the test's namespace; another test's run may hold it
    // already, which is the same for this test.
    let name = format!("hermit-pinned-core:{cpu}");
    let _socket = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
        .and_then(|address| std::os::unix::net::UnixDatagram::bind_addr(&address));
    let output = pinned_hermit(cpu, dir.path(), &ECHO);
    assert!(output.status.success(), "{output:?}");
    assert!(!super::stderr(&output).contains("cpu-"), "{output:?}");

    let _other_run = hold(dir.path(), cpu);
    let output = pinned_hermit(cpu, dir.path(), &ECHO);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(super::stdout(&output), "pinned guest\n");
    assert!(
        super::stderr(&output).contains("hermit: warning: cpu-shared:"),
        "{output:?}"
    );

    let parent = lock_dir();
    let link = parent.path().join("link");
    let target = lock_dir();
    std::os::unix::fs::symlink(target.path(), &link).unwrap();
    let output = pinned_hermit(cpu, &link, &ECHO);
    assert!(output.status.success(), "{output:?}");
    let stderr = super::stderr(&output);
    assert!(
        stderr.contains(&format!(
            "hermit: warning: cpu-netns-only: CPU {cpu} is reserved only against Hermit runs in \
             this network namespace"
        )),
        "{stderr}"
    );
    assert!(stderr.contains("is a symbolic link"), "{stderr}");
}

/// The same seed gives the same guest-visible run whether or not the run
/// shares its CPU: a strict `--verify --verify-strict` run reaches bitwise
/// parity both ways, and the guest's output, as the report hashes it, is the
/// same in both. The notice is Hermit's own stderr, outside the compared
/// guest streams, and appears once for the invocation, not once per run.
#[test]
fn sharing_a_cpu_changes_nothing_the_guest_sees() {
    let cpu = one_cpu();
    let dir = lock_dir();
    let reports = lock_dir();
    let run = |name: &str| {
        let report = reports.path().join(format!("{name}.json"));
        let report_arg = format!("--verify-json={}", report.display());
        let args = [
            "run",
            "--strict",
            "--pin-threads",
            "--chaos",
            "--seed=7",
            "--verify",
            "--verify-strict",
            report_arg.as_str(),
            "--",
            "/bin/sh",
            "-c",
            // Shell builtins only: with `date` in it, this test's run with
            // its CPU free failed --verify-strict in 2 of 5 tries (an fstat
            // result buffer of `date` differed between the pair's two runs),
            // which placement cannot cause and is not what this test checks.
            "echo $RANDOM $$; echo $((RANDOM % 97))",
        ];
        let output = pinned_hermit(cpu, dir.path(), &args);
        assert!(output.status.success(), "{name}: {output:?}");
        let report: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&report).expect("report")).expect("JSON");
        assert_eq!(report["bitwise_parity"], true, "{name}: {report}");
        assert!(
            report["compared_log_messages"]["left"].as_u64() > Some(0),
            "{name}: {report}"
        );
        (output, report)
    };
    let (alone, alone_report) = run("alone");
    assert!(!super::stderr(&alone).contains("cpu-shared"), "{alone:?}");
    let _other_run = hold(dir.path(), cpu);
    let (shared, shared_report) = run("shared");
    assert_eq!(
        super::stderr(&shared)
            .matches("hermit: warning: cpu-shared:")
            .count(),
        1,
        "one notice for the whole --verify invocation, not one per run: {shared:?}"
    );
    assert_eq!(super::stdout(&shared), super::stdout(&alone));
    assert_eq!(
        shared_report["compared_outputs"], alone_report["compared_outputs"],
        "the guest's exit, stdout and stderr hashes"
    );
}
