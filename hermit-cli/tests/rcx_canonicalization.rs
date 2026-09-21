/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end check for rcx/r11 canonicalization (defense-in-depth determinism).
//!
//! The `syscall` instruction clobbers %rcx (return RIP) and %r11 (RFLAGS). Their
//! post-syscall contents are architecturally undefined, so hermit must force
//! them to deterministic values even for a guest that reads them, and must never
//! leak Reverie's private trampoline address through %rcx.
//!
//! `tests/c/rcx_canonicalization.c` issues a syscall, captures %rcx/%r11, and
//! asserts that %rcx equals the address of the instruction right after the
//! syscall (what a faithful SYSRET leaves there); it exits non-zero otherwise.
//! Running it under `hermit run --strict` therefore checks both that %rcx is
//! *canonical* (exit 0 -> the trampoline leak is closed) and *deterministic*
//! (identical stdout across repeated runs).

use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const DETERMINISM_RUNS: usize = 5;
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";

static HERMIT_RCX_LOCK: Mutex<()> = Mutex::new(());
static RCX_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn hermit_rcx_lock() -> MutexGuard<'static, ()> {
    HERMIT_RCX_LOCK
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

fn rcx_guest() -> &'static Path {
    RCX_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            // This integration target is selected by more than one DAG step.
            // Keep concurrent nextest processes from replacing each other's
            // guest binary while Hermit is about to execute it.
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
                .join(format!("rcx-canonicalization-{}", std::process::id()));
            fs::create_dir_all(&build_root)
                .expect("failed to create rcx canonicalization build directory");
            let binary = build_root.join("rcx_canonicalization");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-D_GNU_SOURCE",
                    // gnu11: the guest uses GNU register-asm (`register ... __asm__("rax")`).
                    "-std=gnu11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/rcx_canonicalization.c"))
                .arg("-o")
                .arg(&binary);
            let output = command
                .output()
                .expect("failed to launch cc for rcx canonicalization guest");
            assert!(
                output.status.success(),
                "rcx canonicalization guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            binary
        })
        .as_path()
}

/// Run the guest once under `hermit run --strict` and return its output.
fn run_under_hermit_strict() -> Output {
    let guest = rcx_guest();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .current_dir(
            guest
                .parent()
                .expect("rcx guest should have a build directory"),
        )
        .args([
            "--log=off",
            "run",
            "--strict",
            // Match the other strict e2e tests: these relaxations keep the test
            // usable on VMs without CPUID interception without weakening strict mode.
            "--no-virtualize-cpuid",
            "--max-timeslice=disabled",
            "--base-env=minimal",
        ]);
    configure_execution_root(&mut command);
    command.arg("--").arg(guest);
    command
        .output()
        .expect("failed to launch hermit run for rcx canonicalization guest")
}

#[test]
#[ignore = "validate: fixed /test argument contract"]
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
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces"]
fn rcx_r11_are_canonical_and_deterministic_under_strict() {
    let _guard = hermit_rcx_lock();

    let mut previous: Option<String> = None;
    for iteration in 0..DETERMINISM_RUNS {
        let output = run_under_hermit_strict();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);

        // Exit 0 means the guest's in-process assertion held: %rcx equals the
        // return RIP (no leaked trampoline address) and %r11 looks like RFLAGS.
        assert!(
            output.status.success(),
            "guest reported non-canonical rcx/r11 under hermit (iteration {iteration}):\n\
             status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
        assert!(
            stdout.contains("rcx_is_return_rip=1"),
            "unexpected guest stdout (iteration {iteration}):\n{stdout}\nstderr:\n{stderr}",
        );

        // Every run must produce byte-identical output (deterministic).
        if let Some(prev) = &previous {
            assert_eq!(
                prev, &stdout,
                "rcx/r11 output was not deterministic across runs (iteration {iteration})",
            );
        }
        previous = Some(stdout);
    }
}
