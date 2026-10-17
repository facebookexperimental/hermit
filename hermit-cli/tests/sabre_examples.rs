/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

#[path = "common/dispatch_stats.rs"]
mod dispatch_stats;

#[path = "common/run2_log.rs"]
mod run2_log;

#[path = "common/host_input.rs"]
mod host_input;

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

// Keep examples with manifest-disabled SaBRe verify cells out of this ratchet.
// TODO(#1519): SaBRe does not yet determinize timed-progress-bar.py's busy-wait
// polling on virtual wall-clock advancement.
// TODO(#2895): SaBRe strict verification of rand.py retains different syscall
// streams across its two runs. Other enabled backends still cover both guests.
const NON_RACY_EXAMPLES: [&str; 2] = ["date.sh", "devrand.sh"];
const SABRE_BACKEND_FACT_PREFIX: &str = ":: Backend: sabre static rewriting + ptrace runtime;";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
// Independent comparison runs must receive the same clock input. Keep its
// fractional precision; virtual time still progresses throughout each guest.
const COMPARISON_EPOCH: &str = "--epoch=2026-01-01T00:00:00.123456789Z";

fn execution_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
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

#[test]
fn sabre_non_racy_example_baseline_is_exact() {
    assert_eq!(NON_RACY_EXAMPLES, ["date.sh", "devrand.sh"]);
}

#[test]
fn sabre_pinned_root_arguments_are_exact_and_fail_closed() {
    assert!(execution_root_args(None).unwrap().is_empty());
    assert_eq!(
        execution_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );
    let error = execution_root_args(Some(OsStr::new("/tmp"))).unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));

    let command = example_command_with_execution_root(
        Path::new("/bin/true"),
        &[],
        None,
        false,
        None,
        None,
        Some(OsStr::new("/test")),
    )
    .unwrap();
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(
        args.iter()
            .filter(|arg| arg.to_string_lossy().starts_with("--epoch="))
            .copied()
            .collect::<Vec<_>>(),
        [OsStr::new(COMPARISON_EPOCH)]
    );
    assert!(args.contains(&OsStr::new("--base-env=minimal")));
    assert!(args.windows(2).any(|args| {
        args == [
            OsStr::new("--mount=type=tmpfs,target=/test"),
            OsStr::new("--workdir=/test"),
        ]
    }));

    let error = example_command_with_execution_root(
        Path::new("/bin/true"),
        &[],
        None,
        false,
        None,
        None,
        Some(OsStr::new("/tmp")),
    )
    .unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
}

fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_SABRE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

fn sabre_loader() -> Option<PathBuf> {
    let hermit = hermit_binary();
    let executable_dir = hermit.parent().expect("Hermit binary should have a parent");
    let target_dir = executable_dir
        .parent()
        .expect("Hermit binary should be inside a profile directory");
    let configured = std::env::var_os("HERMIT_SABRE_BINARY").map(PathBuf::from);
    let loader = configured
        .clone()
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = executable_dir.join("libdetcore_sabre.so");
    let revision_file = loader.with_file_name("sabre.revision");
    if !loader.is_file() || !plugin.is_file() {
        if configured.is_some() {
            panic!(
                "configured SaBRe artifacts are unavailable: loader={}, plugin={}, revision={}",
                loader.display(),
                plugin.display(),
                revision_file.display(),
            );
        }
        eprintln!(
            "skipping SaBRe example parity: artifacts are unavailable: loader={}, plugin={}, revision={}",
            loader.display(),
            plugin.display(),
            revision_file.display(),
        );
        return None;
    }

    let revision = if revision_file.is_file() {
        std::fs::read_to_string(&revision_file).unwrap_or_else(|error| {
            panic!(
                "failed to read SaBRe revision provenance {}: {error}",
                revision_file.display(),
            )
        })
    } else if configured.is_some() {
        panic!(
            "configured SaBRe revision provenance is unavailable: loader={}, plugin={}, revision={}",
            loader.display(),
            plugin.display(),
            revision_file.display(),
        )
    } else {
        "unavailable".to_owned()
    };
    let digest = Command::new("sha256sum")
        .arg(&loader)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unavailable".to_owned());
    eprintln!(
        "SaBRe loader: path={}, revision={}, sha256={digest}",
        loader.display(),
        revision.trim(),
    );
    Some(loader)
}

fn kill_process_group(pid: u32, label: &str) {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            panic!("failed to kill {label} process group {pid}: {error}");
        }
    }
}

fn controller_diagnostics(path: Option<&Path>) -> String {
    path.map(|path| {
        std::fs::read_to_string(path).unwrap_or_else(|error| {
            format!(
                "failed to read controller diagnostics {}: {error}",
                path.display()
            )
        })
    })
    .unwrap_or_else(|| "unavailable".to_owned())
}

fn run_bounded(command: Command, label: &str, diagnostic_log: Option<&Path>) -> Output {
    run_bounded_polling(command, label, diagnostic_log, || {})
}

/// [`run_bounded`], calling `poll` before each 10 ms wait while the command runs.
fn run_bounded_polling(
    mut command: Command,
    label: &str,
    diagnostic_log: Option<&Path>,
    mut poll: impl FnMut(),
) -> Output {
    // A typed PMU skid-overshoot refusal, and nothing else, runs it again.
    let (output, (timed_out, rendered)) = hermit_test::hermit_output_with(label, || {
        run_bounded_polling_once(&mut command, label, diagnostic_log, &mut poll)
    });
    if timed_out || !output.status.success() {
        panic!(
            "{label} failed: {rendered}\nstatus: {}\ntimed out: {timed_out}\nstdout:\n{}\nstderr:\n{}\ncontroller diagnostics:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            controller_diagnostics(diagnostic_log),
        );
    }
    output
}

/// One attempt of [`run_bounded_polling`]: its output, whether it timed out,
/// and the rendered command.
fn run_bounded_polling_once(
    command: &mut Command,
    label: &str,
    diagnostic_log: Option<&Path>,
    poll: &mut impl FnMut(),
) -> (Output, (bool, String)) {
    // Verification observes the guest's current directory. The source checkout is shared by
    // concurrent validation nodes, so Cargo or another test can change its metadata or entries
    // between Run1 and Run2. Keep this invocation in one empty directory until both runs and
    // their comparison have completed.
    let working_directory = tempfile::Builder::new()
        .prefix("sabre-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| panic!("failed to create {label} working directory: {error}"));
    command
        .current_dir(working_directory.path())
        // Bash validates inherited PWD and OLDPWD by statting their path components. Leave them
        // unset so it obtains the same directory through getcwd without observing mutable
        // ancestors of this validation checkout.
        .env_remove("PWD")
        .env_remove("OLDPWD")
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    // Drain both pipes while the command runs. Read only after it exits, a
    // command whose output exceeds a pipe's capacity blocks writing it and
    // never exits: Linux gives a new pipe two pages (8 KiB) once its user
    // holds more than `pipe-user-pages-soft` pages, as a busy host can.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = std::io::Read::read_to_end(&mut pipe, &mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if started.elapsed() >= Duration::from_secs(45) => {
                kill_process_group(child.id(), label);
                break true;
            }
            Ok(None) => {
                poll();
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                kill_process_group(child.id(), label);
                let _ = child.wait();
                panic!(
                    "failed to poll {label}: {rendered}: {error}\ncontroller diagnostics:\n{}",
                    controller_diagnostics(diagnostic_log),
                );
            }
        }
    };
    // Hermit should drain its process tree before exiting. Kill any survivors anyway so leaked
    // guest processes cannot retain a pipe descriptor and hold the readers open.
    kill_process_group(child.id(), label);
    let status = child.wait().unwrap_or_else(|error| {
        panic!(
            "failed to collect {label}: {rendered}: {error}\ncontroller diagnostics:\n{}",
            controller_diagnostics(diagnostic_log),
        )
    });
    let output = Output {
        status,
        stdout: stdout.join().expect("the stdout reader panicked"),
        stderr: stderr.join().expect("the stderr reader panicked"),
    };
    (output, (timed_out, rendered))
}

fn example_command(
    example: &Path,
    args: &[&str],
    backend: Option<&Path>,
    verify: bool,
    diagnostic_log: Option<&Path>,
    retained_verify_log_dir: Option<&Path>,
) -> Command {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    example_command_with_execution_root(
        example,
        args,
        backend,
        verify,
        diagnostic_log,
        retained_verify_log_dir,
        requested.as_deref(),
    )
    .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"))
}

fn example_command_with_execution_root(
    example: &Path,
    args: &[&str],
    backend: Option<&Path>,
    verify: bool,
    diagnostic_log: Option<&Path>,
    retained_verify_log_dir: Option<&Path>,
    requested_workdir: Option<&OsStr>,
) -> Result<Command, String> {
    let mut command = Command::new(hermit_binary());
    command.arg(if verify { "--log=info" } else { "--log=warn" });
    if let Some(path) = diagnostic_log {
        command.arg("--log-file").arg(path);
    }
    if let Some(loader) = backend {
        command
            .env("HERMIT_SABRE_BINARY", loader)
            .args(["--backend", "sabre"]);
    }
    command.arg("run");
    command.args([
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        COMPARISON_EPOCH,
    ]);
    if verify {
        command.arg("--verify");
        if let Some(directory) = retained_verify_log_dir {
            command
                .args(["--verify-strict", "--keep-logs", "--verify-log-dir"])
                .arg(directory)
                .arg("--verify-json")
                .arg(directory.join("verify.json"));
        }
    }
    command.args(execution_root_args(requested_workdir)?);
    command.arg("--").arg(example).args(args);
    Ok(command)
}

fn parity_run(example: &Path, args: &[&str], backend: Option<&Path>, label: &str) -> Output {
    // Hermit gives the guest a private /tmp, so keep the controller sidecar in the host-visible
    // Cargo target directory. The freshly created unique file prevents stale or cross-run logs.
    let diagnostic_log = tempfile::Builder::new()
        .prefix("sabre-parity-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| panic!("failed to create {label} diagnostic log: {error}"));
    // Keep the already-produced evidence through success and assertion failure.
    // These host test artifacts never enter either compared guest stream.
    let (_diagnostic_file, diagnostic_log) = diagnostic_log
        .keep()
        .unwrap_or_else(|error| panic!("failed to retain {label} diagnostic log: {error}"));
    eprintln!(
        "SaBRe parity controller log for {label}: {}",
        diagnostic_log.display()
    );
    let output = run_bounded(
        example_command(example, args, backend, false, Some(&diagnostic_log), None),
        label,
        Some(&diagnostic_log),
    );
    let diagnostics = controller_diagnostics(Some(&diagnostic_log));
    let guest_stderr = String::from_utf8_lossy(&output.stderr);

    // Positive control: every SaBRe run emits the structured backend fact into
    // the controller sidecar. Negative controls: ptrace emits no SaBRe fact,
    // and neither backend lets that controller fact leak into captured guest
    // stderr. Guest stderr itself is still compared byte-for-byte below.
    if backend.is_some() {
        assert!(
            diagnostics.contains(SABRE_BACKEND_FACT_PREFIX),
            "SaBRe controller diagnostics omitted the backend fact for {label}:\n{diagnostics}",
        );
    } else {
        assert!(
            !diagnostics.contains(SABRE_BACKEND_FACT_PREFIX),
            "ptrace controller diagnostics unexpectedly carried a SaBRe fact for {label}:\n{diagnostics}",
        );
    }
    assert!(
        !guest_stderr.contains(SABRE_BACKEND_FACT_PREFIX),
        "controller backend fact leaked into captured guest stderr for {label}:\n{guest_stderr}",
    );
    output
}

fn assert_controller_diagnostics_do_not_hide_guest_stderr(loader: &Path) {
    const MARKER: &[u8] = b"guest-stderr-control\n";
    let args = ["-c", "printf 'guest-stderr-control\\n' >&2"];
    let ptrace = parity_run(
        Path::new("/bin/sh"),
        &args,
        None,
        "ptrace controller/guest stderr separation control",
    );
    let sabre = parity_run(
        Path::new("/bin/sh"),
        &args,
        Some(loader),
        "SaBRe controller/guest stderr separation control",
    );

    assert_eq!(ptrace.stderr, MARKER, "ptrace hid or rewrote guest stderr");
    assert_eq!(sabre.stderr, MARKER, "SaBRe hid or rewrote guest stderr");
    assert_eq!(
        sabre.stderr, ptrace.stderr,
        "controller-diagnostic routing must not weaken guest stderr parity",
    );
}

fn assert_backend_parity_and_sabre_verify(
    program: &Path,
    args: &[&str],
    loader: &Path,
    label: &str,
) {
    let ptrace = parity_run(
        program,
        args,
        None,
        &format!("ptrace strict portable reference for {label}"),
    );
    let sabre = parity_run(
        program,
        args,
        Some(loader),
        &format!("SaBRe strict portable parity run for {label}"),
    );
    assert_eq!(
        sabre.status.code(),
        ptrace.status.code(),
        "example: {label}"
    );
    assert_eq!(sabre.stdout, ptrace.stdout, "stdout parity: {label}");
    assert_eq!(sabre.stderr, ptrace.stderr, "stderr parity: {label}");

    assert_sabre_verify(program, args, loader, label);
}

fn assert_sabre_verify(program: &Path, args: &[&str], loader: &Path, label: &str) {
    let retained_logs = tempfile::Builder::new()
        .prefix("sabre-canonical-verify-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| {
            panic!("failed to create canonical verification directory for {label}: {error}")
        })
        .keep();
    eprintln!(
        "SaBRe canonical verification artifacts for {label}: {}",
        retained_logs.display()
    );
    let verify = run_bounded(
        example_command(
            program,
            args,
            Some(loader),
            true,
            None,
            Some(&retained_logs),
        ),
        &format!("SaBRe strict portable verification for {label}"),
        None,
    );
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    assert!(
        diagnostics.contains("Success: deterministic. Determinism verified."),
        "SaBRe verifier omitted its success verdict for {label}:\n{diagnostics}",
    );
    assert!(
        diagnostics.contains("SaBRe syscall DETLOG records included: run1="),
        "SaBRe verifier omitted its syscall DETLOG inclusion count for {label}:\n{diagnostics}",
    );

    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(retained_logs.join("verify.json")).unwrap_or_else(|error| {
            panic!("SaBRe verifier omitted its JSON report for {label}: {error}")
        }),
    )
    .unwrap_or_else(|error| panic!("SaBRe verifier wrote invalid JSON for {label}: {error}"));
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["compare_logs"] == true
            && report["comparison"]["log_scope"] == "info",
        "SaBRe verification was not canonical matched INFO parity for {label}: {report}",
    );
}

fn assert_three_run_determinism(
    program: &Path,
    args: &[&str],
    backend: Option<&Path>,
    backend_label: &str,
    label: &str,
) -> Output {
    let baseline = parity_run(
        program,
        args,
        backend,
        &format!("{backend_label} strict portable run 1 for {label}"),
    );
    for run in 2..=3 {
        let repeated = parity_run(
            program,
            args,
            backend,
            &format!("{backend_label} strict portable run {run} for {label}"),
        );
        assert_eq!(
            repeated.stdout, baseline.stdout,
            "{backend_label} stdout changed across strict runs: {label}, run {run}",
        );
        assert_eq!(
            repeated.stderr, baseline.stderr,
            "{backend_label} stderr changed across strict runs: {label}, run {run}",
        );
    }
    baseline
}

fn assert_date_output_is_sane(output: &Output, backend_label: &str) {
    let rendered = std::str::from_utf8(&output.stdout)
        .unwrap_or_else(|error| panic!("{backend_label} date output was not UTF-8: {error}"))
        .trim();
    let (date_and_time, nanos) = rendered
        .rsplit_once('_')
        .unwrap_or_else(|| panic!("{backend_label} date output lacked nanoseconds: {rendered}"));
    assert_eq!(
        date_and_time.len(),
        19,
        "{backend_label} date/time shape was unexpected: {rendered}",
    );
    assert!(
        nanos.len() == 9 && nanos.bytes().all(|byte| byte.is_ascii_digit()),
        "{backend_label} nanoseconds were malformed: {rendered}",
    );
}

#[test]
fn sabre_root_pid_matches_ptrace() {
    let Some(loader) = sabre_loader() else {
        return;
    };

    assert_controller_diagnostics_do_not_hide_guest_stderr(&loader);

    // The SaBRe ptrace safety net must not consume the root guest's namespace PID before launch.
    // `printf` is a shell builtin, so this observes the root shell rather than a forked utility.
    assert_backend_parity_and_sabre_verify(
        Path::new("/bin/sh"),
        &["-c", "printf 'pid=%s\\n' \"$$\""],
        &loader,
        "root-pid",
    );
}

#[test]
fn sabre_scheduler_empty_info_precedes_fallback_completed_info() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let retained_logs = tempfile::Builder::new()
        .prefix("sabre-verify-logs-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create retained SaBRe verification log directory")
        .keep();
    eprintln!(
        "SaBRe scheduler verification artifacts: {}",
        retained_logs.display()
    );
    // Hermit deletes run 2's log after a match, so a hard link made while the
    // command runs keeps it for the checks below; see the `run2_log` module.
    // The link's name matches neither log prefix.
    let capture = retained_logs.join("captured-run2.log");
    let mut captured = false;
    let verify = run_bounded_polling(
        example_command(
            Path::new("/bin/sh"),
            &["-c", "printf 'ok\\n'"],
            Some(&loader),
            true,
            None,
            Some(&retained_logs),
        ),
        "SaBRe strict verification with retained logs",
        None,
        || {
            if !captured {
                captured = run2_log::link_run2_log(&retained_logs, &capture);
            }
        },
    );
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    assert!(
        diagnostics.contains("Success: deterministic. Determinism verified."),
        "SaBRe strict verification omitted its success verdict:\n{diagnostics}",
    );

    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(retained_logs.join("verify.json"))
            .expect("SaBRe strict verification did not publish its report"),
    )
    .expect("SaBRe strict verification report was not valid JSON");
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["log_scope"] == "info",
        "SaBRe verification report was not canonical bitwise INFO parity: {report}",
    );

    let retained = |prefix: &str| {
        std::fs::read_dir(&retained_logs)
            .expect("failed to read retained SaBRe verification log directory")
            .map(|entry| {
                entry
                    .expect("failed to read retained SaBRe verification log entry")
                    .path()
            })
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix))
            })
            .collect::<Vec<_>>()
    };
    // After a match `--keep-logs` keeps only run 1's log, the golden copy;
    // run 2's log, which matched it, is deleted.
    let logs = retained("run1_log_");
    assert!(
        logs.len() == 1 && logs[0].is_file(),
        "SaBRe strict verification must retain exactly one run1_log_ file: {logs:?}",
    );
    let duplicates = retained("run2_log_");
    assert!(
        duplicates.is_empty(),
        "a matched SaBRe verification must not retain run 2's log: {duplicates:?}",
    );
    assert!(
        captured,
        "run 2's log was not captured while the command ran: {}",
        capture.display(),
    );

    // The INFO comparison covers only INFO records, while these checks count
    // the markers anywhere in each file, so run 2's log is checked as well.
    const SCHEDULER_EMPTY: &str =
        " INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop.";
    const FALLBACK_COMPLETED: &str =
        " INFO hermit::sabre::fallback: SaBRe ptrace fallback completed";
    for (index, path) in [&logs[0], &capture].into_iter().enumerate() {
        let log = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("failed to read run log {}: {error}", path.display()));
        let scheduler_empty = log.match_indices(SCHEDULER_EMPTY).collect::<Vec<_>>();
        let fallback_completed = log.match_indices(FALLBACK_COMPLETED).collect::<Vec<_>>();
        assert_eq!(
            scheduler_empty.len(),
            1,
            "run {} log must contain exactly one scheduler-empty INFO:\n{log}",
            index + 1,
        );
        assert_eq!(
            fallback_completed.len(),
            1,
            "run {} log must contain exactly one fallback-completed INFO:\n{log}",
            index + 1,
        );
        assert!(
            scheduler_empty[0].0 < fallback_completed[0].0,
            "run {} logged fallback completion before scheduler completion:\n{log}",
            index + 1,
        );
    }
    std::fs::remove_file(&capture).expect("failed to remove run 2's checked log");
}

/// The DETLOG records of a log in order, as text that does not depend on the
/// backend: the timestamp a host-side record carries, and a record forwarded
/// from a SaBRe guest lacks, is dropped, and so is the structured suffix.
fn detlog_records(log: &str) -> Vec<String> {
    log.lines()
        .filter(|line| line.contains("DETLOG"))
        .map(|line| {
            let line = line.split(" DETLOG_RECORD=").next().unwrap_or(line);
            match line.find("INFO ") {
                Some(at)
                    if line[..at]
                        .chars()
                        .all(|c| c.is_ascii_digit() || "-T:.Z ".contains(c)) =>
                {
                    line[at..].to_owned()
                }
                _ => line.trim_start().to_owned(),
            }
        })
        .collect()
}

#[test]
fn detlog_records_drop_only_the_timestamp_and_suffix() {
    let log = "2026-10-05T16:16:43.126985Z  INFO detcore::tool_local: DETLOG USER RAND: seed 0 DETLOG_RECORD={}\n\
               INFO detcore::tool_local: DETLOG USER RAND: seed 0 DETLOG_RECORD={}\n\
               not a record\n \
               COMMIT turn 0, dettid 3 DETLOG_RECORD={}";
    assert_eq!(
        detlog_records(log),
        [
            "INFO detcore::tool_local: DETLOG USER RAND: seed 0",
            "INFO detcore::tool_local: DETLOG USER RAND: seed 0",
            "COMMIT turn 0, dettid 3",
        ]
    );
}

/// Every backend's root thread logs its seeding before the guest's first
/// post-exec record. SaBRe's loader writes AT_RANDOM before Detcore runs in the
/// guest, and that write used to log at once, ahead of the seeding records and
/// the first two scheduler commits.
#[test]
fn sabre_and_ptrace_detlogs_agree_through_post_exec() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let run = |backend: Option<&Path>, label: &str| live_detlog_records(backend, label, None);
    let through_post_exec = |records: &[String], label: &str| {
        let end = records
            .iter()
            .position(|record| record.contains("] init auxv AT_RANDOM value to "))
            .unwrap_or_else(|| panic!("{label} logged no AT_RANDOM record: {records:#?}"));
        records[..=end].to_vec()
    };
    let ptrace = run(None, "ptrace /bin/true");
    let sabre = run(Some(&loader), "SaBRe /bin/true");
    let expected = through_post_exec(&ptrace, "ptrace");
    assert!(
        expected
            .iter()
            .any(|record| record.contains("USER RAND: seeding PRNG for root thread")),
        "ptrace logged no root-thread seeding before post-exec: {expected:#?}"
    );
    assert_eq!(through_post_exec(&sabre, "SaBRe"), expected);
}

/// SaBRe's guest plugin has no tracing subscriber: it forwards Detcore's
/// records by the per-target policy the CLI hands it, and ptrace asks the CLI's
/// subscriber at each record's own module. A target-scoped `RUST_LOG` must
/// therefore keep the same records on both backends. When the CLI asked only
/// whether the generic `detcore` target logged INFO, SaBRe forwarded nothing
/// here and dropped the AT_RANDOM record ptrace prints.
#[test]
fn sabre_forwards_a_target_scoped_detlog_filter_as_ptrace_applies_it() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    const SCOPED: &str = "warn,detcore::random=info";
    let ptrace = live_detlog_records(None, "ptrace /bin/true", Some(SCOPED));
    let sabre = live_detlog_records(Some(&loader), "SaBRe /bin/true", Some(SCOPED));
    assert!(
        ptrace
            .iter()
            .any(|record| record.contains("] init auxv AT_RANDOM value to ")),
        "ptrace logged no AT_RANDOM record under RUST_LOG={SCOPED}: {ptrace:#?}"
    );
    assert!(
        ptrace
            .iter()
            .all(|record| record.starts_with("INFO detcore::random: ")),
        "RUST_LOG={SCOPED} let another module's records through: {ptrace:#?}"
    );
    assert_eq!(sabre, ptrace);
}

/// A verification's run log keeps the guest's records in the order they
/// happen, as the live stream does: the SaBRe plugin sends each one on the
/// socket `--verify` passes, from a descriptor reverie-sabre keeps from the
/// guest, and the coordinator writes it at its thread's scheduler turn. So the
/// run log matches ptrace's through the post-exec AT_RANDOM record. When the
/// plugin forwarded records on the guest's stderr, they were cut out after the
/// run and appended, and the SaBRe run log diverged from ptrace's at the third
/// record.
#[test]
fn sabre_verify_log_keeps_forwarded_records_in_ptraces_order() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let through_post_exec = |records: Vec<String>, label: &str| {
        let end = records
            .iter()
            .position(|record| record.contains("] init auxv AT_RANDOM value to "))
            .unwrap_or_else(|| panic!("{label} logged no AT_RANDOM record: {records:#?}"));
        records[..=end].to_vec()
    };
    let ptrace = verify_run(None, &["/bin/true"], "ptrace /bin/true");
    let sabre = verify_run(Some(&loader), &["/bin/true"], "SaBRe /bin/true");
    assert_eq!(
        through_post_exec(sabre.records, "SaBRe"),
        through_post_exec(ptrace.records, "ptrace")
    );
}

/// A guest that dup2s onto every descriptor from 1024 to 1039 (where
/// reverie-sabre keeps, and keeps moving, the forwarding socket) and then
/// closes them all still runs to the end under `--verify --verify-strict`:
/// each dup2 succeeds as it would without forwarding, and records keep
/// reaching the run log after the moves and the cleanup, through the final
/// write of "alive". With a socket that did not move, the dup2 onto its number
/// failed with EBADF.
#[test]
fn sabre_verify_survives_a_guest_dup_onto_the_forwarding_socket() {
    const GUEST: &str = "import os\n\
        for fd in range(1024, 1040):\n\
        \x20   assert os.dup2(1, fd) == fd, fd\n\
        os.closerange(3, 1100)\n\
        print('alive', flush=True)\n";
    let Some(loader) = sabre_loader() else {
        return;
    };
    // -I -B: the guest must not depend on state its first run leaves behind.
    // A writable bytecode cache (PYTHONPYCACHEPREFIX, which the hosted
    // validation sets) made run 1 compile and write the standard library's .pyc
    // files and run 2 read them: different syscalls, a failed comparison, and
    // a 45 s kill on the hosted runner. Isolated mode ignores PYTHON* variables
    // and -B writes no bytecode, so both runs see the same files.
    let run = verify_run(
        Some(&loader),
        &["/usr/bin/python3", "-I", "-B", "-c", GUEST],
        "SaBRe python3 dup2 onto 1024..1039",
    );
    assert_eq!(String::from_utf8_lossy(&run.output.stdout), "alive\n");
    // Records keep arriving after every move and after the cleanup: the
    // cleanup's last record and the final write of "alive" are both in the
    // log, in that order, with the write's result.
    let position = |needle: &str| {
        run.records
            .iter()
            .rposition(|record| record.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} record in the run log: {:#?}", run.records))
    };
    // os.closerange is one close_range(3, 1099) where Python has it (the
    // pinned root's), and one close per descriptor where it does not (the
    // host's): either way it reaches 1099, the socket's number range included.
    let cleanup = run
        .records
        .iter()
        .rposition(|record| {
            record.contains("inbound syscall: close(1099)")
                || record.contains("inbound syscall: close_range(3, 1099,")
        })
        .unwrap_or_else(|| {
            panic!(
                "no close(1099) or close_range(3, 1099, ...) record in the run log: {:#?}",
                run.records
            )
        });
    let alive = position("write(1, ");
    assert!(
        position("inbound syscall: dup2(1, 1039)") < cleanup && cleanup < alive,
        "dup2 onto 1039, the last close and the write of \"alive\" are out of order: {:#?}",
        run.records
    );
    assert!(
        run.records[alive..]
            .iter()
            .any(|record| record.contains("finish syscall #") && record.contains("write(1, ")),
        "the write of \"alive\" has no finished record: {:#?}",
        run.records
    );
}

/// A guest that disturbs the forwarding socket before the SaBRe plugin can
/// adopt it (an executable `.preinit_array` function runs before the plugin's
/// first intercepted syscall, and closes it) cannot make `--verify` accept a
/// log with records missing: every guest thread tells the coordinator, with its
/// next request, how many records it produced, and the coordinator records a
/// determinism loss when fewer arrived, so the comparison is refused. Before
/// that count, the plugin fell back to stderr unnoticed and the run's log kept
/// only the coordinator's records.
#[test]
fn sabre_verify_refuses_when_forwarded_records_go_missing() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

// Hermit does not name the forwarding socket in the guest's environment, so
// the guest finds it as its one inherited SOCK_SEQPACKET descriptor.
static int tool_output_fd(void) {
  for (int fd = 3; fd < 1024; fd++) {
    int type;
    socklen_t length = sizeof type;
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_SEQPACKET)
      return fd;
  }
  _exit(9);
}

static void drop_tool_output(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  (void)envp;
  close(tool_output_fd());
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(int, char **, char **) =
    drop_tool_output;

int main(void) {
  for (int i = 0; i < 20; i++) getpid();
  puts("done");
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-dropped-transport-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "drop", GUEST);
    let report = dir.path().join("verify.json");
    let mut command = Command::new(hermit_binary());
    command
        .arg("--log=info")
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .args(["--verify", "--verify-strict", "--verify-json"])
        .arg(&report)
        .arg("--")
        .arg(&guest);
    let (status, stderr) =
        run_expecting_failure(command, "SaBRe guest that drops the forwarding socket");
    assert!(
        !status.success(),
        "verification accepted the run:\n{stderr}"
    );
    assert!(
        stderr.contains("forwarded records lost: thread"),
        "the refusal does not name the lost records:\n{stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["verdict"], "no_result", "{report}");
    assert_eq!(report["bitwise_parity"], false, "{report}");
}

/// SaBRe's runtime does not promote open-file descriptions before a native
/// fork or exec, so Detcore cannot keep one model of a timerfd there. A strict
/// run's timerfd is refused by name instead of following the host clock (B3,
/// `promotes_open_files_before_native_lifecycle`).
#[test]
fn sabre_refuses_a_timerfd_by_name() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <sys/timerfd.h>
#include <time.h>

int main(void) {
  int fd = timerfd_create(CLOCK_MONOTONIC, 0);
  printf("timerfd %d\n", fd);
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-timerfd-refused-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "timerfd", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .arg("--")
        .arg(&guest);
    let (status, stderr) = run_expecting_failure(command, "SaBRe guest that creates a timerfd");
    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("timerfd_create on a backend whose runtime does not keep one model")
            && stderr.contains("https://github.com/rrnewton/hermit/issues/1923"),
        "the refusal does not name the timerfd:\n{stderr}"
    );
}

/// A guest's close(2) of its stderr closes it, as on ptrace: the next open
/// takes descriptor 2, and a write to the closed descriptor fails with EBADF.
/// (SaBRe used to answer 0 without closing, so the open took 3.)
#[test]
fn sabre_guest_close_of_stderr_closes_it() {
    const GUEST: &str = r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

int main(void) {
  if (close(2) != 0) {
    printf("close failed errno=%d\n", errno);
    return 1;
  }
  ssize_t written = write(2, "x", 1);
  printf("write after close %zd errno=%d\n", written, written < 0 ? errno : 0);
  int fd = open("/dev/null", O_WRONLY);
  printf("reopened as %d\n", fd);
  return fd == 2 ? 0 : 2;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-close-stderr-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "close_stderr", GUEST);
    assert_backend_parity_and_sabre_verify(&guest, &[], &loader, "close of stderr");
}

/// Detcore answers a SIGSTKFLT handler with success but does not install it
/// (the backends reserve that signal; refusing would break the Go runtime). It
/// announces that once per run, in Hermit's own diagnostics: here a parent
/// sets the handler twice and a forked child once more, on ptrace and SaBRe.
#[test]
fn sigstkflt_handler_not_installed_is_announced_once_per_run() {
    const GUEST: &str = r#"
#include <signal.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

static void handler(int signo) { (void)signo; }

static int set_handler(void) {
  struct sigaction action = {0};
  action.sa_handler = handler;
  return sigaction(SIGSTKFLT, &action, NULL);
}

int main(void) {
  int first = set_handler();
  int second = set_handler();
  pid_t child = fork();
  if (child == 0) _exit(set_handler() == 0 ? 0 : 1);
  int status = 0;
  waitpid(child, &status, 0);
  printf("sigaction %d %d child %d\n", first, second, WEXITSTATUS(status));
  return 0;
}
"#;
    const ANNOUNCEMENT: &str = "HERMIT_SIGNAL_HANDLER_NOT_INSTALLED signum=16 (SIGSTKFLT)";
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-sigstkflt-announce-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "sigstkflt_announce", GUEST);
    let mut stdouts = Vec::new();
    for (backend, label) in [(None, "ptrace"), (Some(loader.as_path()), "SaBRe")] {
        let log = tempfile::Builder::new()
            .prefix("sigstkflt-announce-")
            .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("failed to create the diagnostic log");
        let (_file, log) = log.keep().expect("failed to keep the diagnostic log");
        let output = run_bounded(
            example_command(&guest, &[], backend, false, Some(&log), None),
            &format!("{label} SIGSTKFLT announcement"),
            Some(&log),
        );
        assert!(output.status.success(), "{label}: {output:?}");
        let diagnostics = controller_diagnostics(Some(&log));
        assert_eq!(
            diagnostics.matches(ANNOUNCEMENT).count(),
            1,
            "{label} must announce exactly once per run:\n{diagnostics}"
        );
        assert!(
            diagnostics.contains("did not install it"),
            "{label}: {diagnostics}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(ANNOUNCEMENT),
            "{label}: the announcement must not reach the guest's own stderr"
        );
        stdouts.push(output.stdout);
    }
    assert_eq!(stdouts[0], stdouts[1], "guest output parity");
    assert_eq!(
        String::from_utf8_lossy(&stdouts[0]),
        "sigaction 0 0 child 0\n"
    );
}

/// A guest whose preinit code (which runs before the plugin can adopt the
/// forwarding socket) closes the socket and points its own stderr at a file of
/// its own. The plugin was asked for the socket and could not adopt it, so it
/// must not fall back to stderr: no Tool record may land in the guest's file,
/// and the records it could not forward are refused as lost.
#[test]
fn sabre_verify_writes_no_tool_record_into_a_guest_stderr_file() {
    const GUEST: &str = r#"
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

// Hermit does not name the forwarding socket in the guest's environment, so
// the guest finds it as its one inherited SOCK_SEQPACKET descriptor.
static int tool_output_fd(void) {
  for (int fd = 3; fd < 1024; fd++) {
    int type;
    socklen_t length = sizeof type;
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_SEQPACKET)
      return fd;
  }
  _exit(9);
}

static void take_stderr(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  (void)envp;
  close(tool_output_fd());
  int file = open("guest-stderr.txt", O_WRONLY | O_CREAT | O_TRUNC, 0644);
  if (file < 0 || dup2(file, 2) != 2) _exit(3);
  close(file);
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(int, char **, char **) =
    take_stderr;

int main(void) {
  for (int i = 0; i < 20; i++) getpid();
  static const char line[] = "guest line\n";
  if (write(2, line, sizeof line - 1) != (ssize_t)(sizeof line - 1)) return 4;
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-guest-stderr-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "stderr", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .current_dir(dir.path())
        .arg("--log=info")
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .args(["--verify", "--verify-strict", "--"])
        .arg(&guest);
    let (status, stderr) = run_expecting_failure(command, "SaBRe guest that takes over its stderr");
    assert!(
        !status.success(),
        "verification accepted the run:\n{stderr}"
    );
    assert!(
        stderr.contains("forwarded records lost: thread"),
        "the refusal does not name the lost records:\n{stderr}"
    );
    let file = std::fs::read_to_string(dir.path().join("guest-stderr.txt"))
        .expect("the guest did not create its stderr file");
    assert_eq!(
        file, "guest line\n",
        "the guest's stderr file holds bytes the guest did not write"
    );
}

/// A guest whose preinit code closes the forwarding socket and puts a socket
/// pair of its own at that number. The plugin must not adopt the guest's
/// socket: the guest's peer must receive no Tool record, and the records the
/// plugin could not forward are refused as lost.
#[test]
fn sabre_verify_sends_no_tool_record_to_a_guest_socket_at_the_passed_number() {
    const GUEST: &str = r#"
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int peer = -1;

// Hermit does not name the forwarding socket in the guest's environment, so
// the guest finds it as its one inherited SOCK_SEQPACKET descriptor.
static int tool_output_fd(void) {
  for (int fd = 3; fd < 1024; fd++) {
    int type;
    socklen_t length = sizeof type;
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &length) == 0 && type == SOCK_SEQPACKET)
      return fd;
  }
  _exit(9);
}

static void replace_tool_output(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  (void)envp;
  int passed = tool_output_fd();
  int pair[2];
  close(passed);
  if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, pair) != 0) _exit(3);
  if (pair[0] != passed && dup2(pair[0], passed) != passed) _exit(4);
  if (pair[0] != passed) close(pair[0]);
  peer = pair[1];
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(int, char **, char **) =
    replace_tool_output;

int main(void) {
  for (int i = 0; i < 20; i++) getpid();
  if (peer < 0) return 5;
  char buffer[4096];
  ssize_t got = recv(peer, buffer, sizeof buffer, MSG_DONTWAIT);
  if (got >= 0) {
    fprintf(stderr, "guest socket received %zd tool bytes\n", got);
    return 6;
  }
  return errno == EAGAIN ? 0 : 7;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-guest-socket-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "socket", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .arg("--log=info")
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .args(["--verify", "--verify-strict", "--"])
        .arg(&guest);
    let (status, stderr) =
        run_expecting_failure(command, "SaBRe guest that replaces the forwarding socket");
    assert!(
        !stderr.contains("guest socket received"),
        "a Tool record reached the guest's socket:\n{stderr}"
    );
    assert!(
        !status.success(),
        "verification accepted the run:\n{stderr}"
    );
    assert!(
        stderr.contains("forwarded records lost: thread"),
        "the refusal does not name the lost records:\n{stderr}"
    );
}

/// The guest's `/proc/<pid>/exe` links name the guest's images on SaBRe as
/// natively and on ptrace, by every spelling, from the process itself and
/// from its parent and child, and an open of the link opens that image.
/// SaBRe's kernel exec'd its loader, so the kernel's link names the loader;
/// Detcore answers from the image it recorded at each exec
/// (`detcore::exec_record`). The guest checks its first image, a fork child
/// (and its parent's link), a failed exec, an exec through a relative
/// symbolic link, an exec of a second copy (its parent reading its link, and
/// it reading its parent's), an `execveat` of a descriptor, an exec of
/// `/proc/self/exe`, and a script whose interpreter is the guest.
#[test]
fn sabre_exe_link_names_the_guest_image_as_natively() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <libgen.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static void show(const char *label, ssize_t n, const char *buf) {
  if (n < 0)
    printf("%s: error\n", label);
  else
    printf("%s: %.*s\n", label, (int)n, buf);
}

static void show_link(const char *label, pid_t pid) {
  char path[64], buf[4096];
  snprintf(path, sizeof path, "/proc/%d/exe", (int)pid);
  show(label, readlink(path, buf, sizeof buf), buf);
}

/* Whether a read-only open of `path` opens the file the link named. */
static void show_open(const char *label, const char *path, const char *named) {
  struct stat opened, at_path;
  int fd = open(path, O_RDONLY);
  int same = fd >= 0 && fstat(fd, &opened) == 0 && stat(named, &at_path) == 0 &&
             opened.st_dev == at_path.st_dev && opened.st_ino == at_path.st_ino;
  printf("%s: %s\n", label, fd < 0 ? "error" : same ? "the linked file" : "another file");
  if (fd >= 0)
    close(fd);
}

static void report(const char *stage) {
  char buf[4096], path[128], named[4096];
  printf("[%s]\n", stage);
  show("readlink self", readlink("/proc/self/exe", buf, sizeof buf), buf);
  ssize_t target = readlinkat(AT_FDCWD, "/proc/self/exe", buf, sizeof buf);
  show("readlinkat self", target, buf);
  snprintf(named, sizeof named, "%.*s", (int)(target < 0 ? 0 : target), buf);
  show("readlink thread-self", readlink("/proc/thread-self/exe", buf, sizeof buf), buf);
  snprintf(path, sizeof path, "/proc/%d/exe", getpid());
  show("readlink pid", readlink(path, buf, sizeof buf), buf);
  snprintf(path, sizeof path, "/proc/%d/task/%ld/exe", getpid(), (long)syscall(SYS_gettid));
  show("readlink task", readlink(path, buf, sizeof buf), buf);
  int proc = open("/proc", O_RDONLY | O_DIRECTORY);
  show("readlinkat proc-dir", readlinkat(proc, "self/exe", buf, sizeof buf), buf);
  close(proc);
  int self = open("/proc/self", O_RDONLY | O_DIRECTORY);
  show("readlinkat self-dir", readlinkat(self, "exe", buf, sizeof buf), buf);
  close(self);
  printf("readlink short: %zd\n", readlink("/proc/self/exe", buf, 3));
  show("readlink dotdot", readlink("/proc/self/../self/exe", buf, sizeof buf), buf);
  show_open("open dotdot", "/proc/self/../self/exe", named);
  /* exe_self is a symbolic link to /proc/self/exe. */
  show_open("open alias", "./exe_self", named);
  /* Ordinary links Detcore must leave alone. */
  show("readlink root", readlink("/proc/self/root", buf, sizeof buf), buf);
  show("readlink exe_dir/exe", readlink("exe_dir/exe", buf, sizeof buf), buf);
  show("readlink exe_dir/exe short", readlink("exe_dir/exe", buf, 1), buf);
  int writer = open("/proc/self/exe", O_WRONLY);
  printf("open write: %s\n",
         writer >= 0 ? "opened" : errno == ETXTBSY ? "ETXTBSY" : "another error");
  if (writer >= 0)
    close(writer);
  int fd = open("/proc/self/exe", O_RDONLY);
  struct stat opened, at_path;
  unsigned char magic[4] = {0};
  if (fd < 0 || fstat(fd, &opened) != 0 || read(fd, magic, 4) != 4) {
    printf("open self: error\n");
  } else {
    int same = stat(named, &at_path) == 0 && opened.st_dev == at_path.st_dev &&
               opened.st_ino == at_path.st_ino;
    printf("open self: %02x%c%c%c, same file as the link: %s\n", magic[0], magic[1],
           magic[2], magic[3], same ? "yes" : "no");
  }
  if (fd >= 0)
    close(fd);
  printf("open nofollow: %s\n",
         open("/proc/self/exe", O_RDONLY | O_NOFOLLOW) < 0 ? "error" : "opened");
  fflush(stdout);
}

static int wait_for(pid_t child) {
  int status;
  if (waitpid(child, &status, 0) != child)
    return 1;
  return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}

/* Each stage is named by the last argument; a script's interpreter gets the
   script's path before it. */
int main(int argc, char **argv) {
  const char *stage = argc > 1 ? argv[argc - 1] : "first image";
  if (argc == 1) {
    /* Later stages name their images relative to the guest's directory. */
    char directory[4096];
    snprintf(directory, sizeof directory, "%s", argv[0]);
    if (chdir(dirname(directory)) != 0)
      return 2;
  }
  report(stage);
  if (!strcmp(stage, "first image")) {
    pid_t child = fork();
    if (child == 0) {
      report("fork child");
      show_link("parent", getppid());
      /* A failed exec keeps the image. */
      execl("./exe_missing", "exe_missing", (char *)NULL);
      show_link("after a failed exec", getpid());
      fflush(stdout);
      /* A relative symbolic link, which the kernel follows to this image. */
      execl("./exe_alias", "exe_alias", "exec'd image", (char *)NULL);
      _exit(5);
    }
    return wait_for(child);
  }
  if (!strcmp(stage, "exec'd image")) {
    int ready[2], go[2];
    if (pipe(ready) != 0 || pipe(go) != 0)
      return 6;
    pid_t child = fork();
    if (child == 0) {
      char r[16], g[16];
      snprintf(r, sizeof r, "%d", ready[1]);
      snprintf(g, sizeof g, "%d", go[0]);
      /* Another image: a stale record would show. */
      execl("./exe_copy", "exe_copy", r, g, "copied image", (char *)NULL);
      _exit(6);
    }
    char byte;
    if (read(ready[0], &byte, 1) != 1)
      return 6;
    show_link("child", child);
    fflush(stdout);
    if (write(go[1], "x", 1) != 1)
      return 6;
    int status = wait_for(child);
    if (status != 0)
      return status;
    /* execveat of a descriptor. */
    /* Not O_CLOEXEC: SaBRe re-execs its loader on /proc/self/fd/N. */
    int image = open("./exe_copy", O_PATH);
    char *args[] = {"exe_copy", "fd-exec'd image", NULL};
    syscall(SYS_execveat, image, "", args, environ, AT_EMPTY_PATH);
    return 7;
  }
  if (!strcmp(stage, "copied image")) {
    show_link("parent", getppid());
    fflush(stdout);
    char byte;
    if (write(atoi(argv[1]), "x", 1) != 1 || read(atoi(argv[2]), &byte, 1) != 1)
      return 8;
    return 0;
  }
  if (!strcmp(stage, "fd-exec'd image")) {
    /* An exec of the own link runs this image again. */
    execl("/proc/self/exe", "self", "self-exec'd image", (char *)NULL);
    return 9;
  }
  if (!strcmp(stage, "self-exec'd image")) {
    /* A script whose interpreter is exe_link. */
    execl("./exe_script", "exe_script", "script image", (char *)NULL);
    return 10;
  }
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-exe-link-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let directory = std::fs::canonicalize(dir.path()).unwrap();
    let guest = compile_guest(&directory, "exe_link", GUEST);
    let copy = directory.join("exe_copy");
    std::fs::copy(&guest, &copy).unwrap();
    // A relative link, which the kernel follows to the guest.
    std::os::unix::fs::symlink("exe_link", directory.join("exe_alias")).unwrap();
    // An alias of each process's own exe link.
    std::os::unix::fs::symlink("/proc/self/exe", directory.join("exe_self")).unwrap();
    // An ordinary link named `exe`, which Detcore must leave alone.
    std::fs::create_dir(directory.join("exe_dir")).unwrap();
    std::os::unix::fs::symlink("/", directory.join("exe_dir/exe")).unwrap();
    // Its interpreter is the guest; the script runs as the last stage.
    let script = directory.join("exe_script");
    std::fs::write(&script, format!("#!{}\n", guest.display())).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let native = Command::new(&guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8(native.stdout).unwrap();
    let names = |image: &Path| format!("readlink self: {}\n", image.display());
    assert_eq!(
        native_stdout.matches(&names(&guest)).count(),
        4,
        "{native_stdout}"
    );
    assert_eq!(
        native_stdout.matches(&names(&copy)).count(),
        3,
        "{native_stdout}"
    );
    assert_eq!(
        native_stdout.matches("same file as the link: yes").count(),
        7,
        "{native_stdout}"
    );
    for (line, stages) in [
        ("open dotdot: the linked file\n", 7),
        ("open alias: the linked file\n", 7),
        // A write open of a running image is the kernel's ETXTBSY.
        ("open write: ETXTBSY\n", 7),
        ("readlink root: /\n", 7),
        ("readlink exe_dir/exe: /\n", 7),
        ("readlink exe_dir/exe short: /\n", 7),
    ] {
        assert_eq!(
            native_stdout.matches(line).count(),
            stages,
            "{line}{native_stdout}"
        );
    }
    for line in [
        format!("parent: {}\n", guest.display()),
        format!("after a failed exec: {}\n", guest.display()),
        format!("child: {}\n", copy.display()),
    ] {
        assert!(native_stdout.contains(&line), "{line}{native_stdout}");
    }

    let ptrace = parity_run(&guest, &[], None, "ptrace exe link");
    assert_eq!(
        String::from_utf8_lossy(&ptrace.stdout),
        native_stdout,
        "ptrace's exe links differ from Linux's"
    );
    assert_backend_parity_and_sabre_verify(&guest, &[], &loader, "exe link");
}

/// Under SaBRe the kernel exec'd the loader, so the kernel's program break is
/// the loader's; Detcore answers `brk(2)` from its model of the guest's
/// (`Detcore::handle_brk`). The guest's break must start, grow, stay within a
/// page, shrink, refuse a request below its start, refuse growth that would
/// meet another mapping or a grow-down mapping's guard gap, keep its break
/// on a shrink with nothing left to unmap, give a fork child its own break,
/// and carry malloc's arena exactly as natively, for an `ET_EXEC` image,
/// which SaBRe loads at its own addresses.
///
/// Ignored as a known failure until the SaBRe switch (dev-hermit
/// `ai_docs/2026-10-17-sabre-switch-design.md`, step 5c), which must
/// un-ignore it and pass it 40 times in a row: the guest's fork child exits
/// while the parent waits, and SaBRe delivers the parent both Detcore's own
/// `SIGCHLD` (`SI_USER`) and the kernel's (`CLD_EXITED`), so strict verify
/// diverges in about 1 run in 10. The program-break assertions themselves
/// pass; `sabre_program_break_of_a_pie_starts_at_linuxs_rule` keeps running.
#[test]
#[ignore = "known failure: https://github.com/rrnewton/dev-hermit/blob/main/ai_docs/issue-drafts/2026-10-17-sabre-brk-exec-verify-flake.md (SaBRe delivers both Detcore's SI_USER SIGCHLD and the kernel's CLD_EXITED copy; strict verify diverged in 2 of 20 runs on SaBRe, 0 of 20 on ptrace)"]
fn sabre_program_break_is_the_guests_as_natively() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static long brk_(long to) { return syscall(SYS_brk, to); }

int main(void) {
  /* Raw brk first, before stdio or malloc touch the heap; printed after. */
  long r[13];
  r[0] = brk_(0);
  long start = r[0];
  r[1] = brk_(start + 3 * 4096);
  memset((void *)start, 0x5a, 3 * 4096);
  r[2] = brk_(start + 3 * 4096 + 100);
  r[3] = brk_(start + 4096);
  r[4] = brk_(start - 4096);
  r[5] = brk_(0);
  r[6] = brk_(start + 5000);
  r[7] = ((volatile char *)start)[4095] == 0x5a;
  r[8] = brk_(start);
  /* Growth that would come within a page of a mapping is refused. */
  void *blocker = mmap((void *)(start + 2 * 4096), 4096, PROT_READ,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
  r[9] = blocker == (void *)(start + 2 * 4096);
  r[10] = brk_(start + 3 * 4096);
  r[11] = brk_(start + 4096);
  r[12] = brk_(start + 2 * 4096);
  munmap(blocker, 4096);
  brk_(start);
  /* A shrink with nothing left to unmap keeps the old break. */
  long empty[3];
  empty[0] = brk_(start + 3 * 4096);
  munmap((void *)start, 3 * 4096);
  empty[1] = brk_(start + 4096);
  empty[2] = brk_(0);
  /* Growth into a grow-down mapping's guard gap is refused. */
  long gap[2];
  void *down = mmap((void *)(start + 64 * 4096), 4096, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_GROWSDOWN | MAP_FIXED_NOREPLACE, -1, 0);
  gap[0] = down == (void *)(start + 64 * 4096);
  gap[1] = brk_(start + 3 * 4096 + 2 * 4096);
  munmap(down, 4096);
  brk_(start);
  const char *labels[] = {"start", "grow 3 pages", "within the page", "shrink to 1 page",
                          "below the start", "query", "unaligned grow", "kept bytes", "back",
                          "blocker placed", "past the blocker", "a page short of it",
                          "up to it"};
  /* A fork child moves its own break, not its parent's. */
  long before_fork = brk_(0), child_grew = 0;
  int pipefd[2];
  pipe(pipefd);
  pid_t child = fork();
  if (child == 0) {
    long grew = brk_(before_fork + 3 * 4096);
    write(pipefd[1], &grew, sizeof grew);
    _exit(0);
  }
  read(pipefd[0], &child_grew, sizeof child_grew);
  waitpid(child, NULL, 0);
  long parent_after = brk_(0);
  for (int i = 0; i < 13; i++)
    printf("%s: %#lx\n", labels[i], r[i]);
  printf("emptied heap grown to %#lx, shrink answers %#lx, query %#lx\n", empty[0], empty[1],
         empty[2]);
  printf("grow-down mapping placed: %ld, growth into its gap: %#lx\n", gap[0], gap[1]);
  printf("child grew by 3 pages: %s, the parent kept its own: %s\n",
         child_grew == before_fork + 3 * 4096 ? "yes" : "no",
         parent_after == before_fork ? "yes" : "no");
  char *small = malloc(24), *medium = malloc(5000);
  printf("malloc small %p medium %p\n", (void *)small, (void *)medium);
  free(medium);
  printf("sbrk(0) %p\n", sbrk(0));
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-brk-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let name = "brk_exec";
    let guest = compile_guest_with(dir.path(), name, GUEST, &["-fno-pie", "-no-pie"]);
    // Natively without address randomization, as Hermit runs every guest.
    let native = Command::new("setarch")
        .args(["-R"])
        .arg(&guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    let native_stdout = String::from_utf8(native.stdout).unwrap();
    for line in [
        "kept bytes: 0x1\n",
        "blocker placed: 0x1\n",
        "grow-down mapping placed: 1,",
        "child grew by 3 pages: yes, the parent kept its own: yes\n",
    ] {
        assert!(native_stdout.contains(line), "{line}{native_stdout}");
    }
    let ptrace = parity_run(&guest, &[], None, &format!("ptrace {name}"));
    assert_eq!(
        String::from_utf8_lossy(&ptrace.stdout),
        native_stdout,
        "ptrace's program break differs from Linux's: {name}"
    );
    assert_backend_parity_and_sabre_verify(&guest, &[], &loader, name);
}

/// A position-independent image's program break starts where Linux's rule
/// puts it for wherever the image was loaded: its highest loaded segment's
/// end at the image's bias, page-aligned (natively, under ptrace, and under
/// SaBRe, whose loader picks another bias).
#[test]
fn sabre_program_break_of_a_pie_starts_at_linuxs_rule() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <link.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static unsigned long expected;

static int own_image(struct dl_phdr_info *info, size_t size, void *unused) {
  (void)size;
  (void)unused;
  unsigned long end = 0;
  for (int i = 0; i < info->dlpi_phnum; i++)
    if (info->dlpi_phdr[i].p_type == PT_LOAD &&
        info->dlpi_phdr[i].p_vaddr + info->dlpi_phdr[i].p_memsz > end)
      end = info->dlpi_phdr[i].p_vaddr + info->dlpi_phdr[i].p_memsz;
  expected = (info->dlpi_addr + end + 4095) & ~4095UL;
  return 1; /* the program itself is listed first */
}

int main(void) {
  long start = syscall(SYS_brk, 0);
  dl_iterate_phdr(own_image, NULL);
  printf("start as load_elf_binary puts it: %s\n", (unsigned long)start == expected ? "yes" : "no");
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-brk-pie-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest_with(dir.path(), "brk_pie", GUEST, &["-fpie", "-pie"]);
    let native = Command::new("setarch")
        .args(["-R"])
        .arg(&guest)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        "start as load_elf_binary puts it: yes\n"
    );
    let ptrace = parity_run(&guest, &[], None, "ptrace brk_pie");
    assert_eq!(ptrace.stdout, native.stdout, "ptrace brk_pie");
    assert_backend_parity_and_sabre_verify(&guest, &[], &loader, "brk_pie");
}

/// Under SaBRe, `--detlog-heap` records the guest's heap as ptrace's kernel
/// labels it: from the guest's first brk, even where that brk already grows
/// the break, to an unaligned break's page-aligned end, one record per piece
/// once an mprotect splits the heap, and none of the pages the guest
/// unmapped.
#[test]
fn sabre_heap_records_are_ptraces() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>
extern char end;
int main(void) {
  /* The first brk grows the break, to an unaligned end: no query comes
     before it. */
  unsigned long start = ((unsigned long)&end + 4095) & ~4095UL;
  long grown = syscall(SYS_brk, start + 4 * 4096 + 1);
  memset((void *)start, 0x5a, 4 * 4096 + 1);
  /* Split the heap: its second page read-only. */
  int split = mprotect((void *)(start + 4096), 4096, PROT_READ);
  long query = syscall(SYS_brk, 0);
  /* Back to a page boundary, then unmap it all; the break stays. */
  long aligned = syscall(SYS_brk, start + 4 * 4096);
  int gone = munmap((void *)start, 4 * 4096);
  long after = syscall(SYS_brk, 0);
  printf("grown %d split %d query %d gone %d after %d\n", grown == (long)(start + 4 * 4096 + 1), split,
         query == grown, gone, after == aligned && aligned == (long)(start + 4 * 4096));
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-heap-records-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest_with(dir.path(), "heap_records", GUEST, &["-fno-pie", "-no-pie"]);
    let expected = "grown 1 split 0 query 1 gone 0 after 1\n";
    let heap_records = |backend: &str| {
        let mut command = Command::new(hermit_binary());
        command
            .env("HERMIT_SABRE_BINARY", &loader)
            .args(["--backend", backend, "--log", "info", "run", "--strict"])
            .args(["--detlog-heap", COMPARISON_EPOCH, "--"])
            .arg(&guest)
            .stdin(Stdio::null());
        let output = run_bounded(command, backend, None);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{backend}"
        );
        // The records without the thread number, which the backends assign
        // differently, and without what follows the hash.
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| Some(&line[line.find("[memory]")?..]))
            .filter(|record| record.contains(" [heap]->"))
            .map(|record| {
                let record = record
                    .split_once(" DETLOG_RECORD")
                    .map_or(record, |(record, _)| record);
                let (_, rest) = record.split_once("] ").expect("a [memory][dtid N] prefix");
                rest.to_owned()
            })
            .collect::<Vec<_>>()
    };
    let ptrace = heap_records("ptrace");
    assert!(
        ptrace
            .iter()
            .any(|record| record.contains("(READ | PRIVATE)")),
        "the split piece is recorded under ptrace: {ptrace:#?}"
    );
    assert_eq!(heap_records("sabre"), ptrace);
}

/// Guest code that runs before Detcore's first callback under SaBRe (here a
/// pre-init function) moves the launcher's break if it uses brk; Detcore
/// cannot then model the guest's own without splitting its heap, so the
/// guest's next brk is refused by name, where the launcher's break would be
/// a plausible wrong answer. So is every brk of a run whose threads are not
/// sequentialized, where one thread's brk could race another's mapping, and
/// a position-independent guest's brk from the launcher's break, which its
/// libc read in a pre-init function: Linux's answer, the guest's break above
/// that request, would read as a grant at an address nothing mapped.
#[test]
fn sabre_refuses_brk_it_cannot_model_by_name() {
    const QUERIED: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
static void early(void) { sbrk(0); }
__attribute__((section(".preinit_array"), used)) static void (*preinit)(void) = early;
int main(void) {
  char *grown = sbrk(4096);
  if (grown != (void *)-1)
    memset(grown, 1, 4096);
  printf("sbrk after a pre-init query: %s\n", grown == (void *)-1 ? "refused" : "granted");
  return 0;
}
"#;
    const QUERY: &str = r#"
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
int main(void) {
  printf("break: %s\n", syscall(SYS_brk, 0) > 0 ? "yes" : "no");
  return 0;
}
"#;
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>
static void early(void) { sbrk(4096); }
__attribute__((section(".preinit_array"), used)) static void (*preinit)(void) = early;
int main(void) {
  long now = syscall(SYS_brk, 0);
  printf("brk moved before main: %s\n", now > 0 ? "yes" : "no");
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-brk-preinit-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest_with(dir.path(), "brk_preinit", GUEST, &["-fno-pie", "-no-pie"]);
    let ptrace = parity_run(&guest, &[], None, "ptrace brk_preinit");
    assert!(ptrace.status.success(), "{ptrace:?}");
    assert_eq!(
        String::from_utf8_lossy(&ptrace.stdout),
        "brk moved before main: yes\n"
    );
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .arg("--")
        .arg(&guest);
    let (status, stderr) = run_expecting_failure(command, "SaBRe brk_preinit");
    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("guest code moved the launcher's break before Detcore modeled its own"),
        "{stderr}"
    );

    let query = compile_guest_with(dir.path(), "brk_query", QUERY, &["-fno-pie", "-no-pie"]);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args([
            "--backend",
            "sabre",
            "run",
            "--no-sequentialize-threads",
            COMPARISON_EPOCH,
        ])
        .arg("--")
        .arg(&query);
    let (status, stderr) = run_expecting_failure(command, "SaBRe unsequentialized brk_query");
    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("no model of the guest's: threads are not sequentialized"),
        "{stderr}"
    );

    let queried = compile_guest_with(dir.path(), "brk_queried", QUERIED, &["-fpie", "-pie"]);
    let ptrace = parity_run(&queried, &[], None, "ptrace brk_queried");
    assert!(ptrace.status.success(), "{ptrace:?}");
    assert_eq!(
        String::from_utf8_lossy(&ptrace.stdout),
        "sbrk after a pre-init query: granted\n"
    );
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
        .arg("--")
        .arg(&queried);
    let (status, stderr) = run_expecting_failure(command, "SaBRe brk_queried");
    assert_eq!(
        status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{stderr}"
    );
    assert!(
        stderr.contains("brk from the launcher's break, which the guest's libc read before Detcore modeled the guest's"),
        "{stderr}"
    );
}

/// Under SaBRe, whose kernel's `exe` link names the loader, an `exe` link
/// Detcore cannot answer faithfully is refused by name, where the loader's
/// would be a plausible wrong answer: the link of an image no longer at its
/// path, whose current name or Linux's " (deleted)" only the kernel's link to
/// the image could give, and a readlink through a descriptor for the link
/// itself, a spelling Detcore cannot resolve.
#[test]
fn sabre_refuses_exe_links_it_cannot_answer_by_name() {
    const REMOVED: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <unistd.h>

/* Run as a copy of itself: removes that copy, then reads its own exe link,
   which Linux renders with " (deleted)". */
int main(int argc, char **argv) {
  (void)argc;
  char buf[4096];
  if (unlink(argv[0]) != 0)
    return 2;
  ssize_t n = readlink("/proc/self/exe", buf, sizeof buf);
  if (n < 0)
    printf("readlink: error\n");
  else
    printf("readlink: %.*s\n", (int)n, buf);
  return 0;
}
"#;
    const DESCRIPTOR: &str = r#"
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

/* Reads its own exe link through a descriptor for the link itself, a
   spelling Detcore cannot resolve: Linux names the image. */
int main(void) {
  char buf[4096];
  int link = open("/proc/self/exe", O_PATH | O_NOFOLLOW);
  if (link < 0)
    return 2;
  ssize_t n = readlinkat(link, "", buf, sizeof buf);
  if (n < 0)
    printf("readlinkat: error\n");
  else
    printf("readlinkat: %.*s\n", (int)n, buf);
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-exe-refused-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let directory = std::fs::canonicalize(dir.path()).unwrap();
    for (name, source, reason) in [
        ("removed", REMOVED, "no longer at its path"),
        ("descriptor", DESCRIPTOR, "no record of"),
    ] {
        let built = compile_guest(&directory, name, source);
        // Natively the link names the image: removed, as " (deleted)".
        let native_copy = directory.join(format!("{name}-native"));
        std::fs::copy(&built, &native_copy).unwrap();
        let native = Command::new(&native_copy)
            .stdin(Stdio::null())
            .output()
            .expect("failed to run the guest natively");
        let expected = match name {
            "removed" => format!("readlink: {} (deleted)\n", native_copy.display()),
            _ => format!("readlinkat: {}\n", native_copy.display()),
        };
        assert_eq!(String::from_utf8_lossy(&native.stdout), expected, "{name}");
        // Under SaBRe: refused by name.
        let guest = directory.join(format!("{name}-sabre"));
        std::fs::copy(&built, &guest).unwrap();
        let mut command = Command::new(hermit_binary());
        command
            .env("HERMIT_SABRE_BINARY", &loader)
            .args([
                "--backend",
                "sabre",
                "run",
                "--strict",
                COMPARISON_EPOCH,
                "--",
            ])
            .arg(&guest);
        let (status, stderr) = run_expecting_failure(command, name);
        assert_eq!(
            status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{name}: {stderr}"
        );
        assert!(stderr.contains(reason), "{name}: {stderr}");
    }
}

/// A guest that execs itself, where the new image's preinit code removes the
/// plugin's private forwarding policy variable. That must never make the new
/// image's records vanish while the run is still compared: either the image
/// still forwards (measured: the plugin reads its settings at the first
/// intercepted syscall, which the dynamic loader makes before any preinit code
/// runs), so it forwards exactly as many records as the same guest without the
/// removal, or, if an image ever comes up without a forwarder, the coordinator's
/// requirement (`Config::in_guest_detlog_forward_policy`, sent in the connection
/// handshake, not the environment) refuses the run as uncounted.
#[test]
fn sabre_verify_never_loses_an_exec_image_that_drops_its_forwarding_settings() {
    const GUEST: &str = r#"
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static void drop_forwarding(int argc, char **argv, char **envp) {
  (void)envp;
  if (DROP && argc > 1 && strcmp(argv[1], "child") == 0)
    unsetenv("REVERIE_SABRE_HERMIT_FORWARD_DETLOG");
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(int, char **, char **) =
    drop_forwarding;

int main(int argc, char **argv) {
  for (int i = 0; i < 20; i++) getpid();
  if (argc > 1) return 0;
  char *child[] = {argv[0], "child", NULL};
  execv("/proc/self/exe", child);
  return 3;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-exec-drops-forwarding-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let run = |drop: bool| {
        let source = format!("#define DROP {}\n{GUEST}", i32::from(drop));
        let guest = compile_guest(dir.path(), if drop { "drops" } else { "keeps" }, &source);
        let mut command = Command::new(hermit_binary());
        command
            .arg("--log=info")
            .env("HERMIT_SABRE_BINARY", &loader)
            .args(["--backend", "sabre", "run", "--strict", COMPARISON_EPOCH])
            .args(["--verify", "--verify-strict", "--"])
            .arg(&guest);
        run_expecting_failure(command, "SaBRe guest that execs itself")
    };
    let records = |stderr: &str| -> Option<String> {
        stderr
            .lines()
            .find(|line| line.contains("SaBRe syscall DETLOG records included:"))
            .map(str::to_owned)
    };
    let (kept_status, kept) = run(false);
    assert!(
        kept_status.success(),
        "the plain exec guest was refused:\n{kept}"
    );
    let (dropped_status, dropped) = run(true);
    if dropped_status.success() {
        assert_eq!(
            records(&dropped),
            records(&kept),
            "removing the forwarding variable changed the forwarded records of an accepted run:\n{dropped}"
        );
        assert!(records(&kept).is_some(), "{kept}");
    } else {
        assert!(
            dropped.contains("forwarded records uncounted"),
            "the refusal does not name the uncounted records:\n{dropped}"
        );
    }
}

/// `--pipe-wake-latency` (https://github.com/rrnewton/hermit/issues/4380) on
/// SaBRe, where an exec rediscovers the guest's descriptors: two children share
/// a pipe and each execs `tests/c/fifo_peer.c`, the reader blocked in a read,
/// the writer writing one byte at iteration 100 of a 5000-iteration loop. The
/// exec carries each pipe end's channel, including the writer's end left
/// `O_NONBLOCK` (`wake-exec-nonblock`), which no blocking override covers. At
/// `L = 0` the reader runs at iteration 100 on both backends, and unset only
/// once the writer's loop has ended. (A nonzero latency is measured on each
/// backend's own thread time, which counts retired branches only with a PMU,
/// so it is not pinned here.) Actions that ready no poller (a write
/// nobody polls, a zero-byte write, a write to another pipe) change neither
/// SaBRe's output nor its number of committed turns.
#[test]
fn sabre_hands_a_readied_pipe_poller_the_turn_across_exec() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-pipe-wake-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
        .join("tests/c/fifo_peer.c");
    let guest = dir.path().join("fifo_peer");
    let build = Command::new("cc")
        .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile fifo_peer");
    assert!(
        build.status.success(),
        "fifo_peer compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    let run = |sabre: bool, mode: &str, latency: Option<&str>, summary: bool| {
        let scratch = dir.path().join(format!("{mode}-{sabre}-{latency:?}"));
        std::fs::create_dir_all(&scratch).unwrap();
        let mut command = Command::new(hermit_binary());
        command.arg("--log=warn");
        if sabre {
            command
                .env("HERMIT_SABRE_BINARY", &loader)
                .args(["--backend", "sabre"]);
        }
        command.args(["run", "--strict", COMPARISON_EPOCH]);
        if summary {
            command.arg("--summary");
        }
        if let Some(latency) = latency {
            command.arg(format!("--pipe-wake-latency={latency}"));
        }
        command
            .args(execution_root_args(requested.as_deref()).unwrap())
            .arg("--")
            .arg(&guest)
            .arg(mode)
            .arg(&scratch);
        let label = format!("{mode} sabre={sabre} latency={latency:?}");
        let output = run_bounded(command, &label, None);
        assert!(
            output.status.success(),
            "{label} failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };
    for mode in ["wake-exec", "wake-exec-nonblock"] {
        for (sabre, latency, expected) in [
            (false, None, 4999),
            (true, None, 4999),
            (false, Some("0"), 100),
            (true, Some("0"), 100),
        ] {
            let output = run(sabre, mode, latency, false);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("readied at 100, ran at {expected}\n"),
                "{mode} sabre={sabre} latency={latency:?}"
            );
        }
    }
    for (mode, expected) in [
        ("wake-nobody", "counted to 5000\n"),
        ("wake-zero", "readied at 100, not run by 5000\n"),
        ("wake-other", "readied at 100, not run by 5000\n"),
    ] {
        let mut turns = Vec::new();
        for latency in [None, Some("0"), Some("1000000")] {
            let output = run(true, mode, latency, true);
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "{mode}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            let count = stderr
                .split("the hermit scheduler ran ")
                .nth(1)
                .and_then(|rest| rest.split(' ').next())
                .and_then(|count| count.parse::<u64>().ok())
                .unwrap_or_else(|| panic!("{mode}: no turn count in the summary:\n{stderr}"));
            turns.push(count);
        }
        assert!(
            turns.iter().all(|count| *count == turns[0]),
            "{mode}: committed turns differ with the option: {turns:?}"
        );
    }
}

/// A guest RDTSC runs, with Detcore's virtual value, under SaBRe: both one that
/// is the guest's first intercepted event and one that follows its syscalls.
///
/// The loader's RDTSC entry points used to call the plugin's handler without
/// the enter_plugin/exit_plugin bracket that syscalls get, so the syscalls the
/// plugin made while handling the RDTSC came back into it as guest syscalls
/// and waited on a lock it already held: every such guest hung, the first-event
/// case on the tool's own construction. The guest prints the raw readings, so a
/// bitwise strict verification also shows they are virtual: two native
/// readings never repeat across runs.
#[test]
fn sabre_runs_a_guest_rdtsc_before_and_after_its_first_syscall() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <unistd.h>
#include <x86intrin.h>

int main(void) {
  unsigned long long first = __rdtsc();
  if (write(1, "start\n", 6) != 6) return 2;
  unsigned long long second = __rdtsc();
  printf("%llx %llx\n", first, second);
  return first != 0 && second > first ? 0 : 3;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-rdtsc-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "rdtsc", GUEST);
    let guest = guest.to_str().unwrap();
    let sabre = verify_run(Some(&loader), &[guest], "SaBRe guest that reads the TSC");
    let stdout = String::from_utf8(sabre.output.stdout).unwrap();
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("start"), "{stdout}");
    let readings = lines
        .next()
        .unwrap_or_else(|| panic!("no TSC readings: {stdout}"))
        .split(' ')
        .map(|reading| u64::from_str_radix(reading, 16).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(readings.len(), 2, "{stdout}");
    assert!(readings[1] > readings[0], "{stdout}");
}

/// An RDTSC in a guest signal handler gets the virtual value under SaBRe.
///
/// The plugin delivers such a handler with its own boundary flag still set, so
/// an RDTSC router that served every flagged request natively handed the
/// handler the host counter, and the printed value differed between the two
/// verified runs.
#[test]
fn sabre_virtualizes_an_rdtsc_in_a_guest_signal_handler() {
    const GUEST: &str = r#"
#include <signal.h>
#include <stdio.h>
#include <x86intrin.h>

static volatile unsigned long long in_handler;

static void handler(int signal_number) {
  (void)signal_number;
  in_handler = __rdtsc();
}

int main(void) {
  signal(SIGUSR1, handler);
  raise(SIGUSR1);
  printf("%llx\n", in_handler);
  return in_handler != 0 ? 0 : 3;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-rdtsc-handler-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "rdtsc-handler", GUEST);
    let sabre = verify_run(
        Some(&loader),
        &[guest.to_str().unwrap()],
        "SaBRe guest that reads the TSC in a signal handler",
    );
    let stdout = String::from_utf8(sabre.output.stdout).unwrap();
    assert!(
        u64::from_str_radix(stdout.trim(), 16).is_ok_and(|reading| reading != 0),
        "{stdout}"
    );
}

/// A guest RDTSC that runs before the SaBRe plugin exists is refused, not
/// answered with the host counter.
///
/// SaBRe initializes its plugin from the last `.preinit_array` entry, after the
/// guest's own entries, so an RDTSC in one of those has no virtual value.
#[test]
fn sabre_refuses_a_guest_rdtsc_before_its_plugin_starts() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <x86intrin.h>

static unsigned long long early;

static void read_early(void) { early = __rdtsc(); }
__attribute__((section(".preinit_array"), used)) static void (*preinit)(void) =
    read_early;

int main(void) {
  printf("%llx\n", early);
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-rdtsc-preinit-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "rdtsc-preinit", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args([
            "--backend",
            "sabre",
            "run",
            "--strict",
            COMPARISON_EPOCH,
            "--",
        ])
        .arg(&guest);
    let (status, stderr) = run_expecting_failure(command, "SaBRe guest with an early RDTSC");
    assert!(!status.success(), "the early RDTSC was answered:\n{stderr}");
    assert!(
        stderr.contains("guest RDTSC before the plugin was initialized"),
        "the refusal does not name the early RDTSC:\n{stderr}"
    );
}

/// An RDTSC that re-enters a SaBRe tool call already in progress on its thread
/// is refused, not left to deadlock.
///
/// The plugin shares the guest's symbol namespace, so the coordinator RPC's
/// `send` can resolve to a function the guest exports. An RDTSC there runs while
/// the tool holds the thread's state, which the RDTSC handler needs too. With no
/// virtual value available, the run must stop with a message rather than hang
/// or read the host counter.
#[test]
fn sabre_refuses_an_rdtsc_that_reenters_a_tool_call() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <poll.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <x86intrin.h>

static volatile int armed;
static volatile unsigned long long seen;

ssize_t send(int fd, const void *buffer, size_t length, int flags) {
  if (armed)
    seen = __rdtsc();
  return syscall(SYS_sendto, fd, buffer, length, flags, NULL, 0);
}

int main(void) {
  armed = 1;
  poll(NULL, 0, 0);
  armed = 0;
  printf("%llx\n", seen);
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-rdtsc-reentry-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest_with(dir.path(), "rdtsc-reentry", GUEST, &["-rdynamic"]);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args([
            "--backend",
            "sabre",
            "run",
            "--strict",
            COMPARISON_EPOCH,
            "--",
        ])
        .arg(&guest);
    let (status, stderr) =
        run_expecting_failure(command, "SaBRe guest whose exported send reads the TSC");
    assert!(
        !status.success(),
        "the re-entrant RDTSC was answered:\n{stderr}"
    );
    assert!(
        stderr.contains("an RDTSC re-entered a tool call already in progress"),
        "the refusal does not name the re-entrant RDTSC:\n{stderr}"
    );
}

/// A SaBRe guest's libc startup reads Detcore's stack limit, as under ptrace,
/// whatever the host's limit is, and the read leaves ptrace's record.
///
/// The dynamic loader and libc startup run before the SaBRe plugin is
/// initialized, and libc sizes default thread stacks from the `RLIMIT_STACK` it
/// reads there. The loader forwards that read to Hermit, which answers it from
/// Detcore's table; the host's limit itself is left alone, as under ptrace.
/// Launched with a 4 MiB soft limit, the guest used to report a 4 MiB default
/// thread stack under SaBRe and 8 MiB, Detcore's value, under ptrace.
#[test]
fn sabre_guest_startup_reads_detcores_stack_limit() {
    const GUEST: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <sys/resource.h>

int main(void) {
  pthread_attr_t attr;
  size_t size = 0;
  if (pthread_getattr_default_np(&attr) != 0 ||
      pthread_attr_getstacksize(&attr, &size) != 0)
    return 2;
  struct rlimit limit;
  if (getrlimit(RLIMIT_STACK, &limit) != 0)
    return 3;
  printf("default thread stack %zu, RLIMIT_STACK %llu\n", size,
         (unsigned long long)limit.rlim_cur);
  return 0;
}
"#;
    // A soft limit within the inherited hard limit is always allowed, and both
    // 4 MiB and any hard limit below it differ from Detcore's 8 MiB.
    const HOST_SOFT_LIMIT: libc::rlim_t = 4 * 1024 * 1024;
    const RECORD: &str =
        "prlimit64: pid=0, resource=3, mutation=false, old=8388608:18446744073709551615";
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-stack-limit-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "stack-limit", GUEST);
    let run = |backend: Option<&Path>| {
        let mut command = Command::new(hermit_binary());
        if let Some(loader) = backend {
            command
                .env("HERMIT_SABRE_BINARY", loader)
                .args(["--backend", "sabre"]);
        }
        command
            .args(["run", "--strict", COMPARISON_EPOCH, "--"])
            .arg(&guest)
            .stdin(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_STACK, &mut limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                limit.rlim_cur = HOST_SOFT_LIMIT.min(limit.rlim_max);
                if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let label = if backend.is_some() { "SaBRe" } else { "ptrace" };
        let output = run_bounded(command, label, None);
        assert!(
            output.status.success(),
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let expected = "default thread stack 8388608, RLIMIT_STACK 8388608\n";
    assert_eq!(run(None), expected, "ptrace");
    assert_eq!(run(Some(&loader)), expected, "SaBRe");
    // The startup read leaves the record ptrace's handler leaves. glibc's
    // getrlimit from main is a prlimit64 too, so ptrace has two such records;
    // SaBRe without the startup read's record had only main's.
    let guest = guest.to_str().unwrap();
    let count = |backend: Option<&Path>, label: &str| {
        let records = verify_run(backend, &[guest], label).records;
        let count = records
            .iter()
            .filter(|record| record.contains(RECORD))
            .count();
        (count, records)
    };
    let (ptrace, ptrace_records) = count(None, "ptrace");
    let (sabre, sabre_records) = count(Some(&loader), "SaBRe");
    assert_eq!(ptrace, 2, "ptrace: {ptrace_records:#?}");
    assert_eq!(sabre, ptrace, "SaBRe: {sabre_records:#?}");
}

/// Resource-limit reads that SaBRe can only reach through its SIGILL marker,
/// before the plugin exists, are answered from Detcore's table too.
///
/// The rewriter falls back to a SIGILL marker when it cannot relocate the
/// instructions around a syscall. Each helper below makes its `syscall` a
/// branch target followed directly by `ret`, which leaves nothing to relocate.
/// The supervisor used to save trap provenance only for `getrandom`, so such a
/// read stopped the run.
#[test]
fn sabre_guest_startup_limit_reads_through_the_sigill_marker() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <sys/resource.h>

__attribute__((naked, noinline)) static long marker_getrlimit(long resource,
                                                               void *limit) {
  __asm__ volatile("mov $97, %eax\n\tjmp 1f\n1:\tsyscall\n\tret\n");
}

__attribute__((naked, noinline)) static long
marker_prlimit64(long pid, long resource, void *new_limit, void *old_limit) {
  __asm__ volatile("mov %rcx, %r10\n\tmov $302, %eax\n\tjmp 1f\n1:\tsyscall\n\tret\n");
}

static struct rlimit by_getrlimit, by_prlimit64;
static long getrlimit_result = -1, prlimit64_result = -1, negative_result = -1;

static void read_early(void) {
  getrlimit_result = marker_getrlimit(RLIMIT_STACK, &by_getrlimit);
  prlimit64_result = marker_prlimit64(0, RLIMIT_STACK, 0, &by_prlimit64);
  // Detcore's getrlimit rejects a negative resource before a missing buffer.
  negative_result = marker_getrlimit(-1, 0);
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(void) =
    read_early;

int main(void) {
  printf("getrlimit %ld %llu, prlimit64 %ld %llu, negative %ld\n",
         getrlimit_result, (unsigned long long)by_getrlimit.rlim_cur,
         prlimit64_result, (unsigned long long)by_prlimit64.rlim_cur,
         negative_result);
  return 0;
}
"#;
    const HOST_SOFT_LIMIT: libc::rlim_t = 4 * 1024 * 1024;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-limit-marker-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "limit-marker", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args([
            "--backend",
            "sabre",
            "run",
            "--strict",
            COMPARISON_EPOCH,
            "--",
        ])
        .arg(&guest)
        .stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_STACK, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = HOST_SOFT_LIMIT.min(limit.rlim_max);
            if libc::setrlimit(libc::RLIMIT_STACK, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = run_bounded(
        command,
        "SaBRe guest reading limits through the marker",
        None,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "getrlimit 0 8388608, prlimit64 0 8388608, negative -22\n"
    );
}

/// A guest that changes a resource limit before the SaBRe plugin exists is
/// refused, rather than changing the host's limit while Detcore's table, which
/// the guest reads later, stays the same.
#[test]
fn sabre_refuses_a_guest_limit_change_before_its_plugin_starts() {
    const GUEST: &str = r#"
#include <stdio.h>
#include <sys/resource.h>

static void lower_stack(void) {
  struct rlimit limit = {4 * 1024 * 1024, RLIM_INFINITY};
  setrlimit(RLIMIT_STACK, &limit);
}
__attribute__((section(".preinit_array"), used)) static void (*preinit)(void) =
    lower_stack;

int main(void) {
  puts("ran");
  return 0;
}
"#;
    let Some(loader) = sabre_loader() else {
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("sabre-limit-change-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the guest directory");
    let guest = compile_guest(dir.path(), "limit-change", GUEST);
    let mut command = Command::new(hermit_binary());
    command
        .env("HERMIT_SABRE_BINARY", &loader)
        .args([
            "--backend",
            "sabre",
            "run",
            "--strict",
            COMPARISON_EPOCH,
            "--",
        ])
        .arg(&guest);
    let (status, stderr) = run_expecting_failure(command, "SaBRe guest with an early limit change");
    assert!(
        !status.success(),
        "the early limit change was applied:\n{stderr}"
    );
    assert!(
        stderr.contains("guest changed a resource limit before the plugin was initialized"),
        "the refusal does not name the early limit change:\n{stderr}"
    );
}

/// Compiles C `source` into `dir/name` and returns the executable's path.
fn compile_guest(dir: &Path, name: &str, source: &str) -> PathBuf {
    compile_guest_with(dir, name, source, &[])
}

/// [`compile_guest`], passing `flags` to the compiler as well.
fn compile_guest_with(dir: &Path, name: &str, source: &str, flags: &[&str]) -> PathBuf {
    let source_path = dir.join(format!("{name}.c"));
    std::fs::write(&source_path, source).unwrap();
    let guest = dir.join(name);
    let build = Command::new("cc")
        .args(["-O1", "-Wall", "-Werror"])
        .args(flags)
        .arg("-o")
        .arg(&guest)
        .arg(&source_path)
        .output()
        .expect("failed to compile the guest");
    assert!(
        build.status.success(),
        "guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );
    guest
}

/// Runs `command` to its end (at most 45 s), with stdout and stderr in files,
/// and returns its status and stderr, whatever the status.
fn run_expecting_failure(mut command: Command, label: &str) -> (std::process::ExitStatus, String) {
    // A typed PMU skid-overshoot refusal, and nothing else, runs it again.
    hermit_test::hermit_output_with(label, || {
        let (status, stderr) = run_expecting_failure_once(&mut command, label);
        let output = Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.clone().into_bytes(),
        };
        (output, (status, stderr))
    })
    .1
}

/// One attempt of [`run_expecting_failure`].
fn run_expecting_failure_once(
    command: &mut Command,
    label: &str,
) -> (std::process::ExitStatus, String) {
    let stderr = tempfile::Builder::new()
        .prefix("sabre-expected-failure-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the stderr file");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr.reopen().expect("failed to reopen the stderr file"))
        .process_group(0);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {error}"));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll the run") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(45) {
            kill_process_group(child.id(), label);
            panic!("{label} did not finish within 45 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (
        status,
        std::fs::read_to_string(stderr.path()).unwrap_or_default(),
    )
}

struct VerifyRun {
    output: Output,
    /// The DETLOG records of run 1's retained log.
    records: Vec<String>,
}

/// Runs `argv` under `--verify --verify-strict` at INFO (on SaBRe with
/// `backend`'s loader, else on ptrace), requires a bitwise match, and returns
/// the output and run 1's retained records.
fn verify_run(backend: Option<&Path>, argv: &[&str], label: &str) -> VerifyRun {
    let logs = tempfile::Builder::new()
        .prefix("sabre-verify-log-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the verification log directory");
    let report = logs.path().join("verify.json");
    let mut command = Command::new(hermit_binary());
    command.arg("--log=info");
    if let Some(loader) = backend {
        command
            .env("HERMIT_SABRE_BINARY", loader)
            .args(["--backend", "sabre"]);
    }
    command
        .arg("run")
        .args([
            "--strict",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            COMPARISON_EPOCH,
            "--verify",
            "--verify-strict",
            "--keep-logs",
        ])
        .arg("--verify-log-dir")
        .arg(logs.path())
        .arg("--verify-json")
        .arg(&report)
        .arg("--")
        .args(argv)
        .stdin(Stdio::null());
    let output = run_bounded(command, label, None);
    assert!(
        output.status.success(),
        "{label} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&report).unwrap_or_else(|error| panic!("{label} report: {error}")),
    )
    .unwrap();
    assert_eq!(report["bitwise_parity"], true, "{label}: {report}");
    let run1 = std::fs::read_dir(logs.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with("run1_log_"))
        })
        .unwrap_or_else(|| panic!("{label} kept no run 1 log"));
    let records = detlog_records(&std::fs::read_to_string(run1).unwrap());
    VerifyRun { output, records }
}

/// The DETLOG records of one `/bin/true` run's live INFO stream, where the host
/// and the SaBRe guest both write their records as they happen. `rust_log`
/// replaces `--log=info` with that `RUST_LOG` filter.
fn live_detlog_records(backend: Option<&Path>, label: &str, rust_log: Option<&str>) -> Vec<String> {
    let mut command = Command::new(hermit_binary());
    match rust_log {
        Some(filter) => {
            command.env("RUST_LOG", filter);
        }
        None => {
            command.arg("--log=info");
        }
    }
    if let Some(loader) = backend {
        command
            .env("HERMIT_SABRE_BINARY", loader)
            .args(["--backend", "sabre"]);
    }
    command
        .arg("run")
        .args([
            "--strict",
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            COMPARISON_EPOCH,
        ])
        .args(["--", "/bin/true"]);
    // The INFO stream goes to a file: run_bounded reads its pipes only after
    // the run exits, and a full stderr pipe stalls the run until it is killed.
    let stream = tempfile::Builder::new()
        .prefix("sabre-startup-order-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create the INFO stream file");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(
            stream
                .reopen()
                .expect("failed to reopen the INFO stream file"),
        )
        .process_group(0);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {error}"));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("failed to poll the run") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(45) {
            kill_process_group(child.id(), label);
            panic!("{label} did not finish within 45 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let text = std::fs::read_to_string(stream.path())
        .unwrap_or_else(|error| panic!("failed to read the {label} INFO stream: {error}"));
    assert!(status.success(), "{label} failed: {status}\n{text}");
    detlog_records(&text)
}

#[test]
fn sabre_non_racy_examples_verify_current_envelope() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");

    // race.sh is deliberately outside this ratchet: its output is the schedule itself, and the
    // in-process backend does not yet serialize arbitrary guest instructions between callbacks.
    // Both parity sides use the portable profile because this job intentionally runs without PMU
    // or CPUID-faulting support. Route Hermit diagnostics to failure-only sidecars while comparing
    // guest output; strict handling remains enabled, and SaBRe verification keeps info logs.
    for name in NON_RACY_EXAMPLES {
        let program = repository.join("examples").join(name);
        if name == "date.sh" {
            // Removing the #1095 exec reset intentionally exposes that ptrace and
            // SaBRe currently accumulate different, but individually deterministic,
            // continuous clock trajectories. Do not restore fake all-zero parity.
            // Cross-backend trajectory alignment is tracked by task
            // `cross-backend-continuous-clock-trajectory-parity`.
            let ptrace = assert_three_run_determinism(&program, &[], None, "ptrace", name);
            let sabre = assert_three_run_determinism(&program, &[], Some(&loader), "SaBRe", name);
            assert_date_output_is_sane(&ptrace, "ptrace");
            assert_date_output_is_sane(&sabre, "SaBRe");
            assert_sabre_verify(&program, &[], &loader, name);
        } else {
            assert_backend_parity_and_sabre_verify(&program, &[], &loader, name);
        }
    }

    // A matching one-shot timestamp could conceal a frozen clock. This guest
    // requires CLOCK_MONOTONIC to advance across deterministic syscall work.
    let guest_dir = tempfile::Builder::new()
        .prefix("sabre-clock-progress-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create SaBRe clock-progress guest directory");
    let clock_progress = guest_dir.path().join("clock-progress");
    let build = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/liteinst_advanced.c"))
        .arg("-o")
        .arg(&clock_progress)
        .output()
        .expect("failed to compile clock-progress guest");
    assert!(
        build.status.success(),
        "clock-progress guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );
    assert_three_run_determinism(
        &clock_progress,
        &["clock-progress"],
        None,
        "ptrace",
        "clock-progress",
    );
    assert_three_run_determinism(
        &clock_progress,
        &["clock-progress"],
        Some(&loader),
        "SaBRe",
        "clock-progress",
    );
    assert_sabre_verify(
        &clock_progress,
        &["clock-progress"],
        &loader,
        "clock-progress",
    );
}

#[test]
fn sabre_libc_getrandom_is_deterministic() {
    let Some(loader) = sabre_loader() else {
        return;
    };

    // Compile a hermetic caller of the public glibc getrandom function. Host
    // patch packages do not share one implementation: some call the public
    // symbol that SaBRe detours, while others reach an internal libc alias or
    // raw syscall site. Those package-specific paths belong in the measured
    // compatibility corpus rather than this portable mechanism test.
    let guest_dir = tempfile::Builder::new()
        .prefix("sabre-libc-getrandom-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create SaBRe libc-getrandom guest directory");
    let source = guest_dir.path().join("libc-getrandom.c");
    let program = guest_dir.path().join("libc-getrandom");
    std::fs::write(
        &source,
        r#"#include <errno.h>
#include <stdio.h>
#include <sys/random.h>

int main(void) {
  unsigned char bytes[16];
  if (getrandom(bytes, sizeof(bytes), 0) != sizeof(bytes)) return 1;
  for (unsigned int i = 0; i < sizeof(bytes); i++) printf("%02x", bytes[i]);
  putchar('\n');
  errno = 0;
  if (getrandom(bytes, sizeof(bytes), 0x80000000u) != -1 || errno != EINVAL)
    return 2;
  return 0;
}
"#,
    )
    .expect("failed to write SaBRe libc-getrandom guest source");
    let build = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("failed to compile SaBRe libc-getrandom guest");
    assert!(
        build.status.success(),
        "libc-getrandom guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    assert_backend_parity_and_sabre_verify(&program, &[], &loader, "public libc getrandom");
}

/// The SaBRe plugin is constructed again in every forked child. Before the
/// coordinator fingerprint was remembered across fork, that construction wrote
/// the plugin's "published no fingerprint" warning, with build-dependent bytes,
/// to the child's fd 2. This guest points its stderr at a pipe, forks a child
/// that only exits, and reports what the child's stderr received. The parity
/// runs use `--log=warn`, and at that level the pipe must receive nothing on
/// either backend: the warning is gone. This does not claim that the plugin
/// never writes to a forked child's fd 2: at `--log=info` the inherited DETLOG
/// forwarder still writes the child's records there, and the INFO `--verify`
/// run at the end checks only that those bytes repeat.
#[test]
fn sabre_forked_child_gets_no_plugin_warning_on_guest_stderr() {
    let Some(loader) = sabre_loader() else {
        return;
    };

    let guest_dir = tempfile::Builder::new()
        .prefix("sabre-fork-stderr-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create SaBRe fork-stderr guest directory");
    let source = guest_dir.path().join("fork-stderr.c");
    let program = guest_dir.path().join("fork-stderr");
    std::fs::write(
        &source,
        r#"#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
  int pipefd[2];
  int saved = dup(2);
  if (saved < 0 || pipe(pipefd) != 0) return 1;
  if (dup2(pipefd[1], 2) != 2) return 2;
  close(pipefd[1]);
  pid_t child = fork();
  if (child < 0) return 3;
  if (child == 0) _exit(0);
  if (dup2(saved, 2) != 2) return 4;
  close(saved);
  int status;
  if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
      WEXITSTATUS(status) != 0)
    return 5;
  char bytes[4096];
  size_t total = 0;
  ssize_t count;
  while ((count = read(pipefd[0], bytes, sizeof(bytes))) > 0) {
    fwrite(bytes, 1, (size_t)count, stdout);
    total += (size_t)count;
  }
  if (count < 0) return 6;
  printf("forked child wrote %zu bytes to its stderr\n", total);
  return 0;
}
"#,
    )
    .expect("failed to write SaBRe fork-stderr guest source");
    let build = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("failed to compile SaBRe fork-stderr guest");
    assert!(
        build.status.success(),
        "fork-stderr guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    const EXPECTED: &str = "forked child wrote 0 bytes to its stderr\n";
    let ptrace = parity_run(&program, &[], None, "ptrace forked-child stderr");
    let sabre = parity_run(&program, &[], Some(&loader), "SaBRe forked-child stderr");
    for (output, backend) in [(&ptrace, "ptrace"), (&sabre, "SaBRe")] {
        assert_eq!(
            output.status.code(),
            Some(0),
            "{backend} fork-stderr guest failed: {output:?}"
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            EXPECTED,
            "{backend}: a forked child's guest stderr received bytes at --log=warn",
        );
    }
    assert_eq!(sabre.stderr, ptrace.stderr, "stderr parity: fork-stderr");
    assert_sabre_verify(&program, &[], &loader, "fork-stderr");
}

/// SaBRe's dispatch record: its rewrite sites are measured, and every tracer
/// syscall-exit stop follows an entry stop.
#[test]
fn sabre_dispatch_record_reports_its_routes_and_tracer_stops() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let guest = dispatch_stats::build_guest("guest-sabre", &[]);
    let record = dispatch_stats::dispatch_record(
        "sabre",
        &hermit_binary(),
        &[],
        &[("HERMIT_SABRE_BINARY", &loader)],
        &guest,
    );
    assert!(
        record.sites.candidates.is_some_and(|sites| sites > 0),
        "{record}"
    );
    assert!(
        record.sites.patched.is_some_and(|sites| sites > 0),
        "{record}"
    );
    assert!(
        record
            .counters
            .patched_direct_calls
            .is_some_and(|calls| calls > 0),
        "{record}"
    );
    let (Some(exit_stops), Some(entry_stops)) = (
        record.counters.ptrace_syscall_exit_stops,
        record.counters.ptrace_syscall_entry_stops,
    ) else {
        panic!("the SaBRe supervisor must measure its syscall stops: {record}");
    };
    assert!(
        exit_stops <= entry_stops,
        "a syscall exit stop without its entry: {record}"
    );
}

/// [`host_input::verify_across_host_action`] on the SaBRe backend, or `None`,
/// with the reason printed, when its artifacts are not built here (an
/// explicitly configured `HERMIT_SABRE_BINARY` that is missing panics instead).
fn sabre_verify_across_host_action(
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> Option<(
    PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
)> {
    let loader = sabre_loader()?;
    Some(host_input::verify_across_host_action(
        &hermit_binary(),
        &["--backend=sabre"],
        &[("HERMIT_SABRE_BINARY", &loader)],
        name,
        guest,
        replace_in_run1,
        lines,
    ))
}

/// The sar divergence in miniature on SaBRe, whose Detcore runs inside the
/// guest: a host file replaced between run 1's two opens of it. The guest's
/// openat handler reports each open to the coordinator's global state over
/// SaBRe's RPC, so `--verify` finds the replacement, placed before the
/// divergence. It reports it without naming it the cause, because SaBRe's
/// loader runs an exec'd program's `.preinit_array` before Detcore starts:
/// guest code Hermit does not observe could have made the change. The run
/// stays a divergence. The guest starts no child process, so the two runs
/// have no child exit whose order host timing decides
/// ([`host_input::HOST_INPUT_GUEST_WITHOUT_CHILDREN`]).
#[test]
fn sabre_reports_a_replaced_host_file_without_naming_it_the_cause() {
    let Some((root, stderr, report)) = sabre_verify_across_host_action(
        "sabre-host-input-replaced",
        host_input::HOST_INPUT_GUEST_WITHOUT_CHILDREN,
        true,
        ["go", "go"],
    ) else {
        return;
    };
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    let line = stderr
        .lines()
        .find(|line| line.starts_with("HERMIT_HOST_INPUT_CHANGE_UNATTRIBUTED "))
        .unwrap_or_else(|| panic!("the replacement was not reported\n{stderr}"));
    assert!(
        line.contains(&format!(
            "host input changed during run 1: {}",
            root.join("F").display()
        )),
        "{line}"
    );
    assert!(
        line.contains("SaBRe's loader runs an exec'd program's .preinit_array"),
        "{line}"
    );
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGED "), "{stderr}");
}

/// The control on SaBRe: the runs read different lines and no host file
/// changes. The divergence is not attributed to a host input change.
#[test]
fn sabre_names_no_host_input_change_for_another_divergence() {
    let Some((_root, stderr, report)) = sabre_verify_across_host_action(
        "sabre-host-input-unchanged",
        host_input::HOST_INPUT_GUEST,
        false,
        ["one", "two"],
    ) else {
        return;
    };
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}");
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGE"), "{stderr}");
}

/// The counterexample on SaBRe, whose log puts the guest's records after the
/// coordinator's: the guest replaces `F` itself in run 1 only, after reading a
/// different line. The patterns differ as a host replacement's would, but the
/// divergence is recorded first, so the change is not even reported.
#[test]
fn sabre_names_no_host_input_change_for_a_guest_that_replaced_a_file_after_diverging() {
    let Some((_root, stderr, report)) = sabre_verify_across_host_action(
        "sabre-host-input-self-replaced",
        host_input::SELF_REPLACING_GUEST,
        false,
        ["moveA", "moveB"],
    ) else {
        return;
    };
    assert_eq!(
        report.verdict,
        hermit::canonical_verdict::Verdict::Diverged,
        "{stderr}"
    );
    assert_eq!(report.infrastructure_error, None, "{stderr}"); // Refused by the position rule, before the backend is considered.
    assert!(!stderr.contains("HERMIT_HOST_INPUT_CHANGE"), "{stderr}");
}
