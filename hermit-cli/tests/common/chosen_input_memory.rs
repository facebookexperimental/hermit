/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest memory at the old Chosen borrowed-stack footprint is application
//! memory, including valid clone3 inputs and native output words. These are
//! real CLI/ptrace controls; backend fault/cancellation fixtures are separate.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

use super::HERMIT_POLICY_REFUSAL_EXIT;
use super::OWN_PID_NAMESPACE;
use super::assert_success;
use super::hermit_test;
use super::process_build_root;
use super::stderr;
use super::stdout;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let build_root = process_build_root("chosen-input-memory");
        std::fs::create_dir_all(&build_root).expect("create chosen-input-memory directory");
        let executable = build_root.join("chosen_input_memory");
        let output = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/chosen_input_memory.c"))
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile chosen-input-memory guest");
        assert!(
            output.status.success(),
            "chosen-input-memory compilation failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        executable
    })
}

fn run(mode: &str, hermit_args: Option<&[&str]>) -> Output {
    let mut command = Command::new("unshare");
    command.args(OWN_PID_NAMESPACE);
    if let Some(args) = hermit_args {
        command
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args(["run", "--no-namespace", "--base-env=minimal"])
            .args(args)
            .arg("--");
    }
    command.arg(guest()).arg(mode);
    hermit_test::hermit_output(
        &format!("chosen old-stack image: {command:?}"),
        &mut command,
    )
}

fn expected(mode: &str) -> String {
    let outputs = if mode.ends_with("outputs") {
        "preserved"
    } else {
        "absent"
    };
    format!(
        "clone3 old-stack {mode}: parent image preserved; child image preserved; outputs {outputs}\n"
    )
}

/// The native control establishes that the exact input/output alias placement
/// is admitted by Linux. Strict Chosen must retain the full parent/child image,
/// except for the byte-exact parent_tid, child_tid and pidfd kernel outputs.
/// A blanket restoration after the successful clone erases those outputs and
/// fails this same guest oracle.
#[test]
fn chosen_clone_preserves_old_stack_inputs_and_native_output_aliases() {
    for mode in ["input", "outputs"] {
        let native = run(mode, None);
        assert_success(&native, &["native", mode]);
        assert_eq!(stdout(&native), expected(mode), "{}", stderr(&native));

        let chosen = run(mode, Some(&["--pid-base=5000", "--summary"]));
        assert_success(&chosen, &["--pid-base=5000", mode]);
        assert_eq!(stdout(&chosen), expected(mode), "{}", stderr(&chosen));
        assert!(
            stderr(&chosen).contains("Guest process IDs: chosen by Hermit from 5000")
                && !stderr(&chosen).contains("got a host ID")
                && !stderr(&chosen).contains("host-ID report(s) accepted"),
            "{mode}: {}",
            stderr(&chosen)
        );
    }
}

/// Dropping the outer capabilities makes only Hermit's added outer set_tid
/// fail with EPERM. The unchanged original call still creates PID 1 in its new
/// user/PID namespace. Both the original argument block and its set_tid [1]
/// occupy the old overwrite footprint. Require the actual rewritten EPERM and
/// honest host-ID report, so an earlier facility refusal cannot impersonate the
/// failure/fallback path this test exercises.
#[test]
fn chosen_clone_eperm_fallback_preserves_original_input_and_output_aliases() {
    for mode in ["fallback-input", "fallback-outputs"] {
        let native = run(mode, None);
        assert_success(&native, &["native", mode]);
        assert_eq!(stdout(&native), expected(mode), "{}", stderr(&native));

        let tolerant = run(mode, Some(&["--summary"]));
        assert_success(&tolerant, &["--summary", mode]);
        assert_eq!(stdout(&tolerant), expected(mode), "{}", stderr(&tolerant));
        let log = stderr(&tolerant);
        assert!(
            log.contains("the rewrite with ID ")
                && log.contains("in Hermit's PID namespace failed with EPERM")
                && log.contains("got a host ID, not a chosen one")
                && log.contains("1 host-ID report(s) accepted"),
            "{mode}: did not exercise the native EPERM/original-call fallback: {log}"
        );
        assert!(
            !log.contains("its reserved input is unavailable"),
            "{mode}: facility admission substituted for the required rewritten EPERM: {log}"
        );
    }
}

/// The same valid original call is not run after the rewritten EPERM when an
/// explicit pid base requires Chosen. This is a policy refusal before a host
/// child, not a successful memory-preservation run or a native errno result.
#[test]
fn strict_chosen_clone_eperm_refuses_before_original_stack_alias_call() {
    for mode in ["fallback-input", "fallback-outputs"] {
        let refused = run(mode, Some(&["--pid-base=5000", "--summary"]));
        assert_eq!(
            refused.status.code(),
            Some(HERMIT_POLICY_REFUSAL_EXIT),
            "{mode}: stdout:\n{}\nstderr:\n{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert_eq!(stdout(&refused), "", "{mode}: {}", stderr(&refused));
        let log = stderr(&refused);
        assert!(
            log.contains("the rewrite with ID 5001 in Hermit's PID namespace failed with EPERM")
                && !log.contains("got a host ID")
                && !log.contains("host-ID report(s) accepted"),
            "{mode}: {log}"
        );
    }
}
