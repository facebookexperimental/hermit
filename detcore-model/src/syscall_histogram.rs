/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Per-run syscall histograms: how many times the guest made each syscall,
//! and what Detcore did with it.
//!
//! Detcore counts every syscall entry by number, at the same accounting point
//! as the run summary's scalar `syscalls` count, and at the end of its handler
//! records one [`SyscallOutcome`]. Each thread keeps its own
//! [`SyscallHistogram`] and hands its nonzero slots
//! ([`SparseSyscallHistogram`]) to the scheduler with its other final
//! statistics when it deregisters, so a run's histogram covers exactly the
//! calls of the `syscalls` total. The run summary reports the merged histogram
//! as a [`SyscallHistogramSummary`]: sparse, ordered by x86-64 syscall number,
//! each entry carrying the pinned syscall name, together with the
//! [`SyscallObservationScope`] that says whether the accounting point saw
//! every syscall the guest made.
//!
//! This is the producer the compatibility-website plan asks for
//! (dev-hermit `ai_docs/compatibility-website-vision-and-implementation-plan-20260830.md`,
//! "Syscall totals" and Stage 3).
//!
//! Counting reads no guest state and changes nothing the guest can observe.

use reverie_syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

/// What Detcore did with one completed syscall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallOutcome {
    /// Detcore produced the result without handing any syscall to the backend.
    Detcore,
    /// Detcore handed at least one syscall to the backend (the host kernel, or
    /// the backend's own Linux provider) while handling this one.
    Backend,
    /// The guest received `ENOSYS`, or a Detcore refusal policy answered the
    /// call (a fixed-errno refusal, or a backend's typed refusal).
    Refused,
}

/// Counts for one syscall.
///
/// `calls` counts entries. The three outcomes count completed calls, so
/// `calls` minus their sum ([`SyscallOutcomes::unfinished`]) is the number of
/// calls that never returned to Detcore's handler (`exit`, `exit_group`, a
/// successful `execve`, a thread killed during the call) or whose backend does
/// not count injections ([`reverie::Guest::injected_syscalls`]).
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyscallOutcomes {
    /// Entries into Detcore's syscall handler.
    pub calls: u64,
    /// Completed with [`SyscallOutcome::Detcore`].
    pub detcore: u64,
    /// Completed with [`SyscallOutcome::Backend`].
    pub backend: u64,
    /// Completed with [`SyscallOutcome::Refused`].
    pub refused: u64,
}

impl SyscallOutcomes {
    /// Nothing counted.
    pub const ZERO: Self = Self {
        calls: 0,
        detcore: 0,
        backend: 0,
        refused: 0,
    };

    /// Add another count of the same syscall to this one.
    pub fn merge(&mut self, other: &SyscallOutcomes) {
        self.calls += other.calls;
        self.detcore += other.detcore;
        self.backend += other.backend;
        self.refused += other.refused;
    }

    /// Calls entered but given no outcome.
    pub fn unfinished(&self) -> u64 {
        self.calls
            .saturating_sub(self.detcore + self.backend + self.refused)
    }
}

/// Why the accounting point did not see every syscall the guest made.
#[derive(
    Debug,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord
)]
#[serde(rename_all = "snake_case")]
pub enum SyscallObservationGap {
    /// `--passthru-opt`: syscalls outside the optimized subscription bypass
    /// Detcore.
    PassthruOpt,
    /// The backend's runtime starts inside the guest process after the
    /// guest's dynamic loader has run, so the loader's syscalls are not
    /// counted (LiteInst, in-guest-trap, SaBRe).
    InProcessRuntimeStart,
    /// `--untrap-passthrough-syscalls`: the pass-through syscalls it leaves
    /// untrapped run with no stop and are not counted.
    UntrappedPassthrough,
}

/// Whether a histogram counts every syscall the guest made.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyscallObservationScope {
    /// Every syscall the guest made reached the accounting point.
    Complete,
    /// Some syscalls bypassed it, for these reasons; the histogram is a lower
    /// bound and must not be read as complete usage.
    Partial { gaps: Vec<SyscallObservationGap> },
}

impl SyscallObservationScope {
    /// The scope given the known gaps: complete when there are none.
    pub fn from_gaps(gaps: impl IntoIterator<Item = SyscallObservationGap>) -> Self {
        let mut gaps: Vec<_> = gaps.into_iter().collect();
        gaps.sort();
        gaps.dedup();
        if gaps.is_empty() {
            Self::Complete
        } else {
            Self::Partial { gaps }
        }
    }

    /// Whether every syscall was counted.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// One syscall's counts in a [`SyscallHistogramSummary`].
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SyscallHistogramEntry {
    /// The syscall number in the summary's architecture.
    pub nr: i32,
    /// The pinned syscall table's name for `nr`, or `syscall_<nr>` for a
    /// number the table does not name.
    pub name: String,
    /// Entries and outcomes.
    #[serde(flatten)]
    pub counts: SyscallOutcomes,
}

/// A run's syscall histogram as the run summary reports it.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct SyscallHistogramSummary {
    /// The syscall table the numbers belong to; always `x86_64` today.
    pub architecture: String,
    /// Whether the counts cover every syscall the guest made.
    pub observation_scope: SyscallObservationScope,
    /// Every counted syscall, by ascending number; syscalls never made are
    /// absent.
    pub syscalls: Vec<SyscallHistogramEntry>,
}

impl SyscallHistogramSummary {
    /// The architecture this producer counts in.
    pub const ARCHITECTURE: &'static str = "x86_64";

    /// Total entries over every syscall: equal to the run summary's
    /// `syscalls` for a consistent summary.
    pub fn total_calls(&self) -> u64 {
        self.syscalls.iter().map(|entry| entry.counts.calls).sum()
    }

    /// The entry for the syscall named `name`, if it was counted.
    pub fn get(&self, name: &str) -> Option<&SyscallHistogramEntry> {
        self.syscalls.iter().find(|entry| entry.name == name)
    }

    /// The compact machine form of this histogram.
    pub fn compact(&self) -> CompactSyscallHistogram {
        CompactSyscallHistogram {
            architecture: self.architecture.clone(),
            observation_scope: self.observation_scope.clone(),
            counts: self
                .syscalls
                .iter()
                .map(|entry| {
                    let SyscallOutcomes {
                        calls,
                        detcore,
                        backend,
                        refused,
                    } = entry.counts;
                    [
                        u64::from(entry.nr.unsigned_abs()),
                        calls,
                        detcore,
                        backend,
                        refused,
                    ]
                })
                .collect(),
        }
    }
}

/// A run's syscall histogram in the compact form machine records keep: the
/// verification report's `runtime.run1`/`run2`, and from it each cell's
/// result row and the durable series row (run 1 only). Names are left out:
/// a reader takes them from the pinned table for `architecture`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompactSyscallHistogram {
    /// The syscall table the numbers belong to; always `x86_64` today.
    pub architecture: String,
    /// Whether the counts cover every syscall the guest made.
    pub observation_scope: SyscallObservationScope,
    /// `[nr, calls, detcore, backend, refused]` for each counted syscall, by
    /// ascending number ([`SyscallOutcomes`] names the four counts).
    pub counts: Vec<[u64; 5]>,
}

impl CompactSyscallHistogram {
    /// Total entries over every syscall.
    pub fn total_calls(&self) -> u64 {
        self.counts.iter().map(|[_, calls, ..]| calls).sum()
    }

    /// Why this histogram is malformed, if it is: an architecture other than
    /// `x86_64`, a number outside the pinned table, numbers out of ascending
    /// order or repeated, an entry with no calls, or more outcomes than
    /// calls.
    pub fn malformation(&self) -> Option<String> {
        if self.architecture != SyscallHistogramSummary::ARCHITECTURE {
            return Some(format!(
                "syscall histogram architecture is {:?}, expected {:?}",
                self.architecture,
                SyscallHistogramSummary::ARCHITECTURE
            ));
        }
        let mut previous = None;
        for [nr, calls, detcore, backend, refused] in &self.counts {
            let in_table = usize::try_from(*nr)
                .ok()
                .and_then(|nr| nr.checked_sub(Sysno::first().id() as usize))
                .is_some_and(|slot| slot < SYSCALL_TABLE_SIZE);
            if !in_table {
                return Some(format!("syscall {nr} is outside the x86_64 table"));
            }
            if previous.is_some_and(|previous| previous >= *nr) {
                return Some(format!("syscall {nr} is out of ascending order"));
            }
            previous = Some(*nr);
            if *calls == 0 {
                return Some(format!("syscall {nr} has an entry with no calls"));
            }
            let outcomes = detcore
                .checked_add(*backend)
                .and_then(|sum| sum.checked_add(*refused));
            if outcomes.is_none_or(|outcomes| outcomes > *calls) {
                return Some(format!(
                    "syscall {nr} has more outcomes than its {calls} calls"
                ));
            }
        }
        None
    }
}

/// Slots in a [`SyscallHistogram`]: one per number of the pinned x86-64
/// syscall table, gaps included.
pub const SYSCALL_TABLE_SIZE: usize = Sysno::table_size();

/// A histogram indexed by x86-64 syscall number, as Detcore collects it: a
/// dense array with one [`SyscallOutcomes`] per table slot, allocated once
/// when the thread's state is created, so counting a syscall is two array
/// increments with no allocation and no lookup. A thread that exits hands
/// over only its nonzero slots ([`SparseSyscallHistogram`]), and the run's
/// total sums those back into one dense histogram.
///
/// It serializes sparsely, as `(number, counts)` pairs for the nonzero slots in
/// ascending order, so the thread-exit message and any saved thread state stay
/// small.
#[derive(Clone, PartialEq, Eq)]
pub struct SyscallHistogram {
    counts: Box<[SyscallOutcomes; SYSCALL_TABLE_SIZE]>,
}

impl Default for SyscallHistogram {
    fn default() -> Self {
        Self {
            counts: Box::new([SyscallOutcomes::ZERO; SYSCALL_TABLE_SIZE]),
        }
    }
}

/// The slot of `sysno`: every `Sysno` is in the table.
fn slot(sysno: Sysno) -> usize {
    (sysno.id() - Sysno::first().id()) as usize
}

impl SyscallHistogram {
    /// Count one entry of `sysno`.
    pub fn enter(&mut self, sysno: Sysno) {
        self.counts[slot(sysno)].calls += 1;
    }

    /// Record the outcome of one completed call of `sysno`.
    pub fn finish(&mut self, sysno: Sysno, outcome: SyscallOutcome) {
        let counts = &mut self.counts[slot(sysno)];
        match outcome {
            SyscallOutcome::Detcore => counts.detcore += 1,
            SyscallOutcome::Backend => counts.backend += 1,
            SyscallOutcome::Refused => counts.refused += 1,
        }
    }

    /// Add every count of `other` to this histogram.
    pub fn merge(&mut self, other: &SyscallHistogram) {
        for (mine, theirs) in self.counts.iter_mut().zip(other.counts.iter()) {
            mine.merge(theirs);
        }
    }

    /// Total entries over every syscall.
    pub fn total_calls(&self) -> u64 {
        self.counts.iter().map(|counts| counts.calls).sum()
    }

    /// Whether nothing has been counted.
    pub fn is_empty(&self) -> bool {
        self.nonzero().next().is_none()
    }

    /// The counts of one syscall, if it was counted.
    pub fn get(&self, sysno: Sysno) -> Option<&SyscallOutcomes> {
        Some(&self.counts[slot(sysno)]).filter(|counts| **counts != SyscallOutcomes::ZERO)
    }

    /// The nonzero slots as `(number, counts)`, by ascending number.
    fn nonzero(&self) -> impl Iterator<Item = (i32, &SyscallOutcomes)> {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, counts)| **counts != SyscallOutcomes::ZERO)
            .map(|(index, counts)| (Sysno::first().id() + index as i32, counts))
    }

    /// The run-summary form of this histogram, with its observation scope:
    /// only the nonzero slots. A number without a name in the x86-64 table is
    /// reported as `syscall_<number>`.
    pub fn summary(&self, observation_scope: SyscallObservationScope) -> SyscallHistogramSummary {
        SyscallHistogramSummary {
            architecture: SyscallHistogramSummary::ARCHITECTURE.to_owned(),
            observation_scope,
            syscalls: self
                .nonzero()
                .map(|(nr, counts)| SyscallHistogramEntry {
                    nr,
                    name: syscall_name(nr),
                    counts: *counts,
                })
                .collect(),
        }
    }
}

impl std::fmt::Debug for SyscallHistogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.nonzero()).finish()
    }
}

impl Serialize for SyscallHistogram {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Collected first: a length-prefixed format (the bincode RPC) needs
        // the count before the elements.
        let pairs: Vec<_> = self.nonzero().collect();
        pairs.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SyscallHistogram {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let pairs = Vec::<(i32, SyscallOutcomes)>::deserialize(deserializer)?;
        let mut histogram = Self::default();
        let mut previous = None;
        for (nr, counts) in pairs {
            let index = nr
                .checked_sub(Sysno::first().id())
                .and_then(|index| usize::try_from(index).ok())
                .filter(|index| *index < SYSCALL_TABLE_SIZE)
                .ok_or_else(|| {
                    serde::de::Error::custom(format!("syscall {nr} is outside the table"))
                })?;
            if previous.is_some_and(|previous| previous >= nr) {
                return Err(serde::de::Error::custom(format!(
                    "syscall {nr} is out of ascending order"
                )));
            }
            previous = Some(nr);
            histogram.counts[index] = counts;
        }
        Ok(histogram)
    }
}

/// A finished thread's histogram: only its nonzero slots, `(number, counts)`
/// by ascending number.
///
/// A thread counts into a dense [`SyscallHistogram`], which costs a fixed
/// 14,784 bytes; a thread that has exited reports this form instead, so the
/// scheduler keeps a few hundred bytes per finished thread rather than the
/// whole array (a 4,000-thread run kept 59 MB of dense arrays). The run's
/// total is summed back into one dense histogram
/// ([`SparseSyscallHistogram::add_to`]).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct SparseSyscallHistogram {
    entries: Vec<(i32, SyscallOutcomes)>,
}

impl From<&SyscallHistogram> for SparseSyscallHistogram {
    fn from(histogram: &SyscallHistogram) -> Self {
        Self {
            entries: histogram
                .nonzero()
                .map(|(nr, counts)| (nr, *counts))
                .collect(),
        }
    }
}

impl SparseSyscallHistogram {
    /// Add every count of `other` to this histogram, keeping number order.
    pub fn merge(&mut self, other: &SparseSyscallHistogram) {
        let mut merged = Vec::with_capacity(self.entries.len() + other.entries.len());
        let (mut mine, mut theirs) = (
            self.entries.iter().peekable(),
            other.entries.iter().peekable(),
        );
        loop {
            let next = match (mine.peek(), theirs.peek()) {
                (Some((left, _)), Some((right, _))) if left < right => *mine.next().unwrap(),
                (Some((left, _)), Some((right, _))) if right < left => *theirs.next().unwrap(),
                (Some(_), Some(_)) => {
                    let (nr, mut counts) = *mine.next().unwrap();
                    counts.merge(&theirs.next().unwrap().1);
                    (nr, counts)
                }
                (Some(_), None) => *mine.next().unwrap(),
                (None, Some(_)) => *theirs.next().unwrap(),
                (None, None) => break,
            };
            merged.push(next);
        }
        self.entries = merged;
    }

    /// Add every count of this histogram to the dense `total`. A number
    /// outside the pinned table, which no [`SyscallHistogram`] can produce,
    /// is skipped.
    pub fn add_to(&self, total: &mut SyscallHistogram) {
        for (nr, counts) in &self.entries {
            if let Some(slot) = nr
                .checked_sub(Sysno::first().id())
                .and_then(|slot| usize::try_from(slot).ok())
                .filter(|slot| *slot < SYSCALL_TABLE_SIZE)
            {
                total.counts[slot].merge(counts);
            }
        }
    }

    /// Total entries over every syscall.
    pub fn total_calls(&self) -> u64 {
        self.entries.iter().map(|(_, counts)| counts.calls).sum()
    }
}

/// Every syscall the pinned x86-64 table names, as `(number, name)` by
/// ascending number. A coverage report takes its never-exercised list from
/// this, so it names exactly the syscalls a histogram can count.
pub fn pinned_syscall_table() -> Vec<(i32, &'static str)> {
    (0..SYSCALL_TABLE_SIZE)
        .filter_map(|slot| Sysno::new(slot + Sysno::first().id() as usize))
        .map(|sysno| (sysno.id(), sysno.name()))
        .collect()
}

fn syscall_name(nr: i32) -> String {
    usize::try_from(nr)
        .ok()
        .and_then(Sysno::new)
        .map_or_else(|| format!("syscall_{nr}"), |sysno| sysno.name().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_and_outcomes_are_counted_separately() {
        let mut histogram = SyscallHistogram::default();
        histogram.enter(Sysno::read);
        histogram.finish(Sysno::read, SyscallOutcome::Backend);
        histogram.enter(Sysno::read);
        histogram.finish(Sysno::read, SyscallOutcome::Refused);
        histogram.enter(Sysno::getpid);
        histogram.finish(Sysno::getpid, SyscallOutcome::Detcore);
        histogram.enter(Sysno::exit_group);

        assert_eq!(histogram.total_calls(), 4);
        assert_eq!(
            histogram.get(Sysno::read),
            Some(&SyscallOutcomes {
                calls: 2,
                detcore: 0,
                backend: 1,
                refused: 1,
            })
        );
        let exit_group = histogram.get(Sysno::exit_group).unwrap();
        assert_eq!(exit_group.calls, 1);
        assert_eq!(exit_group.unfinished(), 1);
        assert_eq!(histogram.get(Sysno::read).unwrap().unfinished(), 0);
        assert_eq!(histogram.get(Sysno::write), None);
    }

    #[test]
    fn merge_adds_every_count() {
        let mut left = SyscallHistogram::default();
        left.enter(Sysno::read);
        left.finish(Sysno::read, SyscallOutcome::Backend);
        let mut right = SyscallHistogram::default();
        right.enter(Sysno::read);
        right.finish(Sysno::read, SyscallOutcome::Detcore);
        right.enter(Sysno::write);
        left.merge(&right);

        assert_eq!(left.total_calls(), 3);
        let read = left.get(Sysno::read).unwrap();
        assert_eq!((read.calls, read.detcore, read.backend), (2, 1, 1));
        assert_eq!(left.get(Sysno::write).unwrap().calls, 1);

        let mut empty = SyscallHistogram::default();
        assert!(empty.is_empty());
        empty.merge(&left);
        assert_eq!(empty, left);
    }

    #[test]
    fn the_summary_is_ordered_by_number_and_carries_names_and_scope() {
        let mut histogram = SyscallHistogram::default();
        histogram.enter(Sysno::write);
        histogram.enter(Sysno::read);
        histogram.finish(Sysno::read, SyscallOutcome::Backend);
        let summary = histogram.summary(SyscallObservationScope::Complete);
        let json = serde_json::to_string(&summary).unwrap();
        assert_eq!(
            json,
            r#"{"architecture":"x86_64","observation_scope":{"kind":"complete"},"syscalls":[{"nr":0,"name":"read","calls":1,"detcore":0,"backend":1,"refused":0},{"nr":1,"name":"write","calls":1,"detcore":0,"backend":0,"refused":0}]}"#
        );
        assert_eq!(
            serde_json::from_str::<SyscallHistogramSummary>(&json).unwrap(),
            summary
        );
        assert_eq!(summary.total_calls(), 2);
        assert_eq!(summary.get("write").unwrap().nr, 1);
        assert_eq!(syscall_name(-1), "syscall_-1");
        assert_eq!(syscall_name(100_000), "syscall_100000");
    }

    #[test]
    fn a_scope_with_gaps_is_partial_and_sorted() {
        assert_eq!(
            SyscallObservationScope::from_gaps([]),
            SyscallObservationScope::Complete
        );
        let partial = SyscallObservationScope::from_gaps([
            SyscallObservationGap::InProcessRuntimeStart,
            SyscallObservationGap::PassthruOpt,
            SyscallObservationGap::InProcessRuntimeStart,
        ]);
        assert!(!partial.is_complete());
        assert_eq!(
            serde_json::to_string(&partial).unwrap(),
            r#"{"kind":"partial","gaps":["passthru_opt","in_process_runtime_start"]}"#
        );
        let untrapped =
            SyscallObservationScope::from_gaps([SyscallObservationGap::UntrappedPassthrough]);
        assert!(!untrapped.is_complete());
        assert_eq!(
            serde_json::to_string(&untrapped).unwrap(),
            r#"{"kind":"partial","gaps":["untrapped_passthrough"]}"#
        );
    }

    #[test]
    fn every_table_number_has_a_slot_and_the_wire_form_is_sparse() {
        let mut histogram = SyscallHistogram::default();
        // Every number the table names, gaps skipped.
        for sysno in
            (0..SYSCALL_TABLE_SIZE).filter_map(|nr| Sysno::new(nr + Sysno::first().id() as usize))
        {
            histogram.enter(sysno);
        }
        assert_eq!(histogram.total_calls(), Sysno::count() as u64);
        assert_eq!(histogram.get(Sysno::last()).unwrap().calls, 1);

        let mut sparse = SyscallHistogram::default();
        sparse.enter(Sysno::write);
        sparse.finish(Sysno::write, SyscallOutcome::Detcore);
        assert_eq!(
            serde_json::to_string(&sparse).unwrap(),
            r#"[[1,{"calls":1,"detcore":1,"backend":0,"refused":0}]]"#
        );
        let out_of_table = format!(
            r#"[[{SYSCALL_TABLE_SIZE},{{"calls":1,"detcore":0,"backend":0,"refused":0}}]]"#
        );
        assert!(serde_json::from_str::<SyscallHistogram>(&out_of_table).is_err());
        let unordered = r#"[[1,{"calls":1,"detcore":0,"backend":0,"refused":0}],[0,{"calls":1,"detcore":0,"backend":0,"refused":0}]]"#;
        assert!(serde_json::from_str::<SyscallHistogram>(unordered).is_err());
    }

    #[test]
    fn the_pinned_table_names_every_countable_syscall_once() {
        let table = pinned_syscall_table();
        assert_eq!(table.len(), Sysno::count());
        assert_eq!(table.first(), Some(&(0, "read")));
        assert!(table.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let names: std::collections::BTreeSet<_> = table.iter().map(|(_, name)| name).collect();
        assert_eq!(names.len(), table.len());
    }

    #[test]
    fn the_compact_form_keeps_numbers_counts_and_scope() {
        let mut histogram = SyscallHistogram::default();
        histogram.enter(Sysno::openat);
        histogram.finish(Sysno::openat, SyscallOutcome::Backend);
        histogram.enter(Sysno::read);
        histogram.enter(Sysno::read);
        histogram.finish(Sysno::read, SyscallOutcome::Detcore);
        let scope = SyscallObservationScope::from_gaps([SyscallObservationGap::PassthruOpt]);
        let compact = histogram.summary(scope).compact();
        assert_eq!(
            serde_json::to_string(&compact).unwrap(),
            r#"{"architecture":"x86_64","observation_scope":{"kind":"partial","gaps":["passthru_opt"]},"counts":[[0,2,1,0,0],[257,1,0,1,0]]}"#
        );
        assert_eq!(compact.total_calls(), 3);
        assert_eq!(compact.malformation(), None);
    }

    #[test]
    fn a_malformed_compact_histogram_names_its_fault() {
        let good = CompactSyscallHistogram {
            architecture: "x86_64".into(),
            observation_scope: SyscallObservationScope::Complete,
            counts: vec![[0, 2, 1, 1, 0], [1, 1, 0, 0, 0]],
        };
        assert_eq!(good.malformation(), None);
        let faults = [
            (
                CompactSyscallHistogram {
                    architecture: "aarch64".into(),
                    ..good.clone()
                },
                "architecture",
            ),
            (
                CompactSyscallHistogram {
                    counts: vec![[SYSCALL_TABLE_SIZE as u64, 1, 0, 0, 0]],
                    ..good.clone()
                },
                "outside",
            ),
            (
                CompactSyscallHistogram {
                    counts: vec![[1, 1, 0, 0, 0], [1, 1, 0, 0, 0]],
                    ..good.clone()
                },
                "ascending",
            ),
            (
                CompactSyscallHistogram {
                    counts: vec![[1, 0, 0, 0, 0]],
                    ..good.clone()
                },
                "no calls",
            ),
            (
                CompactSyscallHistogram {
                    counts: vec![[1, 1, 1, 1, 0]],
                    ..good.clone()
                },
                "more outcomes",
            ),
        ];
        for (histogram, fault) in faults {
            let message = histogram.malformation().expect("malformed");
            assert!(message.contains(fault), "{message}");
        }
        let unknown = r#"{"architecture":"x86_64","observation_scope":{"kind":"complete"},"counts":[],"names":[]}"#;
        assert!(serde_json::from_str::<CompactSyscallHistogram>(unknown).is_err());
    }

    #[test]
    fn the_finished_form_keeps_only_nonzero_slots_and_sums_back_exactly() {
        let mut first = SyscallHistogram::default();
        first.enter(Sysno::read);
        first.finish(Sysno::read, SyscallOutcome::Backend);
        first.enter(Sysno::exit);
        let mut second = SyscallHistogram::default();
        second.enter(Sysno::read);
        second.finish(Sysno::read, SyscallOutcome::Detcore);
        second.enter(Sysno::getpid);
        second.enter(Sysno::exit);

        let mut sparse = SparseSyscallHistogram::from(&first);
        assert_eq!(sparse.entries.len(), 2);
        sparse.merge(&SparseSyscallHistogram::from(&second));
        assert_eq!(
            sparse.entries.iter().map(|(nr, _)| *nr).collect::<Vec<_>>(),
            [Sysno::read.id(), Sysno::getpid.id(), Sysno::exit.id()]
        );
        assert_eq!(sparse.total_calls(), 5);

        let mut dense = first.clone();
        dense.merge(&second);
        let mut total = SyscallHistogram::default();
        sparse.add_to(&mut total);
        assert_eq!(total, dense);
        // Merging with nothing on either side changes nothing.
        let mut empty = SparseSyscallHistogram::default();
        empty.merge(&sparse);
        assert_eq!(empty, sparse);
        empty.merge(&SparseSyscallHistogram::default());
        assert_eq!(empty, sparse);
    }

    #[test]
    fn the_per_thread_form_round_trips_through_bincode() {
        let mut histogram = SyscallHistogram::default();
        histogram.enter(Sysno::openat);
        histogram.finish(Sysno::openat, SyscallOutcome::Refused);
        let config = bincode::config::legacy();
        let bytes = bincode::serde::encode_to_vec(&histogram, config).unwrap();
        let (decoded, _): (SyscallHistogram, usize) =
            bincode::serde::decode_from_slice(&bytes, config).unwrap();
        assert_eq!(decoded, histogram);
    }
}
