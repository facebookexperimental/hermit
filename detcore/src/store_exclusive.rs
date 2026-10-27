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
        /// The file offset of the first word.
        base: u64,
        index: u64,
        last_load: Option<u64>,
        /// The file offset of each pair's load-exclusive.
        pair_loads: Vec<u64>,
        /// The file offset of each counter read.
        reads: Vec<u64>,
    }

    impl CodeScan {
        /// A scan of words that start at file offset `base`.
        pub(crate) fn at(base: u64) -> Self {
            CodeScan {
                base,
                ..CodeScan::default()
            }
        }

        /// Take the next instruction word.
        pub(crate) fn push(&mut self, insn: u32) {
            match classify(insn) {
                Some(Exclusive::Load) => self.last_load = Some(self.index),
                Some(Exclusive::Store) => {
                    if let Some(load) = self
                        .last_load
                        .filter(|load| self.index - load <= PAIR_WINDOW)
                    {
                        self.pair_loads.push(self.base + 4 * load);
                    }
                }
                _ if is_counter_read(insn) => self.reads.push(self.base + 4 * self.index),
                _ => {}
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
        #[cfg(test)]
        pub(crate) fn pairs(&self) -> u64 {
            self.pair_loads.len() as u64
        }

        /// The EL0 counter reads counted so far.
        #[cfg(test)]
        pub(crate) fn counter_reads(&self) -> u64 {
            self.reads.len() as u64
        }

        /// What the scan found, by file offset.
        pub(crate) fn into_loops(self) -> super::model::FileLoops {
            super::model::FileLoops {
                pair_loads: self.pair_loads,
                counter_reads: self.reads,
            }
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
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Arc;

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
        /// Where the mapping starts in the guest's address space: the tally
        /// keeps one outcome per mapping, so two executable aliases of the
        /// same file bytes are each counted.
        #[serde(default)]
        pub address: u64,
        /// The mapped range of that file.
        pub offset: u64,
        pub len: u64,
        pub found: Found,
        /// What the in-memory rewrite did to it, in a run that asks for one.
        #[serde(default)]
        pub rewrite: Rewrite,
    }

    /// What rewriting one mapping's exclusive loops in the guest's memory
    /// did (`--rewrite-exclusives` without a mount namespace). Every pair the
    /// scan found ends up in exactly one of the three counts.
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
    pub struct Rewrite {
        /// Loops the mapping now holds as single LSE atomics: written by this
        /// scan, or already written (an earlier scan of the same pages, or
        /// pages inherited across `fork`).
        pub rewritten: u64,
        /// Loops left as the file has them: the rewriter refuses them, the
        /// mapping is shared or writable, the bytes in memory differ from the
        /// file's, or the sequence runs past the end of the mapping.
        pub left: u64,
        /// Loops the rewrite was asked for and could not write: the guest's
        /// memory or the file could not be read or written. They still retry.
        pub failed: u64,
    }

    impl Rewrite {
        /// Add another range's counts.
        fn add(self, other: Rewrite) -> Rewrite {
            Rewrite {
                rewritten: self.rewritten + other.rewritten,
                left: self.left + other.left,
                failed: self.failed + other.failed,
            }
        }

        /// Whether every count is zero.
        pub(crate) fn is_empty(&self) -> bool {
            *self == Rewrite::default()
        }
    }

    /// Where a patch of a file falls in one mapping of it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Placement {
        /// Wholly inside the mapping, starting at this guest address.
        At(u64),
        /// Starts inside the mapping and runs past its end. Rewriting part of a
        /// sequence would leave a loop that is neither the old one nor the new
        /// one, so it is left whole.
        CrossesEnd,
        /// Not in the mapping.
        Outside,
    }

    /// Where the file bytes `[offset, offset + len)` fall in a mapping of the
    /// file range `[map_offset, map_offset + map_len)` that starts at guest
    /// address `map_start`.
    pub(crate) fn place(
        offset: u64,
        len: u64,
        map_start: u64,
        map_offset: u64,
        map_len: u64,
    ) -> Placement {
        let map_end = map_offset.saturating_add(map_len);
        if offset < map_offset || offset >= map_end {
            Placement::Outside
        } else if offset.saturating_add(len) > map_end {
            Placement::CrossesEnd
        } else {
            Placement::At(map_start + (offset - map_offset))
        }
    }

    /// An address range, `[start, end)`.
    pub(crate) type Range = (u64, u64);

    /// The `[start, start + len)` a call that takes whole pages acts on: Linux
    /// rounds the length up to the page size.
    pub(crate) fn page_span(start: u64, len: u64, page: u64) -> Range {
        let len = len.max(1).div_ceil(page).saturating_mul(page);
        (start, start.saturating_add(len))
    }

    /// What made a scan happen, which decides where the in-memory rewrite may
    /// write a sequence whose bytes are still the file's.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Trigger {
        /// The exec stop: every mapping is new and no instruction has run.
        Exec,
        /// An `mmap` made the range: whatever was there before is gone.
        Mapped,
        /// An `mprotect` made the range executable: a part that already was
        /// may have run, and only the rest is new.
        Protected,
        /// An `madvise` dropped the range's private pages, so a rewritten
        /// sequence there reads the file's bytes again: those, and only
        /// those, are put back.
        Dropped,
        /// A `munmap`, an `mremap`, or an `mprotect` that took execute
        /// permission away changed where executable text is: the tally
        /// follows, and nothing is new.
        Changed,
    }

    /// Whether any part of `[start, end)` was executable among an address
    /// space's file-backed `ranges` (start, end, executable) at the last
    /// scan. Unknown ranges count as executable.
    pub(crate) fn was_executable(ranges: Option<&[(u64, u64, bool)]>, (start, end): Range) -> bool {
        ranges.is_none_or(|ranges| {
            ranges
                .iter()
                .any(|&(from, to, executable)| executable && from < end && start < to)
        })
    }

    /// Whether no instruction of the sequence at `[start, end)` can have run
    /// when the scan that `trigger` made over `range` finds it: only at the
    /// exec stop, and inside the range a successful `mmap` just made (decided
    /// per sequence, since the kernel may merge a new mapping with an older
    /// neighbour into one maps entry). Text made executable by `mprotect` may
    /// have been executable before, through any number of permission changes
    /// and moves this module does not follow, so it is never new; nor is text
    /// whose pages an `madvise` dropped. Those scans only recount.
    pub(crate) fn sequence_is_new(
        trigger: Trigger,
        range: Option<Range>,
        (start, end): Range,
    ) -> bool {
        match trigger {
            Trigger::Exec => true,
            Trigger::Mapped => range.is_some_and(|(from, to)| from <= start && end <= to),
            Trigger::Protected | Trigger::Dropped | Trigger::Changed => false,
        }
    }

    /// Whether `[address, address + len)` touches more than one page. Such a
    /// sequence is left: a page drop could put back the file's bytes in one
    /// page and keep the replacement in the other, and the guest would run a
    /// mixture of the two.
    pub(crate) fn crosses_page(address: u64, len: u64, page: u64) -> bool {
        len > 0 && address / page != (address + len - 1) / page
    }

    /// File ranges (start, end) in order with overlapping ones merged, so
    /// that bytes two section headers both claim are scanned, and counted,
    /// once. Ranges that only touch stay apart: a pair cannot span two
    /// sections.
    pub(crate) fn merge_overlaps(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
        ranges.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start < last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        merged
    }

    /// One [`ByteCache`] entry: the bytes, what was made from them, and what
    /// it is charged.
    struct Cached<V> {
        bytes: Box<[u8]>,
        value: V,
        charge: usize,
    }

    /// A [`ByteCache`] key: the length and a hash of three sampled pages.
    type CacheKey = (usize, u64);

    /// An upper bound on one B-tree node's allocation, for the
    /// [`ByteCache`] key map and a plan's report maps. A node holds up to 11
    /// keys, 11 values and (inside the tree) 12 child pointers: for a 16-byte
    /// key and a 24-byte value that is under 560 bytes. A B-tree never has
    /// more nodes than entries (no node is empty), so this bounds each entry's
    /// share however sparsely the nodes are filled.
    const MAP_NODE_BOUND: usize = 1024;

    /// What one [`ByteCache`] entry's own allocation costs beyond its bytes
    /// and its value's heap: the entry with its two reference counts.
    pub(crate) const fn cached_entry_overhead<V>() -> usize {
        2 * std::mem::size_of::<usize>() + std::mem::size_of::<Cached<V>>()
    }

    /// An upper bound on what a [`ByteCache`] of at most `max_entries`
    /// retains besides its entries, whatever was inserted, evicted or
    /// cleared: the age queue, whose capacity never exceeds twice the most
    /// entries it held (it grows by doubling, and entries are evicted before
    /// one is added), at most one key-map node per entry, and each key's
    /// candidate list, allocated for the most candidates.
    pub(crate) const fn spine_bound<V>(max_entries: usize) -> usize {
        let slots = if max_entries < 2 {
            4
        } else {
            max_entries.saturating_mul(2)
        };
        spine_of::<V>(slots, max_entries)
    }

    /// The spine of a [`ByteCache`] whose age queue has room for `slots`
    /// and whose key map has `keys` keys: the queue's slots, a node per key
    /// (a bound: there are never more nodes than keys) and each key's
    /// candidate list. Saturating, so an absurd size cannot wrap.
    const fn spine_of<V>(slots: usize, keys: usize) -> usize {
        use std::mem::size_of;
        let per_key = MAP_NODE_BOUND
            .saturating_add(MAX_CANDIDATES_PER_KEY.saturating_mul(size_of::<Arc<Cached<V>>>()));
        slots
            .saturating_mul(size_of::<(CacheKey, Arc<Cached<V>>)>())
            .saturating_add(keys.saturating_mul(per_key))
    }

    /// The most entries [`ByteCache`] compares bytes with for one key.
    pub(crate) const MAX_CANDIDATES_PER_KEY: usize = 4;

    /// The heap a cached plan holds, from the capacities of everything it
    /// allocated, as [`ByteCache`] charges it.
    pub(crate) fn plan_charge(plan: &Option<crate::aarch64_lse::elf::FilePlan>) -> usize {
        use std::mem::size_of;

        use crate::aarch64_lse::elf::FilePatch;
        // The plan in its shared allocation, with the reference counts.
        let mut charge =
            2 * size_of::<usize>() + size_of::<Option<crate::aarch64_lse::elf::FilePlan>>();
        let Some(plan) = plan else {
            return charge;
        };
        let add = |charge: &mut usize, more: usize| *charge = charge.saturating_add(more);
        add(
            &mut charge,
            plan.patches
                .capacity()
                .saturating_mul(size_of::<FilePatch>()),
        );
        for patch in &plan.patches {
            add(&mut charge, patch.original.capacity());
            add(&mut charge, patch.rewritten.capacity());
        }
        let report = &plan.report;
        add(
            &mut charge,
            report
                .refused_examples
                .capacity()
                .saturating_mul(size_of::<(u64, String)>()),
        );
        for (_, why) in &report.refused_examples {
            add(&mut charge, why.capacity());
        }
        add(
            &mut charge,
            report
                .skipped_segments
                .capacity()
                .saturating_mul(size_of::<String>()),
        );
        for why in &report.skipped_segments {
            add(&mut charge, why.capacity());
        }
        add(
            &mut charge,
            (report.patched.len() + report.refused.len()).saturating_mul(MAP_NODE_BOUND),
        );
        charge
    }

    /// Values made from byte strings, found by those bytes: an entry is used
    /// only for bytes equal to its own, so a value that is a pure function of
    /// its bytes comes back exactly as making it again would. The key (the
    /// length and a hash of three sampled 4 KiB pages) only picks the
    /// candidates, at most [`MAX_CANDIDATES_PER_KEY`] of them. Everything it
    /// retains (the entries with their bytes and their values' heap, and the
    /// spine bounded by [`spine_bound`]) stays within the `max_retained` it
    /// is made with, and the entries within `max_entries`: the oldest go
    /// before a new one is added.
    pub(crate) struct ByteCache<V> {
        by_key: BTreeMap<CacheKey, Vec<Arc<Cached<V>>>>,
        order: VecDeque<(CacheKey, Arc<Cached<V>>)>,
        held: usize,
        peak: usize,
        max_charge: usize,
        max_entries: usize,
    }

    impl<V: Clone> ByteCache<V> {
        pub(crate) const fn new(max_retained: usize, max_entries: usize) -> Self {
            ByteCache {
                by_key: BTreeMap::new(),
                order: VecDeque::new(),
                held: 0,
                peak: 0,
                // The entries get what the spine cannot take.
                max_charge: max_retained.saturating_sub(spine_bound::<V>(max_entries)),
                max_entries,
            }
        }

        fn key(bytes: &[u8]) -> CacheKey {
            let mut hasher = std::hash::DefaultHasher::new();
            let sample = 4096.min(bytes.len());
            let middle = (bytes.len() - sample) / 2;
            for part in [
                &bytes[..sample],
                &bytes[middle..middle + sample],
                &bytes[bytes.len() - sample..],
            ] {
                std::hash::Hasher::write(&mut hasher, part);
            }
            (bytes.len(), std::hash::Hasher::finish(&hasher))
        }

        /// The value made from bytes equal to `bytes`, if held.
        pub(crate) fn get(&self, bytes: &[u8]) -> Option<V> {
            self.by_key
                .get(&Self::key(bytes))?
                .iter()
                .find(|cached| *cached.bytes == *bytes)
                .map(|cached| cached.value.clone())
        }

        /// Hold `value`, made from `bytes`, charged the bytes, `value_charge`
        /// (its own heap) and the entry's bookkeeping, after the oldest
        /// entries make room. Not held when its key already has
        /// [`MAX_CANDIDATES_PER_KEY`] candidates, or when it alone is over
        /// the bound.
        pub(crate) fn insert(&mut self, bytes: Vec<u8>, value: V, value_charge: usize) {
            let key = Self::key(&bytes);
            let charge = bytes
                .len()
                .saturating_add(value_charge)
                .saturating_add(cached_entry_overhead::<V>());
            if charge > self.max_charge
                || self.max_entries == 0
                || self
                    .by_key
                    .get(&key)
                    .is_some_and(|candidates| candidates.len() >= MAX_CANDIDATES_PER_KEY)
            {
                return;
            }
            while self.held.saturating_add(charge) > self.max_charge
                || self.order.len() >= self.max_entries
            {
                let Some((key, oldest)) = self.order.pop_front() else {
                    break;
                };
                self.held -= oldest.charge;
                if let Some(candidates) = self.by_key.get_mut(&key) {
                    candidates.retain(|candidate| !Arc::ptr_eq(candidate, &oldest));
                    if candidates.is_empty() {
                        self.by_key.remove(&key);
                    }
                }
            }
            let cached = Arc::new(Cached {
                bytes: bytes.into_boxed_slice(),
                value,
                charge,
            });
            self.held += charge;
            self.by_key
                .entry(key)
                .or_insert_with(|| Vec::with_capacity(MAX_CANDIDATES_PER_KEY))
                .push(cached.clone());
            self.order.push_back((key, cached));
            self.peak = self.peak.max(self.retained());
        }

        /// What the cache retains now, by the bounds it is charged with: its
        /// entries and its spine as allocated (the queue's capacity and a
        /// node and a candidate list per key). Never more than the
        /// `max_retained` it was made with.
        pub(crate) fn retained(&self) -> usize {
            self.held
                .saturating_add(spine_of::<V>(self.order.capacity(), self.by_key.len()))
        }

        /// The most it has retained at once ([`ByteCache::retained`]), its
        /// spine included, since it was made or cleared.
        pub(crate) fn peak(&self) -> usize {
            self.peak
        }

        /// Drop every entry, the spine's allocations and the peak.
        pub(crate) fn clear(&mut self) {
            self.by_key = BTreeMap::new();
            self.order = VecDeque::new();
            self.held = 0;
            self.peak = 0;
        }

        /// What the entries are charged now, and how many there are.
        #[cfg(test)]
        pub(crate) fn held(&self) -> (usize, usize) {
            (self.held, self.order.len())
        }

        /// What the entries may be charged: `max_retained` less the spine.
        #[cfg(test)]
        pub(crate) fn max_charge(&self) -> usize {
            self.max_charge
        }
    }

    /// What a file's executable bytes hold, by file offset: the load of each
    /// exclusive pair and each EL0 counter read. A property of the file, not
    /// of how a process maps it, so one scan serves every mapping of it.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub(crate) struct FileLoops {
        /// The file offset of each pair's load-exclusive.
        pub(crate) pair_loads: Vec<u64>,
        /// The file offset of each counter read.
        pub(crate) counter_reads: Vec<u64>,
    }

    impl FileLoops {
        /// What a mapping of the file range `[start, end)` holds: each pair
        /// belongs to the mapping that holds its load-exclusive, so a loop
        /// split across two mappings is counted once, whatever is mapped
        /// after it, and whether or not the rewriter would take it.
        pub(crate) fn found_in(&self, start: u64, end: u64) -> Found {
            let inside = |offsets: &[u64]| {
                offsets
                    .iter()
                    .filter(|offset| start <= **offset && **offset < end)
                    .count() as u64
            };
            Found {
                pairs: inside(&self.pair_loads),
                counter_reads: inside(&self.counter_reads),
            }
        }
    }

    /// The loops of one mapping left as the file has them: the pairs its
    /// scan attributed to it (each pair belongs to the mapping holding its
    /// load, even when its store is in the next), less those rewritten or
    /// not written.
    pub(crate) fn left_in_mapping(pairs: u64, rewritten: u64, failed: u64) -> u64 {
        pairs.saturating_sub(rewritten + failed)
    }

    /// How far a file opened for a mapping is proven to be the mapped object.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Opened {
        /// Through `/proc/<tid>/map_files`: the mapped object itself.
        Exact,
        /// By name, with the mapping's inode and device.
        SameInodeAndDevice,
        /// By name, with the mapping's inode, on a filesystem whose maps
        /// device is not the one `stat` reports (btrfs, overlayfs): an inode
        /// number there is not an identity across subvolumes or layers.
        SameInodeOnly,
    }

    impl Opened {
        /// What an open proves, or `None` when it found another file.
        pub(crate) fn of(
            exact: bool,
            same_inode: bool,
            same_device: bool,
            device_unreliable: bool,
        ) -> Option<Self> {
            match () {
                _ if !same_inode => None,
                _ if exact => Some(Opened::Exact),
                _ if device_unreliable => Some(Opened::SameInodeOnly),
                _ if same_device => Some(Opened::SameInodeAndDevice),
                _ => None,
            }
        }

        /// Whether a plan from this file may decide what is written into the
        /// mapping. Counting accepts any of them; writing needs the mapped
        /// object proven, since the plan's refusals (branch targets,
        /// relocations) depend on the whole file.
        pub(crate) fn may_plan_writes(self) -> bool {
            self != Opened::SameInodeOnly
        }
    }

    /// What the in-memory rewrite does with one sequence, from what the
    /// guest's memory holds there.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        /// Write the replacement.
        Write,
        /// The replacement is there already.
        Done,
        /// Leave it: not the file's bytes, or code that may be running.
        Leave,
    }

    /// The step for a sequence whose memory holds `current`: written only
    /// where it still holds the file's bytes and no instruction of it can
    /// have run (`new`).
    pub(crate) fn step(current: &[u8], original: &[u8], rewritten: &[u8], new: bool) -> Step {
        if current == rewritten {
            Step::Done
        } else if current == original && new {
            Step::Write
        } else {
            Step::Leave
        }
    }

    /// The guest memory the in-memory rewrite reads and writes, as a trait so
    /// that the write protocol can be tested with injected failures.
    pub(crate) trait GuestCode {
        /// Read exactly `buf.len()` bytes at `address`.
        fn read(&self, address: u64, buf: &mut [u8]) -> std::io::Result<()>;
        /// Write all of `bytes` at `address`. An error may come after a
        /// prefix was written, as `/proc/<pid>/mem` writes page by page.
        fn write(&self, address: u64, bytes: &[u8]) -> std::io::Result<()>;
    }

    /// How writing one sequence ended.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Written {
        /// Memory holds the replacement, read back.
        Rewritten,
        /// Memory holds the original, as before: counted as not written.
        Failed(String),
        /// Memory holds neither, and the original could not be put back:
        /// the guest must not run.
        Broken(String),
    }

    /// Write `rewritten` over `original` at `address`, whole or not at all.
    ///
    /// The original bytes are written first: that makes Linux copy every
    /// page the sequence touches for this process, without changing what any
    /// of them holds, so a failure there (even after a prefix) leaves the
    /// loop as it was. Then the replacement is written and read back; on any
    /// error or other bytes, the original is written back and read back. The
    /// result says what memory holds, never what was intended.
    pub(crate) fn write_sequence(
        code: &impl GuestCode,
        address: u64,
        original: &[u8],
        rewritten: &[u8],
    ) -> Written {
        if let Err(error) = code.write(address, original) {
            return Written::Failed(format!("copying the pages failed: {error}"));
        }
        let holds = |expected: &[u8]| {
            let mut now = vec![0u8; expected.len()];
            code.read(address, &mut now).is_ok() && now == expected
        };
        let problem = match code.write(address, rewritten) {
            Ok(()) if holds(rewritten) => return Written::Rewritten,
            Ok(()) => "the replacement did not read back".to_owned(),
            Err(error) => format!("writing the replacement failed: {error}"),
        };
        // Read back whatever the restore reports: a write that fails part
        // way may still have put every byte back.
        let restore = code.write(address, original).err();
        if holds(original) {
            Written::Failed(problem)
        } else if holds(rewritten) {
            Written::Rewritten
        } else {
            Written::Broken(match restore {
                Some(error) => format!("{problem}, and writing the original back failed: {error}"),
                None => format!("{problem}, and the original did not read back"),
            })
        }
    }

    /// An executable file mapping a scan saw: the file's guest path and the
    /// mapping's guest address range.
    #[derive(
        Clone,
        Debug,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        serde::Serialize,
        serde::Deserialize
    )]
    pub struct LiveMapping {
        pub path: String,
        pub address: u64,
        pub len: u64,
    }

    /// One address space's counts for one file, by mapping: its guest
    /// address range (start, length).
    type RangeCounts = BTreeMap<(u64, u64), (Found, Rewrite)>;

    /// What a run's scans found, by guest path, address space and mapped
    /// file range.
    #[derive(Debug, Default)]
    pub(crate) struct PairTally {
        found: BTreeMap<String, BTreeMap<(String, u64), RangeCounts>>,
    }

    impl PairTally {
        /// Take one scan of address space `space`: `live` is every
        /// executable file mapping it saw (`None` when its maps could not be
        /// read), and `mappings` the outcomes of those it scanned. A mapping
        /// replaces what it overlaps in that address space, and an
        /// executable alias of the same file bytes elsewhere counts as its
        /// own, so the tally holds each address space's executable mappings
        /// as of its last scan.
        pub(crate) fn note(
            &mut self,
            space: &(String, u64),
            mappings: &[ScannedMapping],
            live: Option<&[LiveMapping]>,
        ) {
            // The executable file mappings the scan saw, by guest address
            // range: an earlier outcome for anything else in this address
            // space (unmapped, moved, split, no longer executable) is gone.
            if let Some(live) = live {
                for (path, spaces) in self.found.iter_mut() {
                    if let Some(ranges) = spaces.get_mut(space) {
                        ranges.retain(|&(address, len), _| {
                            live.iter().any(|seen| {
                                seen.path == *path && seen.address == address && seen.len == len
                            })
                        });
                    }
                }
            }
            for mapping in mappings {
                let ranges = self
                    .found
                    .entry(mapping.path.clone())
                    .or_default()
                    .entry(space.clone())
                    .or_default();
                // A mapping replaces what it overlaps in the address space (a
                // range split and rejoined by mprotect, or remapped), and
                // nothing else: an alias of the same file bytes elsewhere is
                // another mapping, with its own outcome.
                let end = mapping.address.saturating_add(mapping.len);
                ranges.retain(|&(from, length), _| {
                    end <= from || from.saturating_add(length) <= mapping.address
                });
                ranges.insert(
                    (mapping.address, mapping.len),
                    (mapping.found, mapping.rewrite),
                );
            }
        }

        /// The exclusive pairs of each guest path that has any.
        pub(crate) fn pairs_per_path(&self) -> BTreeMap<String, u64> {
            self.per_path(|(found, _)| found.pairs)
        }

        /// The EL0 counter reads of each guest path that has any.
        pub(crate) fn counter_reads_per_path(&self) -> BTreeMap<String, u64> {
            self.per_path(|(found, _)| found.counter_reads)
        }

        /// The in-memory rewrite of each guest path it touched: the sums over
        /// the ranges of one address space, the one that left or could not
        /// write the most (then the one with the most loops), so that the
        /// three counts describe one process and the worst exposure is the
        /// one shown.
        pub(crate) fn rewrites_per_path(&self) -> BTreeMap<String, Rewrite> {
            self.found
                .iter()
                .map(|(path, spaces)| {
                    let rewrite = spaces
                        .values()
                        .map(|ranges| {
                            ranges
                                .values()
                                .fold(Rewrite::default(), |sum, (_, rewrite)| sum.add(*rewrite))
                        })
                        .max_by_key(|r| (r.left + r.failed, r.rewritten + r.left + r.failed))
                        .unwrap_or_default();
                    (path.clone(), rewrite)
                })
                .filter(|(_, rewrite)| !rewrite.is_empty())
                .collect()
        }

        /// The exclusive loops of each guest path that can still retry, in the
        /// address space where the most can: what an in-memory rewrite left
        /// or could not write, and every pair where none was attempted. Every
        /// path with pairs or a rewrite outcome is considered.
        pub(crate) fn remaining_per_path(&self) -> BTreeMap<String, u64> {
            self.found
                .iter()
                .map(|(path, spaces)| {
                    let most = spaces
                        .values()
                        .map(|ranges| {
                            let pairs: u64 = ranges.values().map(|(found, _)| found.pairs).sum();
                            let rewrite = ranges
                                .values()
                                .fold(Rewrite::default(), |sum, (_, rewrite)| sum.add(*rewrite));
                            if rewrite.is_empty() {
                                pairs
                            } else {
                                rewrite.left + rewrite.failed
                            }
                        })
                        .max()
                        .unwrap_or(0);
                    (path.clone(), most)
                })
                .filter(|(_, count)| *count != 0)
                .collect()
        }

        /// One count of each guest path that has a nonzero one: the sum over
        /// the ranges one address space mapped, and the largest such sum over
        /// address spaces, so that a library many processes map counts once.
        /// A path is tallied when its mapping has either kind of finding, so
        /// the zero of the other kind is left out here.
        fn per_path(&self, of: impl Fn(&(Found, Rewrite)) -> u64) -> BTreeMap<String, u64> {
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
    use std::sync::Arc;
    use std::sync::Mutex;

    use detcore_model::summary::ExclusiveRewrite;
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
    use super::model::ByteCache;
    use super::model::CpuPmu;
    use super::model::FileLoops;
    use super::model::Found;
    use super::model::GuestCode;
    use super::model::LiveMapping;
    use super::model::Opened;
    use super::model::PairTally;
    use super::model::Placement;
    use super::model::Range;
    use super::model::Rewrite;
    use super::model::ScannedMapping;
    use super::model::Step;
    use super::model::Trigger;
    use super::model::Written;
    use super::model::arm_cpu_pmus;
    use super::model::combine;
    use super::model::covering;
    use super::model::crosses_page;
    use super::model::left_in_mapping;
    use super::model::merge_overlaps;
    use super::model::page_span;
    use super::model::place;
    use super::model::plan_charge;
    use super::model::sequence_is_new;
    use super::model::step;
    use super::model::was_executable;
    use super::model::write_sequence;
    use crate::Config;
    use crate::Detcore;
    use crate::RecordOrReplay;
    use crate::aarch64_lse::elf::FilePlan;
    use crate::procmaps::MMapPath;
    use crate::procmaps::MemoryMap;
    use crate::types::MmId;

    /// Whether this run reads its guests through host procfs: only where a
    /// guest thread is the host task of the same ID and Detcore runs outside
    /// it (ptrace), and not under `hermit record` or `hermit replay`, whose
    /// replayed guest maps placeholders.
    pub(crate) fn reads_guest_tasks(cfg: &Config) -> bool {
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
            /// What the scan found in each mapping it scanned that has
            /// anything.
            mappings: Vec<ScannedMapping>,
            /// The guest address range of every executable file mapping the
            /// scan saw; `None` when the maps could not be read.
            live: Option<Vec<LiveMapping>>,
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
        /// The guest paths this run has warned it could not rewrite.
        warned_rewrite_failures: BTreeSet<String>,
        /// Whether this run has said what rewriting in memory means for the
        /// guest.
        noted_rewrite: bool,
    }

    impl RunTally {
        /// The tally of a new run. The plan cache starts empty too, so that
        /// what the run plans and the peak its summary reports are the run's
        /// own, also when one Hermit process runs several (`analyze` trials).
        pub(crate) fn new_run() -> Self {
            PLANS.lock().unwrap().clear();
            Self::default()
        }

        /// Take one report.
        pub(crate) fn note(&mut self, cfg: &Config, report: &Report) {
            match report {
                Report::Attach { root } => {
                    if counts_store_exclusives(cfg) && self.counters.is_none() {
                        self.counters = Some(Counters::open(*root));
                    }
                }
                Report::Pairs {
                    space,
                    mappings,
                    live,
                } => {
                    for mapping in mappings {
                        if mapping.found.counter_reads != 0
                            && self.warned_counter_reads.insert(mapping.path.clone())
                        {
                            warn_counter_reads(&mapping.path, mapping.found.counter_reads);
                        }
                        if mapping.rewrite.rewritten != 0 && !self.noted_rewrite {
                            self.noted_rewrite = true;
                            info!(
                                "[store-exclusive] exclusive loops are rewritten in the guest's \
                                 memory, not in its files: a guest that reads its own code (to \
                                 checksum or verify it) sees single LSE atomics where its files \
                                 hold load/store-exclusive loops, and each rewritten page is a \
                                 private copy. The summary lists every rewritten image."
                            );
                        }
                        if mapping.rewrite.failed != 0
                            && self.warned_rewrite_failures.insert(mapping.path.clone())
                        {
                            warn!(
                                "[store-exclusive] the in-memory rewrite of {} did not complete: \
                                 {} not written (a mapping that could not be read counts one; \
                                 debug logs say why). Those loops still retry, and a recorded \
                                 schedule may not replay",
                                mapping.path, mapping.rewrite.failed
                            );
                        }
                    }
                    self.pairs.note(space, mappings, live.as_deref());
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
            summary.exclusive_rewrite_cache_peak = PLANS.lock().unwrap().peak() as u64;
            summary.counter_reads = self.pairs.counter_reads_per_path();
            summary.exclusive_rewrites = self
                .pairs
                .rewrites_per_path()
                .into_iter()
                .map(|(path, rewrite)| {
                    let Rewrite {
                        rewritten,
                        left,
                        failed,
                    } = rewrite;
                    (
                        path,
                        ExclusiveRewrite {
                            rewritten,
                            left,
                            failed,
                        },
                    )
                })
                .collect();
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
            // After an in-memory rewrite, only what it left or could not
            // write can still retry, in the process where the most can.
            let libraries: Vec<String> = self
                .pairs
                .remaining_per_path()
                .into_iter()
                .filter(|(_, pairs)| *pairs > 0)
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
        /// The executable file mappings, by guest address range, that the
        /// last scan of this address space reported: one not among them (new,
        /// or a piece of a split) is scanned whatever range triggered the
        /// scan.
        reported: BTreeSet<LiveMapping>,
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

        /// Whether `[start, end)` was executable at the last scan: then its
        /// code may already have run, and a thread may be stopped inside one
        /// of its sequences. Unknown ranges count as executable.
        fn was_executable(&self, range: Range) -> bool {
            was_executable(self.ranges.as_deref(), range)
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
                    // A fork's copy: the child's tally starts empty.
                    self.reported.clear();
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

    fn scan_mapping(tid: Pid, path: &Path, map: &MemoryMap, key: FileRange) -> Option<Found> {
        // The object the mapping maps, as the rewrite plans it. Nothing is
        // cached: the scan reads the file's code each time, so no identity
        // can carry one file's loops to another.
        let result = (|| {
            let (file, _) = open_mapped(tid, path, map, key)?;
            let stamp = file_stamp(&file)?;
            let loops = scan_file(&file, stamp.0)?;
            // Loops from bytes that changed while they were read are not
            // used.
            if file_stamp(&file)? != stamp {
                return Err(std::io::Error::other(
                    "the file changed while it was scanned",
                ));
            }
            Ok(loops.found_in(key.offset, key.offset.saturating_add(key.len)))
        })();
        result
            .map_err(|error: std::io::Error| {
                debug!(
                    "[store-exclusive] {} not scanned through /proc/{}/root: {error}",
                    path.display(),
                    tid.as_raw()
                );
            })
            .ok()
    }

    /// A file's size, modification time and status-change time.
    type PlanStamp = (u64, (i64, i64), (i64, i64));

    /// The stamp of an open file. The status-change time cannot be set from
    /// user space, so a file rewritten in place at the same size and with its
    /// modification time put back still gets a new stamp.
    fn file_stamp(file: &File) -> std::io::Result<PlanStamp> {
        let metadata = file.metadata()?;
        Ok((
            metadata.size(),
            (metadata.mtime(), metadata.mtime_nsec()),
            (metadata.ctime(), metadata.ctime_nsec()),
        ))
    }

    /// Open the file a mapping maps, for planning its rewrite.
    ///
    /// `/proc/<tid>/map_files/<start>-<end>` is the mapped object itself, so
    /// it is tried first; Linux opens it only for a tracer with
    /// `CAP_CHECKPOINT_RESTORE` or `CAP_SYS_ADMIN`. Otherwise the mapping's
    /// name is opened inside the guest's root and must lead to the same
    /// inode, and, outside btrfs (where a maps line names the superblock's
    /// device rather than the subvolume's) and overlayfs (where it names the
    /// layer's), the same device. What the open proves is returned with it:
    /// a scan accepts any, a rewrite plan only a proven one.
    fn open_mapped(
        tid: Pid,
        path: &Path,
        map: &MemoryMap,
        key: FileRange,
    ) -> std::io::Result<(File, Opened)> {
        let (start, end) = map.address;
        let exact = format!("/proc/{}/map_files/{start:x}-{end:x}", tid.as_raw());
        let (file, exact) = match File::open(&exact) {
            Ok(file) => (file, true),
            Err(_) => (open_in_root(tid, path)?, false),
        };
        let metadata = file.metadata()?;
        let (major, minor) = key.dev;
        let same_device = metadata.dev() == libc::makedev(major as u32, minor as u32);
        // SAFETY: an all-zero `statfs` is a valid buffer for fstatfs.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: a valid descriptor and a `statfs` buffer.
        // A filesystem that cannot be told counts as unreliable.
        let device_unreliable = unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0
            || [BTRFS_SUPER_MAGIC, OVERLAYFS_SUPER_MAGIC].contains(&(fs.f_type as i64));
        let opened = Opened::of(
            exact,
            metadata.ino() == key.inode,
            same_device,
            device_unreliable,
        )
        .ok_or_else(|| {
            std::io::Error::other("the name now leads to another file than the mapped one")
        })?;
        Ok((file, opened))
    }

    /// `BTRFS_SUPER_MAGIC` (include/uapi/linux/magic.h).
    const BTRFS_SUPER_MAGIC: i64 = 0x9123_683e;

    /// `OVERLAYFS_SUPER_MAGIC` (include/uapi/linux/magic.h).
    const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;

    /// The largest file planned in memory: the whole file is read, and a
    /// larger one is counted as not rewritten rather than risk the
    /// allocation.
    const MAX_PLANNED_FILE: u64 = 1 << 30;

    /// The rewrite plans this Hermit process has made, found by the exact
    /// bytes they were made from ([`ByteCache`]): a plan is a pure function of
    /// those bytes, so a hit is the plan planning them again would give, and
    /// no file identity (device, inode, path, time) or digest is trusted. It
    /// retains at most 128 MiB (a process on a small device maps tens of MB of
    /// code) and 1024 entries; anything past a bound only costs a re-plan.
    /// Each run starts it empty ([`RunTally::new_run`]): Hermit runs one
    /// guest at a time in a process (`--verify` runs each in its own,
    /// `analyze` its trials one after another). Runs that did overlap in one
    /// process would share it, and one's start would empty it under the
    /// other: that costs re-planning and the accuracy of the full report's
    /// peak, never a plan's bytes or the INFO view.
    static PLANS: Mutex<ByteCache<Arc<Option<FilePlan>>>> =
        Mutex::new(ByteCache::new(128 << 20, 1024));

    /// The rewrite plan of the file a mapping maps, from the mapped object's
    /// bytes, read afresh each time. The bytes are used only if the file kept
    /// its size, modification and status-change times (the last cannot be
    /// set from user space) while they were read; planning them is skipped
    /// when bytes equal to them were planned before ([`PLANS`]).
    fn plan_for(
        tid: Pid,
        path: &Path,
        map: &MemoryMap,
        key: FileRange,
    ) -> Result<Arc<Option<FilePlan>>, String> {
        let (file, opened) = open_mapped(tid, path, map, key).map_err(|error| error.to_string())?;
        if !opened.may_plan_writes() {
            return Err(
                "the mapped object cannot be proven without /proc/<pid>/map_files on this \
                 filesystem (btrfs or overlayfs, where an inode number is not an identity)"
                    .to_owned(),
            );
        }
        let stamp = file_stamp(&file).map_err(|error| error.to_string())?;
        let too_large =
            || format!("the file is larger than the {MAX_PLANNED_FILE} bytes planned in memory");
        if stamp.0 > MAX_PLANNED_FILE {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(stamp.0 as usize)
            .map_err(|error| error.to_string())?;
        // Read at most one byte past the bound, so a file that grows while it
        // is read can neither pass the bound nor grow the buffer unchecked.
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&file, MAX_PLANNED_FILE + 1),
            &mut bytes,
        )
        .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_PLANNED_FILE {
            return Err(too_large());
        }
        // The bytes read are the file the stamp names only if nothing
        // changed it meanwhile.
        if file_stamp(&file).map_err(|error| error.to_string())? != stamp
            || bytes.len() as u64 != stamp.0
        {
            return Err("the file changed while it was read".to_owned());
        }
        if let Some(plan) = PLANS.lock().unwrap().get(&bytes) {
            return Ok(plan);
        }
        let plan = Arc::new(
            crate::aarch64_lse::elf::plan_bytes(&bytes).map_err(|error| error.to_string())?,
        );
        let charge = plan_charge(&plan);
        PLANS.lock().unwrap().insert(bytes, plan.clone(), charge);
        Ok(plan)
    }

    /// The guest's memory through `/proc/<tid>/mem`.
    struct ProcMem(File);

    impl GuestCode for ProcMem {
        fn read(&self, address: u64, buf: &mut [u8]) -> std::io::Result<()> {
            self.0.read_exact_at(buf, address)
        }

        fn write(&self, address: u64, bytes: &[u8]) -> std::io::Result<()> {
            self.0.write_all_at(bytes, address)
        }
    }

    /// Rewrite the canonical exclusive loops of one executable file mapping
    /// in the guest's memory. `pairs` is what the scan found in it.
    ///
    /// A sequence is written only where the guest's memory still holds the
    /// file's bytes, only where no instruction of it can have run (`new` says
    /// so for its address range: made by this exec or this `mmap`), and only
    /// within one page. Memory that already holds the replacement counts as
    /// rewritten; anything else (relocated text, a debugger's breakpoint, a
    /// runtime that rewrote itself, text a page drop put back, or old code
    /// that may be running) is left.
    ///
    /// The write goes through `/proc/<tid>/mem`, which may write a private
    /// read-only page the way a debugger does: Linux copies the page for this
    /// process alone, so the file and every other process mapping it keep the
    /// original, and the kernel keeps the instruction cache coherent with the
    /// written page. A shared or writable mapping is left alone. Each
    /// sequence is written whole or not at all ([`write_sequence`]); if the
    /// guest's memory ends up holding neither, the run ends here.
    #[allow(clippy::too_many_arguments)]
    fn rewrite_mapping(
        tid: Pid,
        path: &Path,
        key: FileRange,
        map: &MemoryMap,
        pairs: u64,
        new: &dyn Fn(u64, u64) -> bool,
    ) -> (Rewrite, u64) {
        let all_left = Rewrite {
            left: pairs,
            ..Rewrite::default()
        };
        // A failure before the plan is placed counts at least one, even in a
        // mapping whose scan paired nothing: it may hold a split sequence.
        let all_failed = Rewrite {
            failed: pairs.max(1),
            ..Rewrite::default()
        };
        if map.perms.contains(MMPermissions::SHARED) || map.perms.contains(MMPermissions::WRITE) {
            debug!(
                "[store-exclusive] {} at {:#x} is shared or writable; its exclusive loops are left",
                path.display(),
                map.address.0
            );
            return (all_left, 0);
        }
        let plan = match plan_for(tid, path, map, key) {
            Ok(plan) => plan,
            Err(error) => {
                debug!(
                    "[store-exclusive] cannot read {} to plan its rewrite: {error}",
                    path.display()
                );
                return (all_failed, 0);
            }
        };
        let Some(plan) = plan.as_ref() else {
            return (all_left, 0);
        };
        let memory = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/proc/{}/mem", tid.as_raw()))
        {
            Ok(memory) => ProcMem(memory),
            Err(error) => {
                debug!(
                    "[store-exclusive] cannot open /proc/{}/mem to rewrite {}: {error}",
                    tid.as_raw(),
                    path.display()
                );
                return (all_failed, 0);
            }
        };
        let mut rewrite = Rewrite::default();
        let page = page_size();
        // Sequences that start here and run into the next mapping: left
        // whole. The scan counted each pair in the mapping that holds its
        // load, so they are among the `pairs` of this one or the next.
        let mut crossing = 0;
        for patch in &plan.patches {
            let address = match place(
                patch.offset,
                patch.original.len() as u64,
                map.address.0,
                key.offset,
                key.len,
            ) {
                Placement::Outside => continue,
                Placement::CrossesEnd => {
                    crossing += 1;
                    continue;
                }
                Placement::At(address) => address,
            };
            if crosses_page(address, patch.original.len() as u64, page) {
                debug!(
                    "[store-exclusive] the sequence at {address:#x} crosses a page; left, so that \
                     no page drop can leave half of it"
                );
                continue;
            }
            let mut current = vec![0u8; patch.original.len()];
            if let Err(error) = memory.read(address, &mut current) {
                debug!("[store-exclusive] cannot read the guest's code at {address:#x}: {error}");
                rewrite.failed += 1;
                continue;
            }
            let end = address + patch.original.len() as u64;
            match step(
                &current,
                &patch.original,
                &patch.rewritten,
                new(address, end),
            ) {
                Step::Done => {
                    rewrite.rewritten += 1;
                }
                Step::Leave => {
                    debug!(
                        "[store-exclusive] the guest's code at {address:#x} is not the file's, \
                         or may already have run; left"
                    );
                }
                Step::Write => {
                    match write_sequence(&memory, address, &patch.original, &patch.rewritten) {
                        Written::Rewritten => {
                            rewrite.rewritten += 1;
                        }
                        Written::Failed(why) => {
                            debug!(
                                "[store-exclusive] the guest's code at {address:#x} was not \
                                 rewritten: {why}"
                            );
                            rewrite.failed += 1;
                        }
                        Written::Broken(why) => panic!(
                            "the in-memory rewrite of {} left the guest's code at {address:#x} \
                             holding neither its loop nor the replacement: {why}",
                            path.display()
                        ),
                    }
                }
            }
        }
        // Every pair the scan found that was neither rewritten nor failed is
        // left: refused by the rewriter, not the file's bytes in memory, or
        // old code that may be running; and so is each sequence split across
        // this mapping's end.
        rewrite.left = left_in_mapping(pairs, rewrite.rewritten, rewrite.failed);
        (rewrite, crossing)
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
    fn scan_file(file: &File, size: u64) -> std::io::Result<FileLoops> {
        let ranges = code_ranges(file, size)?;
        // Bounded like a plan: the whole of a file's code is read, whatever
        // a process maps of it, so a huge file counts as not scanned.
        if ranges.iter().map(|(start, end)| end - start).sum::<u64>() > MAX_PLANNED_FILE {
            return Err(std::io::Error::other(format!(
                "its code is larger than the {MAX_PLANNED_FILE} bytes scanned"
            )));
        }
        let mut loops = FileLoops::default();
        for (start, end) in ranges {
            // A pair cannot span two sections.
            let mut section = CodeScan::at(start);
            read_words(file, start, end, &mut section)?;
            let found = section.into_loops();
            loops.pair_loads.extend(found.pair_loads);
            loops.counter_reads.extend(found.counter_reads);
        }
        Ok(loops)
    }

    /// The file ranges that hold instructions: the file's executable ELF
    /// sections, or the whole file when it has no section headers. Scanned
    /// whole, whatever a process maps of them.
    fn code_ranges(file: &File, size: u64) -> std::io::Result<Vec<(u64, u64)>> {
        let mut header = [0u8; 64];
        let table = match file.read_exact_at(&mut header, 0) {
            Ok(()) => elf_section_table(&header),
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => None,
            Err(error) => return Err(error),
        };
        let Some((offset, count)) = table else {
            return Ok(vec![(0, size)]);
        };
        let mut table = vec![0u8; count * 64];
        file.read_exact_at(&mut table, offset)?;
        Ok(merge_overlaps(
            executable_sections(&table)
                .into_iter()
                .map(|(offset, length)| (offset, offset.saturating_add(length).min(size)))
                .filter(|(start, end)| start < end)
                .collect(),
        ))
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

    /// The name the summary and the warning give a scan whose maps could not
    /// be read.
    const UNREADABLE_MAPS: &str =
        "(the executable mappings of a process whose maps could not be read)";

    /// Scan a thread's executable file mappings that overlap `range` (all of
    /// them for `None`): log each with pairs, or that could not be scanned,
    /// at INFO, and report the pairs to the run's tally.
    async fn report_pairs<G, T>(guest: &mut G, range: Option<(u64, u64)>, trigger: Trigger)
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
                // Fail closed: a run that asked for the rewrite is warned
                // and told in its summary that this scan could not happen,
                // so whatever was mapped may still retry.
                if guest.config().rewrite_exclusives_in_memory {
                    let space = {
                        let thread = guest.thread_state();
                        let mut metadata = thread.memory_metadata.lock().unwrap();
                        metadata
                            .exclusive_scan
                            .space(thread.mm_id, || thread.pedigree.to_string())
                    };
                    let mappings = vec![ScannedMapping {
                        path: UNREADABLE_MAPS.to_owned(),
                        address: 0,
                        offset: 0,
                        len: 0,
                        found: Found::default(),
                        rewrite: Rewrite {
                            failed: 1,
                            ..Rewrite::default()
                        },
                    }];
                    send(
                        guest,
                        Report::Pairs {
                            space,
                            mappings,
                            live: None,
                        },
                    )
                    .await;
                }
                return;
            }
        };
        let live: Vec<LiveMapping> = maps
            .iter()
            .filter(|map| map.perms.contains(MMPermissions::EXECUTE))
            .filter_map(|map| match &map.pathname {
                MMapPath::Path(path) => Some(LiveMapping {
                    path: path.display().to_string(),
                    address: map.address.0,
                    len: map.address.1 - map.address.0,
                }),
                _ => None,
            })
            .collect();
        let (space, reported) = {
            let thread = guest.thread_state();
            let mut metadata = thread.memory_metadata.lock().unwrap();
            metadata.exclusive_scan.reseed(&maps);
            let space = metadata
                .exclusive_scan
                .space(thread.mm_id, || thread.pedigree.to_string());
            let reported = std::mem::replace(
                &mut metadata.exclusive_scan.reported,
                live.iter().cloned().collect(),
            );
            (space, reported)
        };
        let mut mappings = Vec::new();
        for map in &maps {
            let MMapPath::Path(path) = &map.pathname else {
                continue;
            };
            let (start, end) = map.address;
            // The mappings the triggering call touched, and any the tally
            // does not hold yet (new to this address space, or a piece of a
            // mapping a call split).
            let seen = LiveMapping {
                path: path.display().to_string(),
                address: start,
                len: end - start,
            };
            if !map.perms.contains(MMPermissions::EXECUTE)
                || (range.is_some_and(|(from, to)| end <= from || to <= start)
                    && reported.contains(&seen))
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
            let in_memory = guest.config().rewrite_exclusives_in_memory;
            match scan_mapping(guest.tid(), path, map, key) {
                Some(found) => {
                    if found != Found::default() {
                        // Only what was found: a mapping with pairs alone
                        // keeps the line it always had.
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
                            "[detcore, dtid {}] {} in executable mapping {} (offset {:#x}, {:#x} \
                             bytes)",
                            dettid, what, shown, key.offset, key.len
                        );
                    }
                    // Every executable mapping is planned, even one whose scan
                    // paired nothing: a loop split across two mappings pairs
                    // in neither, and is counted as left where it starts.
                    let (rewrite, split) = if in_memory {
                        // Whether no instruction of `[start, end)` can have
                        // run. Decided per sequence: the kernel may merge a
                        // new mapping with an older neighbour into one entry.
                        let new =
                            |start: u64, end: u64| sequence_is_new(trigger, range, (start, end));
                        rewrite_mapping(guest.tid(), path, key, map, found.pairs, &new)
                    } else {
                        (Rewrite::default(), 0)
                    };
                    if !rewrite.is_empty() {
                        // Named only when there are any, so the line is
                        // otherwise the one it always was.
                        let split = if split == 0 {
                            String::new()
                        } else {
                            format!(" ({} running past the mapping's end)", split)
                        };
                        info!(
                            "[detcore, dtid {}] rewrote {} of the {} in memory in {} (offset \
                             {:#x}): {} left as the file has them{}, {} could not be written",
                            dettid,
                            rewrite.rewritten,
                            counted(found.pairs, "store-exclusive (LL/SC) pair"),
                            shown,
                            key.offset,
                            rewrite.left,
                            split,
                            rewrite.failed
                        );
                    }
                    if found != Found::default() || !rewrite.is_empty() {
                        mappings.push(ScannedMapping {
                            path: shown,
                            address: start,
                            offset: key.offset,
                            len: key.len,
                            found,
                            rewrite,
                        });
                    }
                }
                None => {
                    info!(
                        "[detcore, dtid {}] store-exclusive (LL/SC) pairs not scanned in \
                         executable mapping {} (offset {:#x}, {:#x} bytes)",
                        dettid, shown, key.offset, key.len
                    );
                    // Fail closed: a run that asked for the rewrite is told
                    // about a mapping it could not even read, whose loops, if
                    // it has any, still retry.
                    if in_memory {
                        mappings.push(ScannedMapping {
                            path: shown,
                            address: start,
                            offset: key.offset,
                            len: key.len,
                            found: Found::default(),
                            rewrite: Rewrite {
                                failed: 1,
                                ..Rewrite::default()
                            },
                        });
                    }
                }
            }
        }
        send(
            guest,
            Report::Pairs {
                space,
                mappings,
                live: Some(live),
            },
        )
        .await;
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
            report_pairs(guest, None, Trigger::Exec).await;
        }
    }

    /// After a successful syscall: the address range to scan, for an `mmap`
    /// that maps `PROT_EXEC` from a file, or an `mprotect` that adds
    /// `PROT_EXEC` where a file mapping may not be scanned yet. Anything
    /// else reads no maps: a file mapping made without `PROT_EXEC` is only
    /// recorded, and `PROT_EXEC` on anonymous memory (a JIT) is skipped.
    pub(crate) fn range_to_scan<G, T>(
        guest: &G,
        call: &Syscall,
        result: i64,
    ) -> Option<(Range, Trigger)>
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        if !reads_guest_tasks(guest.config()) {
            return None;
        }
        let page = page_size();
        let span = |start: u64, len: usize| page_span(start, len as u64, page);
        let metadata = &guest.thread_state().memory_metadata;
        // Rewriting in memory, every transition to executable is rescanned,
        // so that the counts follow a range made writable and executable
        // again (it is recounted, never written); otherwise only a range
        // that may hold a mapping not scanned.
        let in_memory = guest.config().rewrite_exclusives_in_memory;
        let made_executable = |start: Option<u64>, len: usize| {
            let range = span(start.unwrap_or(0), len);
            let needed = in_memory || metadata.lock().unwrap().exclusive_scan.needs_scan(range);
            needed.then_some((range, Trigger::Protected))
        };
        // Whether executable file text was in a range at the last scan: a
        // call that unmaps, moves, replaces or protects it changes the
        // tally, and the rescan that follows (which never writes) lets it
        // drop what is gone. Every mode, so that a count-only tally follows
        // too; anywhere else, nothing to do.
        let was_executable = |range: Range| {
            metadata
                .lock()
                .unwrap()
                .exclusive_scan
                .was_executable(range)
        };
        let changed = |range: Range| was_executable(range).then_some((range, Trigger::Changed));
        match call {
            Syscall::Mmap(mmap) => {
                let range = span(result as u64, mmap.len());
                let file = !mmap.flags().contains(MapFlags::MAP_ANONYMOUS) && mmap.fd() >= 0;
                if file && mmap.prot().contains(ProtFlags::PROT_EXEC) {
                    return Some((range, Trigger::Mapped));
                }
                if file {
                    metadata.lock().unwrap().exclusive_scan.add_unscanned(range);
                }
                // A fixed mapping that replaced executable text.
                changed(range)
            }
            // Executable text moved away, or a fixed destination replaced
            // some. The rescan of the new range also learns the address
            // space's ranges afresh, so a later page drop is judged by where
            // the text is now.
            Syscall::Mremap(mremap) => {
                let old = span(
                    mremap.addr().map_or(0, |addr| addr.as_raw() as u64),
                    mremap.old_len(),
                );
                let new = span(result as u64, mremap.new_len());
                (was_executable(old) || was_executable(new)).then_some((new, Trigger::Changed))
            }
            Syscall::Munmap(munmap) => changed(span(
                munmap.addr().map_or(0, |addr| addr.as_raw() as u64),
                munmap.len(),
            )),
            Syscall::Mprotect(mprotect) if mprotect.protection().contains(ProtFlags::PROT_EXEC) => {
                made_executable(
                    mprotect.addr().map(|addr| addr.as_raw() as u64),
                    mprotect.len(),
                )
            }
            Syscall::PkeyMprotect(mprotect) if mprotect.prot() & libc::PROT_EXEC != 0 => {
                made_executable(
                    mprotect.addr().map(|addr| addr.as_raw() as u64),
                    mprotect.len(),
                )
            }
            // Text no longer executable.
            Syscall::Mprotect(mprotect) => changed(span(
                mprotect.addr().map_or(0, |addr| addr.as_raw() as u64),
                mprotect.len(),
            )),
            Syscall::PkeyMprotect(mprotect) => changed(span(
                mprotect.addr().map_or(0, |addr| addr.as_raw() as u64),
                mprotect.len(),
            )),
            // Dropping a private file mapping's pages puts the file's bytes
            // back, undoing an in-memory rewrite there: the scan recounts
            // those loops as left. Only where an executable file mapping
            // was: an allocator's frequent drops of anonymous memory need no
            // scan. (MADV_REMOVE fails on a private mapping; it is listed so
            // that a shared one is recounted too.)
            Syscall::Madvise(madvise)
                if guest.config().rewrite_exclusives_in_memory
                    && matches!(
                        madvise.advice(),
                        libc::MADV_DONTNEED | libc::MADV_REMOVE | MADV_DONTNEED_LOCKED
                    ) =>
            {
                let start = madvise.addr().map_or(0, |addr| addr.as_raw() as u64);
                let range = span(start, madvise.len());
                let executable = metadata
                    .lock()
                    .unwrap()
                    .exclusive_scan
                    .was_executable(range);
                executable.then_some((range, Trigger::Dropped))
            }
            _ => None,
        }
    }

    /// The host's page size, which is the guest's.
    fn page_size() -> u64 {
        // SAFETY: sysconf has no preconditions.
        match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
            size if size > 0 => size as u64,
            _ => 4096,
        }
    }

    /// `MADV_DONTNEED_LOCKED` (Linux 5.18), which the `libc` crate does not
    /// name on every target.
    const MADV_DONTNEED_LOCKED: i32 = 24;

    /// After a [`range_to_scan`]: the executable file mappings in that range.
    pub(crate) async fn report_new_mapping<G, T>(guest: &mut G, (range, trigger): (Range, Trigger))
    where
        G: Guest<Detcore<T>>,
        T: RecordOrReplay,
    {
        report_pairs(guest, Some(range), trigger).await;
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
    use super::model::ByteCache;
    use super::model::CpuPmu;
    use super::model::Found;
    use super::model::GuestCode;
    use super::model::LiveMapping;
    use super::model::Opened;
    use super::model::PairTally;
    use super::model::Placement;
    use super::model::Rewrite;
    use super::model::ScannedMapping;
    use super::model::Step;
    use super::model::Trigger;
    use super::model::Written;
    use super::model::arm_cpu_pmus;
    use super::model::cached_entry_overhead;
    use super::model::combine;
    use super::model::covering;
    use super::model::crosses_page;
    use super::model::left_in_mapping;
    use super::model::merge_overlaps;
    use super::model::page_span;
    use super::model::parse_cpu_list;
    use super::model::place;
    use super::model::plan_charge;
    use super::model::sequence_is_new;
    use super::model::spine_bound;
    use super::model::step;
    use super::model::was_executable;
    use super::model::write_sequence;

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
            // Each test mapping at its own address, one per file offset.
            address: 0x40_0000 + offset,
            offset,
            len,
            found: Found {
                pairs,
                counter_reads: 0,
            },
            rewrite: Rewrite::default(),
        }
    }

    #[test]
    fn tallies_pairs_per_address_space_and_path() {
        let libc = "/lib64/libc.so.6".to_owned();
        let parent = ("0".to_owned(), 1);
        let child = ("01".to_owned(), 0);
        let mut tally = PairTally::default();
        tally.note(&parent, &[mapping(&libc, 0, 0x1000, 19)], None);
        // Two ranges of one file in one address space add up.
        tally.note(&parent, &[mapping(&libc, 0x4000, 0x1000, 2)], None);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // The same ranges in another address space count once.
        tally.note(&child, &[mapping(&libc, 0, 0x1000, 19)], None);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // mprotect split the first range and rejoined it: the new ranges
        // replace the ones they overlap.
        tally.note(
            &parent,
            &[
                mapping(&libc, 0, 0x800, 12),
                mapping(&libc, 0x800, 0x800, 7),
            ],
            None,
        );
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        tally.note(&parent, &[mapping(&libc, 0, 0x1000, 19)], None);
        assert_eq!(tally.pairs_per_path()[&libc], 21);
        // A later image of the same process is another address space.
        tally.note(
            &(parent.0.clone(), 2),
            &[mapping(&libc, 0, 0x1000, 19)],
            None,
        );
        assert_eq!(tally.pairs_per_path()[&libc], 21);
    }

    /// The tally holds each address space's executable mappings as of its
    /// last scan: a scan names every executable file mapping it saw, and an
    /// outcome for anything else there (unmapped, moved, split, no longer
    /// executable) is dropped, while a scan whose maps could not be read
    /// drops nothing.
    #[test]
    fn the_tally_holds_each_address_spaces_mappings_as_of_its_last_scan() {
        let libc = "/lib64/libc.so.6";
        let space = ("0".to_owned(), 1);
        let left = |offset, pairs| ScannedMapping {
            rewrite: Rewrite {
                left: pairs,
                ..Rewrite::default()
            },
            ..mapping(libc, offset, 0x1000, pairs)
        };
        let whole = ScannedMapping {
            len: 0x3000,
            ..left(0, 3)
        };
        let base = whole.address;
        let seen = |path: &str, address, len| LiveMapping {
            path: path.to_owned(),
            address,
            len,
        };
        let mut tally = PairTally::default();
        tally.note(&space, &[whole], Some(&[seen(libc, base, 0x3000)]));
        assert_eq!(tally.remaining_per_path().get(libc), Some(&3));
        // An mprotect of the middle page splits the mapping in three; the
        // scan that follows reports every piece the tally does not hold, so
        // the outer pieces are not lost to the middle one.
        let pieces = [
            seen(libc, base, 0x1000),
            seen(libc, base + 0x1000, 0x1000),
            seen(libc, base + 0x2000, 0x1000),
        ];
        tally.note(
            &space,
            &[left(0, 1), left(0x1000, 1), left(0x2000, 1)],
            Some(&pieces),
        );
        assert_eq!(tally.remaining_per_path().get(libc), Some(&3));
        assert_eq!(tally.pairs_per_path().get(libc), Some(&3));
        // Maps that could not be read leave the tally as it was.
        tally.note(&space, &[], None);
        assert_eq!(tally.remaining_per_path().get(libc), Some(&3));
        // Unmapped, and another file mapped at the same address: a later
        // scan of the address space no longer sees it.
        tally.note(
            &space,
            &[mapping("/bin/other", 0, 0x1000, 1)],
            Some(&[seen("/bin/other", base, 0x1000)]),
        );
        assert_eq!(tally.remaining_per_path().get(libc), None);
        assert_eq!(tally.pairs_per_path().get(libc), None);
        // Another address space is not touched by this one's scans.
        let mut two = PairTally::default();
        two.note(&("01".to_owned(), 0), &[left(0, 2)], None);
        two.note(&space, &[], Some(&[]));
        assert_eq!(two.remaining_per_path().get(libc), Some(&2));
    }

    /// The plan cache returns a value only for bytes equal to those it was
    /// made from, even when the sampled key matches, and never passes its
    /// bounds on the bytes, values and bookkeeping charged, on entries, or on
    /// candidates per key: the oldest entries go before a new one is added.
    #[test]
    fn the_plan_cache_answers_only_for_equal_bytes_and_stays_bounded() {
        // 20,000 bytes: the key samples 0..4096, 7952..12048 and
        // 15904..20000, so bytes 5000 to 7000 are not sampled.
        let a = vec![7u8; 20_000];
        let variant = |at: usize| {
            let mut bytes = a.clone();
            bytes[at] ^= 1;
            bytes
        };
        let entry = 20_000 + cached_entry_overhead::<u32>();
        let max = 6 * entry;
        let spine = spine_bound::<u32>(16);
        let mut cache = ByteCache::<u32>::new(max + spine, 16);
        assert_eq!(cache.max_charge(), max, "the spine is set aside");
        let within = |cache: &ByteCache<u32>| {
            let (held, entries) = cache.held();
            assert!(held <= max && entries <= 16);
            assert!(cache.retained() <= max + spine && cache.peak() <= max + spine);
            assert!(cache.peak() >= cache.retained());
        };
        cache.insert(a.clone(), 1, 0);
        assert_eq!(cache.get(&a), Some(1));
        assert_eq!(cache.get(&variant(5000)), None, "same key, other bytes");
        for (value, at) in [(2, 5000), (3, 6000), (4, 7000)] {
            cache.insert(variant(at), value, 0);
            within(&cache);
        }
        assert_eq!(cache.get(&variant(6000)), Some(3));
        // A fifth candidate for the key is not held.
        cache.insert(variant(5001), 9, 0);
        assert_eq!(cache.get(&variant(5001)), None);
        assert_eq!(cache.held(), (4 * entry, 4));
        // The value's heap counts: room is made first, oldest first, and the
        // bound is never passed.
        let big = vec![1u8; 20_001];
        cache.insert(big.clone(), 10, 2 * entry);
        within(&cache);
        assert_eq!((cache.get(&a), cache.get(&variant(5000))), (None, None));
        assert_eq!(cache.get(&big), Some(10));
        assert_eq!(cache.held(), (5 * entry + 1, 3));
        // The peak counts the spine too: the most retained so far is now.
        assert!(cache.retained() > 5 * entry + 1);
        assert_eq!(cache.peak(), cache.retained());
        // Exactly at the bound is allowed; one past it by itself is not held.
        let fill = vec![3u8; 20_002];
        cache.insert(
            fill.clone(),
            11,
            max - 20_002 - cached_entry_overhead::<u32>(),
        );
        assert_eq!((cache.get(&fill), cache.held()), (Some(11), (max, 1)));
        cache.insert(vec![2u8; 100], 12, max);
        assert_eq!(cache.get(&[2u8; 100]), None);
        within(&cache);
        // The entry bound: many small files.
        let mut small = ByteCache::<u32>::new(usize::MAX, 3);
        for n in 0..10u8 {
            small.insert(vec![n; 10 + usize::from(n)], u32::from(n), 0);
            assert!(small.held().1 <= 3);
        }
        assert_eq!(small.get(&[9u8; 19]), Some(9));
        assert_eq!(small.get(&[0u8; 10]), None);
        // A new run starts empty, with no peak, its spine released.
        small.clear();
        assert_eq!(
            (small.held(), small.peak(), small.retained()),
            ((0, 0), 0, 0)
        );
        assert_eq!(small.get(&[9u8; 19]), None);
        // And starts counting afresh.
        small.insert(vec![5u8; 7], 5, 0);
        let fresh = 7 + cached_entry_overhead::<u32>();
        assert_eq!(small.held(), (fresh, 1));
        assert_eq!(small.peak(), small.retained());
        assert!(small.retained() > fresh);
        // A bound the spine alone would fill holds nothing.
        let mut tiny = ByteCache::<u32>::new(spine_bound::<u32>(8), 8);
        tiny.insert(vec![1u8; 1], 1, 0);
        assert_eq!((tiny.max_charge(), tiny.held()), (0, (0, 0)));
    }

    /// A plan is charged for what it holds, by capacity, not length: spare
    /// room in its vectors counts.
    #[test]
    fn a_cached_plan_is_charged_its_allocations() {
        use std::mem::size_of;

        use crate::aarch64_lse::elf::FilePatch;
        use crate::aarch64_lse::elf::FilePlan;
        let mut patches = Vec::with_capacity(10);
        patches.push(FilePatch {
            offset: 0,
            original: Vec::with_capacity(64),
            rewritten: vec![0; 4],
        });
        let mut plan = FilePlan {
            patches,
            ..FilePlan::default()
        };
        plan.report.patched.insert("rmw", 1);
        plan.report.refused_examples = Vec::with_capacity(3);
        plan.report
            .refused_examples
            .push((0x10, String::with_capacity(40)));
        let shared = 2 * size_of::<usize>() + size_of::<Option<FilePlan>>();
        let expected = shared
            + 10 * size_of::<FilePatch>()
            + 64
            + 4
            + 3 * size_of::<(u64, String)>()
            + 40
            + 1024;
        assert_eq!(plan_charge(&Some(plan)), expected);
        assert_eq!(plan_charge(&None), shared);
    }

    /// The spine's bound is the queue at twice the entries (at least 4
    /// slots), a node and a 4-candidate list per entry, and saturates.
    #[test]
    fn the_plan_cache_spine_bound_is_exact_and_saturates() {
        use std::mem::size_of;
        let arc = size_of::<usize>();
        let slot = size_of::<(usize, u64)>() + arc;
        let per_key = 1024 + 4 * arc;
        assert_eq!(spine_bound::<u32>(16), 32 * slot + 16 * per_key);
        assert_eq!(spine_bound::<u32>(1), 4 * slot + per_key);
        assert_eq!(spine_bound::<u32>(0), 4 * slot);
        assert_eq!(spine_bound::<u32>(usize::MAX), usize::MAX);
        assert_eq!(ByteCache::<u32>::new(usize::MAX, usize::MAX).held(), (0, 0));
    }

    #[test]
    fn merges_overlapping_sections_but_keeps_touching_ones_apart() {
        assert_eq!(
            merge_overlaps(vec![
                (0x200, 0x300),
                (0x100, 0x250),
                (0x300, 0x400),
                (0x120, 0x130)
            ]),
            [(0x100, 0x300), (0x300, 0x400)]
        );
        assert!(merge_overlaps(Vec::new()).is_empty());
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
            address: 0x7000_0000,
            offset: 0,
            len: 0x1000,
            found: Found {
                pairs: 0,
                counter_reads: 1,
            },
            rewrite: Rewrite::default(),
        };
        tally.note(
            &space,
            &[linker, mapping("/lib64/libc.so.6", 0, 0x1000, 3)],
            None,
        );
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

    /// A sequence is new, so the in-memory rewrite may write it, only at the
    /// exec stop and inside the range an mmap just made.
    #[test]
    fn only_a_sequence_no_instruction_of_which_can_have_run_is_new() {
        let seq = |start: u64| (start, start + 24);
        assert!(sequence_is_new(Trigger::Exec, None, seq(0x1000)));
        // An mmap: inside its range only. The kernel can merge the new
        // mapping with an older neighbour into one maps entry, whose older
        // part may have run.
        let mapped = Some((0x2000, 0x4000));
        assert!(sequence_is_new(Trigger::Mapped, mapped, seq(0x2100)));
        assert!(!sequence_is_new(Trigger::Mapped, mapped, seq(0x1f00)));
        assert!(!sequence_is_new(Trigger::Mapped, mapped, seq(0x3ff0)));
        // Text made executable by mprotect may have run before, through
        // permission changes and moves; text a drop put back has. Never new.
        for trigger in [Trigger::Protected, Trigger::Dropped, Trigger::Changed] {
            assert!(!sequence_is_new(trigger, mapped, seq(0x2100)));
        }
    }

    /// The memory decides the step: the file's bytes are written only when
    /// new, the replacement is kept, and anything else is left, including
    /// the file's bytes in old text (guest-modified, or put back by a drop).
    #[test]
    fn writes_the_files_bytes_only_when_new() {
        let (original, rewritten, other) =
            (&[1u8, 2, 3, 4][..], &[9u8, 9, 9, 9][..], &[0u8; 4][..]);
        assert_eq!(step(original, original, rewritten, true), Step::Write);
        assert_eq!(step(original, original, rewritten, false), Step::Leave);
        for new in [true, false] {
            assert_eq!(step(rewritten, original, rewritten, new), Step::Done);
            assert_eq!(step(other, original, rewritten, new), Step::Leave);
        }
    }

    /// A file opened by name proves the mapped object by inode and device;
    /// where the maps device is not stat's (btrfs, overlayfs) the inode alone
    /// is enough to count but never to plan a write.
    #[test]
    fn only_a_proven_mapped_object_plans_a_write() {
        for exact in [false, true] {
            for same_device in [false, true] {
                for unreliable in [false, true] {
                    assert_eq!(Opened::of(exact, false, same_device, unreliable), None);
                }
            }
        }
        assert_eq!(Opened::of(true, true, false, true), Some(Opened::Exact));
        assert_eq!(
            Opened::of(false, true, true, false),
            Some(Opened::SameInodeAndDevice)
        );
        assert_eq!(Opened::of(false, true, false, false), None);
        for same_device in [false, true] {
            assert_eq!(
                Opened::of(false, true, same_device, true),
                Some(Opened::SameInodeOnly)
            );
        }
        assert!(Opened::Exact.may_plan_writes());
        assert!(Opened::SameInodeAndDevice.may_plan_writes());
        assert!(!Opened::SameInodeOnly.may_plan_writes());
    }

    /// A page drop is rescanned where executable file text was, and
    /// everywhere once the ranges are unknown (as after a move), so a drop at
    /// a moved mapping's new address is never missed.
    #[test]
    fn a_drop_is_rescanned_wherever_text_may_be() {
        let known = [(0x1000, 0x3000, true), (0x3000, 0x4000, false)];
        assert!(was_executable(Some(&known), (0x2000, 0x2001)));
        assert!(!was_executable(Some(&known), (0x3000, 0x4000)));
        assert!(!was_executable(Some(&known), (0x9000, 0xa000)));
        assert!(was_executable(None, (0x9000, 0xa000)));
    }

    /// A sequence within one page is written; one touching two is left, so
    /// that no page drop can leave half of it. Page-taking calls act on whole
    /// pages.
    #[test]
    fn writes_only_within_a_page_and_spans_whole_pages() {
        assert!(!crosses_page(0x1000, 24, 0x1000));
        assert!(!crosses_page(0x1fe8, 24, 0x1000), "ends at the last byte");
        assert!(crosses_page(0x1fec, 24, 0x1000));
        assert!(!crosses_page(0x1fec, 24, 0x10000));
        assert_eq!(page_span(0x1000, 0x1001, 0x1000), (0x1000, 0x3000));
        assert_eq!(page_span(0x1000, 0x1000, 0x1000), (0x1000, 0x2000));
        assert_eq!(page_span(0x1000, 0, 0x1000), (0x1000, 0x2000));
        assert_eq!(page_span(0x10000, 1, 0x10000), (0x10000, 0x20000));
    }

    /// How a write to [`FakeCode`] goes wrong.
    #[derive(Clone, Copy)]
    enum Fault {
        /// Writes up to the first page boundary after the start, then fails,
        /// as a `/proc/<pid>/mem` write does when a later page fails.
        AfterFirstPage,
        /// Writes everything, but one byte reads back changed.
        ReadsBackChanged,
    }

    /// Guest code in a buffer of pages, whose `n`th write can be made to fail.
    struct FakeCode {
        bytes: std::cell::RefCell<Vec<u8>>,
        page: usize,
        writes: std::cell::Cell<usize>,
        faults: Vec<(usize, Fault)>,
    }

    impl FakeCode {
        fn new(bytes: &[u8], faults: Vec<(usize, Fault)>) -> Self {
            FakeCode {
                bytes: std::cell::RefCell::new(bytes.to_vec()),
                page: 8,
                writes: std::cell::Cell::new(0),
                faults,
            }
        }
    }

    impl GuestCode for FakeCode {
        fn read(&self, address: u64, buf: &mut [u8]) -> std::io::Result<()> {
            let at = address as usize;
            buf.copy_from_slice(&self.bytes.borrow()[at..at + buf.len()]);
            Ok(())
        }

        fn write(&self, address: u64, bytes: &[u8]) -> std::io::Result<()> {
            let n = self.writes.get();
            self.writes.set(n + 1);
            let at = address as usize;
            let fault = self
                .faults
                .iter()
                .find(|(nth, _)| *nth == n)
                .map(|(_, fault)| *fault);
            let mut memory = self.bytes.borrow_mut();
            match fault {
                None => {
                    memory[at..at + bytes.len()].copy_from_slice(bytes);
                    Ok(())
                }
                Some(Fault::AfterFirstPage) => {
                    let prefix = (at / self.page + 1) * self.page - at;
                    memory[at..at + prefix].copy_from_slice(&bytes[..prefix]);
                    Err(std::io::Error::from_raw_os_error(libc::ENOMEM))
                }
                Some(Fault::ReadsBackChanged) => {
                    memory[at..at + bytes.len()].copy_from_slice(bytes);
                    memory[at] ^= 0xff;
                    Ok(())
                }
            }
        }
    }

    /// A sequence is written whole or not at all, and the result says what
    /// memory holds: a failure part-way, on either write, ends with the
    /// original back in place, and only an original that cannot be put back
    /// is reported as broken.
    #[test]
    fn a_sequence_is_written_whole_or_restored_and_says_what_memory_holds() {
        // A 12-byte sequence at 4 in pages of 8 bytes: it crosses a page.
        let image = [0u8, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let original = &image[4..16];
        let rewritten = [0xau8; 12];
        let run = |faults: Vec<(usize, Fault)>| {
            let code = FakeCode::new(&image, faults);
            let written = write_sequence(&code, 4, original, &rewritten);
            let holds = code.bytes.borrow()[4..16].to_vec();
            (written, holds)
        };
        let failed = |written: &Written| matches!(written, Written::Failed(_));
        let broken = |written: &Written| matches!(written, Written::Broken(_));

        assert_eq!(run(vec![]), (Written::Rewritten, rewritten.to_vec()));
        // Copying the pages fails after the first: nothing changed.
        let (written, holds) = run(vec![(0, Fault::AfterFirstPage)]);
        assert!(failed(&written), "{written:?}");
        assert_eq!(holds, original);
        // The replacement fails on its second page, or reads back changed:
        // the original is written back.
        for fault in [Fault::AfterFirstPage, Fault::ReadsBackChanged] {
            let (written, holds) = run(vec![(1, fault)]);
            assert!(failed(&written), "{written:?}");
            assert_eq!(holds, original);
        }
        // A restore that reports an error may still have put every byte
        // back: what memory holds decides. Here the failed replacement wrote
        // only its first page and the failed restore wrote that page back.
        let (written, holds) = run(vec![(1, Fault::AfterFirstPage), (2, Fault::AfterFirstPage)]);
        assert!(failed(&written), "{written:?}");
        assert_eq!(holds, original);
        // Memory holding neither, after the restore, must not run.
        for restore in [Fault::AfterFirstPage, Fault::ReadsBackChanged] {
            let (written, holds) = run(vec![(1, Fault::ReadsBackChanged), (2, restore)]);
            assert!(broken(&written), "{written:?}");
            assert!(holds != original && holds != rewritten, "{holds:?}");
        }
    }

    /// A file's loops are found by file offset, once, whatever a process
    /// maps; each mapping holds the pairs whose load-exclusive it maps. So a
    /// loop split across two mappings, at any instruction boundary, is
    /// counted exactly once, by the mapping that holds its load, whether or
    /// not the rewriter would take it and whatever follows the mapping; the
    /// left count rewrite_mapping keeps then names it exactly once.
    #[test]
    fn a_loop_split_across_two_mappings_is_counted_once() {
        // ldaxr x0, [x2]; add x1, x0, #1; stlxr w3, x1, [x2]; cbnz w3, -12
        let canonical = [LDAXR_X0_X2, 0x9100_0401, STLXR_W3_X1_X2, 0x35FF_FFA3];
        // ldaxr; mul (not an idiom the rewriter takes); stlxr; cbnz
        let refused = [LDAXR_X0_X2, 0x9B02_7C01, STLXR_W3_X1_X2, 0x35FF_FFA3];
        let base = 0x1000u64;
        for words in [canonical, refused] {
            let mut scan = CodeScan::at(base);
            words.iter().for_each(|word| scan.push(*word));
            let loops = scan.into_loops();
            assert_eq!(loops.pair_loads, [base], "the pair is at its load");
            for split in 0..=4u64 {
                let boundary = base + 4 * split;
                let first = loops.found_in(0, boundary).pairs;
                let second = loops.found_in(boundary, base + 0x1000).pairs;
                assert_eq!(first + second, 1, "split at {split}");
                assert_eq!(first, u64::from(split >= 1), "split at {split}");
                assert_eq!(
                    left_in_mapping(first, 0, 0) + left_in_mapping(second, 0, 0),
                    1,
                    "split at {split}"
                );
            }
            // Mapped alone, with nothing after it, a mapping holding the
            // load still counts the loop: the file holds it.
            assert_eq!(loops.found_in(base, base + 4).pairs, 1);
            assert_eq!(loops.found_in(base + 4, base + 16).pairs, 0);
        }
        let original: Vec<u8> = canonical.iter().flat_map(|w| w.to_le_bytes()).collect();
        let len = original.len() as u64;
        for split in (4..len).step_by(4) {
            let first = place(base, len, 0x40_0000, 0, base + split);
            let second = place(base, len, 0x50_0000, base + split, 0x1000);
            assert_eq!(first, Placement::CrossesEnd, "split at {split}");
            assert_eq!(second, Placement::Outside, "split at {split}");
        }
        // Counter reads are attributed by offset too.
        let mut scan = CodeScan::at(0);
        [0xD53B_E040u32, 0xD53B_E040]
            .iter()
            .for_each(|word| scan.push(*word));
        let loops = scan.into_loops();
        assert_eq!(loops.found_in(0, 4).counter_reads, 1);
        assert_eq!(loops.found_in(4, 8).counter_reads, 1);
    }

    #[test]
    fn a_patch_is_placed_only_where_the_mapping_holds_all_of_it() {
        // A mapping of file bytes [0x1000, 0x3000) at 0x7f00_0000.
        let at = |offset, len| place(offset, len, 0x7f00_0000, 0x1000, 0x2000);
        assert_eq!(at(0x1000, 16), Placement::At(0x7f00_0000));
        assert_eq!(at(0x2ff0, 16), Placement::At(0x7f00_1ff0));
        assert_eq!(at(0x2ff8, 16), Placement::CrossesEnd);
        assert_eq!(at(0x0ff0, 16), Placement::Outside);
        assert_eq!(at(0x3000, 16), Placement::Outside);
    }

    /// The in-memory rewrite is tallied like the pairs: summed over one
    /// address space's ranges, the largest sum over address spaces, and only
    /// for the paths it touched.
    #[test]
    fn tallies_the_in_memory_rewrite_per_path() {
        let libc = "/lib64/libc.so.6";
        let with = |offset, pairs, rewritten, left, failed| ScannedMapping {
            rewrite: Rewrite {
                rewritten,
                left,
                failed,
            },
            ..mapping(libc, offset, 0x1000, pairs)
        };
        let mut tally = PairTally::default();
        tally.note(
            &("0".to_owned(), 1),
            &[with(0, 20, 18, 2, 0), with(0x4000, 3, 3, 0, 0)],
            None,
        );
        // A child whose write failed for one loop.
        tally.note(&("01".to_owned(), 0), &[with(0, 20, 17, 2, 1)], None);
        tally.note(
            &("0".to_owned(), 1),
            &[mapping("/bin/plain", 0, 0x1000, 4)],
            None,
        );
        let rewrites = tally.rewrites_per_path();
        // One process's counts, so that they add up to its pairs: the one
        // where the most can still retry, here the child's 2 left and 1 not
        // written, though the parent mapped more.
        assert_eq!(
            rewrites.get(libc),
            Some(&Rewrite {
                rewritten: 17,
                left: 2,
                failed: 1,
            })
        );
        assert!(!rewrites.contains_key("/bin/plain"), "{rewrites:?}");
        // What can still retry: the worst process's left and not written,
        // every pair where no rewrite was attempted, and a split sequence
        // that only the rewrite saw.
        tally.note(
            &("0".to_owned(), 1),
            &[with(0x9000, 0, 0, 1, 0)].map(|m| ScannedMapping {
                path: "/lib64/libsplit.so".to_owned(),
                ..m
            }),
            None,
        );
        let remaining = tally.remaining_per_path();
        assert_eq!(remaining.get(libc), Some(&3));
        assert_eq!(remaining.get("/bin/plain"), Some(&4));
        assert_eq!(remaining.get("/lib64/libsplit.so"), Some(&1));
        // Two executable aliases of the same file bytes in one address
        // space are two mappings: a rewritten copy does not hide an
        // original one.
        let mut aliases = PairTally::default();
        let rewritten_copy = with(0, 3, 3, 0, 0);
        let original_copy = ScannedMapping {
            address: 0x7f00_0000,
            ..with(0, 3, 0, 3, 0)
        };
        aliases.note(&("0".to_owned(), 1), &[original_copy, rewritten_copy], None);
        assert_eq!(aliases.remaining_per_path().get(libc), Some(&3));
        assert_eq!(aliases.pairs_per_path().get(libc), Some(&6));
        assert_eq!(
            aliases.rewrites_per_path().get(libc),
            Some(&Rewrite {
                rewritten: 3,
                left: 3,
                failed: 0,
            })
        );
        // With equal remaining exposure, the process with more loops.
        let mut tie = PairTally::default();
        tie.note(&("0".to_owned(), 1), &[with(0, 20, 18, 2, 0)], None);
        tie.note(&("01".to_owned(), 0), &[with(0, 4, 2, 2, 0)], None);
        assert_eq!(
            tie.rewrites_per_path().get(libc),
            Some(&Rewrite {
                rewritten: 18,
                left: 2,
                failed: 0,
            })
        );
    }
}
