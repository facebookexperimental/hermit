/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs::File;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Error;
use clap::Parser;
use hermit::Backend;
use tracing::metadata::LevelFilter;

use super::tracing::BoundedWriter;
use super::tracing::LatchedWriter;
use super::tracing::TracingGuard;
use super::tracing::WriteErrorLatch;
use super::tracing::init_file_tracing;
use super::tracing::init_file_tracing_with_evidence;
use super::tracing::init_stderr_tracing;
use super::tracing::init_stderr_tracing_with_evidence;
use super::tracing::log_max_bytes;

/// Hermit provides a sandbox for deterministic and reproducible execution.
/// Arbitrary programs run inside (guests) become deterministic
/// functions of their inputs. Configuration flags control the initial
/// environment.
///
/// See the "run" and "record" subcommands to run programs within hermit.
/// In both modes, the host file system is visible
/// to the command run inside hermit, and the results will depend on the contents
/// (but not timestamps or inode numbers) of those inputs.
///
/// In run mode, networking is disallowed.  Run mode guarantees that if you
/// run twice with the same input files, you will receive bitwise identical
/// outputs from the computation.
///
/// In record mode, inputs (both files and network traffic) are captured
/// in a content addressible store (CAS).  In this preview version of
/// hermit, the CAS is stored locally in your home directory (~/.hermit).
///
/// Below are options common to all subcommands.
#[derive(Debug, Parser, Clone)]
pub struct GlobalOpts {
    /// The verbosity level of log output.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    pub log: Option<LevelFilter>,

    /// Log to a file instead of the terminal. The path is resolved on the HOST,
    /// exactly like a shell redirect, not inside the container the guest runs in.
    #[clap(long, value_name = "FILE", env = "HERMIT_LOG_FILE")]
    pub log_file: Option<PathBuf>,

    /// The log file, already opened in the HOST's filename namespace.
    ///
    /// WHY THE FILE IS CARRIED INSTEAD OF RE-OPENED FROM THE PATH. Tracing has to be
    /// initialized INSIDE the container -- see `init_tracing`, whose reason is that
    /// the tracer may create a thread -- and the container mounts a fresh writable
    /// /tmp over its root (container.rs: "A fresh writable /tmp is mounted separately
    /// for ordinary scratch files"). So opening the path at that point resolves it in
    /// the GUEST namespace. For `--log-file /tmp/x.log` the create then SUCCEEDS, into
    /// the guest's tmpfs, and the file dies with the container: exit 0, no log, no
    /// warning. Measured 2026-08-20; one debugging session was lost to it.
    ///
    /// An open file descriptor is unaffected by a later mount-namespace change, so
    /// opening on the host and carrying the handle in is mechanically what a shell
    /// redirect does. Only the OPEN moves out of the container; tracing itself is
    /// still initialized inside, so the stated reason for that is untouched.
    ///
    /// `Arc` because `GlobalOpts` is `Clone` (verify clones it per run) and because
    /// each `init_tracing` needs its own owned `File`, produced with `try_clone`.
    #[clap(skip)]
    pub log_file_handle: Option<Arc<File>>,

    /// Fresh anonymous host file receiving the opt-in ordinary-run evidence log.
    /// It is opened before entering the container and never replaces `--log-file`.
    #[clap(skip)]
    pub(crate) run_evidence_log_handle: Option<Arc<File>>,

    /// Process-shared error state for the private run-evidence writer.
    #[clap(skip)]
    pub(crate) run_evidence_write_error: Option<WriteErrorLatch>,

    /// Select the process instrumentation backend. This is the preferred, global
    /// position (e.g. `hermit --backend ptrace run ...`); for backwards
    /// compatibility `run` also accepts `--backend` after the subcommand.
    #[clap(long, value_enum, value_name = "BACKEND")]
    pub backend: Option<Backend>,
}

impl GlobalOpts {
    /// Open `--log-file` in the HOST's filename namespace.
    ///
    /// Call this from `main`, before any container exists. That placement is the
    /// whole point: it is the moment a shell would perform `> file`.
    ///
    /// Reports the path on failure instead of proceeding. A run that was asked for a
    /// log and produces neither the log nor an error is indistinguishable from a run
    /// whose log was legitimately empty, and the person debugging cannot tell which.
    pub fn open_log_file(&mut self) -> Result<(), Error> {
        if let Some(path) = &self.log_file {
            let file = File::create(path).with_context(|| {
                format!("cannot open --log-file {} for writing", path.display())
            })?;
            self.log_file_handle = Some(Arc::new(file));
        }
        Ok(())
    }

    pub(crate) fn set_run_evidence_log_handle(
        &mut self,
        handle: Arc<File>,
        write_error: WriteErrorLatch,
    ) {
        self.run_evidence_log_handle = Some(handle);
        self.run_evidence_write_error = Some(write_error);
    }

    fn run_evidence_writer(&self, limit: u64) -> Option<LatchedWriter<BoundedWriter<File>>> {
        self.run_evidence_log_handle.as_ref().map(|evidence| {
            let evidence = evidence
                .try_clone()
                .expect("cannot duplicate the private run-evidence descriptor");
            let evidence = BoundedWriter::new(evidence, limit);
            let write_error = self
                .run_evidence_write_error
                .as_ref()
                .expect("private run-evidence writer is missing its error latch")
                .clone();
            LatchedWriter::new(evidence, write_error)
        })
    }

    /// Initalizes tracing. If using a container, this must be done *inside* of
    /// the container because the tracer may create a new thread.
    ///
    /// The file itself is NOT opened here when it came from `--log-file`; see
    /// `log_file_handle` for why opening at this point resolves the path in the
    /// guest namespace and silently loses it.
    #[must_use = "This function returns a guard that should not be immediately dropped"]
    pub fn init_tracing(&self) -> Option<TracingGuard> {
        if let Some(handle) = &self.log_file_handle {
            // Each subscriber needs an owned File; `try_clone` dups the descriptor,
            // so every run writes through the same host-side open file.
            let file_writer = handle
                .try_clone()
                .expect("cannot duplicate the host log file descriptor");
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let file_writer = BoundedWriter::new(file_writer, limit);
            if let Some(evidence) = self.run_evidence_writer(limit) {
                Some(init_file_tracing_with_evidence(
                    self.log,
                    file_writer,
                    evidence,
                ))
            } else {
                Some(init_file_tracing(self.log, file_writer))
            }
        } else if let Some(path) = &self.log_file {
            // An internal caller set the path directly rather than going through
            // `open_log_file` -- today that is verify's double-run setup, which
            // creates its own temp files and is measured NOT to hit the namespace
            // problem. Keep the historical behaviour for those, rather than changing
            // a path this task did not investigate.
            let file_writer = File::create(path).expect("Failed to open log file");
            // Bounded so a run that makes no progress cannot fill the disk: a
            // livelocked guest logged 928.8 GiB over 11.7 hours before this.
            // The bound is on the LOG only; the run is unaffected.
            // A malformed bound is fatal rather than silently defaulted, so a
            // typo in the value meant to DISABLE the bound cannot quietly
            // re-enable it.
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let file_writer = BoundedWriter::new(file_writer, limit);
            if let Some(evidence) = self.run_evidence_writer(limit) {
                Some(init_file_tracing_with_evidence(
                    self.log,
                    file_writer,
                    evidence,
                ))
            } else {
                Some(init_file_tracing(self.log, file_writer))
            }
        } else if self.run_evidence_log_handle.is_some() {
            let limit = log_max_bytes().unwrap_or_else(|e| panic!("{e}"));
            let evidence = self
                .run_evidence_writer(limit)
                .expect("run-evidence handle was present");
            Some(init_stderr_tracing_with_evidence(self.log, evidence))
        } else {
            init_stderr_tracing(self.log);
            None
        }
    }
}
