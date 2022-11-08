/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cmp;
use std::fmt;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use reverie::syscalls::SyscallInfo;
use reverie::Pid;

use crate::error::Error;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventReader;

/// An error that is raised when the replayer receives an unexpected event. This
/// error should help give us enough context to debug the desynchronization
/// error.
pub struct DesyncError {
    // The thread ID.
    pub thread: Pid,
    // The event count and also the point of desynchronization in the stream of
    // syscalls for this thread.
    pub count: u64,
    // The actual syscall.
    pub actual: DebugEvent,
    // The expected syscall.
    pub expected: DebugEvent,
}

impl fmt::Display for DesyncError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (actual, a) = self.actual.syscall().into_parts();
        let (expected, b) = self.expected.syscall().into_parts();

        writeln!(
            f,
            "On thread {}, got unexpected syscall (count = {}):",
            self.thread, self.count
        )?;
        writeln!(
            f,
            "    {}(arg0: {:#x}, arg1: {:#x}, arg2: {:#x}, arg3: {:#x}, arg4: {:#x}, arg5: {:#x})",
            actual, a.arg0, a.arg1, a.arg2, a.arg3, a.arg4, a.arg5
        )?;
        writeln!(f, "Expected:")?;
        writeln!(
            f,
            "    {}(arg0: {:#x}, arg1: {:#x}, arg2: {:#x}, arg3: {:#x}, arg4: {:#x}, arg5: {:#x})",
            expected, b.arg0, b.arg1, b.arg2, b.arg3, b.arg4, b.arg5
        )?;
        Ok(())
    }
}

pub struct DesyncSummary<'a> {
    error: &'a DesyncError,
    data_dir: &'a Path,
    context_before: usize,
    context_after: usize,
}

impl<'a> fmt::Display for DesyncSummary<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut events = match EventReader::open(self.data_dir, self.error.thread) {
            Ok(events) => events,
            Err(err) => return write!(f, "Failed to read events: {}", err),
        };

        writeln!(f, "{}", self.error)?;
        writeln!(f, "Additional context:")?;

        let begin_count = self.error.count.saturating_sub(self.context_before as u64);
        let end_count = self.error.count.saturating_add(self.context_after as u64);

        let mut event = events.next_debug_event();
        let mut count = 1;

        // Need to skip past the events preceding the beginning of the context.
        while event.is_ok() && count < begin_count {
            event = events.next_debug_event();
            count += 1;
        }

        if count > 1 {
            writeln!(f, "    # Skipped {} syscalls...", count - 1)?;
        }

        while let Ok(expected) = event {
            // Ignore all events past the end of the context.
            if count > end_count {
                break;
            }

            match count.cmp(&self.error.count) {
                cmp::Ordering::Equal => {
                    // The point of desynchronization. This is where things
                    // initially don't match up.
                    writeln!(f, "  - {}  ← Expected this", expected)?;
                    writeln!(f, "  + {}  ← but got this instead!", self.error.actual)?;
                }
                cmp::Ordering::Greater => {
                    // Any syscalls that were previously recorded were not
                    // encountered this time, so prefix with (-).
                    writeln!(f, "  - {}", expected)?;
                }
                cmp::Ordering::Less => {
                    // Any syscalls before the desynchronization point already
                    // matched.
                    writeln!(f, "    {}", expected)?;
                }
            }

            event = events.next_debug_event();
            count += 1;
        }

        Ok(())
    }
}

impl DesyncError {
    /// Generates a summary report on a desynchronization error. This is shorter
    /// than a report and just shows the surrounding syscalls as context.
    pub fn summary<'a>(
        &'a self,
        data_dir: &'a Path,
        context_before: usize,
        context_after: usize,
    ) -> DesyncSummary<'a> {
        DesyncSummary {
            error: self,
            data_dir,
            context_before,
            context_after,
        }
    }

    /// Generates a human readable report on a desynchronization error. This
    /// report should contain all of the information needed to reproduce and
    /// diagnose the failure.
    ///
    /// All reports go to a temporary file and the path of the report is returned.
    pub fn generate_report(&self, data_dir: &Path) -> Result<PathBuf, Error> {
        let (file, temp_path) = tempfile::Builder::new()
            .prefix("hermit-report-")
            .suffix(".diff")
            .tempfile()?
            .into_parts();

        let mut file = io::BufWriter::new(file);

        let mut events = EventReader::open(data_dir, self.thread)?;

        // Since we have previously recorded the complete event stream and we
        // have the index of the current mismatch, we can use that information
        // to pinpoint the mismatch in the generated report.
        let mut count = 0;
        while let Ok(expected) = events.next_debug_event() {
            count += 1;

            match count.cmp(&self.count) {
                cmp::Ordering::Equal => {
                    // The point of desynchronization. This is where things
                    // initially don't match up.
                    writeln!(file, "- {}", expected)?;
                    writeln!(file, "+ {}", self.actual)?;
                }
                cmp::Ordering::Greater => {
                    // Any syscalls that were previously recorded were not
                    // encountered this time, so prefix with (-).
                    writeln!(file, "- {}", expected)?;
                }
                cmp::Ordering::Less => {
                    // Any syscalls before the desynchronization point already
                    // matched.
                    writeln!(file, "  {}", expected)?;
                }
            }
        }

        Ok(temp_path.keep()?)
    }
}
