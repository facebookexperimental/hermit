/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Store-exclusive sequences on aarch64: detection only.
//!
//! An aarch64 load-exclusive/store-exclusive loop (`LDAXR` .. `STLXR`, then
//! `CBNZ` back) runs once more whenever the local exclusive monitor is cleared
//! between its load and its store, and any exception clears it: a host
//! interrupt, a PMU overflow, a ptrace stop, a single step. Each retry adds
//! retired branches, which Detcore counts to place preemptions and to advance
//! virtual time, so a program that runs such loops can be preempted at other
//! points in two runs, and a recorded schedule may not replay. LSE atomics
//! (`LDADD`, `CAS`, `SWP`; Armv8.1-A) are single instructions and never retry.
//!
//! This module reports and changes nothing a guest sees:
//!
//! - The exclusive pairs in each executable file mapping, counted after exec
//!   and after an `mmap` or `mprotect` that maps `PROT_EXEC` from a file. The
//!   count is a function of the file's bytes, so it goes to INFO and to the
//!   run summary's INFO view.
//! - In a run that records or replays preemptions, the store-exclusives the
//!   guest's tasks failed (`STREX_FAIL_SPEC`) and, where the core counts
//!   them, executed (`STREX_SPEC`), counted from the start of the root
//!   task's first image on every task it creates and read once at the end of
//!   the run. Host interrupts decide them, so they never reach INFO or the
//!   INFO view: only the full report, the summary JSON, the preemption
//!   recording and that run's warnings carry them.
//!
//! The decoder and the model are pure and compile everywhere; the rest runs
//! only on aarch64.

#[cfg_attr(
    not(target_arch = "aarch64"),
    allow(
        dead_code,
        reason = "only aarch64 scans its mappings; the decoder's tests run on every architecture"
    )
)]
mod decode {
    /// An instruction of the load/store exclusive class.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Exclusive {
        /// `LDXR`, `LDAXR`, `LDXP`, `LDAXP` and their byte and halfword forms.
        Load,
        /// `STXR`, `STLXR`, `STXP`, `STLXP` and their byte and halfword forms.
        Store,
    }

    /// Bits 29:24 (`001000`) and 23 (`o2`) of the load/store exclusive class.
    /// `o2 = 1` is load-acquire/store-release (`LDAR`, `STLR`) or `CAS`,
    /// neither of which can fail.
    const CLASS_MASK: u32 = 0x3F80_0000;
    const CLASS: u32 = 0x0800_0000;
    /// `L`: a load.
    const LOAD: u32 = 1 << 22;
    /// `o1`: a register pair.
    const PAIR: u32 = 1 << 21;
    /// The high size bit, which every exclusive pair sets. With `o1 = 1` and
    /// this bit clear the encoding is `CASP` (Armv8.1-A), a single
    /// instruction.
    const PAIR_SIZE: u32 = 1 << 31;

    /// Whether `insn` is a load-exclusive, a store-exclusive, or neither.
    pub(crate) const fn classify(insn: u32) -> Option<Exclusive> {
        if insn & CLASS_MASK != CLASS || (insn & PAIR != 0 && insn & PAIR_SIZE == 0) {
            None
        } else if insn & LOAD != 0 {
            Some(Exclusive::Load)
        } else {
            Some(Exclusive::Store)
        }
    }

    /// A store-exclusive is one of a pair when a load-exclusive is at most
    /// this many instructions before it.
    pub(crate) const PAIR_WINDOW: u64 = 16;

    /// `MRS <Xt>, <system register>` with the system register fixed and only
    /// `Rt` (bits 4:0) free.
    const MRS_MASK: u32 = !0x1F;
    /// The virtual counter, read from EL0: `MRS Xt, CNTVCT_EL0` and its
    /// self-synchronized form `CNTVCTSS_EL0`. Linux lets every task read it
    /// directly (or, on CPUs with a counter erratum, traps and emulates the
    /// read in the kernel), so the guest gets the host's count with no stop
    /// Hermit sees and nothing Detcore can virtualize. It is this
    /// architecture's `RDTSC`, without `PR_SET_TSC` to trap it.
    ///
    /// Not counted: `CNTPCT_EL0`, `CNTPCTSS_EL0` and the timer registers
    /// (`CNTV_TVAL_EL0` and the like), whose EL0 access Linux disables, so a
    /// read faults with `SIGILL`, which is visible and deterministic; and
    /// `CNTFRQ_EL0`, a constant frequency rather than a time.
    const COUNTER_READS: [u32; 2] = [0xD53B_E040, 0xD53B_E0C0];

    /// Whether `insn` reads the virtual counter from EL0.
    pub(crate) fn is_counter_read(insn: u32) -> bool {
        COUNTER_READS.contains(&(insn & MRS_MASK))
    }

    /// What one pass over a code range finds: the exclusive pairs, whose
    /// retries make a branch count depend on host interrupts, and the EL0
    /// counter reads, which bypass virtual time outright.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct CodeScan {
        index: u64,
        last_load: Option<u64>,
        pairs: u64,
        counter_reads: u64,
    }

    impl CodeScan {
        /// Take the next instruction word.
        pub(crate) fn push(&mut self, insn: u32) {
            match classify(insn) {
                Some(Exclusive::Load) => self.last_load = Some(self.index),
                Some(Exclusive::Store)
                    if self
                        .last_load
                        .is_some_and(|load| self.index - load <= PAIR_WINDOW) =>
                {
                    self.pairs += 1
                }
                _ => self.counter_reads += u64::from(is_counter_read(insn)),
            }
            self.index += 1;
        }

        /// Take little-endian instruction words, as aarch64 stores code in
        /// either data endianness. A trailing partial word is ignored, so a
        /// stream split into several calls must split at multiples of four.
        pub(crate) fn push_bytes(&mut self, bytes: &[u8]) {
            for word in bytes.as_chunks::<4>().0 {
                self.push(u32::from_le_bytes(*word));
            }
        }

        /// The pairs counted so far.
        pub(crate) fn pairs(&self) -> u64 {
            self.pairs
        }

        /// The EL0 counter reads counted so far.
        pub(crate) fn counter_reads(&self) -> u64 {
            self.counter_reads
        }
    }

    /// Where an ELF64 little-endian file's section header table is: its
    /// offset and entry count, from the 64-byte file header. `None` for any
    /// other file, or one without section headers (or more than fit in
    /// `e_shnum`), whose mapped bytes are then scanned whole.
    pub(crate) fn elf_section_table(header: &[u8; 64]) -> Option<(u64, usize)> {
        const SECTION_HEADER_SIZE: u16 = 64;
        let half = |at: usize| u16::from_le_bytes([header[at], header[at + 1]]);
        let offset = u64::from_le_bytes(header[0x28..0x30].try_into().unwrap());
        let (entry_size, count) = (half(0x3A), half(0x3C));
        // `\x7fELF`, ELFCLASS64, ELFDATA2LSB.
        (header[..6] == *b"\x7fELF\x02\x01"
            && offset != 0
            && count != 0
            && entry_size == SECTION_HEADER_SIZE)
            .then_some((offset, usize::from(count)))
    }

    /// The file ranges (offset, size) of the sections that hold instructions
    /// (`SHF_EXECINSTR`, with file contents) in an ELF64 little-endian
    /// section header table. Only these are scanned, so read-only data that
    /// shares the executable segment cannot pass for an exclusive pair.
    pub(crate) fn executable_sections(table: &[u8]) -> Vec<(u64, u64)> {
        const SHT_NOBITS: u32 = 8;
        const SHF_EXECINSTR: u64 = 0x4;
        table
            .as_chunks::<64>()
            .0
            .iter()
            .filter_map(|entry| {
                let word = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap());
                let kind = u32::from_le_bytes(entry[4..8].try_into().unwrap());
                (kind != SHT_NOBITS && word(8) & SHF_EXECINSTR != 0 && word(32) != 0)
                    .then(|| (word(24), word(32)))
            })
            .collect()
    }

    /// One pass over `words`.
    #[cfg(test)]
    pub(crate) fn scan_words(words: impl IntoIterator<Item = u32>) -> CodeScan {
        let mut scan = CodeScan::default();
        words.into_iter().for_each(|word| scan.push(word));
        scan
    }

    /// The exclusive pairs in `words`.
    #[cfg(test)]
    pub(crate) fn count_pairs(words: impl IntoIterator<Item = u32>) -> u64 {
        scan_words(words).pairs()
    }
}

#[cfg_attr(
    not(target_arch = "aarch64"),
    allow(
        dead_code,
        reason = "only aarch64 counts store-exclusives; the model's tests run on every architecture"
    )
)]
mod model {
    use std::collections::BTreeMap;
    use std::path::Path;

    /// An Arm CPU PMU, as `/sys/bus/event_source/devices/<name>` describes
    /// it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct CpuPmu {
        pub(crate) name: String,
        /// The `perf_event_attr.type` that selects this PMU.
        pub(crate) kind: u32,
        /// The CPUs it counts on.
        pub(crate) cpus: Vec<usize>,
    }

    /// Parse a kernel CPU list such as `0-3,8,10-11`.
    pub(crate) fn parse_cpu_list(list: &str) -> Option<Vec<usize>> {
        let mut cpus = Vec::new();
        for part in list.trim().split(',').filter(|part| !part.is_empty()) {
            match part.split_once('-') {
                Some((first, last)) => {
                    let (first, last): (usize, usize) = (first.parse().ok()?, last.parse().ok()?);
                    if first > last {
                        return None;
                    }
                    cpus.extend(first..=last);
                }
                None => cpus.push(part.parse().ok()?),
            }
        }
        Some(cpus)
    }

    /// The Arm CPU PMUs among the entries `names` of `devices`
    /// (`/sys/bus/event_source/devices`): each named `arm*` with a `type` and
    /// a `cpus` file. A host with one kind of core has one; a host with
    /// several kinds (big.LITTLE) has one per kind, and each counts only on
    /// its own CPUs. Sorted by name.
    pub(crate) fn arm_cpu_pmus(
        devices: &Path,
        names: impl IntoIterator<Item = String>,
    ) -> Vec<CpuPmu> {
        let mut pmus: Vec<CpuPmu> = names
            .into_iter()
            .filter(|name| name.starts_with("arm"))
            .filter_map(|name| {
                let read = |file| std::fs::read_to_string(devices.join(&name).join(file)).ok();
                let kind = read("type")?.trim().parse().ok()?;
                let cpus = parse_cpu_list(&read("cpus")?)?;
                Some(CpuPmu { name, kind, cpus })
            })
            .collect();
        pmus.sort_by(|a, b| a.name.cmp(&b.name));
        pmus
    }

    /// The PMUs that count on at least one of `cpus`, each with the first
    /// such CPU.
    pub(crate) fn covering(pmus: &[CpuPmu], cpus: &[usize]) -> Vec<(CpuPmu, usize)> {
        pmus.iter()
            .filter_map(|pmu| {
                let cpu = pmu.cpus.iter().find(|cpu| cpus.contains(cpu))?;
                Some((pmu.clone(), *cpu))
            })
            .collect()
    }

    /// One event's reads on each PMU, as (value, time enabled, time running),
    /// combined: the sum of the values and whether the event was on a counter
    /// for the whole run, or `None` when it was enabled but never on a
    /// counter (a core that accepts the event and does not count it). A
    /// task-bound event on one PMU of several is enabled whenever the task
    /// runs, but runs only while the task is on that PMU's CPUs, so the PMUs'
    /// times running add up to the time enabled when nothing was multiplexed.
    pub(crate) fn combine(reads: &[(u64, u64, u64)]) -> Option<(u64, bool)> {
        let value = reads.iter().map(|read| read.0).sum();
        let enabled = reads.iter().map(|read| read.1).max().unwrap_or(0);
        let running: u64 = reads.iter().map(|read| read.2).sum();
        if enabled > 0 && running == 0 {
            return None;
        }
        Some((value, running >= enabled))
    }

    /// What a scan of one mapping found.
    #[derive(
        Clone,
        Copy,
        Debug,
        Default,
        PartialEq,
        Eq,
        serde::Serialize,
        serde::Deserialize
    )]
    pub struct Found {
        /// Exclusive pairs: a branch count that depends on host interrupts.
        pub pairs: u64,
        /// EL0 generic-timer counter reads: time the guest sees that virtual
        /// time does not control.
        pub counter_reads: u64,
    }

    /// What a scan of one mapping found, with the mapping. It crosses to the
    /// global tool in a `Report`, which is reachable from the crate's public
    /// RPC types, so it serializes and is nominally `pub`.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ScannedMapping {
        /// The guest path of the mapped file.
        pub path: String,
        /// The mapped range of that file.
        pub offset: u64,
        pub len: u64,
        pub found: Found,
    }

    /// One address space's counts for one file, by mapped file range
    /// (offset, length).
    type RangeCounts = BTreeMap<(u64, u64), Found>;

    /// What a run's scans found, by guest path, address space and mapped
    /// file range.
    #[derive(Debug, Default)]
    pub(crate) struct PairTally {
        found: BTreeMap<String, BTreeMap<(String, u64), RangeCounts>>,
    }

    impl PairTally {
        /// Take one scan of address space `space`. A mapping replaces the
        /// ranges of the same file it overlaps, so a mapping split and
        /// rejoined by mprotect counts once.
        pub(crate) fn note(&mut self, space: &(String, u64), mappings: &[ScannedMapping]) {
            for mapping in mappings {
                let ranges = self
                    .found
                    .entry(mapping.path.clone())
                    .or_default()
                    .entry(space.clone())
                    .or_default();
                let end = mapping.offset.saturating_add(mapping.len);
                ranges.retain(|&(from, length), _| {
                    end <= from || from.saturating_add(length) <= mapping.offset
                });
                ranges.insert((mapping.offset, mapping.len), mapping.found);
            }
        }

        /// The exclusive pairs of each guest path that has any.
        pub(crate) fn pairs_per_path(&self) -> BTreeMap<String, u64> {
            self.per_path(|found| found.pairs)
        }

        /// The EL0 counter reads of each guest path that has any.
        pub(crate) fn counter_reads_per_path(&self) -> BTreeMap<String, u64> {
            self.per_path(|found| found.counter_reads)
        }

        /// One count of each guest path that has a nonzero one: the sum over
        /// the ranges one address space mapped, and the largest such sum over
        /// address spaces, so that a library many processes map counts once.
        /// A path is tallied when its mapping has either kind of finding, so
        /// the zero of the other kind is left out here.
        fn per_path(&self, of: impl Fn(&Found) -> u64) -> BTreeMap<String, u64> {
            self.found
                .iter()
                .map(|(path, spaces)| {
                    let most = spaces
                        .values()
                        .map(|ranges| ranges.values().map(&of).sum::<u64>())
                        .max()
                        .unwrap_or(0);
                    (path.clone(), most)
                })
                .filter(|(_, count)| *count != 0)
                .collect()
        }
    }
}

#[cfg(target_arch = "aarch64")]
pub(crate) use runtime::*;

#[cfg(target_arch = "aarch64")]
mod runtime {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::ffi::CString;
    use std::fs::File;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;
    use std::sync::Mutex;

    use detcore_model::summary::RunSummary;
    use detcore_model::summary::StoreExclusiveCounts;
    use procfs::process::MMPermissions;
    use reverie::Guest;
    use reverie::Pid;
    use reverie::syscalls::MapFlags;
    use reverie::syscalls::ProtFlags;
    use reverie::syscalls::Syscall;
    use serde::Deserialize;
    use serde::Serialize;
    use tracing::debug;
    use tracing::info;
    use tracing::warn;

    use super::decode::CodeScan;
    use super::decode::elf_section_table;
    use super::decode::executable_sections;
    use super::model::CpuPmu;
    use super::model::Found;
    use super::model::PairTally;
    use super::model::ScannedMapping;
    use super::model::arm_cpu_pmus;
    use super::model::combine;
    use super::model::covering;
    use crate::Config;
    use crate::Detcore;
    use crate::RecordOrReplay;
    use crate::procmaps::MMapPath;
    use crate::procmaps::MemoryMap;
    use crate::types::MmId;

    /// Whether this run reads its guests through host procfs: only where a
    /// guest thread is the host task of the same ID and Detcore runs outside
    /// it (ptrace), and not under `hermit record` or `hermit replay`, whose
    /// replayed guest maps placeholders.
    fn reads_guest_tasks(cfg: &Config) -> bool {
        cfg.backend.host_task_reads_reach_guest_threads
            && !cfg.backend.tool_shares_guest_descriptor_table
            && !cfg.recordreplay_modes
    }

    /// Whether this run records or replays preemptions.
    fn records_or_replays(cfg: &Config) -> bool {
        cfg.record_preemptions
            || cfg.record_preemptions_to.is_some()
            || cfg.replay_preemptions_from.is_some()
    }

    /// Whether this run counts store-exclusives: one that records or replays
    /// preemptions, whose divergence a count can explain. Other runs keep
    /// their PMU counters to themselves.
    fn counts_store_exclusives(cfg: &Config) -> bool {
        reads_guest_tasks(cfg) && records_or_replays(cfg)
    }

    /// `count` and `noun`, plural unless the count is 1.
    fn counted(count: u64, noun: &str) -> String {
        if count == 1 {
            format!("1 {noun}")
        } else {
            format!("{count} {noun}s")
        }
    }

    // ---------------------------------------------------------------------
    // The run's tally, kept by the global state.

    /// A report from a guest thread's Detcore to the run's global state. It
    /// carries no time and changes no scheduler state (see
    /// `GlobalState::receive_rpc`).
    #[derive(PartialEq, Debug, Eq, Clone, Serialize, Deserialize)]
    pub enum Report {
        /// The root guest task, stopped at the start of its first image:
        /// open the run's counters on it.
        Attach {
            /// Its host thread ID.
            root: i32,
        },
        /// One scan's exclusive pairs.
        Pairs {
            /// The scanned address space (see [`ScannedRanges::space`]).
            space: (String, u64),
            /// What the scan found in each mapping that has anything.
            mappings: Vec<ScannedMapping>,
        },
    }

    /// Say once per run and file that it reads the virtual counter from EL0.
    /// Unlike an exclusive pair, which only makes a branch count
    /// host-dependent, this hands the guest a host time Detcore never sees.
    fn warn_counter_reads(path: &str, reads: u64) {
        warn!(
            "[store-exclusive] {path} reads the generic timer's virtual counter from EL0 ({}). \
             Those reads make no stop Hermit sees, so Detcore cannot virtualize them: a guest \
             whose control flow depends on one is not deterministic, and a recorded schedule \
             may not replay. Reading the counter directly is this architecture's RDTSC, and \
             Linux has no per-process switch to trap it.",
            counted(reads, "CNTVCT_EL0 or CNTVCTSS_EL0 read")
        );
    }

    /// A run that executed at least this many store-exclusives warns even
    /// when none failed, where the core counts executed ones (`STREX_SPEC`).
    /// glibc's start-up executes a few in every process (2 with glibc 2.34),
    /// and at the failure rate measured under Hermit on Neoverse V2, about 1
    /// in 12,000, a thousand give a replay roughly a 1 in 12 chance to retry
    /// one.
    const WARN_EXECUTED: u64 = 1000;

    /// What a run learned of its store-exclusives, for its summary, its
    /// preemption recording and its warnings.
    #[derive(Debug, Default)]
    pub(crate) struct RunTally {
        pairs: PairTally,
        /// The run's counters, opened when the root guest task starts its
        /// first image.
        counters: Option<Result<Counters, String>>,
        /// Their values, read once at the end of the run.
        counts: Option<Result<StoreExclusiveCounts, String>>,
        /// What the end-of-run read found uncounted, to warn about.
        uncounted: Vec<String>,
        /// The guest paths this run has warned read the counter from EL0.
        warned_counter_reads: BTreeSet<String>,
    }

    impl RunTally {
        /// Take one report.
        pub(crate) fn note(&mut self, cfg: &Config, report: &Report) {
            match report {
                Report::Attach { root } => {
                    if counts_store_exclusives(cfg) && self.counters.is_none() {
                        self.counters = Some(Counters::open(*root));
                    }
                }
                Report::Pairs { space, mappings } => {
                    for mapping in mappings {
                        if mapping.found.counter_reads != 0
                            && self.warned_counter_reads.insert(mapping.path.clone())
                        {
                            warn_counter_reads(&mapping.path, mapping.found.counter_reads);
                        }
                    }
                    self.pairs.note(space, mappings);
                }
            }
        }

        /// The run's counts, read the first time they are asked for, which is
        /// at the end of the run, once every guest task has exited.
        fn counts(&mut self) -> Option<&Result<StoreExclusiveCounts, String>> {
            if self.counts.is_none()
                && let Some(counters) = self.counters.take()
            {
                self.counts =
                    Some(counters.and_then(|counters| counters.read(&mut self.uncounted)));
            }
            self.counts.as_ref()
        }

        /// The failed store-exclusives, for the preemption recording.
        pub(crate) fn failures_for_recording(&mut self) -> u64 {
            match self.counts() {
                Some(Ok(counts)) => counts.failed,
                _ => 0,
            }
        }

        /// Fill the summary: the per-path pairs (INFO view and full report),
        /// and the counts or why there are none (full report only).
        pub(crate) fn fill_summary(&mut self, summary: &mut RunSummary) {
            summary.exclusive_pairs = self.pairs.pairs_per_path();
            summary.counter_reads = self.pairs.counter_reads_per_path();
            match self.counts() {
                Some(Ok(counts)) => summary.store_exclusives = Some(*counts),
                Some(Err(reason)) => summary.store_exclusives_unavailable = Some(reason.clone()),
                None => {}
            }
        }

        /// The end-of-run warnings of a run that records or replays
        /// preemptions: a counter the core did not count, and, when any
        /// store-exclusive failed, or enough executed that one could fail in
        /// a replay, the libraries with exclusive pairs and how to remove
        /// them.
        pub(crate) fn warnings(&mut self, cfg: &Config) -> Vec<String> {
            if !records_or_replays(cfg) {
                return Vec::new();
            }
            let counts = self.counts().cloned();
            let mut warnings = self.uncounted.clone();
            let Some(Ok(counts)) = counts else {
                return warnings;
            };
            if counts.failed == 0
                && counts
                    .executed
                    .is_none_or(|executed| executed < WARN_EXECUTED)
            {
                return warnings;
            }
            let what = match counts.executed {
                Some(executed) => format!(
                    "this run executed {}, and {} failed",
                    counted(executed, "store-exclusive"),
                    counts.failed
                ),
                None => format!(
                    "{} failed in this run",
                    counted(counts.failed, "store-exclusive")
                ),
            };
            let partial = if counts.fully_counted {
                ""
            } else {
                "; not counted for the whole run (multiplexed, or the task ran on CPUs whose PMU \
                 does not carry these events)"
            };
            let libraries: Vec<String> = self
                .pairs
                .pairs_per_path()
                .iter()
                .filter(|(_, pairs)| **pairs > 0)
                .map(|(path, pairs)| format!("{path} ({pairs})"))
                .collect();
            let sites = if libraries.is_empty() {
                "No executable file mapping has any, so they ran in other code".to_owned()
            } else {
                format!("Exclusive pairs are in {}", libraries.join(", "))
            };
            warnings.push(format!(
                "{what}{partial}. A store-exclusive that fails retries its load/store-exclusive \
                 loop and adds retired branches, so recorded preemptions may not replay. {sites}. \
                 Build with -march=armv8.1-a or later, or with -moutline-atomics, which takes the \
                 LSE path on cores with LSE such as Neoverse V2 and Cortex-A78"
            ));
            warnings
        }
    }

    /// The warning for a replay whose recording counted failures, given at
    /// replay start so that a divergence is labelled before it happens.
    pub(crate) fn warn_at_replay_start(recorded_failures: u64) {
        if recorded_failures > 0 {
            warn!(
                "the recording being replayed counted {}; replay may diverge (each retried \
                 store-exclusive adds retired branches)",
                counted(recorded_failures, "store-exclusive failure")
            );
        }
    }

    // ---------------------------------------------------------------------
    // Counters.

    /// `STREX_FAIL_SPEC` and `STREX_SPEC`: the store-exclusives speculatively
    /// executed that failed, and all those speculatively executed. Both are
    /// IMPLEMENTATION DEFINED Armv8 PMU events (Neoverse V2 implements both,
    /// Cortex-A78 at least the first), and Linux accepts a raw event number
    /// whether or not the core counts it, so [`probe`] checks each on every
    /// PMU. `STREX_FAIL_SPEC` is required; `STREX_SPEC` is used where every
    /// PMU counts it.
    const STREX_FAIL_SPEC: u64 = 0x6E;
    const STREX_SPEC: u64 = 0x6F;
    const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 1 << 3;
    /// `perf_event_attr` flag bits.
    const INHERIT: u64 = 1 << 1;
    const EXCLUDE_KERNEL: u64 = 1 << 5;
    const EXCLUDE_HV: u64 = 1 << 6;
    /// `read_format` bits: a read also returns the time the event was
    /// enabled and the time it was on a counter, which differ when the PMU
    /// multiplexed it or the task ran on another PMU's CPUs.
    const TOTAL_TIME_ENABLED: u64 = 1 << 0;
    const TOTAL_TIME_RUNNING: u64 = 1 << 1;
    /// Where the kernel lists its PMUs.
    const DEVICES: &str = "/sys/bus/event_source/devices";

    /// The first 64 bytes of `struct perf_event_attr` (`PERF_ATTR_SIZE_VER0`),
    /// which is all a counting event needs; the kernel zero-extends a short
    /// attribute.
    #[repr(C)]
    #[derive(Default)]
    struct PerfEventAttr {
        type_: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }

    /// Open a user-space counting event (not sampled) of `pmu` on task `pid`,
    /// or on the calling thread for 0, inherited by the tasks it creates when
    /// `inherit` is set. Naming the PMU's own type binds the event to that
    /// PMU, where `PERF_TYPE_RAW` would bind it to the first one that accepts
    /// it.
    fn open_event(pmu: &CpuPmu, config: u64, pid: i32, inherit: bool) -> std::io::Result<OwnedFd> {
        let mut attr = PerfEventAttr {
            type_: pmu.kind,
            size: std::mem::size_of::<PerfEventAttr>() as u32,
            config,
            read_format: TOTAL_TIME_ENABLED | TOTAL_TIME_RUNNING,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV | if inherit { INHERIT } else { 0 },
            ..Default::default()
        };
        // SAFETY: `attr` is a valid `perf_event_attr` prefix whose `size`
        // field gives its length, and it outlives the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &mut attr as *mut PerfEventAttr,
                pid,
                -1,
                -1,
                PERF_FLAG_FD_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: perf_event_open returned a new descriptor nothing else owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
    }

    /// An event's value, time enabled and time running.
    fn read_event(event: &OwnedFd) -> std::io::Result<(u64, u64, u64)> {
        let mut values = [0u64; 3];
        // SAFETY: reads at most 24 bytes into a buffer of 24.
        let read = unsafe { libc::read(event.as_raw_fd(), values.as_mut_ptr().cast(), 24) };
        match read {
            24 => Ok((values[0], values[1], values[2])),
            read if read < 0 => Err(std::io::Error::last_os_error()),
            _ => Err(std::io::Error::other("short read of a perf event")),
        }
    }

    /// The CPUs task `pid` may run on.
    fn affinity(pid: i32) -> std::io::Result<Vec<usize>> {
        // SAFETY: an all-zero `cpu_set_t` is an empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: `set` is a `cpu_set_t` of the size passed.
        if unsafe { libc::sched_getaffinity(pid, std::mem::size_of_val(&set), &mut set) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: every CPU number is below `CPU_SETSIZE`.
        Ok((0..libc::CPU_SETSIZE as usize)
            .filter(|cpu| unsafe { libc::CPU_ISSET(*cpu, &set) })
            .collect())
    }

    /// Run the calling thread on `cpus` only.
    fn set_affinity(cpus: &[usize]) -> std::io::Result<()> {
        // SAFETY: an all-zero `cpu_set_t` is an empty set.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for &cpu in cpus {
            // SAFETY: the caller's CPU numbers came from the kernel's CPU list.
            unsafe { libc::CPU_SET(cpu, &mut set) };
        }
        // SAFETY: `set` is a `cpu_set_t` of the size passed.
        if unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Run the calling thread on `cpu` only.
    fn pin(cpu: usize) -> std::io::Result<()> {
        set_affinity(&[cpu])
    }

    /// What a PMU counted of the probe's store-exclusives.
    struct Probe {
        /// `STREX_FAIL_SPEC`'s increase over [`PROBE_LOOPS`] failing ones.
        failed: u64,
        /// How many of those the CPU itself reported failed.
        observed_failed: u64,
        /// `STREX_SPEC`'s increase over all [`PROBE_LOOPS`] x 2, if it opens.
        executed: Option<u64>,
    }

    const PROBE_LOOPS: u64 = 1000;

    /// Check what `pmu` counts, on the calling thread of this (tracer)
    /// process pinned to `cpu`, one of its CPUs, and given back its own CPUs
    /// after: [`PROBE_LOOPS`] store-exclusives that fail (`LDXR`; `CLREX`,
    /// which clears the exclusive monitor; `STXR`), then as many exclusive
    /// increments that succeed.
    ///
    /// Not on a new thread: the tracer runs in the guest's PID namespace, so
    /// a thread it starts takes the next PID there, and every guest thread
    /// started after it would get another ID than in a run that does not
    /// probe. Runs that record preemptions (`--record-preemptions-to`) or
    /// replay them (`--replay-preemptions-from`) probe; a replay of such a
    /// run's schedule (`--replay-schedule-from`) does not, so it would lose
    /// its threads at their first clone.
    fn probe(pmu: &CpuPmu, cpu: usize) -> Result<Probe, String> {
        /// Gives the calling thread back its CPUs when the probe ends, also
        /// when it unwinds.
        struct Restore(Vec<usize>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Err(error) = set_affinity(&self.0) {
                    warn!(
                        "[store-exclusive] cannot give the tracer thread back its CPUs after \
                         the probe, so it stays on one CPU: {error}"
                    );
                }
            }
        }
        let own = affinity(0)
            .map_err(|error| format!("cannot read the tracer thread's CPU affinity: {error}"))?;
        let _restore = Restore(own);
        probe_on(pmu, cpu)
    }

    fn probe_on(pmu: &CpuPmu, cpu: usize) -> Result<Probe, String> {
        pin(cpu).map_err(|error| {
            format!(
                "cannot run the store-exclusive probe on CPU {cpu} of PMU {}: {error}",
                pmu.name
            )
        })?;
        let failed = open_event(pmu, STREX_FAIL_SPEC, 0, false).map_err(|error| {
            format!(
                "perf_event_open of STREX_FAIL_SPEC (raw 0x6e) on PMU {} failed: {error}",
                pmu.name
            )
        })?;
        let executed = open_event(pmu, STREX_SPEC, 0, false).ok();
        let value = |event: &OwnedFd| read_event(event).map(|read| read.0).unwrap_or(0);
        let (failed_before, executed_before) = (value(&failed), executed.as_ref().map(value));
        let mut word = 0u64;
        let mut observed_failed = 0u64;
        for _ in 0..PROBE_LOOPS {
            let status: u64;
            // SAFETY: an exclusive load and store of a local word.
            unsafe {
                core::arch::asm!(
                    "ldxr {value}, [{word}]",
                    "clrex",
                    "stxr {status:w}, {value}, [{word}]",
                    word = in(reg) &raw mut word,
                    value = out(reg) _,
                    status = out(reg) status,
                    options(nostack),
                );
            }
            observed_failed += status & 1;
        }
        for _ in 0..PROBE_LOOPS {
            // SAFETY: an exclusive increment of a local word.
            unsafe {
                core::arch::asm!(
                    "2:",
                    "ldxr {value}, [{word}]",
                    "add {value}, {value}, #1",
                    "stxr {status:w}, {value}, [{word}]",
                    "cbnz {status:w}, 2b",
                    word = in(reg) &raw mut word,
                    value = out(reg) _,
                    status = out(reg) _,
                    options(nostack),
                );
            }
        }
        std::hint::black_box(word);
        Ok(Probe {
            failed: value(&failed).saturating_sub(failed_before),
            observed_failed,
            executed: executed
                .as_ref()
                .zip(executed_before)
                .map(|(event, before)| value(event).saturating_sub(before)),
        })
    }

    /// One PMU's events on the root guest task, inherited by every task it
    /// creates. A task's counts fold into these when it exits.
    #[derive(Debug)]
    struct PmuEvents {
        failed: OwnedFd,
        executed: Option<OwnedFd>,
    }

    /// The run's counters: the events of every PMU that counts on a CPU the
    /// guest may run on.
    #[derive(Debug)]
    struct Counters(Vec<PmuEvents>);

    impl Counters {
        fn open(root: i32) -> Result<Self, String> {
            let cpus = affinity(root)
                .map_err(|error| format!("cannot read the guest's CPU affinity: {error}"))?;
            let mut names = Vec::new();
            crate::util::find_in_directory(Path::new(DEVICES), |name| {
                names.push(name.to_string_lossy().into_owned());
                None::<()>
            })
            .map_err(|error| format!("cannot list the PMUs in {DEVICES}: {error}"))?;
            let pmus = arm_cpu_pmus(Path::new(DEVICES), names);
            let covering = covering(&pmus, &cpus);
            if covering.is_empty() {
                return Err(format!(
                    "no Arm CPU PMU in {DEVICES} counts on the guest's CPUs"
                ));
            }
            let mut count_executed = true;
            for (pmu, cpu) in &covering {
                let probe = probe(pmu, *cpu)?;
                let executed = probe
                    .executed
                    .map_or("does not open".to_owned(), |executed| {
                        format!("counted {executed} of {}", 2 * PROBE_LOOPS)
                    });
                debug!(
                    "[store-exclusive] probe of PMU {} on CPU {cpu}: STREX_FAIL_SPEC counted {} of \
                     {PROBE_LOOPS} failing store-exclusives (the CPU reported {} failed); \
                     STREX_SPEC {executed}",
                    pmu.name, probe.failed, probe.observed_failed
                );
                if probe.failed == 0 {
                    return Err(format!(
                        "store-exclusive counting unavailable on this core: STREX_FAIL_SPEC (raw \
                         0x6e) on PMU {} counted none of {PROBE_LOOPS} failing store-exclusives \
                         on CPU {cpu}",
                        pmu.name
                    ));
                }
                count_executed &= probe.executed.is_some_and(|executed| executed > 0);
            }
            let open = |pmu: &CpuPmu, config, name| {
                open_event(pmu, config, root, true).map_err(|error| {
                    format!(
                        "perf_event_open of {name} on PMU {} for the guest failed: {error}",
                        pmu.name
                    )
                })
            };
            covering
                .iter()
                .map(|(pmu, _)| {
                    Ok(PmuEvents {
                        failed: open(pmu, STREX_FAIL_SPEC, "STREX_FAIL_SPEC")?,
                        executed: if count_executed {
                            Some(open(pmu, STREX_SPEC, "STREX_SPEC")?)
                        } else {
                            None
                        },
                    })
                })
                .collect::<Result<_, String>>()
                .map(Self)
        }

        /// Read the counts. A counter that was enabled but never on a
        /// counter reports nothing, and adds a warning to `uncounted`.
        fn read(self, uncounted: &mut Vec<String>) -> Result<StoreExclusiveCounts, String> {
            let reads = |events: Vec<&OwnedFd>, name: &str| {
                events
                    .into_iter()
                    .map(read_event)
                    .collect::<std::io::Result<Vec<_>>>()
                    .map_err(|error| format!("reading {name} failed: {error}"))
            };
            let never = |name: &str| {
                format!(
                    "{name} was enabled for the run but never on a counter: this core accepts \
                     the event and does not count it"
                )
            };
            let failed = reads(
                self.0.iter().map(|pmu| &pmu.failed).collect(),
                "STREX_FAIL_SPEC",
            )?;
            let Some((failed, failed_whole)) = combine(&failed) else {
                let reason = never("STREX_FAIL_SPEC (raw 0x6e)");
                uncounted.push(format!("store-exclusives were not counted: {reason}"));
                return Err(reason);
            };
            let executed: Option<Vec<&OwnedFd>> =
                self.0.iter().map(|pmu| pmu.executed.as_ref()).collect();
            let executed = match executed {
                Some(events) => match combine(&reads(events, "STREX_SPEC")?) {
                    Some(combined) => Some(combined),
                    None => {
                        uncounted.push(format!(
                            "executed store-exclusives are not reported: {}",
                            never("STREX_SPEC (raw 0x6f)")
                        ));
                        None
                    }
                },
                None => None,
            };
            Ok(StoreExclusiveCounts {
                failed,
                executed: executed.map(|(executed, _)| executed),
                fully_counted: failed_whole && executed.is_none_or(|(_, whole)| whole),
            })
        }
    }

    // ---------------------------------------------------------------------
    // Static scan, in each guest thread's Detcore.

    /// The file-backed address ranges of one address space that this module
    /// has seen, and whether each was scanned, kept in its
    /// `crate::memory::MemoryMetadata` (so a fork copies it and an exec
    /// starts afresh), with the space's identity for the run's tally.
    #[derive(Clone, Debug, Default)]
    pub(crate) struct ScannedRanges {
        /// Unknown until a scan reads the address space's maps.
        ranges: Option<Vec<(u64, u64, bool)>>,
        /// The address space this record was made for, and the pedigree of
        /// the thread that scanned it first.
        owner: Option<(MmId, String)>,
    }

    impl ScannedRanges {
        /// Whether `range` may hold a file mapping not scanned yet: the
        /// ranges are unknown, or it overlaps one recorded unscanned.
        fn needs_scan(&self, (start, end): (u64, u64)) -> bool {
            self.ranges.as_ref().is_none_or(|ranges| {
                ranges
                    .iter()
                    .any(|&(from, to, scanned)| !scanned && from < end && start < to)
            })
        }

        /// A file mapping made without `PROT_EXEC`, once the ranges are known.
        fn add_unscanned(&mut self, (start, end): (u64, u64)) {
            if let Some(ranges) = &mut self.ranges {
                ranges.push((start, end, false));
            }
        }

        /// The file-backed mappings that a maps read lists. An executable one
        /// counts as scanned: every call that made it executable scanned it.
        fn reseed(&mut self, maps: &[MemoryMap]) {
            self.ranges = Some(
                maps.iter()
                    .map(|map| {
                        (
                            map.address.0,
                            map.address.1,
                            map.perms.contains(MMPermissions::EXECUTE),
                        )
                    })
                    .collect(),
            );
        }

        /// The identity of address space `mm` for the run's tally: the
        /// pedigree of the first thread that scanned it, which no other
        /// thread of the run has, and its exec generation. A fork's copy of
        /// this record names the parent's space, so it is replaced.
        fn space(&mut self, mm: MmId, scanner: impl FnOnce() -> String) -> (String, u64) {
            let pedigree = match &self.owner {
                Some((owner, pedigree)) if *owner == mm => pedigree.clone(),
                _ => {
                    let pedigree = scanner();
                    self.owner = Some((mm, pedigree.clone()));
                    pedigree
                }
            };
            (pedigree, mm.generation())
        }
    }

    /// A scanned file range, as `/proc/<pid>/maps` names it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct FileRange {
        dev: (i32, i32),
        inode: u64,
        offset: u64,
        len: u64,
    }

    /// A scan result. A count stands while the file keeps the size and
    /// modification time it was taken at; a failure stands for good, so that
    /// each mapping of the file is reported the same way.
    #[derive(Clone, Copy, Debug)]
    enum Scan {
        Counted {
            size: u64,
            mtime: (i64, i64),
            found: Found,
        },
        Failed,
    }

    /// Scan results for this Hermit process.
    static SCANS: Mutex<BTreeMap<FileRange, Scan>> = Mutex::new(BTreeMap::new());

    /// The exclusive pairs of one mapping, or `None` where it cannot be
    /// scanned.
    fn scan_mapping(tid: Pid, path: &Path, key: FileRange) -> Option<Found> {
        let cached = SCANS.lock().unwrap().get(&key).copied();
        if let Some(Scan::Failed) = cached {
            return None;
        }
        let result = (|| {
            let file = open_in_root(tid, path)?;
            let metadata = file.metadata()?;
            // The maps device is the superblock's, which on btrfs is not the
            // subvolume device `stat` gives, so only the inode is compared.
            if metadata.ino() != key.inode {
                return Err(std::io::Error::other(
                    "the name now leads to another file than the mapped one",
                ));
            }
            let (size, mtime) = (metadata.size(), (metadata.mtime(), metadata.mtime_nsec()));
            let found = match cached {
                Some(Scan::Counted {
                    size: was,
                    mtime: then,
                    found,
                }) if (was, then) == (size, mtime) => found,
                _ => scan_file(&file, size, &key)?,
            };
            Ok(Scan::Counted { size, mtime, found })
        })();
        let scan = result.unwrap_or_else(|error: std::io::Error| {
            debug!(
                "[store-exclusive] {} not scanned through /proc/{}/root: {error}",
                path.display(),
                tid.as_raw()
            );
            Scan::Failed
        });
        SCANS.lock().unwrap().insert(key, scan);
        match scan {
            Scan::Counted { found, .. } => Some(found),
            Scan::Failed => None,
        }
    }

    /// Open `path` as the guest names it: resolved inside the guest's root
    /// (`RESOLVE_IN_ROOT`, from an `O_PATH` descriptor of `/proc/<tid>/root`),
    /// so that neither a symbolic link nor `..` leads out of it.
    fn open_in_root(tid: Pid, path: &Path) -> std::io::Result<File> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(format!("/proc/{}/root", tid.as_raw()))?;
        let relative = path.strip_prefix("/").unwrap_or(path);
        let relative =
            CString::new(relative.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        /// `struct open_how` (`OPEN_HOW_SIZE_VER0`).
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: libc::RESOLVE_IN_ROOT,
        };
        // SAFETY: a valid descriptor, a NUL-terminated path and an `open_how`
        // of the size passed, all alive for the call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                relative.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: openat2 returned a new descriptor nothing else owns.
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }

    /// Scan the mapped range of a file of `size` bytes, in its executable
    /// ELF sections only, for what [`Found`] names.
    fn scan_file(file: &File, size: u64, key: &FileRange) -> std::io::Result<Found> {
        // A mapping can extend past the end of its file.
        let mapped = (key.offset, size.min(key.offset.saturating_add(key.len)));
        let mut found = Found::default();
        for (start, end) in code_ranges(file, mapped)? {
            // A pair cannot span two sections.
            let mut section = CodeScan::default();
            read_words(file, start, end, &mut section)?;
            found.pairs += section.pairs();
            found.counter_reads += section.counter_reads();
        }
        Ok(found)
    }

    /// The parts of `mapped` (a file range) that hold instructions: its
    /// intersections with the file's executable ELF sections, or all of it
    /// when the file has no section headers.
    fn code_ranges(file: &File, mapped: (u64, u64)) -> std::io::Result<Vec<(u64, u64)>> {
        let mut header = [0u8; 64];
        let table = match file.read_exact_at(&mut header, 0) {
            Ok(()) => elf_section_table(&header),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(error) => return Err(error),
        };
        let Some((offset, count)) = table else {
            return Ok(vec![mapped]);
        };
        let mut table = vec![0u8; count * 64];
        file.read_exact_at(&mut table, offset)?;
        Ok(executable_sections(&table)
            .into_iter()
            .filter_map(|(offset, size)| {
                let start = offset.max(mapped.0);
                let end = offset.saturating_add(size).min(mapped.1);
                (start < end).then_some((start, end))
            })
            .collect())
    }

    /// Feed `counter` the words of `file` in `start..end`.
    fn read_words(
        file: &File,
        start: u64,
        end: u64,
        counter: &mut CodeScan,
    ) -> std::io::Result<()> {
        let mut buffer = vec![0u8; (1 << 20).min((end - start) as usize)];
        let mut at = start;
        while at < end {
            let chunk =
                usize::try_from(end - at).map_or(buffer.len(), |left| left.min(buffer.len()));
            file.read_exact_at(&mut buffer[..chunk], at)?;
            counter.push_bytes(&buffer[..chunk]);
            at += chunk as u64;
        }
        Ok(())
    }

    /// Scan a thread's executable file mappings that overlap `range` (all of
    /// them for `None`): log each with pairs, or that could not be scanned,
    /// at INFO, and report the pairs to the run's tally.
    async fn report_pairs<G, T>(guest: &mut G, range: Option<(u64, u64)>)
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        let dettid = guest.thread_state().dettid;
        debug!(
            "[detcore, dtid {}] store-exclusive scan of {:x?}",
            dettid,
            range.map_or(0..u64::MAX, |(start, end)| start..end)
        );
        let maps = match crate::procmaps::from_pid(guest.tid(), |map| {
            map.inode != 0 && matches!(map.pathname, MMapPath::Path(_))
        }) {
            Ok(maps) => maps,
            Err(error) => {
                debug!(
                    "[store-exclusive] cannot read /proc/{}/maps: {error}",
                    guest.tid().as_raw()
                );
                return;
            }
        };
        let space = {
            let thread = guest.thread_state();
            let mut metadata = thread.memory_metadata.lock().unwrap();
            metadata.exclusive_scan.reseed(&maps);
            metadata
                .exclusive_scan
                .space(thread.mm_id, || thread.pedigree.to_string())
        };
        let mut mappings = Vec::new();
        for map in &maps {
            let MMapPath::Path(path) = &map.pathname else {
                continue;
            };
            let (start, end) = map.address;
            if !map.perms.contains(MMPermissions::EXECUTE)
                || range.is_some_and(|(from, to)| end <= from || to <= start)
            {
                continue;
            }
            let key = FileRange {
                dev: map.dev,
                inode: map.inode,
                offset: map.offset,
                len: end - start,
            };
            let shown = path.display().to_string();
            match scan_mapping(guest.tid(), path, key) {
                Some(found) if found == Found::default() => {}
                Some(found) => {
                    // Only what was found: a mapping with pairs alone keeps the
                    // line it always had.
                    let what = [
                        (found.pairs, "store-exclusive (LL/SC) pair"),
                        (found.counter_reads, "EL0 generic-timer counter read"),
                    ]
                    .into_iter()
                    .filter(|(count, _)| *count != 0)
                    .map(|(count, noun)| counted(count, noun))
                    .collect::<Vec<_>>()
                    .join(" and ");
                    info!(
                        "[detcore, dtid {}] {} in executable mapping {} (offset {:#x}, {:#x} bytes)",
                        dettid, what, shown, key.offset, key.len
                    );
                    mappings.push(ScannedMapping {
                        path: shown,
                        offset: key.offset,
                        len: key.len,
                        found,
                    });
                }
                None => info!(
                    "[detcore, dtid {}] store-exclusive (LL/SC) pairs not scanned in executable \
                     mapping {} (offset {:#x}, {:#x} bytes)",
                    dettid, shown, key.offset, key.len
                ),
            }
        }
        if !mappings.is_empty() {
            send(guest, Report::Pairs { space, mappings }).await;
        }
    }

    async fn send<G, T>(guest: &mut G, report: Report)
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        let answer = guest
            .send_rpc(crate::tool_global::store_exclusive_request(report))
            .await;
        crate::tool_global::store_exclusive_reply(answer);
    }

    /// After a successful exec, while the thread is stopped at the start of
    /// the new image: in the root guest process, open the run's counters if
    /// this run counts (only its first exec does), so that they count the
    /// guest's own images and no launcher code; then scan every executable
    /// file mapping of the image (the program and its interpreter).
    pub(crate) async fn report_after_exec<G, T>(guest: &mut G)
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        if counts_store_exclusives(guest.config()) && guest.is_root_process() {
            let root = guest.tid().as_raw();
            send(guest, Report::Attach { root }).await;
        }
        if reads_guest_tasks(guest.config()) {
            report_pairs(guest, None).await;
        }
    }

    /// After a successful syscall: the address range to scan, for an `mmap`
    /// that maps `PROT_EXEC` from a file, or an `mprotect` that adds
    /// `PROT_EXEC` where a file mapping may not be scanned yet. Anything
    /// else reads no maps: a file mapping made without `PROT_EXEC` is only
    /// recorded, and `PROT_EXEC` on anonymous memory (a JIT) is skipped.
    pub(crate) fn range_to_scan<G, T>(guest: &G, call: &Syscall, result: i64) -> Option<(u64, u64)>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        if !reads_guest_tasks(guest.config()) {
            return None;
        }
        let span = |start: u64, len: usize| (start, start.saturating_add(len.max(1) as u64));
        match call {
            Syscall::Mmap(mmap)
                if !mmap.flags().contains(MapFlags::MAP_ANONYMOUS) && mmap.fd() >= 0 =>
            {
                let range = span(result as u64, mmap.len());
                if mmap.prot().contains(ProtFlags::PROT_EXEC) {
                    return Some(range);
                }
                let metadata = &guest.thread_state().memory_metadata;
                metadata.lock().unwrap().exclusive_scan.add_unscanned(range);
                None
            }
            Syscall::Mprotect(mprotect) if mprotect.protection().contains(ProtFlags::PROT_EXEC) => {
                let start = mprotect.addr().map_or(0, |addr| addr.as_raw() as u64);
                let range = span(start, mprotect.len());
                let metadata = &guest.thread_state().memory_metadata;
                let needed = metadata.lock().unwrap().exclusive_scan.needs_scan(range);
                needed.then_some(range)
            }
            _ => None,
        }
    }

    /// After a [`range_to_scan`]: the executable file mappings in that range.
    pub(crate) async fn report_new_mapping<G, T>(guest: &mut G, range: (u64, u64))
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        report_pairs(guest, Some(range)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::decode::CodeScan;
    use super::decode::Exclusive;
    use super::decode::PAIR_WINDOW;
    use super::decode::classify;
    use super::decode::count_pairs;
    use super::decode::elf_section_table;
    use super::decode::executable_sections;
    use super::decode::is_counter_read;
    use super::decode::scan_words;
    use super::model::CpuPmu;
    use super::model::Found;
    use super::model::PairTally;
    use super::model::ScannedMapping;
    use super::model::arm_cpu_pmus;
    use super::model::combine;
    use super::model::covering;
    use super::model::parse_cpu_list;

    // Encodings from GNU as (`.arch armv8.1-a`).
    const LDAXR_X0_X2: u32 = 0xC85F_FC40; // ldaxr x0, [x2]
    const STLXR_W3_X1_X2: u32 = 0xC803_FC41; // stlxr w3, x1, [x2]
    const LDAR_X0_X1: u32 = 0xC8DF_FC20; // ldar x0, [x1]
    const STLR_X0_X1: u32 = 0xC89F_FC20; // stlr x0, [x1]
    const CASAL_X1_X2_X0: u32 = 0xC8E1_FC02; // casal x1, x2, [x0]
    const CASPAL_X: u32 = 0x4860_FC82; // caspal x0, x1, x2, x3, [x4]
    const CASP_W: u32 = 0x0820_7C82; // casp w0, w1, w2, w3, [x4]
    const LDAXP_X0_X1_X2: u32 = 0xC87F_8440; // ldaxp x0, x1, [x2]
    const STLXP_W3_X0_X1_X2: u32 = 0xC823_8440; // stlxp w3, x0, x1, [x2]
    const LDXP_W0_W1_X2: u32 = 0x887F_0440; // ldxp w0, w1, [x2]
    const STXP_W3_W0_W1_X2: u32 = 0x8823_0440; // stxp w3, w0, w1, [x2]
    const LDXRB_W0_X1: u32 = 0x085F_7C20; // ldxrb w0, [x1]
    const STXRB_W2_W0_X1: u32 = 0x0802_7C20; // stxrb w2, w0, [x1]
    const LDXRH_W0_X1: u32 = 0x485F_7C20; // ldxrh w0, [x1]
    const STXRH_W2_W0_X1: u32 = 0x4802_7C20; // stxrh w2, w0, [x1]
    const LDXR_W0_X1: u32 = 0x885F_7C20; // ldxr w0, [x1]
    const STXR_W2_W0_X1: u32 = 0x8802_7C20; // stxr w2, w0, [x1]
    const LDADD_W1_W0_X2: u32 = 0xB821_0040; // ldadd w1, w0, [x2]
    const CLREX: u32 = 0xD503_3F5F; // clrex
    const ADD_X1_X0_1: u32 = 0x9100_0401; // add x1, x0, #1
    const CBNZ_W3: u32 = 0x3500_0003; // cbnz w3, .

    #[test]
    fn classifies_exclusive_loads_and_stores() {
        for load in [
            LDAXR_X0_X2,
            LDAXP_X0_X1_X2,
            LDXP_W0_W1_X2,
            LDXRB_W0_X1,
            LDXRH_W0_X1,
            LDXR_W0_X1,
        ] {
            assert_eq!(classify(load), Some(Exclusive::Load), "{load:#010x}");
        }
        for store in [
            STLXR_W3_X1_X2,
            STLXP_W3_X0_X1_X2,
            STXP_W3_W0_W1_X2,
            STXRB_W2_W0_X1,
            STXRH_W2_W0_X1,
            STXR_W2_W0_X1,
        ] {
            assert_eq!(classify(store), Some(Exclusive::Store), "{store:#010x}");
        }
    }

    #[test]
    fn ordered_accesses_and_lse_are_not_exclusive() {
        for insn in [
            LDAR_X0_X1,
            STLR_X0_X1,
            CASAL_X1_X2_X0,
            CASPAL_X,
            CASP_W,
            LDADD_W1_W0_X2,
            CLREX,
            ADD_X1_X0_1,
            CBNZ_W3,
            0,
            u32::MAX,
        ] {
            assert_eq!(classify(insn), None, "{insn:#010x}");
        }
    }

    #[test]
    fn counts_retry_loops() {
        // ldaxr; add; stlxr; cbnz: the compiler's fetch_add without LSE.
        let rmw = [LDAXR_X0_X2, ADD_X1_X0_1, STLXR_W3_X1_X2, CBNZ_W3];
        assert_eq!(count_pairs(rmw), 1);
        assert_eq!(count_pairs(rmw.iter().chain(&rmw).copied()), 2);
        assert_eq!(count_pairs([LDXP_W0_W1_X2, STXP_W3_W0_W1_X2]), 1);
        // A store without a load, a load without a store, LSE, ordered.
        assert_eq!(count_pairs([STLXR_W3_X1_X2, ADD_X1_X0_1]), 0);
        assert_eq!(count_pairs([LDAXR_X0_X2, ADD_X1_X0_1, CBNZ_W3]), 0);
        assert_eq!(count_pairs([LDAR_X0_X1, CASAL_X1_X2_X0, STLR_X0_X1]), 0);
        assert_eq!(count_pairs([CASPAL_X, CASP_W, LDADD_W1_W0_X2]), 0);
    }

    #[test]
    fn pairs_end_sixteen_instructions_after_the_load() {
        let with_gap = |gap: u64| {
            std::iter::once(LDAXR_X0_X2)
                .chain(std::iter::repeat_n(ADD_X1_X0_1, gap as usize))
                .chain(std::iter::once(STLXR_W3_X1_X2))
        };
        // The store is `gap + 1` instructions after the load.
        assert_eq!(count_pairs(with_gap(PAIR_WINDOW - 1)), 1);
        assert_eq!(count_pairs(with_gap(PAIR_WINDOW)), 0);
        assert_eq!(PAIR_WINDOW, 16);
        // The nearest load decides.
        let refreshed = [LDAXR_X0_X2]
            .into_iter()
            .chain(std::iter::repeat_n(ADD_X1_X0_1, 20))
            .chain([LDXR_W0_X1, ADD_X1_X0_1, STXR_W2_W0_X1]);
        assert_eq!(count_pairs(refreshed), 1);
    }

    fn elf_header(shoff: u64, shentsize: u16, shnum: u16) -> [u8; 64] {
        let mut header = [0u8; 64];
        header[..6].copy_from_slice(b"\x7fELF\x02\x01");
        header[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        header[0x3A..0x3C].copy_from_slice(&shentsize.to_le_bytes());
        header[0x3C..0x3E].copy_from_slice(&shnum.to_le_bytes());
        header
    }

    fn section(kind: u32, flags: u64, offset: u64, size: u64) -> [u8; 64] {
        let mut entry = [0u8; 64];
        entry[4..8].copy_from_slice(&kind.to_le_bytes());
        entry[8..16].copy_from_slice(&flags.to_le_bytes());
        entry[24..32].copy_from_slice(&offset.to_le_bytes());
        entry[32..40].copy_from_slice(&size.to_le_bytes());
        entry
    }

    #[test]
    fn finds_the_section_table_of_little_endian_elf64_only() {
        assert_eq!(
            elf_section_table(&elf_header(0x1000, 64, 30)),
            Some((0x1000, 30))
        );
        assert_eq!(elf_section_table(&elf_header(0, 64, 30)), None);
        assert_eq!(elf_section_table(&elf_header(0x1000, 64, 0)), None);
        assert_eq!(elf_section_table(&elf_header(0x1000, 40, 30)), None);
        let mut elf32 = elf_header(0x1000, 64, 30);
        elf32[4] = 1;
        assert_eq!(elf_section_table(&elf32), None);
        let mut big_endian = elf_header(0x1000, 64, 30);
        big_endian[5] = 2;
        assert_eq!(elf_section_table(&big_endian), None);
        assert_eq!(elf_section_table(&[0u8; 64]), None);
    }

    #[test]
    fn scans_only_sections_that_hold_instructions() {
        const PROGBITS: u32 = 1;
        const NOBITS: u32 = 8;
        const ALLOC: u64 = 0x2;
        const EXECINSTR: u64 = 0x4;
        let table: Vec<u8> = [
            section(0, 0, 0, 0),
            section(PROGBITS, ALLOC, 0x200, 0x100), // .rodata
            section(PROGBITS, ALLOC | EXECINSTR, 0x400, 0x800), // .text
            section(PROGBITS, ALLOC | EXECINSTR, 0xC00, 0), // empty
            section(NOBITS, ALLOC | EXECINSTR, 0xC00, 0x40), // no file bytes
            section(PROGBITS, ALLOC | EXECINSTR, 0x1000, 0x20), // .plt
        ]
        .concat();
        assert_eq!(
            executable_sections(&table),
            vec![(0x400, 0x800), (0x1000, 0x20)]
        );
    }

    #[test]
    fn bytes_are_little_endian_words_across_chunks() {
        let bytes: Vec<u8> = [LDAXR_X0_X2, ADD_X1_X0_1, STLXR_W3_X1_X2, CBNZ_W3]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let mut whole = CodeScan::default();
        whole.push_bytes(&bytes);
        assert_eq!(whole.pairs(), 1);
        let mut split = CodeScan::default();
        split.push_bytes(&bytes[..8]);
        split.push_bytes(&bytes[8..]);
        assert_eq!(split.pairs(), 1);
        // Big-endian words are other instructions.
        let mut swapped = CodeScan::default();
        for word in [LDAXR_X0_X2, STLXR_W3_X1_X2] {
            swapped.push_bytes(&word.to_be_bytes());
        }
        assert_eq!(swapped.pairs(), 0);
    }

    #[test]
    fn parses_kernel_cpu_lists() {
        assert_eq!(parse_cpu_list("0-71\n"), Some((0..=71).collect()));
        assert_eq!(
            parse_cpu_list("0-3,8,10-11"),
            Some(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list(""), Some(vec![]));
        assert_eq!(parse_cpu_list("3-1"), None);
        assert_eq!(parse_cpu_list("x"), None);
    }

    /// A big.LITTLE host lists one Arm PMU per kind of core, each with its
    /// own type and CPUs, beside PMUs that are not CPU PMUs.
    #[test]
    fn finds_each_arm_cpu_pmu_in_a_two_cluster_sysfs_tree() {
        let root = std::env::temp_dir().join(format!(
            "store-exclusive-sysfs-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let tree: [(&str, &[(&str, &str)]); 6] = [
            ("armv8_cortex_a55", &[("type", "8\n"), ("cpus", "0-3\n")]),
            ("armv8_cortex_a78", &[("type", "9\n"), ("cpus", "4-7\n")]),
            ("arm_spe_0", &[("type", "12\n"), ("cpumask", "0-7\n")]),
            ("nvidia_scf_pmu_0", &[("type", "13\n"), ("cpumask", "0\n")]),
            ("software", &[("type", "1\n")]),
            ("armv8_broken", &[("type", "x\n"), ("cpus", "0\n")]),
        ];
        for (name, files) in tree {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            for (file, contents) in files {
                std::fs::write(dir.join(file), contents).unwrap();
            }
        }
        let names = [".", ".."]
            .into_iter()
            .chain(tree.iter().map(|(name, _)| *name))
            .map(str::to_owned);
        let pmus = arm_cpu_pmus(&root, names);
        for (name, files) in tree {
            for (file, _) in files {
                std::fs::remove_file(root.join(name).join(file)).unwrap();
            }
            std::fs::remove_dir(root.join(name)).unwrap();
        }
        std::fs::remove_dir(&root).unwrap();
        let little = CpuPmu {
            name: "armv8_cortex_a55".to_owned(),
            kind: 8,
            cpus: vec![0, 1, 2, 3],
        };
        let big = CpuPmu {
            name: "armv8_cortex_a78".to_owned(),
            kind: 9,
            cpus: vec![4, 5, 6, 7],
        };
        assert_eq!(pmus, vec![little.clone(), big.clone()]);
        assert_eq!(
            covering(&pmus, &(0..8).collect::<Vec<_>>()),
            vec![(little.clone(), 0), (big.clone(), 4)]
        );
        assert_eq!(covering(&pmus, &[6, 7]), vec![(big, 6)]);
        assert_eq!(covering(&pmus, &[2]), vec![(little, 2)]);
        assert_eq!(covering(&pmus, &[9]), vec![]);
    }

    #[test]
    fn combines_reads_across_pmus() {
        // One PMU, always on a counter.
        assert_eq!(combine(&[(20, 1000, 1000)]), Some((20, true)));
        // Two kinds of core: the task's time splits between the PMUs.
        assert_eq!(combine(&[(5, 1000, 300), (7, 1000, 700)]), Some((12, true)));
        // Multiplexed: on a counter for part of the run only.
        assert_eq!(combine(&[(5, 1000, 400)]), Some((5, false)));
        assert_eq!(
            combine(&[(5, 1000, 300), (7, 1000, 400)]),
            Some((12, false))
        );
        // Enabled but never on a counter: the core does not count the event.
        assert_eq!(combine(&[(0, 1000, 0)]), None);
        assert_eq!(combine(&[(0, 1000, 0), (0, 1000, 0)]), None);
        // Never enabled: nothing ran.
        assert_eq!(combine(&[(0, 0, 0)]), Some((0, true)));
    }

    /// A mapping as the scan reports it, for the tally tests.
    fn mapping(path: &str, offset: u64, len: u64, pairs: u64) -> ScannedMapping {
        ScannedMapping {
            path: path.to_owned(),
            offset,
            len,
            found: Found {
                pairs,
                counter_reads: 0,
            },
        }
    }

    #[test]
    fn tallies_pairs_per_address_space_and_path() {
        let libc = "/lib64/libc.so.6".to_owned();
        let parent = ("0".to_owned(), 1);
        let child = ("01".to_owned(), 0);
        let mut tally = PairTally::default();
        tally.note(&parent, &[mapping(&libc, 0, 0x1000, 19)]);
        // Two ranges of one file in one address space add up.
        tally.note(&parent, &[mapping(&libc, 0x4000, 0x1000, 2)]);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // The same ranges in another address space count once.
        tally.note(&child, &[mapping(&libc, 0, 0x1000, 19)]);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // mprotect split the first range and rejoined it: the new ranges
        // replace the ones they overlap.
        tally.note(
            &parent,
            &[
                mapping(&libc, 0, 0x800, 12),
                mapping(&libc, 0x800, 0x800, 7),
            ],
        );
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        tally.note(&parent, &[mapping(&libc, 0, 0x1000, 19)]);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // A later image of the same process is another address space.
        tally.note(&(parent.0.clone(), 2), &[mapping(&libc, 0, 0x1000, 19)]);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
    }

    /// The virtual counter, read from EL0: `MRS Xt, CNTVCT_EL0` and
    /// `CNTVCTSS_EL0`, with every `Rt`. Encodings from GNU as.
    #[test]
    fn counts_the_virtual_counter_reads_an_image_can_make() {
        for encoding in [0xd53b_e040u32, 0xd53b_e0c0] {
            for rt in 0..32 {
                assert!(is_counter_read(encoding | rt), "{:#010x}", encoding | rt);
            }
        }
        // Not counted: `CNTPCT_EL0` and `CNTPCTSS_EL0`, which fault at EL0 on
        // Linux; `CNTFRQ_EL0`, a constant; `CNTV_TVAL_EL0`, a timer register
        // that also faults; `CNTKCTL_EL1`, EL1 state; a `NOP` and a
        // load-exclusive.
        for other in [
            0xd53b_e020u32,
            0xd53b_e0a0,
            0xd53b_e000,
            0xd53b_e300,
            0xd538_e104,
            0xd503_201f,
            0xc85f_fc64,
        ] {
            assert!(!is_counter_read(other), "{other:#010x}");
        }
        // One pass counts both kinds, and a counter read is not mistaken for
        // part of an exclusive pair: ldaxr; mrs x1, cntvct_el0; stlxr; cbnz.
        let scan = scan_words([0xc85f_fc64, 0xd53b_e041, 0xc805_fc64, 0x35ff_ffa5]);
        assert_eq!(scan.pairs(), 1);
        assert_eq!(scan.counter_reads(), 1);
    }

    /// A mapping is tallied when it has either kind of finding, and each
    /// per-path view leaves out the paths with none of its kind.
    #[test]
    fn tallies_each_finding_only_where_there_is_one() {
        let space = ("0".to_owned(), 1);
        let mut tally = PairTally::default();
        let linker = ScannedMapping {
            path: "/system/bin/linker64".to_owned(),
            offset: 0,
            len: 0x1000,
            found: Found {
                pairs: 0,
                counter_reads: 1,
            },
        };
        tally.note(&space, &[linker, mapping("/lib64/libc.so.6", 0, 0x1000, 3)]);
        assert_eq!(
            tally.pairs_per_path().into_iter().collect::<Vec<_>>(),
            vec![("/lib64/libc.so.6".to_owned(), 3)]
        );
        assert_eq!(
            tally
                .counter_reads_per_path()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![("/system/bin/linker64".to_owned(), 1)]
        );
    }
}
