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
    // guest processes cannot retain a pipe descriptor and hang `wait_with_output`.
    kill_process_group(child.id(), label);
    let output = child.wait_with_output().unwrap_or_else(|error| {
        panic!(
            "failed to collect {label}: {rendered}: {error}\ncontroller diagnostics:\n{}",
            controller_diagnostics(diagnostic_log),
        )
    });
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
#include <string.h>
#include <unistd.h>

static void drop_tool_output(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  for (char **entry = envp; *entry; entry++)
    if (strncmp(*entry, "REVERIE_SABRE_TOOL_OUTPUT_FD=", 29) == 0)
      close(atoi(*entry + 29));
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
#include <unistd.h>

static void take_stderr(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  for (char **entry = envp; *entry; entry++)
    if (strncmp(*entry, "REVERIE_SABRE_TOOL_OUTPUT_FD=", 29) == 0)
      close(atoi(*entry + 29));
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

static void replace_tool_output(int argc, char **argv, char **envp) {
  (void)argc;
  (void)argv;
  for (char **entry = envp; *entry; entry++) {
    if (strncmp(*entry, "REVERIE_SABRE_TOOL_OUTPUT_FD=", 29) == 0) {
      int passed = atoi(*entry + 29);
      int pair[2];
      close(passed);
      if (socketpair(AF_UNIX, SOCK_SEQPACKET, 0, pair) != 0) _exit(3);
      if (pair[0] != passed && dup2(pair[0], passed) != passed) _exit(4);
      if (pair[0] != passed) close(pair[0]);
      peer = pair[1];
    }
  }
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
