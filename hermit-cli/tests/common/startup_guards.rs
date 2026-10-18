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

use rand::RngExt as _;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let root = super::process_build_root("startup-guards");
        fs::create_dir_all(&root).expect("create startup guard guest directory");
        let guest = root.join("startup_guards");
        let compiled = Command::new("cc")
            .args([
                "-O2",
                "-g",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fstack-protector-all",
                "-fno-optimize-sibling-calls",
            ])
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

#[derive(Debug)]
struct Observations {
    constructor: Guards,
    main: Guards,
    atexit: Guards,
    random: [[u8; 32]; 2],
}

fn hex<const N: usize>(field: &str, prefix: &str) -> [u8; N] {
    let value = field.strip_prefix(prefix).expect("probe field name");
    assert_eq!(value.len(), 2 * N, "{field:?}");
    assert!(
        value.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{field:?}"
    );
    std::array::from_fn(|index| u8::from_str_radix(&value[2 * index..2 * index + 2], 16).unwrap())
}

fn parse_guards(line: &str) -> Guards {
    let fields: Vec<_> = line.split(' ').collect();
    assert_eq!(fields.len(), 3, "{line:?}");
    // Integer fields print in big-endian hexadecimal; AT_RANDOM prints bytes
    // in their actual address order, as in the original probe.
    Guards {
        canary: u64::from_be_bytes(hex(fields[0], "canary=")),
        pointer_guard: u64::from_be_bytes(hex(fields[1], "pointer_guard=")),
        at_random: hex(fields[2], "at_random="),
    }
}

fn parse(output: &Output) -> Observations {
    assert!(
        output.status.success(),
        "startup guard guest failed: {output:?}"
    );
    let stdout = std::str::from_utf8(&output.stdout).expect("ASCII probe stdout");
    let text = stdout.strip_suffix('\n').expect("probe final newline");
    let lines: Vec<_> = text.split('\n').collect();
    assert_eq!(lines.len(), 6, "{stdout:?}");
    let constructor = parse_guards(
        lines[0]
            .strip_prefix("constructor ")
            .expect("constructor row"),
    );
    let main = parse_guards(lines[1]);
    let stream: Vec<_> = lines[2].split(' ').collect();
    assert_eq!(stream.len(), 3, "{stdout:?}");
    assert_eq!(stream[0], "stream", "{stdout:?}");
    let random = [hex(stream[1], "first="), hex(stream[2], "second=")];
    assert_eq!(
        lines[3],
        "lifecycle constructor_calls=1 protected_ctor=528 protected_main=560 setjmp_return=7 longjmp_count=1",
        "{stdout:?}"
    );
    let atexit = parse_guards(lines[4].strip_prefix("atexit ").expect("atexit row"));
    assert_eq!(
        lines[5], "complete protected_atexit=592 atexit_calls=1",
        "{stdout:?}"
    );
    Observations {
        constructor,
        main,
        atexit,
        random,
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
            "--seed=0",
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
    let observations = runs.each_ref().map(parse);
    let guards = observations.each_ref().map(|value| &value.main);
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
        guards[0].at_random, guards[1].at_random,
        "{backend}: independent AT_RANDOM pair: {observations:?}"
    );
    let mut prng = detcore::random::root_prng(0);
    let expected_auxv = detcore::random::draw_initial_auxv(&mut prng);
    let expected_random: [[u8; 32]; 2] = std::array::from_fn(|_| {
        let mut bytes = [0; 32];
        prng.fill(&mut bytes[..]);
        bytes
    });
    for value in &observations {
        for snapshot in [&value.constructor, &value.main, &value.atexit] {
            let derived = (
                u64::from_le_bytes(snapshot.at_random[..8].try_into().unwrap()) & !0xff,
                u64::from_le_bytes(snapshot.at_random[8..].try_into().unwrap()),
            );
            assert_eq!(
                (snapshot.canary, snapshot.pointer_guard),
                derived,
                "{backend}: each lifecycle observation derives from its AT_RANDOM: {value:?}"
            );
            assert_eq!(
                snapshot, &value.main,
                "{backend}: constructor/main/atexit identity: {value:?}"
            );
            assert_eq!(
                snapshot.at_random, expected_auxv,
                "{backend}: exactly one initial Detcore draw: {value:?}"
            );
        }
        assert_eq!(
            value.random, expected_random,
            "{backend}: two ordinary getrandom32 calls continue after exactly one auxv draw: {value:?}"
        );
    }
    assert_eq!(
        runs[0].stdout, runs[1].stdout,
        "{backend}: two independent strict runs: {guards:?}"
    );
}

// Named issue draft, not an invented GitHub issue number:
// ai_docs/issue-drafts/2026-10-17-in-guest-trap-canary-pointer-guard-host-entropy.md
// Both active regressions require the real packaged runtime to establish
// deterministic loader guards before constructor, main and atexit work.
#[cfg(feature = "liteinst")]
#[test]
fn liteinst_startup_guards_derive_from_at_random_and_repeat() {
    assert_startup_guards("liteinst");
}

#[cfg(feature = "liteinst")]
#[test]
fn in_guest_trap_startup_guards_derive_from_at_random_and_repeat() {
    assert_startup_guards("in-guest-trap");
}
