/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A process's `smaps`, `smaps_rollup` and `numa_maps` rendered from the
//! backend's memory map (`reverie::Guest::memory_map`), where Detcore renders
//! them (`procfs::RenderedFiles::address_space`: where the guest's procfs is
//! not the kernel's, KVM). Each is the text Linux prints for the listed
//! mappings (`show_smap`, `__show_smap`, `show_smaps_rollup` and
//! `show_numa_map` in `fs/proc/task_mmu.c`, v6.17), which the sanitizer that
//! serves the kernel's file of the same name on the other backends then
//! normalizes (`procfs::sanitize_smaps`, `sanitize_smaps_rollup`,
//! `sanitize_numa_maps`). So for every mapping it lists, the guest reads the
//! bytes the sanitizer gives the kernel's file for that mapping on ptrace,
//! but for `ProtectionKey`, which follows the kernel serving the process
//! (below).
//!
//! A mapping's line of `maps` is its header in `smaps` ([`super::render`]'s,
//! from the memory map's facts). Linux prints more about it than the memory
//! map lists, and each such field is derived the same way on every backend,
//! from facts, or the file is refused by name:
//!
//! - `Size` is the range's; `KernelPageSize` and `MMUPageSize` are 4 kB for
//!   every mapping that is not hugetlbfs memory (`vma_kernel_pagesize`).
//! - The accounting fields count the pages the host holds; each sanitizer
//!   prints every one of them as `0 kB` (`procfs::SMAPS_ACCOUNTING_FIELDS`),
//!   and they are rendered as those of an untouched mapping, zero.
//! - `THPeligible` (`thp_vma_allowable_orders(vma, vma->vm_flags, TVA_SMAPS |
//!   TVA_ENFORCE_SYSFS, THP_ORDERS_ALL)`, include/linux/huge_mm.h and
//!   mm/huge_memory.c): for an anonymous mapping that no `MADV_HUGEPAGE`
//!   marked (Detcore answers every `MADV_HUGEPAGE` and `MADV_NOHUGEPAGE`
//!   itself and passes neither on, `syscalls::memory`), 1 where the host's
//!   transparent huge pages allow an order ([`HostThp`]), the address space
//!   has not disabled them (`PR_SET_THP_DISABLE`, which Detcore passes to the
//!   backend and asks it about), and the mapping holds a naturally aligned
//!   block of that order, tried from the largest. The host's settings are
//!   those the kernel beneath ptrace consults for the same answer, and the
//!   ones a KVM guest reads in `/sys/kernel/mm/transparent_hugepage`, so the
//!   value follows the host's settings exactly as ptrace's sanitized file
//!   does. Where the host lets an anonymous mapping have huge pages without
//!   `MADV_HUGEPAGE` (`always`), the value also follows the mapping's size and
//!   alignment, so KVM's 8188 kB stack window, which holds an aligned 2 MiB
//!   block, prints 1 where a native-sized stack prints 0: the same layout
//!   difference as its range and `Size`.
//! - `ProtectionKey` is printed where the kernel serving the process enables
//!   protection keys (`arch_pkeys_enabled`, `cpu_feature_enabled(OSPKE)`): a
//!   fact of that kernel, not of the CPU the guest is shown (ptrace's guests
//!   are shown a virtual CPU without OSPKE, and the host kernel beneath them
//!   prints the line). Detcore asks that kernel, through the backend
//!   (`Detcore::serving_kernel_enables_protection_keys`). Where it enables
//!   none, as KVM's does (its vCPU runs with CR4.PKE clear, and LinuxEmu
//!   answers the protection-key calls as Linux does on such a CPU), the line
//!   is not printed, as Linux prints none. Where it enables them, the file is
//!   refused: a mapping's key then follows more calls than Detcore records
//!   (`mprotect(PROT_EXEC)` alone gives the execute-only key,
//!   `__arch_override_mprotect_pkey`), so a 0 could be wrong. The renderer
//!   still prints the line with the key 0 from facts that say keys are
//!   enabled and unused ([`SmapsFacts::protection_keys_enabled`]), as the host
//!   kernel does for the test process ([`host_protection_keys_enabled`]).
//! - `VmFlags`: for the process's initial stack (`[stack]`), the flags
//!   `setup_arg_pages` (fs/exec.c) leaves it with, `VM_STACK_FLAGS` with the
//!   protection the memory map lists: VM_READ, VM_WRITE and VM_EXEC as the
//!   protection says, VM_MAYREAD, VM_MAYWRITE, VM_MAYEXEC, VM_GROWSDOWN and
//!   VM_ACCOUNT (`rd wr mr mw me gd ac`). `mprotect(2)` changes only the
//!   protection bits there: it keeps VM_ACCOUNT on a mapping that has an
//!   `anon_vma` (`mprotect_fixup`), which the stack always has, as exec wrote
//!   the arguments into it. The flags `madvise(2)` access-pattern advice,
//!   fork and dump advice, and `mlock(2)` set (`sr`, `rr`, `dc`, `dd`, `wf`,
//!   `lo`, `lf`) are not in the memory map, so a map of an address space where
//!   such a call reached the backend for a listed mapping
//!   (`MemoryMetadata::mapping_flags_changed`) is refused; a call on a mapping
//!   the memory map does not list changes nothing the file shows. Any other mapping's flags depend on how it was made
//!   (`MAP_GROWSDOWN`, `MAP_NORESERVE`, the file's open mode and mount, the
//!   history of VM_ACCOUNT), which the memory map does not say, so `smaps`
//!   refuses any mapping but the initial stack.
//!
//! `numa_maps` prints, for each mapping:
//!
//! - its policy: `default`. Detcore answers every memory-policy call itself,
//!   for one virtual NUMA node, and records no policy (`mbind` and
//!   `set_mempolicy` change nothing; `get_mempolicy` answers `MPOL_DEFAULT`),
//!   so the policy Linux prints, the mapping's or else the task's
//!   (`__get_vma_policy`, `get_task_policy`), is the default, which
//!   `mpol_to_str` prints as `default`;
//! - its name: `stack` for the initial stack;
//! - its page counts (`anon=`, `dirty=`, ..., `N<node>=`), which the sanitizer
//!   drops, rendered as none;
//! - `kernelpagesize_kB=4`, which Linux prints only for a mapping that holds
//!   at least one resident page (`if (!md->pages) goto out`). The initial
//!   stack does from exec on: its top page holds the arguments, the
//!   environment and the auxiliary vector exec wrote. Until advice that
//!   discards pages reaches it (`MADV_DONTNEED`, `MADV_DONTNEED_LOCKED`,
//!   `MADV_GUARD_INSTALL`, or `MADV_WIPEONFORK` for a later fork's copy:
//!   `MemoryMetadata::pages_discarded`): a thread running on another stack can
//!   discard all of it, and then nothing says whether a page came back, so
//!   `numa_maps` is refused. Whether any other mapping holds a page is the
//!   host's residency, which is no fact Detcore has, so `numa_maps` too
//!   refuses any mapping but the initial stack.
//!
//! `smaps_rollup` prints one header for the whole address space, from its
//! first mapping's start to its last one's end (`show_smaps_rollup`, the gate
//! area excluded, which is not in the address space's tree), and the sums of
//! the accounting fields, which the sanitizer prints as `0 kB`. Only a
//! complete memory map says where the address space starts: an incomplete
//! one (KVM's lists the stack window only) is refused, since a reader must
//! not infer from a missing entry that no mapping is there
//! (`reverie::MemoryMap::complete`).
//!
//! An incomplete list renders only the mappings it lists, as [`super::render`]
//! does for `maps`: the others are missing from the file, a named difference
//! of the backend (on KVM, <https://github.com/rrnewton/reverie/issues/1211>).
//! An incomplete list that lists nothing is refused.

use std::path::Path;

use reverie::MappingBacking;
use reverie::MappingFact;
use reverie::MemoryMap;

/// `SEQ_PUT_DEC`'s labels in `__show_smap` (fs/proc/task_mmu.c, v6.17), with
/// their padding and the width of the number after them, in Linux's order,
/// and whether a label is printed only in `smaps_rollup` (`rollup_mode`).
const ACCOUNTING: [(&str, usize, bool); 22] = [
    ("Rss:            ", 8, false),
    ("Pss:            ", 8, false),
    ("Pss_Dirty:      ", 8, false),
    ("Pss_Anon:       ", 8, true),
    ("Pss_File:       ", 8, true),
    ("Pss_Shmem:      ", 8, true),
    ("Shared_Clean:   ", 8, false),
    ("Shared_Dirty:   ", 8, false),
    ("Private_Clean:  ", 8, false),
    ("Private_Dirty:  ", 8, false),
    ("Referenced:     ", 8, false),
    ("Anonymous:      ", 8, false),
    ("KSM:            ", 8, false),
    ("LazyFree:       ", 8, false),
    ("AnonHugePages:  ", 8, false),
    ("ShmemPmdMapped: ", 8, false),
    ("FilePmdMapped:  ", 8, false),
    ("Shared_Hugetlb: ", 8, false),
    // `seq_put_decimal_ull_width(m, " kB\nPrivate_Hugetlb: ", ..., 7)`: one
    // column of the number's width goes to the longer label.
    ("Private_Hugetlb: ", 7, false),
    ("Swap:           ", 8, false),
    ("SwapPss:        ", 8, false),
    ("Locked:         ", 8, false),
];

/// The orders of transparent huge page an anonymous mapping can have
/// (`THP_ORDERS_ALL_ANON`: orders 2 to `PMD_ORDER`, 9 on x86-64 with 4 KiB
/// pages, include/linux/huge_mm.h in v6.17 and v7.1.3).
const THP_ORDERS_ALL_ANON: u64 = ((1 << 10) - 1) & !0b11;

/// The size of a page, and of every page of a mapping that is not hugetlbfs
/// memory (`vma_kernel_pagesize`, `vma_mmu_pagesize`).
const PAGE_SIZE: u64 = 4096;

/// What the host kernel's transparent huge page settings allow an anonymous
/// mapping, as `/sys/kernel/mm/transparent_hugepage` shows them: the inputs
/// of `thp_vma_allowable_orders`'s mask with `TVA_ENFORCE_SYSFS`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct HostThp {
    /// Whether the global setting (`enabled`) is `always`
    /// (`hugepage_global_always`).
    pub(crate) global_always: bool,
    /// The orders whose own setting (`hugepages-<size>kB/enabled`) is
    /// `always` (`huge_anon_orders_always`).
    pub(crate) always: u64,
    /// The orders whose own setting is `inherit`
    /// (`huge_anon_orders_inherit`): the PMD order alone where the kernel has
    /// no per-size settings.
    pub(crate) inherit: u64,
}

impl HostThp {
    /// The orders an anonymous mapping no `MADV_HUGEPAGE` marked may have
    /// (`thp_vma_allowable_orders`): those set to `always`, and those set to
    /// `inherit` where the global setting is `always`.
    pub(crate) fn anonymous_orders(&self) -> u64 {
        let mut orders = self.always;
        if self.global_always {
            orders |= self.inherit;
        }
        orders & THP_ORDERS_ALL_ANON
    }

    /// The settings under `root`, the kernel's
    /// `/sys/kernel/mm/transparent_hugepage`: `Ok(None)` where it does not
    /// exist, a kernel without transparent huge pages or on hardware that has
    /// none (`hugepage_init` makes no directory then), where every mapping's
    /// answer is 0. `Err` names a setting that could not be read or parsed.
    pub(crate) fn read(root: &Path) -> Result<Option<Self>, String> {
        let global = match std::fs::read(root.join("enabled")) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !root.exists() => {
                return Ok(None);
            }
            Err(error) => return Err(format!("{}/enabled: {error}", root.display())),
        };
        let global = selected(&global)
            .ok_or_else(|| format!("{}/enabled has no selected setting", root.display()))?;
        let mut thp = HostThp {
            global_always: global == "always",
            always: 0,
            inherit: 0,
        };
        // Every size directory, `hugepages-<size>kB`, listed with raw
        // `getdents64` (`util::find_in_directory`), in the order of size.
        let mut sizes = Vec::new();
        crate::util::find_in_directory(root, |name| {
            if let Some(kilobytes) = name
                .to_str()
                .and_then(|name| name.strip_prefix("hugepages-"))
                .and_then(|name| name.strip_suffix("kB"))
                .and_then(|size| size.parse::<u64>().ok())
            {
                sizes.push(kilobytes);
            }
            None::<()>
        })
        .map_err(|error| format!("{}: {error}", root.display()))?;
        sizes.sort_unstable();
        let per_size = !sizes.is_empty();
        for kilobytes in sizes {
            let directory = root.join(format!("hugepages-{kilobytes}kB"));
            let pages = kilobytes * 1024 / PAGE_SIZE;
            if !pages.is_power_of_two() || pages < 2 {
                return Err(format!("{}: an unexpected size", directory.display()));
            }
            let order = pages.trailing_zeros();
            let path = directory.join("enabled");
            // A size anonymous memory cannot have (order 1, which only files
            // and shared memory use) has no `enabled`.
            let text = match std::fs::read(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            match selected(&text) {
                Some("always") => thp.always |= 1 << order,
                Some("inherit") => thp.inherit |= 1 << order,
                Some("madvise" | "never") => {}
                _ => return Err(format!("{} has no setting Linux gives", path.display())),
            }
        }
        if !per_size {
            // Before per-size settings, the PMD size followed the global
            // setting alone (`huge_anon_orders_inherit`'s default).
            thp.inherit = 1 << 9;
        }
        Ok(Some(thp))
    }
}

/// The host kernel's settings ([`HostThp::read`] of
/// `/sys/kernel/mm/transparent_hugepage`), read once per tool process, so
/// that every `smaps` a run renders takes the same answer even if the host's
/// administrator changes them while it runs, as every open of a run's
/// `/proc/diskstats` takes the host's table as the run first read it
/// (`tool_global::host_diskstats`).
pub(crate) fn host_thp() -> Result<Option<HostThp>, String> {
    static HOST: std::sync::OnceLock<Result<Option<HostThp>, String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| HostThp::read(Path::new("/sys/kernel/mm/transparent_hugepage")))
        .clone()
}

/// The word a sysfs choice file selects, the one in brackets, as
/// `always [madvise] never` selects `madvise`.
fn selected(text: &[u8]) -> Option<&str> {
    std::str::from_utf8(text)
        .ok()?
        .split_whitespace()
        .find_map(|word| {
            word.strip_prefix('[')
                .and_then(|word| word.strip_suffix(']'))
        })
}

/// Whether the host kernel prints `ProtectionKey` in `smaps`
/// (`arch_pkeys_enabled`): on x86-64, whether it enabled the CPU's
/// protection keys, which the OSPKE bit of CPUID leaf 7 reports (it mirrors
/// CR4.PKE), read in the test's own process, which runs on the host kernel.
/// Only for the test that compares the renderer with that kernel's file: a
/// guest's answer comes from the kernel serving it (the module's docs).
/// `None` on another architecture.
#[cfg(test)]
pub(crate) fn host_protection_keys_enabled() -> Option<bool> {
    #[cfg(target_arch = "x86_64")]
    {
        // Leaf 7 is read only where leaf 0 says the CPU has it.
        if std::arch::x86_64::__get_cpuid_max(0).0 < 7 {
            return Some(false);
        }
        Some(std::arch::x86_64::__cpuid_count(7, 0).ecx & (1 << 4) != 0)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// What, beyond the memory map, the derived fields of a process's `smaps`
/// need (the module's docs say which field takes which).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SmapsFacts {
    /// Whether Linux prints `ProtectionKey`: whether the kernel serving the
    /// process enables protection keys (the module's docs). Never true for a
    /// guest, whose file is refused there; true for the test that compares
    /// the renderer with the host kernel's file.
    pub(crate) protection_keys_enabled: bool,
    /// Whether the address space allocated or applied a protection key
    /// (`MemoryMetadata::protection_keys`).
    pub(crate) protection_keys_used: bool,
    /// Whether a call that sets mapping flags the memory map does not list
    /// reached the backend for the address space
    /// (`MemoryMetadata::mapping_flags_changed`).
    pub(crate) mapping_flags_changed: bool,
    /// The orders of transparent huge page an anonymous mapping may have
    /// ([`HostThp::anonymous_orders`]), none where the host has none.
    pub(crate) thp_orders: u64,
    /// Whether the address space disabled transparent huge pages
    /// (`PR_GET_THP_DISABLE`), where [`Self::thp_orders`] makes it matter;
    /// `None` where it was not asked because no order is allowed.
    pub(crate) thp_disabled: Option<bool>,
}

/// Linux's `THPeligible` for an anonymous mapping `[start, end)` that no
/// `MADV_HUGEPAGE` or `MADV_NOHUGEPAGE` marked, which may have huge pages of
/// `orders` (`thp_vma_allowable_orders`), in an address space that has
/// (`disabled`) or has not disabled them: the first of those orders, from the
/// largest, for which the mapping holds a naturally aligned block of that
/// size (`thp_vma_suitable_order` at `vm_end` less the size).
fn thp_eligible(orders: u64, disabled: bool, start: u64, end: u64) -> bool {
    if orders == 0 || disabled {
        return false;
    }
    (2..=9)
        .rev()
        .filter(|order| orders & (1 << order) != 0)
        .any(|order| {
            let size = PAGE_SIZE << order;
            end.checked_sub(size).is_some_and(|address| {
                let aligned = address & !(size - 1);
                aligned >= start && aligned + size <= end
            })
        })
}

/// The `VmFlags` line Linux prints for the initial stack with protection
/// `prot` (the module's docs): its flags in the order of their bits
/// (`show_smap_vma_flags`), each with the space after it.
fn stack_vm_flags(prot: u32) -> String {
    let mut flags = String::from("VmFlags: ");
    for (bit, mnemonic) in [
        (libc::PROT_READ, "rd "),
        (libc::PROT_WRITE, "wr "),
        (libc::PROT_EXEC, "ex "),
    ] {
        if prot & bit as u32 != 0 {
            flags.push_str(mnemonic);
        }
    }
    // VM_MAYREAD, VM_MAYWRITE, VM_MAYEXEC, VM_GROWSDOWN and VM_ACCOUNT.
    flags.push_str("mr mw me gd ac \n");
    flags
}

/// A named refusal of a whole file: why Detcore cannot render it.
pub(crate) type Refusal = String;

/// The range and name of `mapping`, as the reason of a refusal names it.
fn named(mapping: &MappingFact) -> String {
    let mut line = Vec::new();
    match super::line(mapping, &mut line) {
        Some(()) => String::from_utf8_lossy(&line).trim_end().to_string(),
        None => format!("{:08x}-{:08x}", mapping.start, mapping.end),
    }
}

/// Refuse an incomplete list that lists nothing: KVM's, once LinuxEmu has
/// lost track of the stack window ([`super::render`]).
fn refuse_empty_incomplete(map: &MemoryMap, file: &str) -> Result<(), Refusal> {
    if map.mappings.is_empty() && !map.complete {
        Err(format!(
            "Detcore renders a process's {file} from the backend's memory map, which lists \
             only some of its mappings and at this point none"
        ))
    } else {
        Ok(())
    }
}

/// Whether `mapping` is the kernel's gate area, which lies above the user
/// address space (`[vsyscall]` on x86-64) and is no mapping of the address
/// space's tree: kernel addresses have their top bit set.
fn is_gate_area(mapping: &MappingFact) -> bool {
    mapping.start >= 1 << 63
}

/// The text of a process's `smaps` for `map`, as Linux prints it
/// (`show_smap`), with the fields the memory map does not list derived from
/// `facts`, or the refusal that names why Detcore cannot give one of them.
pub(crate) fn render_smaps(map: &MemoryMap, facts: &SmapsFacts) -> Result<Vec<u8>, Refusal> {
    refuse_empty_incomplete(map, "smaps")?;
    let mut out = Vec::new();
    for mapping in &map.mappings {
        if !matches!(mapping.backing, MappingBacking::Stack) {
            return Err(format!(
                "Linux's smaps prints the flags it keeps on each mapping, and for {} \
                 they are not in the backend's memory map",
                named(mapping)
            ));
        }
        if facts.mapping_flags_changed {
            return Err(format!(
                "Linux's smaps prints the flags it keeps on each mapping, and madvise \
                 advice or mlock that reached the backend may have set flags on {} that \
                 the backend's memory map does not list",
                named(mapping)
            ));
        }
        if facts.protection_keys_enabled && facts.protection_keys_used {
            return Err(format!(
                "Linux's smaps prints each mapping's protection key, and the address space \
                 allocated or applied one, which the backend's memory map does not list \
                 for {}",
                named(mapping)
            ));
        }
        let thp_disabled = match (facts.thp_orders, facts.thp_disabled) {
            (0, _) => false,
            (_, Some(disabled)) => disabled,
            (_, None) => {
                return Err(format!(
                    "Linux's smaps prints whether {} may have transparent huge pages, and \
                     whether the address space disabled them was not asked",
                    named(mapping)
                ));
            }
        };
        super::line(mapping, &mut out).ok_or_else(|| {
            format!(
                "the backend's memory map lists a mapping Detcore cannot print: {:08x}-{:08x}",
                mapping.start, mapping.end
            )
        })?;
        let kilobytes = (mapping.end - mapping.start) >> 10;
        out.extend_from_slice(
            format!(
                "Size:           {kilobytes:>8} kB\n\
                 KernelPageSize: {:>8} kB\n\
                 MMUPageSize:    {:>8} kB\n",
                PAGE_SIZE >> 10,
                PAGE_SIZE >> 10,
            )
            .as_bytes(),
        );
        accounting(&mut out, false);
        out.extend_from_slice(
            format!(
                "THPeligible:    {:>8}\n",
                u8::from(thp_eligible(
                    facts.thp_orders,
                    thp_disabled,
                    mapping.start,
                    mapping.end
                ))
            )
            .as_bytes(),
        );
        if facts.protection_keys_enabled {
            out.extend_from_slice(format!("ProtectionKey:  {:>8}\n", 0).as_bytes());
        }
        out.extend_from_slice(stack_vm_flags(mapping.prot).as_bytes());
    }
    Ok(out)
}

/// Append `__show_smap`'s accounting fields, each zero, those of
/// `smaps_rollup` too where `rollup`.
fn accounting(out: &mut Vec<u8>, rollup: bool) {
    for (label, width, rollup_only) in ACCOUNTING {
        if rollup_only && !rollup {
            continue;
        }
        out.extend_from_slice(format!("{label}{:>width$} kB\n", 0).as_bytes());
    }
}

/// Why Detcore cannot render a process's `smaps_rollup` from `map`, if it
/// cannot: an incomplete list does not say where the address space starts.
/// The one rule [`render_smaps_rollup`] refuses by, and the one a listing
/// of the process's directory leaves the name out by
/// (`Detcore::task_placeholder_snapshot`), so that a listing never names a
/// `smaps_rollup` its read refuses (dh-62azc).
pub(crate) fn smaps_rollup_refusal(map: &MemoryMap) -> Option<Refusal> {
    (!map.complete).then(|| {
        "Linux's smaps_rollup gives the range from the address space's first mapping to \
         its last one, and the backend's memory map lists only some of its mappings"
            .to_string()
    })
}

/// The text of a process's `smaps_rollup` for `map`, as Linux prints it
/// (`show_smaps_rollup`), or the refusal that names why Detcore cannot give
/// it ([`smaps_rollup_refusal`]).
pub(crate) fn render_smaps_rollup(map: &MemoryMap) -> Result<Vec<u8>, Refusal> {
    if let Some(refusal) = smaps_rollup_refusal(map) {
        return Err(refusal);
    }
    let mut own = map.mappings.iter().filter(|mapping| !is_gate_area(mapping));
    let first = own.next();
    let (start, end) = match first {
        Some(first) => (
            first.start,
            own.next_back().map_or(first.end, |last| last.end),
        ),
        // `vma_start` and `last_vma_end` stay 0 for an empty tree.
        None => (0, 0),
    };
    let header = format!("{start:08x}-{end:08x} ---p 00000000 00:00 0 ");
    let mut out = header.clone().into_bytes();
    // `seq_pad`: the column `show_vma_header_prefix` set, then one space.
    out.extend(std::iter::repeat_n(
        b' ',
        super::NAME_COLUMN.saturating_sub(header.len()) + 1,
    ));
    out.extend_from_slice(b"[rollup]\n");
    accounting(&mut out, true);
    Ok(out)
}

/// The text of a process's `numa_maps` for `map`, as Linux prints it
/// (`show_numa_map`, the module's docs), or the refusal that names why
/// Detcore cannot give a row: in an address space where advice that discards
/// pages reached a listed mapping (`pages_discarded`,
/// `MemoryMetadata::pages_discarded`), whether it still holds a resident
/// page, which decides `kernelpagesize_kB`, is not a fact.
pub(crate) fn render_numa_maps(map: &MemoryMap, pages_discarded: bool) -> Result<Vec<u8>, Refusal> {
    refuse_empty_incomplete(map, "numa_maps")?;
    let mut out = Vec::new();
    for mapping in &map.mappings {
        // The gate area has no memory map of its own: no row (`if (!mm)`).
        if is_gate_area(mapping) {
            continue;
        }
        if !matches!(mapping.backing, MappingBacking::Stack) {
            return Err(format!(
                "Linux's numa_maps says of each mapping whether it holds a resident page, \
                 and for {} that is the host's residency, not in the backend's memory map",
                named(mapping)
            ));
        }
        if pages_discarded {
            return Err(format!(
                "Linux's numa_maps says of each mapping whether it holds a resident page, and \
                 madvise advice that discards pages reached the backend for {}, so whether it \
                 still holds one is not a fact",
                named(mapping)
            ));
        }
        out.extend_from_slice(
            format!(
                "{:08x} default stack kernelpagesize_kB={}\n",
                mapping.start,
                PAGE_SIZE >> 10
            )
            .as_bytes(),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// The `[stack]` block of `/proc/self/smaps` a native process printed on
    /// this repository's Linux 7.1.3 test host (transparent huge pages
    /// `madvise`, 2048 kB `inherit`, every other size `never`; OSPKE set).
    const LINUX_STACK: &str = "\
        7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
        Size:                144 kB\n\
        KernelPageSize:        4 kB\n\
        MMUPageSize:           4 kB\n\
        Rss:                  24 kB\n\
        Pss:                  24 kB\n\
        Pss_Dirty:            24 kB\n\
        Shared_Clean:          0 kB\n\
        Shared_Dirty:          0 kB\n\
        Private_Clean:         0 kB\n\
        Private_Dirty:        24 kB\n\
        Referenced:           24 kB\n\
        Anonymous:            24 kB\n\
        KSM:                   0 kB\n\
        LazyFree:              0 kB\n\
        AnonHugePages:         0 kB\n\
        ShmemPmdMapped:        0 kB\n\
        FilePmdMapped:         0 kB\n\
        Shared_Hugetlb:        0 kB\n\
        Private_Hugetlb:       0 kB\n\
        Swap:                  0 kB\n\
        SwapPss:               0 kB\n\
        Locked:                0 kB\n\
        THPeligible:           0\n\
        ProtectionKey:         0\n\
        VmFlags: rd wr mr mw me gd ac \n";

    /// The facts of that host, for an address space that set no flag and no
    /// key.
    const HOST: SmapsFacts = SmapsFacts {
        protection_keys_enabled: true,
        protection_keys_used: false,
        mapping_flags_changed: false,
        thp_orders: 0,
        thp_disabled: None,
    };

    fn stack(start: u64, end: u64, prot: libc::c_int) -> MappingFact {
        MappingFact::new(start, end, prot as u32, false, 0, MappingBacking::Stack).unwrap()
    }

    fn read_write() -> libc::c_int {
        libc::PROT_READ | libc::PROT_WRITE
    }

    /// The `[stack]` block renders as Linux printed it, but for the
    /// accounting fields, which count the host's pages; through the
    /// sanitizer the two are the same bytes.
    #[test]
    fn the_stack_renders_as_linux_prints_it() {
        let map = MemoryMap::new(
            vec![stack(0x7ffc_ec80_f000, 0x7ffc_ec83_3000, read_write())],
            false,
        )
        .unwrap();
        let rendered = render_smaps(&map, &HOST).unwrap();
        assert_eq!(
            String::from_utf8(rendered.clone()).unwrap(),
            "\
            7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
            Size:                144 kB\n\
            KernelPageSize:        4 kB\n\
            MMUPageSize:           4 kB\n\
            Rss:                   0 kB\n\
            Pss:                   0 kB\n\
            Pss_Dirty:             0 kB\n\
            Shared_Clean:          0 kB\n\
            Shared_Dirty:          0 kB\n\
            Private_Clean:         0 kB\n\
            Private_Dirty:         0 kB\n\
            Referenced:            0 kB\n\
            Anonymous:             0 kB\n\
            KSM:                   0 kB\n\
            LazyFree:              0 kB\n\
            AnonHugePages:         0 kB\n\
            ShmemPmdMapped:        0 kB\n\
            FilePmdMapped:         0 kB\n\
            Shared_Hugetlb:        0 kB\n\
            Private_Hugetlb:       0 kB\n\
            Swap:                  0 kB\n\
            SwapPss:               0 kB\n\
            Locked:                0 kB\n\
            THPeligible:           0\n\
            ProtectionKey:         0\n\
            VmFlags: rd wr mr mw me gd ac \n"
        );
        let table = BTreeMap::new();
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps(&rendered, &table)).unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps(
                LINUX_STACK.as_bytes(),
                &table
            ))
            .unwrap()
        );
    }

    /// Each accounting field, in Linux's order and only those, survives the
    /// sanitizer as `0 kB`; dropping one changes the sanitized bytes.
    #[test]
    fn every_accounting_field_is_rendered_in_linuxs_order() {
        let map =
            MemoryMap::new(vec![stack(0x7000_0000, 0x7002_1000, read_write())], false).unwrap();
        let sanitized =
            crate::procfs::sanitize_smaps(&render_smaps(&map, &HOST).unwrap(), &BTreeMap::new());
        let labels: Vec<&str> = std::str::from_utf8(&sanitized)
            .unwrap()
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':').map(|(label, _)| label))
            .collect();
        let expected: Vec<&str> = ["Size", "KernelPageSize", "MMUPageSize"]
            .into_iter()
            .chain(
                ACCOUNTING
                    .iter()
                    .filter(|(_, _, rollup_only)| !rollup_only)
                    .map(|(label, _, _)| label.split(':').next().unwrap()),
            )
            .chain(["THPeligible", "ProtectionKey", "VmFlags"])
            .collect();
        assert_eq!(labels, expected);
        assert_eq!(
            ACCOUNTING
                .iter()
                .map(|(label, _, _)| label.split(':').next().unwrap())
                .collect::<Vec<_>>(),
            crate::procfs::SMAPS_ACCOUNTING_FIELDS
        );
    }

    /// The stack's flags follow its protection; `ProtectionKey` is printed
    /// only where the host enables keys.
    #[test]
    fn the_stack_flags_follow_its_protection() {
        let rendered = |prot, facts: &SmapsFacts| {
            let map = MemoryMap::new(vec![stack(0x7000_0000, 0x7002_1000, prot)], false).unwrap();
            String::from_utf8(render_smaps(&map, facts).unwrap()).unwrap()
        };
        assert!(rendered(read_write(), &HOST).ends_with(
            "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me gd ac \n"
        ));
        assert!(
            rendered(libc::PROT_READ | libc::PROT_EXEC, &HOST)
                .ends_with("VmFlags: rd ex mr mw me gd ac \n")
        );
        assert!(rendered(libc::PROT_NONE, &HOST).ends_with("VmFlags: mr mw me gd ac \n"));
        let no_keys = SmapsFacts {
            protection_keys_enabled: false,
            ..HOST
        };
        let text = rendered(read_write(), &no_keys);
        assert!(!text.contains("ProtectionKey"), "{text}");
        assert!(text.ends_with("THPeligible:           0\nVmFlags: rd wr mr mw me gd ac \n"));
    }

    /// `THPeligible`: 1 only where an allowed order's aligned block fits in
    /// the mapping, the address space has not disabled huge pages, and the
    /// host allows an order at all.
    #[test]
    fn thp_eligibility_takes_an_aligned_block_of_an_allowed_order() {
        let pmd = 1 << 9;
        let two_mib = 2 << 20;
        // The block must lie whole within the mapping.
        assert!(!thp_eligible(
            pmd,
            false,
            0x7000_0000,
            0x7000_0000 + two_mib - 4096
        ));
        assert!(thp_eligible(pmd, false, 0x7000_0000, 0x7000_0000 + two_mib));
        assert!(!thp_eligible(
            pmd,
            false,
            0x7000_1000,
            0x7000_0000 + two_mib + 4096
        ));
        assert!(thp_eligible(
            pmd,
            false,
            0x7000_1000,
            0x7000_0000 + 2 * two_mib
        ));
        assert!(!thp_eligible(pmd, true, 0x7000_0000, 0x7000_0000 + two_mib));
        assert!(!thp_eligible(0, false, 0x7000_0000, 0x7000_0000 + two_mib));
        // A smaller order fits where the PMD one does not.
        assert!(thp_eligible(pmd | 1 << 4, false, 0x7000_0000, 0x7001_0000));
        assert!(!thp_eligible(pmd | 1 << 4, false, 0x7000_1000, 0x7001_0000));
        // A mapping smaller than the block, near address 0.
        assert!(!thp_eligible(pmd, false, 0, 4096));
        let map = MemoryMap::new(
            vec![stack(0x7000_0000, 0x7000_0000 + two_mib, read_write())],
            false,
        )
        .unwrap();
        let facts = SmapsFacts {
            thp_orders: pmd,
            thp_disabled: Some(false),
            ..HOST
        };
        let text = String::from_utf8(render_smaps(&map, &facts).unwrap()).unwrap();
        assert!(text.contains("THPeligible:           1\n"), "{text}");
        let unasked = SmapsFacts {
            thp_disabled: None,
            ..facts
        };
        assert!(render_smaps(&map, &unasked).is_err());
    }

    /// The host's settings, as sysfs shows them: the global setting and each
    /// size's, an `always` order allowed whatever the global setting, an
    /// `inherit` one only under a global `always`.
    #[test]
    #[allow(clippy::disallowed_methods)] // test cleanup of its own temp directory
    fn host_thp_settings_are_read_as_linux_combines_them() {
        let root = std::env::temp_dir().join(format!(
            "smaps-host-thp-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = root.join("transparent_hugepage");
        assert_eq!(HostThp::read(&path), Ok(None));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("enabled"), "always [madvise] never\n").unwrap();
        for (kilobytes, setting) in [(2048, "inherit"), (64, "always"), (8, "never")] {
            let size = path.join(format!("hugepages-{kilobytes}kB"));
            std::fs::create_dir(&size).unwrap();
            let choices = ["always", "inherit", "madvise", "never"]
                .map(|choice| {
                    if choice == setting {
                        format!("[{choice}]")
                    } else {
                        choice.to_string()
                    }
                })
                .join(" ");
            std::fs::write(size.join("enabled"), format!("{choices}\n")).unwrap();
        }
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(
            thp,
            HostThp {
                global_always: false,
                always: 1 << 4,
                inherit: 1 << 9,
            }
        );
        assert_eq!(thp.anonymous_orders(), 1 << 4);
        std::fs::write(path.join("enabled"), "[always] madvise never\n").unwrap();
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(thp.anonymous_orders(), 1 << 4 | 1 << 9);
        std::fs::write(path.join("enabled"), "always madvise never\n").unwrap();
        assert!(HostThp::read(&path).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Every mapping but the initial stack, and a stack whose flags or key
    /// the memory map may not tell, refuses the file by name.
    #[test]
    fn what_the_memory_map_cannot_tell_refuses_smaps() {
        let anonymous = MappingFact::new(
            0x7fff_ff7f_0000,
            0x7fff_ff80_0000,
            0,
            false,
            0,
            MappingBacking::Anonymous,
        )
        .unwrap();
        let split = MemoryMap::new(
            vec![
                anonymous,
                stack(0x7fff_ff80_0000, 0x7fff_ffff_f000, read_write()),
            ],
            false,
        )
        .unwrap();
        let refusal = render_smaps(&split, &HOST).unwrap_err();
        assert!(refusal.contains("7fffff7f0000-7fffff800000"), "{refusal}");
        assert!(render_numa_maps(&split, false).is_err());
        let lone = MemoryMap::new(
            vec![stack(0x7fff_ff80_0000, 0x7fff_ffff_f000, read_write())],
            false,
        )
        .unwrap();
        for facts in [
            SmapsFacts {
                mapping_flags_changed: true,
                ..HOST
            },
            SmapsFacts {
                protection_keys_used: true,
                ..HOST
            },
        ] {
            let refusal = render_smaps(&lone, &facts).unwrap_err();
            assert!(refusal.contains("[stack]"), "{refusal}");
        }
        // A key matters only where Linux prints keys.
        render_smaps(
            &lone,
            &SmapsFacts {
                protection_keys_enabled: false,
                protection_keys_used: true,
                ..HOST
            },
        )
        .unwrap();
        // An incomplete list that lists nothing is no answer; a complete one
        // is the empty file of a task without an address space.
        let none = MemoryMap::new(Vec::new(), false).unwrap();
        assert!(render_smaps(&none, &HOST).is_err());
        assert!(render_numa_maps(&none, false).is_err());
        let empty = MemoryMap::new(Vec::new(), true).unwrap();
        assert_eq!(render_smaps(&empty, &HOST).unwrap(), b"");
        assert_eq!(render_numa_maps(&empty, false).unwrap(), b"");
    }

    /// The stack's row is Linux's, through the sanitizer too; once advice
    /// that discards pages reached the stack, whether it holds one, which
    /// decides `kernelpagesize_kB`, is not a fact, and the file is refused.
    #[test]
    fn the_stack_numa_maps_row_is_linuxs() {
        let map = MemoryMap::new(
            vec![stack(0x7ffc_ec80_f000, 0x7ffc_ec83_3000, read_write())],
            false,
        )
        .unwrap();
        let rendered = render_numa_maps(&map, false).unwrap();
        assert_eq!(
            rendered,
            b"7ffcec80f000 default stack kernelpagesize_kB=4\n"
        );
        let linux =
            b"7ffcec80f000 default stack anon=6 dirty=6 active=2 N0=6 kernelpagesize_kB=4\n";
        assert_eq!(
            crate::procfs::sanitize_numa_maps(&rendered),
            crate::procfs::sanitize_numa_maps(linux)
        );
        let refusal = render_numa_maps(&map, true).unwrap_err();
        assert!(
            refusal.contains("discards pages") && refusal.contains("[stack]"),
            "{refusal}"
        );
    }

    /// A complete map's rollup spans its first mapping to its last but the
    /// gate area, as Linux printed it; an incomplete one is refused.
    #[test]
    fn the_rollup_spans_a_complete_map_and_refuses_an_incomplete_one() {
        let maps = b"\
            00400000-00401000 r--p 00000000 00:2a 5352979878                         /p\n\
            7ffcec80f000-7ffcec833000 rw-p 00000000 00:00 0                          [stack]\n\
            ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]\n";
        let map = MemoryMap::from_maps(maps).unwrap();
        let rendered = render_smaps_rollup(&map).unwrap();
        let text = String::from_utf8(rendered.clone()).unwrap();
        assert!(
            text.starts_with(
                "00400000-7ffcec833000 ---p 00000000 00:00 0                              [rollup]\n\
                 Rss:                   0 kB\nPss:                   0 kB\nPss_Dirty:             0 kB\n\
                 Pss_Anon:              0 kB\n"
            ),
            "{text}"
        );
        let linux = b"\
            00400000-7ffcec833000 ---p 00000000 00:00 0                              [rollup]\n\
            Rss:                2528 kB\n\
            Pss:                 228 kB\n\
            Pss_Dirty:           176 kB\n\
            Pss_Anon:            176 kB\n\
            Pss_File:             52 kB\n\
            Pss_Shmem:             0 kB\n\
            Shared_Clean:       2348 kB\n\
            Shared_Dirty:          0 kB\n\
            Private_Clean:         4 kB\n\
            Private_Dirty:       176 kB\n\
            Referenced:         2528 kB\n\
            Anonymous:           176 kB\n\
            KSM:                   0 kB\n\
            LazyFree:              0 kB\n\
            AnonHugePages:         0 kB\n\
            ShmemPmdMapped:        0 kB\n\
            FilePmdMapped:         0 kB\n\
            Shared_Hugetlb:        0 kB\n\
            Private_Hugetlb:       0 kB\n\
            Swap:                  0 kB\n\
            SwapPss:               0 kB\n\
            Locked:                0 kB\n";
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps_rollup(&rendered)).unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps_rollup(linux)).unwrap()
        );
        assert!(
            render_smaps_rollup(&MemoryMap::new(map.mappings.clone(), false).unwrap()).is_err()
        );
        assert!(
            String::from_utf8(render_smaps_rollup(&MemoryMap::new(Vec::new(), true).unwrap()).unwrap())
                .unwrap()
                .starts_with(
                    "00000000-00000000 ---p 00000000 00:00 0                                  [rollup]\n"
                )
        );
    }

    /// This test process's own `[stack]`, as the host kernel prints it in
    /// `smaps` and `numa_maps`, is what Detcore renders from its `maps` line
    /// with this host's facts, through the sanitizers: the derived fields
    /// (`THPeligible`, `ProtectionKey`, `VmFlags`, `kernelpagesize_kB`) are
    /// the kernel's. Another thread of the process may change its mappings
    /// while a file is read, so a read that finds no stack is read again.
    #[test]
    fn this_process_stack_renders_as_the_host_kernel_prints_it() {
        let thp = HostThp::read(Path::new("/sys/kernel/mm/transparent_hugepage")).unwrap();
        let thp_orders = thp.map_or(0, |thp| thp.anonymous_orders());
        // SAFETY: PR_GET_THP_DISABLE reads a flag of this process.
        let disabled = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        assert!(disabled >= 0);
        let facts = SmapsFacts {
            protection_keys_enabled: host_protection_keys_enabled().unwrap(),
            protection_keys_used: false,
            mapping_flags_changed: false,
            thp_orders,
            thp_disabled: Some(disabled != 0),
        };
        let block = |text: &str| -> Option<String> {
            let start = text.find("[stack]\n")?;
            let start = text[..start].rfind('\n').map_or(0, |at| at + 1);
            let end = text[start..].find("VmFlags:")? + start;
            let end = text[end..].find('\n')? + end + 1;
            Some(text[start..end].to_string())
        };
        let kernel = (0..100)
            .find_map(|_| block(&String::from_utf8(std::fs::read("/proc/self/smaps").ok()?).ok()?))
            .expect("a stack block");
        let header = kernel.lines().next().unwrap();
        let fact = MemoryMap::from_maps(format!("{header}\n").as_bytes())
            .unwrap()
            .mappings
            .remove(0);
        let start = fact.start;
        let map = MemoryMap::new(vec![fact], false).unwrap();
        let table = BTreeMap::new();
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps(
                &render_smaps(&map, &facts).unwrap(),
                &table
            ))
            .unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps(kernel.as_bytes(), &table)).unwrap()
        );
        let row = std::fs::read_to_string("/proc/self/numa_maps")
            .unwrap()
            .lines()
            .find(|line| line.starts_with(&format!("{start:08x} ")))
            .map(|line| format!("{line}\n"))
            .expect("the stack's row");
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_numa_maps(
                &render_numa_maps(&map, false).unwrap()
            ))
            .unwrap(),
            String::from_utf8(crate::procfs::sanitize_numa_maps(row.as_bytes())).unwrap()
        );
    }
}
