/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Native terminal ownership must permit the later process clone when Linux's
//! actual exit effects have completed. The fixture waits on native clear-TID,
//! including a zombie leader with a live sibling, before making that clone.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use super::OWN_PID_NAMESPACE;
use super::assert_success;
use super::hermit_test;
use super::process_build_root;
use super::stderr;
use super::stdout;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let root = process_build_root("chosen-terminal");
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("chosen_terminal");
        let compiled = Command::new("cc")
            .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../detcore/tests/fixtures/chosen_terminal.c"),
            )
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "terminal fixture compile: {compiled:?}"
        );
        binary
    })
}

fn run_control(mode: &str, expected: &str) {
    for chosen in [false, true] {
        let mut command = Command::new("unshare");
        command.args(OWN_PID_NAMESPACE);
        if chosen {
            command.arg(env!("CARGO_BIN_EXE_hermit")).args([
                "run",
                "--no-namespace",
                "--base-env=minimal",
                "--pid-base=5000",
                "--max-timeslice=disabled",
                "--epoch=2026-01-01T00:00:00Z",
                "--summary",
                "--",
            ]);
        }
        command.arg(guest()).arg(mode);
        let output =
            hermit_test::hermit_output(&format!("chosen terminal: {command:?}"), &mut command);
        assert_success(&output, &[mode]);
        assert_eq!(
            stdout(&output),
            expected,
            "chosen={chosen}: {}",
            stderr(&output)
        );
        if chosen {
            let log = stderr(&output);
            assert!(
                log.contains("Guest process IDs: chosen by Hermit from 5000"),
                "{log}"
            );
            assert!(
                !log.contains("got a host ID") && !log.contains("host-ID report(s) accepted"),
                "{log}"
            );
        }
    }
}

#[test]
fn ordinary_thread_exit_preserves_later_chosen_clone() {
    run_control(
        "ordinary-thread-exit",
        "ordinary exit: later clone completed\n",
    );
}

#[test]
fn zombie_leader_exit_preserves_later_chosen_clone() {
    run_control("zombie-leader", "zombie leader: later clone completed\n");
}
