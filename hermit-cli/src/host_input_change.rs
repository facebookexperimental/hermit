/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Name a host file that changed during one verification run.
//!
//! Each run of `hermit run --verify` records the host identity of every file
//! the guest opens, in order, with the open that found it (see
//! `detcore_model::host_input`). A file that a run opens several times shows a
//! PATTERN of identities: each open finds an identity it has found at that
//! path before, or a new one. A file the guest itself creates, rewrites or
//! replaces changes at the same opens in both runs, even though its host inode
//! numbers and times differ between them. So the two runs' patterns agree
//! unless something outside the guest changed a file underneath one run: that
//! run finds a new identity at an open where the other run found a known one.
//!
//! Only one kind of change is named: the file was REPLACED, so the open found
//! a different host inode (`dev`, `ino`). Hermit mints deterministic inodes in
//! the order it first sees host inodes, so a replacement gives that run one
//! more inode and shifts every later deterministic inode number, whatever the
//! guest does with the file. A change of size or time alone is not named.
//!
//! A pattern difference is evidence of a host change only if the runs had not
//! diverged yet. A guest that behaves differently in the two runs can, for
//! example, replace a file in one run only, and then the patterns differ too,
//! but that behaviour shows in the compared log before the open. So the
//! difference names the cause of a divergence only when the comparison
//! compared everything (one strict enough for bitwise parity) and the open
//! that found it is recorded, in BOTH runs' logs, before the first divergent
//! record ([`explains_divergence`]): up to that record both runs did the same
//! things, so the guest did not cause the difference.
//!
//! Positions are records in the log, not times. On a backend whose log is not
//! one stream in time order, such as SaBRe, which appends the records its
//! guest forwards after the coordinator's own, each part is still in time
//! order, and the comparator reports the first difference in the earliest
//! part. An open in a later part than that difference is therefore refused,
//! and an open before it in the same part did happen before it.
//!
//! Nor is a change named when the guest itself rebound that path, or a
//! directory above it, in either run: renamed to or from it, linked or
//! unlinked it, or created something there. Those operations can replace the
//! file without the compared log showing any difference (a rename onto a name
//! that already names the same file succeeds and changes nothing), so a
//! replacement there may be the guest's own.
//!
//! Nothing else is concluded. The walk stops at the first open whose path,
//! thread or syscall differs between the runs and at the first difference that
//! is not a replacement, an incomplete log names nothing, an open whose finish
//! record a log does not show exactly once is not placed, and paths below
//! `/proc`, `/sys` and `/dev` are skipped: those are kernel objects whose
//! metadata is not file content and which Hermit virtualizes separately.

use std::collections::HashMap;
use std::path::Path;

use detcore::detlog::DetLogEvent;
use detcore_model::host_input::HostFileIdentity;
use detcore_model::host_input::HostInputLogEnd;
use detcore_model::host_input::HostInputRecord;
use detcore_model::host_input::HostMutationRecord;

use crate::canonical_verdict::InfrastructureError;
use crate::canonical_verdict::VerificationRun;

/// One run's host-input log.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostInputs {
    /// The records, in the order the run made the opens.
    pub records: Vec<HostInputRecord>,
    /// The paths the guest itself rebound in its file namespace during the
    /// run ([`HostMutationRecord`]).
    pub mutated: Vec<String>,
    /// Whether every line parsed and the log ends with the
    /// [`HostInputLogEnd`] line counting exactly these lines. Detcore writes
    /// the log once, when the run ends, so a run that did not end normally, a
    /// failed write or a cut file leaves it incomplete.
    pub complete: bool,
}

/// Read one run's log. A missing or empty file is an incomplete log.
pub fn read_host_inputs(path: &Path) -> std::io::Result<HostInputs> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_host_inputs(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HostInputs::default()),
        Err(error) => Err(error),
    }
}

fn parse_host_inputs(text: &str) -> HostInputs {
    let mut lines: Vec<&str> = text.lines().collect();
    let end = lines
        .pop()
        .and_then(|line| serde_json::from_str::<HostInputLogEnd>(line).ok());
    let mut records = Vec::new();
    let mut mutated = Vec::new();
    let mut parsed = true;
    for line in &lines {
        if let Ok(record) = serde_json::from_str::<HostInputRecord>(line) {
            records.push(record);
        } else if let Ok(mutation) = serde_json::from_str::<HostMutationRecord>(line) {
            mutated.push(mutation.mutated);
        } else {
            parsed = false;
        }
    }
    let complete = parsed && end.is_some_and(|end| end.records == lines.len() as u64);
    HostInputs {
        records,
        mutated,
        complete,
    }
}

/// Whether the guest may have rebound `path`, or a directory above it, in its
/// file namespace: one of `mutated` is `path` or an ancestor of it. Detcore
/// reports `/`, the ancestor of every absolute path, for a change whose path
/// it could not establish. An open whose path could not be made absolute
/// cannot be matched against those changes at all, so it counts as rebound
/// too.
fn guest_rebound(mutated: &[String], path: &str) -> bool {
    !Path::new(path).is_absolute()
        || mutated
            .iter()
            .any(|rebound| Path::new(path).starts_with(Path::new(rebound)))
}

/// Kernel objects rather than host input files; see the module documentation.
fn is_kernel_object(path: &str) -> bool {
    ["/proc", "/sys", "/dev"].iter().any(|root| {
        path == *root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// The opens one run has made so far: for each path, the distinct
/// identities found there and the identity found at its latest open.
#[derive(Default)]
struct Pattern<'a> {
    seen: HashMap<&'a str, (Vec<HostFileIdentity>, HostFileIdentity)>,
}

impl<'a> Pattern<'a> {
    /// Record an open. Returns whether its identity is one this run has not
    /// found at the path before, and the identity of the path's previous open.
    fn open(&mut self, record: &'a HostInputRecord) -> (bool, Option<HostFileIdentity>) {
        match self.seen.get_mut(record.path.as_str()) {
            None => {
                self.seen.insert(
                    record.path.as_str(),
                    (vec![record.identity], record.identity),
                );
                (true, None)
            }
            Some((distinct, latest)) => {
                let previous = std::mem::replace(latest, record.identity);
                let new = !distinct.contains(&record.identity);
                if new {
                    distinct.push(record.identity);
                }
                (new, Some(previous))
            }
        }
    }
}

/// A pattern difference between the runs: in `run`, the open that thread
/// `dtid` made as its syscall number `syscall` found `path` changed from
/// `before` to `after`, while the other run found a known identity there.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternDifference {
    pub run: VerificationRun,
    pub path: String,
    pub dtid: u64,
    pub syscall: u64,
    pub before: HostFileIdentity,
    pub after: HostFileIdentity,
}

impl PatternDifference {
    /// The typed outcome, for a difference that [`explains_divergence`].
    pub fn into_infrastructure_error(self) -> InfrastructureError {
        InfrastructureError::HostInputChanged {
            run: self.run,
            path: self.path,
            before: self.before,
            after: self.after,
        }
    }
}

/// The first open, in order, at which exactly one run found an identity it
/// had not found at that path before while the other run found one it had,
/// when that identity is a different host inode.
///
/// Returns `None` when either log is incomplete, when the patterns agree up to
/// the first open whose path, thread or syscall number differs, when both
/// runs found an unfamiliar identity at the same open, which no single run
/// explains, or when the first such difference keeps the host inode.
pub fn find_pattern_difference(run1: &HostInputs, run2: &HostInputs) -> Option<PatternDifference> {
    if !run1.complete || !run2.complete {
        return None;
    }
    let mut pattern1 = Pattern::default();
    let mut pattern2 = Pattern::default();
    for (left, right) in run1.records.iter().zip(&run2.records) {
        if (&left.path, left.dtid, left.syscall) != (&right.path, right.dtid, right.syscall) {
            return None;
        }
        if is_kernel_object(&left.path) {
            continue;
        }
        let (new1, previous1) = pattern1.open(left);
        let (new2, previous2) = pattern2.open(right);
        let (run, previous, record) = match (new1, new2) {
            (true, false) => (VerificationRun::Run1, previous1, left),
            (false, true) => (VerificationRun::Run2, previous2, right),
            _ => continue,
        };
        // Both runs find a new identity at a path's first open, so only a
        // path seen before reaches this point.
        let before = previous?;
        if (before.dev, before.ino) == (record.identity.dev, record.identity.ino) {
            return None;
        }
        // The guest may have replaced it itself, through operations whose
        // effects the compared log does not show (a rename onto a name that
        // already names the same file succeeds and changes nothing).
        if [run1, run2]
            .iter()
            .any(|run| guest_rebound(&run.mutated, &record.path))
        {
            return None;
        }
        return Some(PatternDifference {
            run,
            path: record.path.clone(),
            dtid: record.dtid,
            syscall: record.syscall,
            before,
            after: record.identity,
        });
    }
    None
}

/// Where an open sits in a run's log (`contents`): the position, as the
/// comparator counts records (`detcore::logdiff::log_record_events`), of the
/// record on which thread `dtid` finished its syscall number `syscall`, an
/// open. `None` when the log does not show that record exactly once: a log
/// without it says nothing, and a thread number and syscall count can repeat
/// (a thread that execs from a non-leader thread takes the leader's number and
/// keeps its own count), so a repeated one is ambiguous; also `None` for a log
/// the comparator could not read either.
///
/// The open's record is a `syscall_result` whose Hermit-written prefix reads
/// `[syscall][detcore, dtid D] finish syscall #N: ` followed by `openat(`,
/// `open(` or `creat(`, the calls Detcore's openat handler serves. The prefix
/// is read where it first occurs in the record, before any guest-chosen text
/// such as a path.
pub fn open_position(contents: &str, dtid: u64, syscall: u64) -> Option<usize> {
    let mut found = None;
    for (position, human, event) in detcore::logdiff::log_record_events(contents).ok()? {
        if matches!(event, Some(DetLogEvent::SyscallResult { .. }))
            && finished_open(human) == Some((dtid, syscall))
        {
            if found.is_some() {
                return None;
            }
            found = Some(position);
        }
    }
    found
}

/// The thread and that thread's syscall number of a finished open's record,
/// read from the Hermit-written prefix where it first occurs.
fn finished_open(human: &str) -> Option<(u64, u64)> {
    const PREFIX: &str = "[syscall][detcore, dtid ";
    let rest = &human[human.find(PREFIX)? + PREFIX.len()..];
    let (dtid, rest) = rest.split_once("] finish syscall #")?;
    let (syscall, call) = rest.split_once(": ")?;
    let numeric = |digits: &str| {
        (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| digits.parse().ok())
            .flatten()
    };
    ["openat(", "open(", "creat("]
        .iter()
        .any(|name| call.starts_with(name))
        .then_some((numeric(dtid)?, numeric(syscall)?))
}

/// Whether a pattern difference explains a divergence: the comparison was one
/// strict enough for bitwise parity (`complete_comparison`), and the open that
/// found the difference (placed by [`open_position`] in each run's log) is
/// recorded before the first divergent record in both logs
/// (`first_divergent_positions`, the comparator's positions of that record),
/// so the two runs had done the same things up to that open. Without every
/// position nothing is attributed.
pub fn explains_divergence(
    complete_comparison: bool,
    open_positions: [Option<usize>; 2],
    first_divergent_positions: Option<(Option<usize>, Option<usize>)>,
) -> bool {
    let Some((Some(left), Some(right))) = first_divergent_positions else {
        return false;
    };
    complete_comparison
        && matches!(open_positions, [Some(open1), Some(open2)] if open1 < left && open2 < right)
}

#[cfg(test)]
mod tests {
    use detcore::detlog::record_suffix;

    use super::*;

    fn identity(ino: u64) -> HostFileIdentity {
        HostFileIdentity {
            dev: 33,
            ino,
            size: 100,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
        }
    }

    /// A complete log of opens by thread 3, as its syscalls 10, 11, ...
    fn opens(records: &[(&str, u64)]) -> HostInputs {
        HostInputs {
            records: records
                .iter()
                .enumerate()
                .map(|(index, (path, ino))| HostInputRecord {
                    path: (*path).into(),
                    dtid: 3,
                    syscall: index as u64 + 10,
                    identity: identity(*ino),
                })
                .collect(),
            mutated: Vec::new(),
            complete: true,
        }
    }

    /// The sar divergence: two processes open /etc/ld.so.cache, and a package
    /// update replaces it between them in run 1 only.
    #[test]
    fn a_file_replaced_during_run_1_is_named() {
        let run1 = opens(&[
            ("/etc/ld.so.cache", 7),
            ("/lib64/libc.so.6", 9),
            ("/etc/ld.so.cache", 8),
        ]);
        let run2 = opens(&[
            ("/etc/ld.so.cache", 8),
            ("/lib64/libc.so.6", 9),
            ("/etc/ld.so.cache", 8),
        ]);
        assert_eq!(
            find_pattern_difference(&run1, &run2),
            Some(PatternDifference {
                run: VerificationRun::Run1,
                path: "/etc/ld.so.cache".into(),
                dtid: 3,
                syscall: 12,
                before: identity(7),
                after: identity(8),
            })
        );
        let named = find_pattern_difference(&run2, &run1).unwrap();
        assert_eq!(named.run, VerificationRun::Run2);
        assert_eq!(
            named.into_infrastructure_error().to_string(),
            "host input changed during run 2: /etc/ld.so.cache (dev:ino 0:33:7 size 100 mtime 1700000000.000000000 -> dev:ino 0:33:8 size 100 mtime 1700000000.000000000)"
        );
    }

    /// A file the guest creates and replaces itself has new host inodes in
    /// every run, but changes at the same opens in both.
    #[test]
    fn a_file_the_guest_creates_and_replaces_is_not_a_host_change() {
        let run1 = opens(&[("/test/out", 100), ("/test/out", 100), ("/test/out", 101)]);
        let run2 = opens(&[("/test/out", 200), ("/test/out", 200), ("/test/out", 201)]);
        assert_eq!(find_pattern_difference(&run1, &run2), None);
    }

    /// A file rewritten in place by the guest changes size and time at the
    /// same opens in both runs, with different times.
    #[test]
    fn a_file_the_guest_rewrites_is_not_a_host_change() {
        let mut run1 = opens(&[("/test/log", 5), ("/test/log", 5)]);
        let mut run2 = opens(&[("/test/log", 6), ("/test/log", 6)]);
        run1.records[1].identity.size = 200;
        run1.records[1].identity.mtime_sec = 1;
        run2.records[1].identity.size = 200;
        run2.records[1].identity.mtime_sec = 2;
        assert_eq!(find_pattern_difference(&run1, &run2), None);
    }

    /// Identical opens, as in a divergence with another cause.
    #[test]
    fn matching_patterns_name_nothing() {
        let run = opens(&[("/etc/a", 1), ("/etc/b", 2), ("/etc/a", 1)]);
        assert_eq!(find_pattern_difference(&run, &run), None);
    }

    /// Past the first open whose path, thread or syscall number differs the
    /// runs no longer make the same opens, so a later difference is not
    /// evidence.
    #[test]
    fn nothing_is_concluded_after_the_runs_open_differently() {
        let run1 = opens(&[("/etc/a", 1), ("/etc/b", 2), ("/etc/a", 3)]);
        let run2 = opens(&[("/etc/a", 1), ("/etc/c", 2), ("/etc/a", 1)]);
        assert_eq!(find_pattern_difference(&run1, &run2), None);

        let changed = opens(&[("/etc/a", 1), ("/etc/a", 2)]);
        let mut later = opens(&[("/etc/a", 1), ("/etc/a", 1)]);
        assert!(find_pattern_difference(&changed, &later).is_some());
        later.records[1].syscall += 1;
        assert_eq!(find_pattern_difference(&changed, &later), None);
        later.records[1].syscall -= 1;
        later.records[1].dtid += 1;
        assert_eq!(find_pattern_difference(&changed, &later), None);
    }

    /// A record missing from one run would shift its pattern against the
    /// other's, so an incomplete log names nothing.
    #[test]
    fn an_incomplete_log_names_nothing() {
        let run1 = opens(&[("/etc/x", 1), ("/etc/x", 2)]);
        let mut run2 = opens(&[("/etc/x", 1), ("/etc/x", 1)]);
        assert!(find_pattern_difference(&run1, &run2).is_some());
        run2.complete = false;
        assert_eq!(find_pattern_difference(&run1, &run2), None);
        assert_eq!(find_pattern_difference(&run2, &run1), None);
    }

    #[test]
    fn kernel_objects_are_not_host_inputs() {
        let run1 = opens(&[("/proc/stat", 1), ("/proc/stat", 2)]);
        let run2 = opens(&[("/proc/stat", 1), ("/proc/stat", 1)]);
        assert_eq!(find_pattern_difference(&run1, &run2), None);
        assert!(is_kernel_object("/sys/bus/i2c/devices"));
        assert!(is_kernel_object("/dev"));
        assert!(!is_kernel_object("/device/x"));
        assert!(!is_kernel_object("/etc/ld.so.cache"));
    }

    /// A file that changes back: run 1 finds A, B, A and run 2 finds A, A, A.
    /// The change is at the second open.
    #[test]
    fn a_change_and_its_reversal_is_named_at_the_change() {
        let run1 = opens(&[("/etc/x", 1), ("/etc/x", 2), ("/etc/x", 1)]);
        let run2 = opens(&[("/etc/x", 1), ("/etc/x", 1), ("/etc/x", 1)]);
        let difference = find_pattern_difference(&run1, &run2).unwrap();
        assert_eq!(
            (difference.syscall, difference.before, difference.after),
            (11, identity(1), identity(2))
        );
    }

    /// Only a log whose lines all parse and that ends with the count of
    /// exactly its records is complete.
    #[test]
    fn only_a_log_with_its_end_line_is_complete() {
        let line = |ino| serde_json::to_string(&opens(&[("/etc/a", ino)]).records[0]).unwrap();
        let parsed = parse_host_inputs(&format!("{}\n{}\n{{\"records\":2}}\n", line(1), line(2)));
        assert!(parsed.complete);
        assert_eq!(parsed.records.len(), 2);
        let empty_run = parse_host_inputs("{\"records\":0}\n");
        assert!(empty_run.complete);
        assert!(empty_run.records.is_empty());
        for incomplete in [
            format!("{}\n{}\n", line(1), line(2)),
            format!("{}\n{}\n{{\"records\":3}}\n", line(1), line(2)),
            format!("{}\n{{\"records\":2}}\n", line(1)),
            format!(
                "{}\n{{\"path\":\"/etc/b\",\"dtid\"\n{{\"records\":2}}\n",
                line(1)
            ),
            String::new(),
        ] {
            assert!(!parse_host_inputs(&incomplete).complete, "{incomplete}");
        }
        let directory = tempfile::tempdir().unwrap();
        let absent = read_host_inputs(&directory.path().join("absent")).unwrap();
        assert!(!absent.complete);
    }

    /// A log record as Hermit writes it: a wall-clock prefix, then the
    /// record, which is how the comparator separates records.
    fn record(text: &str) -> String {
        format!("2026-10-05T19:00:00.000000Z {text}")
    }

    fn commit(turn: u64) -> String {
        record(&format!(
            "INFO detcore::scheduler: COMMIT turn {turn}{}",
            record_suffix(DetLogEvent::SchedulerCommit {
                scheduler_turn: turn,
                virtual_nanoseconds: turn * 10,
                internal_io_poll: false,
                runtime_maps_read: false,
            })
        ))
    }

    fn finished(dtid: u64, syscall: u64, call: &str) -> String {
        record(&format!(
            "INFO detcore: DETLOG [syscall][detcore, dtid {dtid}] finish syscall #{syscall}: {call} = Ok(3){}",
            record_suffix(DetLogEvent::SyscallResult {
                finished_syscall_number: syscall,
            })
        ))
    }

    fn finish(dtid: u64, syscall: u64) -> String {
        finished(dtid, syscall, "openat(-100, \"/etc/a\", O_RDONLY)")
    }

    /// An open is placed at its own finish record's position, as the
    /// comparator counts records (from 1: the first record follows the empty
    /// text before the first wall-clock prefix).
    #[test]
    fn an_open_is_placed_at_its_finish_record() {
        let log = [commit(4), finish(3, 12), commit(5), finish(5, 12)].join("\n");
        assert_eq!(open_position(&log, 3, 12), Some(2));
        assert_eq!(open_position(&log, 5, 12), Some(4));
        assert_eq!(open_position(&log, 3, 13), None);
        for call in ["open(\"/etc/a\", O_RDONLY)", "creat(\"/etc/a\", 0644)"] {
            let log = [commit(4), finished(3, 12, call)].join("\n");
            assert_eq!(open_position(&log, 3, 12), Some(2), "{call}");
        }
        assert_eq!(open_position("not a Hermit log", 3, 12), None);
    }

    /// Only a structured record of a finished open, read at Hermit's own
    /// prefix and found exactly once, places an open. A thread number and
    /// syscall count that repeat (a non-leader exec takes the leader's number
    /// and keeps its own count) are ambiguous, and a guest path that spells
    /// another syscall's prefix is not that syscall.
    #[test]
    fn only_one_anchored_open_record_places_an_open() {
        let repeated = [commit(4), finish(3, 12), commit(12), finish(3, 12)].join("\n");
        assert_eq!(open_position(&repeated, 3, 12), None);
        let not_an_open = [commit(4), finished(3, 12, "stat(\"/etc/a\")")].join("\n");
        assert_eq!(open_position(&not_an_open, 3, 12), None);
        let prose_only = record(
            "INFO detcore: DETLOG [syscall][detcore, dtid 3] finish syscall #12: openat(..) = Ok(3)",
        );
        assert_eq!(open_position(&prose_only, 3, 12), None);
        // dtid 3's syscall 7 opens a file whose name spells dtid 3's syscall 12.
        let spoofed = finished(
            3,
            7,
            "openat(-100, \"[syscall][detcore, dtid 3] finish syscall #12: openat(\")",
        );
        let log = [commit(2), spoofed, commit(9), finish(3, 12)].join("\n");
        assert_eq!(open_position(&log, 3, 12), Some(4));
        assert_eq!(open_position(&log, 3, 7), Some(2));
    }

    /// A pattern difference explains a divergence only when the comparison
    /// compared everything and its open is recorded, in both runs, before
    /// the first divergent record. A guest that behaved differently first,
    /// such as one that replaced the file in one run only, has diverged by
    /// the open, and is not explained.
    #[test]
    fn only_an_open_before_the_divergence_in_both_runs_explains_it() {
        let divergent = Some((Some(40), Some(42)));
        assert!(explains_divergence(true, [Some(10), Some(10)], divergent));
        assert!(explains_divergence(true, [Some(39), Some(41)], divergent));
        assert!(!explains_divergence(false, [Some(10), Some(10)], divergent));
        assert!(!explains_divergence(true, [Some(40), Some(10)], divergent));
        assert!(!explains_divergence(true, [Some(10), Some(42)], divergent));
        assert!(!explains_divergence(true, [None, Some(10)], divergent));
        assert!(!explains_divergence(true, [Some(10), None], divergent));
        assert!(!explains_divergence(true, [Some(10), Some(10)], None));
        assert!(!explains_divergence(
            true,
            [Some(10), Some(10)],
            Some((None, Some(42)))
        ));
        assert!(!explains_divergence(
            true,
            [Some(10), Some(10)],
            Some((Some(40), None))
        ));
    }

    /// A replacement of a file whose path, or a directory above it, the
    /// guest rebound in either run is not named: the guest may have made it
    /// with operations whose effects the compared log does not show.
    #[test]
    fn a_replacement_where_the_guest_rebound_the_path_is_not_named() {
        let run1 = opens(&[("/data/F", 1), ("/data/F", 2)]);
        let run2 = opens(&[("/data/F", 2), ("/data/F", 2)]);
        assert!(find_pattern_difference(&run1, &run2).is_some());
        for (in_run1, rebound) in [(true, "/data/F"), (false, "/data/F"), (false, "/data")] {
            let (mut run1, mut run2) = (run1.clone(), run2.clone());
            let run = if in_run1 { &mut run1 } else { &mut run2 };
            run.mutated.push(rebound.into());
            assert_eq!(find_pattern_difference(&run1, &run2), None, "{rebound}");
        }
        let mut unrelated = run1.clone();
        unrelated.mutated = vec!["/data/F.tmp".into(), "/tmp".into(), "/data/Fx".into()];
        assert!(find_pattern_difference(&unrelated, &run2).is_some());
        assert!(guest_rebound(&["/".into()], "/etc/ld.so.cache"));
        assert!(!guest_rebound(&["/etc/ld.so".into()], "/etc/ld.so.cache"));
        // An open recorded under a path that could not be made absolute
        // (through a directory descriptor with no known path) cannot be
        // matched against the guest's changes, so it is never named, with or
        // without them.
        assert!(guest_rebound(&[], "F"));
        assert!(guest_rebound(&["/".into()], "F"));
        let relative1 = opens(&[("F", 1), ("F", 2)]);
        let relative2 = opens(&[("F", 2), ("F", 2)]);
        assert_eq!(find_pattern_difference(&relative1, &relative2), None);
    }

    /// A log's mutation lines count toward its end line like its records.
    #[test]
    fn a_log_with_mutation_lines_is_complete_when_its_end_counts_them() {
        let record = serde_json::to_string(&opens(&[("/etc/a", 1)]).records[0]).unwrap();
        let mutation = serde_json::to_string(&HostMutationRecord {
            mutated: "/data/F".into(),
        })
        .unwrap();
        let parsed = parse_host_inputs(&format!("{record}\n{mutation}\n{{\"records\":2}}\n"));
        assert!(parsed.complete);
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.mutated, vec!["/data/F".to_string()]);
        assert!(!parse_host_inputs(&format!("{record}\n{mutation}\n{{\"records\":1}}\n")).complete);
    }

    /// A change of size or time that keeps the host inode is not the
    /// replacement whose new inode shifts Hermit's numbering, so it is not
    /// named, and the walk concludes nothing past it.
    #[test]
    fn a_change_that_keeps_the_host_inode_is_not_named() {
        let mut touched = opens(&[("/etc/x", 1), ("/etc/x", 1), ("/etc/y", 5), ("/etc/y", 6)]);
        touched.records[1].identity.mtime_sec += 1;
        let steady = opens(&[("/etc/x", 1), ("/etc/x", 1), ("/etc/y", 5), ("/etc/y", 5)]);
        assert_eq!(find_pattern_difference(&touched, &steady), None);
        touched.records[1].identity.mtime_sec -= 1;
        assert_eq!(
            find_pattern_difference(&touched, &steady).unwrap().path,
            "/etc/y"
        );
    }
}
