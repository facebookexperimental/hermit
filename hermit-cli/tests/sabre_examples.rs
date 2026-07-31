/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

const NON_RACY_EXAMPLES: [&str; 4] = ["date.sh", "devrand.sh", "rand.py", "timed-progress-bar.py"];

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

fn run_bounded(mut command: Command, label: &str, diagnostic_log: Option<&Path>) -> Output {
    command
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
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
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

fn example_command(
    example: &Path,
    backend: Option<&Path>,
    verify: bool,
    diagnostic_log: Option<&Path>,
) -> Command {
    let mut command = Command::new(hermit_binary());
    command.arg(if verify { "--log=info" } else { "--log=error" });
    if let Some(path) = diagnostic_log {
        command.arg("--log-file").arg(path);
    }
    command.arg("run");
    if let Some(loader) = backend {
        command
            .env("HERMIT_SABRE_BINARY", loader)
            .args(["--backend", "sabre"]);
    }
    command.args([
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if verify {
        command.arg("--verify");
    }
    command.arg("--").arg(example);
    command
}

fn parity_run(example: &Path, backend: Option<&Path>, label: &str) -> Output {
    // Hermit gives the guest a private /tmp, so keep the controller sidecar in the host-visible
    // Cargo target directory. The freshly created unique file prevents stale or cross-run logs.
    let diagnostic_log = tempfile::Builder::new()
        .prefix("sabre-parity-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| panic!("failed to create {label} diagnostic log: {error}"));
    run_bounded(
        example_command(example, backend, false, Some(diagnostic_log.path())),
        label,
        Some(diagnostic_log.path()),
    )
}

#[test]
fn sabre_non_racy_examples_verify_and_match_ptrace() {
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
        let example = repository.join("examples").join(name);
        let ptrace = parity_run(
            &example,
            None,
            &format!("ptrace strict portable reference for {name}"),
        );
        let sabre = parity_run(
            &example,
            Some(&loader),
            &format!("SaBRe strict portable parity run for {name}"),
        );
        assert_eq!(sabre.status.code(), ptrace.status.code(), "example: {name}");
        assert_eq!(sabre.stdout, ptrace.stdout, "stdout parity: {name}");
        assert_eq!(sabre.stderr, ptrace.stderr, "stderr parity: {name}");

        let verify = run_bounded(
            example_command(&example, Some(&loader), true, None),
            &format!("SaBRe strict portable verification for {name}"),
            None,
        );
        let diagnostics = format!(
            "{}{}",
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr),
        );
        assert!(
            diagnostics.contains("Success: deterministic. Determinism verified."),
            "SaBRe verifier omitted its success verdict for {name}:\n{diagnostics}",
        );
    }
}
