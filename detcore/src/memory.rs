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
}

impl MemoryMetadata {
    /// Create an empty address-space model.
    pub(crate) fn new() -> Self {
        Self::default()
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
    pub(crate) fn remap(
        &mut self,
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
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

        self.unmap(old_start, old_len);
        self.unmap(new_start, new_len);
        if let Some((object, object_offset)) = source {
            self.insert_mapping(new_start, new_len, object, object_offset);
        }
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
        mappings.remap(0x5000, 0x1000, 0x9000, 0x1000);
        assert_eq!(
            before_remap,
            mappings.futex_id(mm(10), 0x9010),
            "mremap must retain the backing-object offset"
        );
        assert_ne!(original, before_remap);
    }
}
