/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::process::Command;
use std::process::Output;

const DEFAULT_NONDETERMINISM_RETRIES: usize = 10;
const NONDETERMINISM_MARKER: &str = "Failure: nondeterministic.";
const DETERMINISM_MARKER: &str = "Success: deterministic. Determinism verified.";

/// Assertions showing that Hermit removes a guest program's nondeterminism.
///
/// Every consuming test must identify the behavior it exercises immediately
/// above the test with a comment such as `// NONDET_SOURCE: timestamp`. Labels
/// use lowercase kebab-case; common labels include `timestamp`, `pid`, `race`,
/// and `thread-order`.
pub struct NondeterminismCase<'a> {
    source: &'static str,
    program: &'a Path,
    args: &'a [&'a str],
    retries: usize,
}

impl<'a> NondeterminismCase<'a> {
    pub fn new(source: &'static str, program: &'a Path, args: &'a [&'a str]) -> Self {
        assert!(
            !source.is_empty()
                && source
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "NONDET_SOURCE must be a nonempty lowercase kebab-case label, got {source:?}",
        );
        Self {
            source,
            program,
            args,
            retries: DEFAULT_NONDETERMINISM_RETRIES,
        }
    }

    /// Changes how many executions may follow the initial naked baseline.
    ///
    /// Noop verification also gets this many retries because a race can produce
    /// matching output during any one two-run verification attempt.
    pub fn with_retries(mut self, retries: usize) -> Self {
        assert!(retries > 0, "nondeterminism retries must be nonzero");
        self.retries = retries;
        self
    }

    /// Runs the guest directly, without Hermit, until an output difference is observed.
    pub fn assert_nondeterministic_without_hermit(&self) {
        let baseline = self.run_naked();
        let mut last = None;

        for _ in 0..self.retries {
            let candidate = self.run_naked();
            if outputs_differ(&baseline, &candidate) {
                return;
            }
            last = Some(candidate);
        }

        panic!(
            "NONDET_SOURCE {} was not observed across {} naked executions\ncommand: {}\nbaseline:\n{}\nlast:\n{}",
            self.source,
            self.retries + 1,
            self.rendered_guest_command(),
            render_output(&baseline),
            render_output(last.as_ref().expect("at least one retry ran")),
        );
    }

    /// Uses the closest verify-capable passthrough preset and expects verification to fail.
    ///
    /// `--strace-only` still virtualizes some syscall results. Use this assertion
    /// only for a source that the mode passes through, and always pair it with
    /// [`Self::assert_nondeterministic_without_hermit`].
    pub fn assert_nondeterministic_with_noop_verify(&self) {
        let mut last = None;

        for _ in 0..=self.retries {
            let output = self.run_noop_passthrough(&["run", "--strace-only", "--verify", "--"]);
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    stderr.contains(NONDETERMINISM_MARKER),
                    "noop verification failed for a reason other than observed nondeterminism\nNONDET_SOURCE: {}\ncommand: {}\n{}",
                    self.source,
                    self.rendered_guest_command(),
                    render_output(&output),
                );
                return;
            }
            last = Some(output);
        }

        panic!(
            "NONDET_SOURCE {} was not observed across {} noop verification attempts\ncommand: {}\nlast:\n{}",
            self.source,
            self.retries + 1,
            self.rendered_guest_command(),
            render_output(last.as_ref().expect("at least one verification ran")),
        );
    }

    /// Requires two strict Hermit executions to match completely.
    pub fn assert_deterministic_with_strict(&self) {
        let output = self.run_hermit(&["run", "--strict", "--verify", "--"]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stderr.contains(DETERMINISM_MARKER),
            "strict Hermit did not eliminate NONDET_SOURCE {}\ncommand: {}\n{}",
            self.source,
            self.rendered_guest_command(),
            render_output(&output),
        );
    }

    fn run_naked(&self) -> Output {
        let mut command = Command::new(self.program);
        command.args(self.args);
        run_command(command, self.source, "naked guest")
    }

    fn run_hermit(&self, mode_args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args(mode_args).arg(self.program).args(self.args);
        run_command(command, self.source, "Hermit verification")
    }

    /// Like [`Self::run_hermit`], but for the deliberately non-deterministic
    /// passthrough preset (`--strace-only`).
    ///
    /// This preset is not `hermit run`'s Detcore path, so the fail-closed test
    /// ratchet's `HERMIT_FAIL_CLOSED=1` must not apply here: otherwise a
    /// passed-through syscall (e.g. `clock_gettime`) turns into an
    /// unsupported-syscall panic and masks the nondeterminism this assertion is
    /// designed to observe. Outside the ratchet the variable is unset, so
    /// removing it is a no-op.
    fn run_noop_passthrough(&self, mode_args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args(mode_args).arg(self.program).args(self.args);
        command.env_remove("HERMIT_FAIL_CLOSED");
        run_command(command, self.source, "Hermit verification")
    }

    fn rendered_guest_command(&self) -> String {
        format!("{:?} {:?}", self.program, self.args)
    }
}

fn run_command(mut command: Command, source: &str, phase: &str) -> Output {
    let rendered = format!("{command:?}");
    command.output().unwrap_or_else(|error| {
        panic!("failed to start {phase} for NONDET_SOURCE {source}: {rendered}: {error}")
    })
}

fn outputs_differ(left: &Output, right: &Output) -> bool {
    left.status != right.status || left.stdout != right.stdout || left.stderr != right.stderr
}

fn render_output(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}
