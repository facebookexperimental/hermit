/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Process-local memory mapping metadata used to resolve shared futex keys.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::types::FutexID;
use crate::types::MmId;
use crate::types::SharedMemoryObjectId;

const PAGE_SIZE: usize = 4096;

fn page_aligned_len(len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    len.checked_add(PAGE_SIZE - 1)
        .expect("a successful memory range must fit in the address space")
        & !(PAGE_SIZE - 1)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SharedMapping {
    len: usize,
    object: SharedMemoryObjectId,
    object_offset: u64,
}

impl SharedMapping {
    fn end(self, start: usize) -> usize {
        start
            .checked_add(self.len)
            .expect("a successful memory mapping must fit in the address space")
    }

    fn offset_at(self, start: usize, address: usize) -> u64 {
        self.object_offset
            .checked_add((address - start) as u64)
            .expect("a mapped backing-object offset must fit in u64")
    }
}

/// Shared mappings visible in one Linux memory address space.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MemoryMetadata {
    next_anonymous_sequence: u64,
    shared_mappings: BTreeMap<usize, SharedMapping>,
    /// The guest's program break when it was first observed, i.e. the base of
    /// the brk heap.
    #[serde(default)]
    brk_start: Option<u64>,
    /// The guest's most recently observed program break.
    #[serde(default)]
    brk_current: Option<u64>,
    /// Detcore's model of the program break, where Detcore answers `brk(2)`
    /// itself ([`BrkModel`]); `None` elsewhere, and where the image's layout
    /// could not be read.
    #[serde(default)]
    pub(crate) brk_model: Option<BrkModel>,
    /// Why there is no [`BrkModel`] where Detcore answers `brk(2)` itself
    /// (`Detcore::handle_brk` refuses a brk by this name).
    #[serde(default)]
    pub(crate) brk_unmodeled: Option<String>,
}

/// Detcore's model of a process's program break: where Linux starts it for
/// the image the process exec'd (`crate::exec_record::ElfLayout`) and where
/// it stands now. Detcore keeps one, and answers `brk(2)` from it, only where
/// the run declares a launcher, whose break the kernel's is
/// (`Detcore::handle_brk`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BrkModel {
    /// `mm->start_brk`.
    pub(crate) start: u64,
    /// `mm->brk`, as `brk(2)` last set it (not page-aligned).
    pub(crate) current: u64,
    /// `mm->end_data - mm->start_data`
    /// (`crate::exec_record::ElfLayout::data_span`).
    pub(crate) data_span: u64,
    /// The kernel's break when the model was made, the launcher's: what a
    /// libc that queried the break before Detcore's first callback holds
    /// as the guest's (`Detcore::handle_brk`).
    pub(crate) launcher_break: u64,
}

impl MemoryMetadata {
    /// Create an empty address-space model.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Inspect the actual replacement model without erasing old observations.
    pub(crate) fn is_fresh_for_native_exec(&self) -> bool {
        self.next_anonymous_sequence == 0
            && self.shared_mappings.is_empty()
            && self.brk_start.is_none()
            && self.brk_current.is_none()
            && self.brk_model.is_none()
            && self.brk_unmodeled.is_none()
    }

    /// Record a program break reported by a `brk(2)` return.
    ///
    /// Both `brk(NULL)` (a query) and `brk(addr)` (a request) return the break
    /// in effect afterwards, so every successful return is a valid observation.
    /// The first one seen fixes the heap base.
    pub(crate) fn observe_brk(&mut self, brk: u64) {
        if brk == 0 {
            return;
        }
        self.brk_start.get_or_insert(brk);
        self.brk_current = Some(brk);
    }

    /// The guest's brk heap as a half-open `[start, end)` guest-address range,
    /// or `None` before the break has moved.
    ///
    /// This is the *observed* heap rather than the kernel's `[heap]` label.
    /// The label only exists when the kernel's own program break is the guest's:
    /// under a backend that loads the guest itself (DynamoRIO), `mm->start_brk`
    /// belongs to the loader, the kernel tags nothing, and the guest's heap is
    /// an ordinary anonymous mapping. Binding to the observed break keeps the
    /// heap identifiable on every backend.
    ///
    /// Where Detcore models the break, the heap is the model's: its start is
    /// known before any brk returns, which the first observation is not when
    /// the guest's first brk already grows the break.
    pub(crate) fn brk_heap_range(&self) -> Option<(u64, u64)> {
        let (start, current) = match self.brk_model {
            Some(model) => (model.start, model.current),
            None => (self.brk_start?, self.brk_current?),
        };
        (current > start).then_some((start, current))
    }

    /// Resolve a futex address, falling back to a private key outside tracked shared mappings.
    pub(crate) fn futex_id(&self, mm: MmId, address: usize) -> FutexID {
        let Some((&start, &mapping)) = self.shared_mappings.range(..=address).next_back() else {
            return FutexID::private(mm, address);
        };
        let Some(word_end) = address.checked_add(std::mem::size_of::<u32>()) else {
            return FutexID::private(mm, address);
        };
        if word_end > mapping.end(start) {
            return FutexID::private(mm, address);
        }

        FutexID::shared(mapping.object, mapping.offset_at(start, address))
    }

    /// Record a new anonymous shared mapping.
    pub(crate) fn map_anonymous(&mut self, mm: MmId, start: usize, len: usize) {
        let object = SharedMemoryObjectId::Anonymous {
            origin: mm,
            sequence: self.next_anonymous_sequence,
        };
        self.next_anonymous_sequence = self
            .next_anonymous_sequence
            .checked_add(1)
            .expect("anonymous shared mapping sequence exhausted");
        self.insert_mapping(start, len, object, 0);
    }

    /// Record a mapping with a resolved backing object.
    pub(crate) fn map_object(
        &mut self,
        start: usize,
        len: usize,
        object: SharedMemoryObjectId,
        object_offset: u64,
    ) {
        self.insert_mapping(start, len, object, object_offset);
    }

    fn insert_mapping(
        &mut self,
        start: usize,
        len: usize,
        object: SharedMemoryObjectId,
        object_offset: u64,
    ) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        start
            .checked_add(len)
            .expect("a successful memory mapping must fit in the address space");
        self.unmap(start, len);
        self.shared_mappings.insert(
            start,
            SharedMapping {
                len,
                object,
                object_offset,
            },
        );
    }

    /// Remove a range, retaining any mapped portions on either side.
    pub(crate) fn unmap(&mut self, start: usize, len: usize) {
        let len = page_aligned_len(len);
        if len == 0 {
            return;
        }
        let end = start
            .checked_add(len)
            .expect("a successful memory range operation must fit in the address space");
        let overlapping = self
            .shared_mappings
            .range(..end)
            .filter_map(|(&mapping_start, &mapping)| {
                (mapping.end(mapping_start) > start).then_some((mapping_start, mapping))
            })
            .collect::<Vec<_>>();

        for (mapping_start, mapping) in overlapping {
            self.shared_mappings.remove(&mapping_start);
            let mapping_end = mapping.end(mapping_start);
            if mapping_start < start {
                self.shared_mappings.insert(
                    mapping_start,
                    SharedMapping {
                        len: start - mapping_start,
                        ..mapping
                    },
                );
            }
            if mapping_end > end {
                self.shared_mappings.insert(
                    end,
                    SharedMapping {
                        len: mapping_end - end,
                        object_offset: mapping.offset_at(mapping_start, end),
                        ..mapping
                    },
                );
            }
        }
    }

    /// Move or resize a mapping after a successful `mremap`.
    /// Record a successful `mremap` from `old_start` to `new_start`. The new
    /// range keeps the source's backing object and offset. The source range
    /// is dropped unless `keep_source` (`MREMAP_DONTUNMAP`), with which Linux
    /// leaves the old mapping in place (mm/mremap.c), so both ranges then
    /// alias the same object.
    pub(crate) fn remap(
        &mut self,
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
        keep_source: bool,
    ) {
        let old_len = page_aligned_len(old_len);
        let new_len = page_aligned_len(new_len);
        let old_end = old_start
            .checked_add(old_len)
            .expect("a successful mremap source must fit in the address space");
        let source = self
            .shared_mappings
            .range(..=old_start)
            .next_back()
            .and_then(|(&mapping_start, &mapping)| {
                (mapping.end(mapping_start) >= old_end)
                    .then_some((mapping.object, mapping.offset_at(mapping_start, old_start)))
            });

        if !keep_source {
            self.unmap(old_start, old_len);
        }
        self.unmap(new_start, new_len);
        if let Some((object, object_offset)) = source {
            self.insert_mapping(new_start, new_len, object, object_offset);
        }
    }
}

#[cfg(test)]
mod brk_tests {
    use super::*;

    #[test]
    fn native_exec_fresh_memory_requires_sequence_mappings_and_brk_to_be_empty() {
        let owner = crate::types::DetTid::from_raw(42);
        let mut memory = MemoryMetadata::new();
        assert!(memory.is_fresh_for_native_exec());
        memory.map_anonymous(MmId::initial(owner), 0x5000, 0x1000);
        assert!(!memory.is_fresh_for_native_exec());
        memory.unmap(0x5000, 0x1000);
        assert!(memory.shared_mappings.is_empty());
        assert!(
            !memory.is_fresh_for_native_exec(),
            "the old allocator sequence remains"
        );
        let mut brk = MemoryMetadata::new();
        brk.observe_brk(0x405000);
        assert_eq!(brk.brk_heap_range(), None);
        assert!(
            !brk.is_fresh_for_native_exec(),
            "even an unexpanded old heap has observations"
        );
        assert!(MemoryMetadata::new().is_fresh_for_native_exec());
    }

    #[test]
    fn brk_range_tracks_the_observed_program_break() {
        let mut mm = MemoryMetadata::new();
        // Nothing observed yet: no heap to report.
        assert_eq!(mm.brk_heap_range(), None);

        // The first observation fixes the base. A heap that has not grown is
        // empty, not a zero-length record.
        mm.observe_brk(0x405000);
        assert_eq!(mm.brk_heap_range(), None);

        // Growth is reported as [start, current).
        mm.observe_brk(0x426000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x426000)));
        mm.observe_brk(0x447000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x447000)));

        // A repeated brk(NULL) query must not move the base.
        mm.observe_brk(0x447000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x447000)));

        // Shrinking the break is legal and narrows the heap; the base is sticky.
        mm.observe_brk(0x410000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x410000)));

        // Back to the base means an empty heap again, not an inverted range.
        mm.observe_brk(0x405000);
        assert_eq!(mm.brk_heap_range(), None);
    }

    #[test]
    fn a_modeled_heap_is_the_models_from_the_first_growth() {
        // A guest whose first brk grows the break: the observations alone
        // would take its result as the base and report no heap.
        let mut mm = MemoryMetadata::new();
        mm.brk_model = Some(BrkModel {
            start: 0x405000,
            current: 0x426000,
            data_span: 0x1000,
            launcher_break: 0x555555580000,
        });
        mm.observe_brk(0x426000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x426000)));
        mm.brk_model = Some(BrkModel {
            current: 0x405000,
            ..mm.brk_model.unwrap()
        });
        assert_eq!(mm.brk_heap_range(), None);
    }

    #[test]
    fn a_zero_break_is_ignored() {
        // brk returning 0 is not a usable address; recording it would make the
        // heap base 0 and hash a wild range.
        let mut mm = MemoryMetadata::new();
        mm.observe_brk(0);
        assert_eq!(mm.brk_heap_range(), None);
        mm.observe_brk(0x405000);
        mm.observe_brk(0);
        mm.observe_brk(0x426000);
        assert_eq!(mm.brk_heap_range(), Some((0x405000, 0x426000)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DetTid;

    fn mm(tid: i32) -> MmId {
        MmId::initial(DetTid::from_raw(tid))
    }

    #[test]
    fn file_mappings_alias_by_backing_offset() {
        let mut mappings = MemoryMetadata::new();
        let object = SharedMemoryObjectId::File {
            device: 1,
            inode: 2,
        };
        mappings.map_object(0x1000, 0x1000, object, 0);
        mappings.map_object(0x4000, 0x1000, object, 0);
        mappings.map_object(0x8000, 0x1000, object, 0x1000);

        assert_eq!(
            mappings.futex_id(mm(10), 0x1010),
            mappings.futex_id(mm(11), 0x4010),
            "aliases of one file offset must share a futex key"
        );
        assert_ne!(
            mappings.futex_id(mm(10), 0x1010),
            mappings.futex_id(mm(10), 0x8010),
            "different file offsets must not alias"
        );
        assert!(
            matches!(mappings.futex_id(mm(10), 0x1ffc), FutexID::Shared { .. }),
            "mmap lengths must be rounded to the kernel's page boundary"
        );
    }

    #[test]
    fn forked_anonymous_mapping_retains_identity() {
        let mut parent = MemoryMetadata::new();
        parent.map_anonymous(mm(10), 0x1000, 0x1000);
        let mut child = parent.clone();

        assert_eq!(
            parent.futex_id(mm(10), 0x1010),
            child.futex_id(mm(11), 0x1010),
            "fork must retain the backing identity of inherited shared mappings"
        );
        child.map_anonymous(mm(11), 0x4000, 0x1000);
        assert_ne!(
            parent.futex_id(mm(10), 0x1010),
            child.futex_id(mm(11), 0x4010),
            "independent anonymous mappings must use distinct objects"
        );
    }

    #[test]
    fn unmap_and_remap_preserve_only_live_aliases() {
        let mut mappings = MemoryMetadata::new();
        mappings.map_anonymous(mm(10), 0x1000, 0x3000);
        let original = mappings.futex_id(mm(10), 0x2010);

        mappings.unmap(0x2000, 0x1000);
        assert_eq!(
            mappings.futex_id(mm(10), 0x2010),
            FutexID::private(mm(10), 0x2010),
            "an unmapped word must no longer resolve through its old object"
        );
        assert!(
            matches!(mappings.futex_id(mm(10), 0x3010), FutexID::Shared { .. }),
            "the right-hand mapping fragment must retain its shared identity"
        );

        mappings.map_anonymous(mm(10), 0x5000, 0x1000);
        let before_remap = mappings.futex_id(mm(10), 0x5010);
        mappings.remap(0x5000, 0x1000, 0x9000, 0x1000, false);
        assert_eq!(
            before_remap,
            mappings.futex_id(mm(10), 0x9010),
            "mremap must retain the backing-object offset"
        );
        assert_ne!(original, before_remap);
    }

    /// `MREMAP_DONTUNMAP` leaves the old mapping in place, so the old and new
    /// ranges both resolve to the same backing object
    /// (https://github.com/rrnewton/hermit/pull/4020).
    #[test]
    fn a_dontunmap_remap_keeps_both_aliases_of_the_object() {
        let mut mappings = MemoryMetadata::new();
        mappings.map_anonymous(mm(10), 0x5000, 0x1000);
        let before = mappings.futex_id(mm(10), 0x5010);
        mappings.remap(0x5000, 0x1000, 0x9000, 0x1000, true);
        assert!(matches!(before, FutexID::Shared { .. }));
        assert_eq!(mappings.futex_id(mm(10), 0x5010), before);
        assert_eq!(mappings.futex_id(mm(10), 0x9010), before);
    }
}
