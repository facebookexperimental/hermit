/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! GNU loader startup guards must derive from the guest's AT_RANDOM, and
//! remain identical between independent strict executions. These reads never
//! rewrite TLS, AT_RANDOM, saved canaries, or mangled pointers.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let root = super::process_build_root("startup-guards");
        fs::create_dir_all(&root).expect("create startup guard guest directory");
        let guest = root.join("startup_guards");
        let compiled = Command::new("cc")
            .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/startup_guards.c"))
            .arg("-o")
            .arg(&guest)
            .output()
            .expect("compile dynamically linked GNU startup guard probe");
        assert!(compiled.status.success(), "{compiled:?}");
        guest
    })
}

#[derive(Debug, PartialEq, Eq)]
struct Guards {
    canary: u64,
    pointer_guard: u64,
    at_random: [u8; 16],
}

fn parse(output: &Output) -> Guards {
    assert!(
        output.status.success(),
        "startup guard guest failed: {output:?}"
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("ASCII probe stdout");
    let line = stdout.strip_suffix('\n').expect("probe final newline");
    let fields: Vec<_> = line.split(' ').collect();
    assert_eq!(fields.len(), 3, "{stdout:?}");
    let hex = |field: &str, prefix: &str, length: usize| {
        let value = field.strip_prefix(prefix).expect("probe field name");
        assert_eq!(value.len(), length, "{stdout:?}");
        assert!(
            value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "{stdout:?}"
        );
        value.to_owned()
    };
    let canary = u64::from_str_radix(&hex(fields[0], "canary=", 16), 16).unwrap();
    let pointer_guard = u64::from_str_radix(&hex(fields[1], "pointer_guard=", 16), 16).unwrap();
    let random = hex(fields[2], "at_random=", 32);
    let at_random = std::array::from_fn(|index| {
        u8::from_str_radix(&random[2 * index..2 * index + 2], 16).unwrap()
    });
    Guards {
        canary,
        pointer_guard,
        at_random,
    }
}

fn assert_startup_guards(backend: &str) {
    let _guard = super::hermit_run_guard();
    let guest = guest().to_str().expect("UTF-8 guest path");
    // Execute BOTH independent processes before checking either result. Each
    // command gets exactly one attempt; no --verify or skid retry hides a run.
    let runs: [Output; 2] = std::array::from_fn(|_| {
        let mut command = super::hermit_command(&[
            "--log=error",
            "--backend",
            backend,
            "run",
            "--strict",
            "--max-timeslice=disabled",
            "--epoch=2026-01-01T00:00:00Z",
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            guest,
        ]);
        command.stdin(Stdio::null());
        super::hermit_test::hermit_output_attempts("startup guard probe", 1, &mut command)
    });
    let guards = runs.each_ref().map(parse);
    for run in &runs {
        let selected = match backend {
            "liteinst" => Some(
                "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool",
            ),
            "in-guest-trap" => Some(
                "hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)",
            ),
            "ptrace" => None,
            _ => panic!("unreviewed backend {backend}"),
        };
        if let Some(selected) = selected {
            let stderr = std::str::from_utf8(&run.stderr).expect("UTF-8 Hermit diagnostics");
            assert!(
                stderr.lines().any(|line| line == selected),
                "{backend}: {run:?}"
            );
        }
    }
    let actual = guards
        .each_ref()
        .map(|guard| (guard.canary, guard.pointer_guard));
    let derived = guards.each_ref().map(|guard| {
        let canary = u64::from_le_bytes(guard.at_random[..8].try_into().unwrap()) & !0xff;
        let pointer_guard = u64::from_le_bytes(guard.at_random[8..].try_into().unwrap());
        (canary, pointer_guard)
    });
    assert_eq!(
        actual, derived,
        "{backend}: GNU loader guard derivation: {guards:?}"
    );
    assert_eq!(
        actual[0], actual[1],
        "{backend}: independent startup guard pair: {guards:?}"
    );
    assert_eq!(
        runs[0].stdout, runs[1].stdout,
        "{backend}: two independent strict runs: {guards:?}"
    );
}

// Named issue draft, not an invented GitHub issue number:
// ai_docs/issue-drafts/2026-10-17-in-guest-trap-canary-pointer-guard-host-entropy.md
// Both tests assert the desired success and exact equality. Remove only the
// ignore annotations when the startup entropy defect is fixed.
#[cfg(feature = "liteinst")]
#[test]
#[ignore = "known failure: https://github.com/rrnewton/dev-hermit/blob/main/ai_docs/issue-drafts/2026-10-17-in-guest-trap-canary-pointer-guard-host-entropy.md"]
fn liteinst_startup_guards_derive_from_at_random_and_repeat() {
    assert_startup_guards("liteinst");
}

#[cfg(feature = "liteinst")]
#[test]
#[ignore = "known failure: https://github.com/rrnewton/dev-hermit/blob/main/ai_docs/issue-drafts/2026-10-17-in-guest-trap-canary-pointer-guard-host-entropy.md"]
fn in_guest_trap_startup_guards_derive_from_at_random_and_repeat() {
    assert_startup_guards("in-guest-trap");
}
