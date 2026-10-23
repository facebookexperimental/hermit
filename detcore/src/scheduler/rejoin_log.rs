/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The rejoin log: the scheduler turn at which each backgrounded call
//! rejoined the run queue during `hermit record`, so that `hermit replay`
//! readmits it at the same turn.
//!
//! A backgrounded call (`BlockingExternalIO`, or an `rt_sigsuspend` outside the
//! scheduler) runs in the host kernel while guest turns continue. Under record,
//! `step2c_process_io_blockers` readmits it at the first scheduler pass that
//! finds it finished, so the turn depends on host timing. Replay serves many
//! of these calls from the recording, and they finish at a different moment,
//! so readmitting them "when finished" can change the schedule
//! (<https://github.com/rrnewton/hermit/pull/3908>). Record therefore logs
//! each readmission, and replay obeys the log instead of host timing.
//!
//! `Scheduler::turn` advances only on a committed or skipped turn, never while
//! a scheduler pass spins, so it names the same point in both runs as long as
//! the two schedules agree up to it. It does not advance when the scheduler,
//! with nothing to run, moves virtual time to the next timer, so each entry
//! also carries `Scheduler::empty_queue_wakes`, the count of those moves.
//! Record may readmit a call either before or after such a move within one
//! turn, and the pair tells replay which.
//!
//! The file, `rejoins` in the recording directory, is text: a header line, then
//! one line per readmission, in order:
//!
//! ```text
//! hermit-rejoins 1
//! <turn> <wakes> <dettid>:<syscall ordinal> [<dettid>:<syscall ordinal> ...]
//! ```
//!
//! The syscall ordinal is the call's `ExternalOpId::sequence`, so replay can
//! check that the parked call is the one the recording readmitted. Each line
//! is written with one `write` as it happens, and a final line without its
//! newline means the recording was cut short; replay refuses such a log.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;

use crate::types::DetTid;

/// Name of the rejoin log within the recording directory.
pub(crate) const REJOIN_LOG_NAME: &str = "rejoins";

const HEADER: &str = "hermit-rejoins 1";

/// One readmission: the calls `step2c_process_io_blockers` readmitted together,
/// in `DetTid` order, and the point at which it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Rejoin {
    pub(crate) turn: u64,
    /// `Scheduler::empty_queue_wakes` at the readmission.
    pub(crate) wakes: u64,
    /// Each readmitted thread and the `ExternalOpId::sequence` of its call.
    pub(crate) calls: Vec<(DetTid, u64)>,
}

impl Rejoin {
    /// Where the readmission happened; both parts only grow during a run.
    pub(crate) fn point(&self) -> (u64, u64) {
        (self.turn, self.wakes)
    }

    fn to_line(&self) -> String {
        let mut line = format!("{} {}", self.turn, self.wakes);
        for (dtid, sequence) in &self.calls {
            line.push_str(&format!(" {}:{}", dtid.as_raw(), sequence));
        }
        line.push('\n');
        line
    }

    fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split(' ');
        let turn = words
            .next()
            .and_then(|word| word.parse::<u64>().ok())
            .ok_or_else(|| format!("bad turn in {line:?}"))?;
        let wakes = words
            .next()
            .and_then(|word| word.parse::<u64>().ok())
            .ok_or_else(|| format!("bad wake count in {line:?}"))?;
        let mut calls: Vec<(DetTid, u64)> = Vec::new();
        for word in words {
            let call = word
                .split_once(':')
                .and_then(|(dtid, sequence)| {
                    Some((
                        DetTid::from_raw(dtid.parse::<i32>().ok()?),
                        sequence.parse::<u64>().ok()?,
                    ))
                })
                .ok_or_else(|| format!("bad call {word:?} in {line:?}"))?;
            if calls.last().is_some_and(|(last, _)| *last >= call.0) {
                return Err(format!("threads out of order in {line:?}"));
            }
            calls.push(call);
        }
        if calls.is_empty() {
            return Err(format!("no calls in {line:?}"));
        }
        Ok(Rejoin { turn, wakes, calls })
    }
}

/// Record side: appends each readmission to the log as it happens.
#[derive(Debug)]
pub(crate) struct RejoinWriter {
    file: File,
}

impl RejoinWriter {
    /// Create the log in `dir`, header included, so that a recording with no
    /// readmissions still has one.
    pub(crate) fn create(dir: &Path) -> io::Result<Self> {
        let mut file = File::create(dir.join(REJOIN_LOG_NAME))?;
        file.write_all(format!("{HEADER}\n").as_bytes())?;
        Ok(RejoinWriter { file })
    }

    /// Append one readmission with a single write.
    pub(crate) fn append(&mut self, rejoin: &Rejoin) -> io::Result<()> {
        self.file.write_all(rejoin.to_line().as_bytes())
    }
}

/// Replay side: read the log the recording in `dir` wrote.
pub(crate) fn read(dir: &Path) -> Result<VecDeque<Rejoin>, String> {
    let path = dir.join(REJOIN_LOG_NAME);
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    parse(&text)
}

fn parse(text: &str) -> Result<VecDeque<Rejoin>, String> {
    let mut lines = text.split_inclusive('\n');
    if lines.next() != Some(&format!("{HEADER}\n")) {
        return Err(format!("the log does not start with {HEADER:?}"));
    }
    let mut rejoins: VecDeque<Rejoin> = VecDeque::new();
    for line in lines {
        let line = line
            .strip_suffix('\n')
            .ok_or_else(|| format!("the last line, {line:?}, is incomplete"))?;
        let rejoin = Rejoin::parse(line)?;
        if rejoins
            .back()
            .is_some_and(|last| last.point() > rejoin.point())
        {
            return Err(format!("readmissions out of order at {line:?}"));
        }
        rejoins.push_back(rejoin);
    }
    Ok(rejoins)
}

/// Replay could not follow the rejoin log, so it would readmit a backgrounded
/// call at a point the recording did not; the scheduler loop ends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejoinRefusal {
    pub(crate) reason: String,
}

impl std::fmt::Display for RejoinRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hermit replay refused to continue: {}, so a backgrounded call would rejoin \
             the schedule at a different point than in the recording",
            self.reason
        )
    }
}

impl std::error::Error for RejoinRefusal {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejoin(turn: u64, wakes: u64, calls: &[(i32, u64)]) -> Rejoin {
        Rejoin {
            turn,
            wakes,
            calls: calls
                .iter()
                .map(|(dtid, sequence)| (DetTid::from_raw(*dtid), *sequence))
                .collect(),
        }
    }

    #[test]
    fn written_log_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RejoinWriter::create(dir.path()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), VecDeque::new());
        let written = vec![
            rejoin(4, 0, &[(3, 17)]),
            rejoin(4, 1, &[(3, 19), (5, 2)]),
            rejoin(6, 1, &[(5, 3)]),
        ];
        for entry in &written {
            writer.append(entry).unwrap();
        }
        assert_eq!(read(dir.path()).unwrap(), VecDeque::from(written));
    }

    #[test]
    fn malformed_logs_are_refused() {
        let missing = tempfile::tempdir().unwrap();
        assert!(read(missing.path()).is_err());
        for text in [
            "",
            "hermit-rejoins 2\n",
            "hermit-rejoins 1\n7 0 3:4",
            "hermit-rejoins 1\n7 0\n",
            "hermit-rejoins 1\n7 3:4\n",
            "hermit-rejoins 1\n7 0 3\n",
            "hermit-rejoins 1\n7 0 5:1 3:4\n",
            "hermit-rejoins 1\n7 0 3:4\n6 0 3:5\n",
            "hermit-rejoins 1\n7 1 3:4\n7 0 3:5\n",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }
}
