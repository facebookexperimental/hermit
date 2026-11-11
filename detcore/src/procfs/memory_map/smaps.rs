/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A process's `smaps` and `smaps_rollup` rendered from the backend's
//! memory map (`reverie::Guest::memory_map`), where Detcore renders them
//! (`procfs::RenderedFiles::address_space`: where the guest's procfs is not
//! the kernel's, KVM). Each is the text Linux prints for the listed mappings
//! (`show_smap`, `__show_smap` and `show_smaps_rollup` in
//! `fs/proc/task_mmu.c`, v6.17), which the sanitizer that serves the
//! kernel's file of the same name on the other backends then normalizes
//! (`procfs::sanitize_smaps`, `sanitize_smaps_rollup`). So for every mapping
//! it lists, the guest reads the
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
//! - `THPeligible` (`thp_vma_allowable_orders(vma, vma->vm_flags, TVA_SMAPS,
//!   THP_ORDERS_ALL)`, include/linux/huge_mm.h and mm/huge_memory.c, v7.1.3):
//!   0 for a mapping whose flags hold `VM_NOHUGEPAGE`, which `MAP_STACK`
//!   sets (`vma_thp_disabled`). A mapping whose flags hold `VM_HUGEPAGE` is
//!   refused: Detcore answers every `MADV_HUGEPAGE` and `MADV_NOHUGEPAGE`
//!   itself and passes neither on (`syscalls::memory`), so only a backend's
//!   own flags could hold it, and such a mapping's orders follow host
//!   settings Detcore does not read. For anonymous memory (`vma_is_anonymous`:
//!   a private anonymous mapping, the heap, the stack), 1 where the host's
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
//!   difference as its range and `Size`. Any other mapping (a file's, shared
//!   memory's) prints 0 where no order a file may have fits it, a block
//!   aligned to its size in the address space and in the file
//!   (`thp_vma_suitable_order`), whatever backs it. Where the backend asked
//!   the kernel it runs on (`MappingFact::thp_eligible`: the value that
//!   kernel prints for a mapping of its own of the same open file, with the
//!   same protection, sharing, length and offset within a PMD-sized block,
//!   which LinuxEmu asks for a file on overlayfs or tmpfs), that answer is
//!   the value, but 0 where the address space disabled huge pages
//!   (`PR_SET_THP_DISABLE`, asked of the backend as for anonymous memory:
//!   either of its modes refuses them to a mapping `MADV_HUGEPAGE` did not
//!   mark, `vma_thp_disabled`). The kernel beneath ptrace gives the guest's
//!   own mapping of the file the same answer. Otherwise what backs it
//!   decides, after the host's `enabled` setting ([`HostThp::enabled`]): a
//!   file on a disk filesystem (btrfs, ext2/3/4, XFS), which the host's mount
//!   table names for the device the memory map gives ([`HostFilesystems`]),
//!   prints 0 under `madvise` and `never`. Under `always` it depends on the
//!   filesystem's huge page fault handler and on whether any task holds the
//!   file open for writing (`file_thp_enabled`), which no fact says, so the
//!   file is refused. Shared memory follows
//!   shmem's own settings, its mount's `huge=` option and the file's size
//!   (`shmem_allowable_huge_orders`), and overlayfs the filesystem of a layer
//!   it does not show, so where the backend did not ask, a file on tmpfs, on
//!   overlayfs, on any other filesystem or on a device the mount table does
//!   not list (the kernel's internal mount that backs shared anonymous memory
//!   and memfds) is refused by name.
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
//! - `VmFlags`: the flags the memory map gives the mapping
//!   ([`MappingFact::vm_flags`], which the backend establishes), as
//!   `show_smap_vma_flags` prints them: the two-letter name of each, in the
//!   order of its bit, each with the space after it ([`vm_flags_line`]). A
//!   mapping's flags depend on how it was made (`MAP_GROWSDOWN`,
//!   `MAP_NORESERVE`, the file's open mode and mount, the history of
//!   VM_ACCOUNT), so where the memory map gives none `smaps` is refused, but
//!   for the process's initial stack (`[stack]`), whose flags Detcore derives:
//!   the flags `setup_arg_pages` (fs/exec.c) leaves it with,
//!   `VM_STACK_FLAGS` with the protection the memory map lists: VM_READ,
//!   VM_WRITE and VM_EXEC as the protection says, VM_MAYREAD, VM_MAYWRITE,
//!   VM_MAYEXEC, VM_GROWSDOWN and VM_ACCOUNT (`rd wr mr mw me gd ac`).
//!   `mprotect(2)` changes only the protection bits there: it keeps
//!   VM_ACCOUNT on a mapping that has an `anon_vma` (`mprotect_fixup`), which
//!   the stack always has, as exec wrote the arguments into it. The fork and
//!   dump advice of `madvise(2)` sets flags (`dc`, `dd`, `wf`) no memory map
//!   gives, and Detcore does not record which mapping such a call reached, so
//!   a map of an address space where it, access-pattern advice or `mlock(2)`
//!   reached the backend for a listed mapping
//!   (`MemoryMetadata::mapping_flags_changed`) is refused; a call on a mapping
//!   the memory map does not list changes nothing the file shows.
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

/// The orders of transparent huge page a mapping that is not anonymous
/// memory can have (`THP_ORDERS_ALL_FILE_DEFAULT`: orders 1 to
/// `MAX_PAGECACHE_ORDER`, 9 on x86-64 with 4 KiB pages, include/linux/huge_mm.h
/// and include/linux/pagemap.h in v7.1.3).
const THP_ORDERS_ALL_FILE_DEFAULT: u64 = ((1 << 10) - 1) & !0b1;

/// The filesystems of the disks Hermit's hosts use, whose files are not
/// shmem's, so a mapping of one that `MADV_HUGEPAGE` did not mark may have
/// huge pages only where the host's setting is `always`
/// (`__thp_vma_allowable_orders`, mm/huge_memory.c:183-191 at v7.1.3). Under
/// `always` the filesystem's own fault handler (`huge_fault`, which XFS
/// gives every file) and whether any task holds the file open for writing
/// (`file_thp_enabled`) decide (:197-202).
const DISK_FILESYSTEMS: [&str; 5] = ["btrfs", "ext2", "ext3", "ext4", "xfs"];

/// The size of a page, and of every page of a mapping that is not hugetlbfs
/// memory (`vma_kernel_pagesize`, `vma_mmu_pagesize`).
const PAGE_SIZE: u64 = 4096;

/// The host's global transparent huge page setting, `enabled` under
/// `/sys/kernel/mm/transparent_hugepage` (`hugepage_global_enabled` and
/// `hugepage_global_always`, include/linux/huge_mm.h).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThpEnabled {
    /// `always`: a mapping may have huge pages that no `MADV_HUGEPAGE`
    /// marked.
    Always,
    /// `madvise`: only a mapping `MADV_HUGEPAGE` marked.
    Madvise,
    /// `never`.
    Never,
}

/// What the host kernel's transparent huge page settings allow a mapping, as
/// `/sys/kernel/mm/transparent_hugepage` shows them: the inputs of
/// `thp_vma_allowable_orders`'s mask under the sysfs settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostThp {
    /// The global setting (`enabled`), read when the settings are.
    pub(crate) enabled: ThpEnabled,
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
        if self.enabled == ThpEnabled::Always {
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
        let enabled = match selected(&global) {
            Some("always") => ThpEnabled::Always,
            Some("madvise") => ThpEnabled::Madvise,
            Some("never") => ThpEnabled::Never,
            Some(_) => {
                return Err(format!(
                    "{}/enabled has no setting Linux gives",
                    root.display()
                ));
            }
            None => {
                return Err(format!(
                    "{}/enabled has no selected setting",
                    root.display()
                ));
            }
        };
        let mut thp = HostThp {
            enabled,
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

/// The filesystems of the supervisor's mounts, each by the device of its
/// superblock, as its `/proc/self/mountinfo` lists them: the `major:minor`
/// field, the superblock's `s_dev`, and the filesystem type after the
/// separator (`show_mountinfo`, fs/proc_namespace.c). A file's device in the
/// memory map (`MappingBacking::File`) is that field of that table, the
/// mount the backend opened the file through, so it names the filesystem the
/// file is on; a mount of the kernel's own (the internal one that backs
/// shared anonymous memory and memfds) is in no table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct HostFilesystems {
    types: std::collections::BTreeMap<u64, String>,
}

impl HostFilesystems {
    /// The filesystems `mountinfo`, the text of a `mountinfo` file, lists.
    /// `Err` names a line that is not one Linux prints.
    pub(crate) fn parse(mountinfo: &str) -> Result<Self, String> {
        let mut types = std::collections::BTreeMap::new();
        for line in mountinfo.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            let device = fields.get(2).and_then(|device| {
                let (major, minor) = device.split_once(':')?;
                Some(libc::makedev(major.parse().ok()?, minor.parse().ok()?))
            });
            // The optional fields end at the separator, which the type follows.
            let filesystem = fields
                .iter()
                .skip(6)
                .position(|field| *field == "-")
                .and_then(|at| fields.get(6 + at + 1));
            match (device, filesystem) {
                (Some(device), Some(filesystem)) => {
                    types.insert(device, (*filesystem).to_string());
                }
                _ => return Err(format!("a mountinfo line Detcore cannot read: {line}")),
            }
        }
        Ok(Self { types })
    }

    /// The type of the filesystem whose superblock has device `raw_device`
    /// (`libc::makedev`'s encoding), where a mount of it is listed.
    fn filesystem(&self, raw_device: u64) -> Option<&str> {
        self.types.get(&raw_device).map(String::as_str)
    }
}

/// The supervisor's filesystems ([`HostFilesystems::parse`] of its
/// `/proc/self/mountinfo`), read once per tool process, as the host's
/// transparent huge page settings are ([`host_thp`]), so every `smaps` a run
/// renders takes the same answer. A run's mounts are made before its guest
/// starts, and the guest makes none (Detcore refuses `mount(2)` and
/// `umount2(2)`): a device a later mount of the host's added is in no table,
/// so a mapping of it is refused rather than misread. A device number names
/// one superblock until that superblock is gone, and a mapped file keeps its
/// superblock, so the table names a mapped file's filesystem unless the host
/// itself unmounted a filesystem and mounted another while the run went on.
pub(crate) fn host_filesystems() -> Result<&'static HostFilesystems, &'static str> {
    static HOST: std::sync::OnceLock<Result<HostFilesystems, String>> = std::sync::OnceLock::new();
    HOST.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("/proc/self/mountinfo: {error}"))?;
        HostFilesystems::parse(&text)
    })
    .as_ref()
    .map_err(String::as_str)
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
    /// The host's global transparent huge page setting
    /// ([`HostThp::enabled`]); `None` where the host has no transparent huge
    /// pages, where every mapping's answer is 0.
    pub(crate) thp_enabled: Option<ThpEnabled>,
    /// The orders of transparent huge page an anonymous mapping may have
    /// ([`HostThp::anonymous_orders`]), none where the host has none.
    pub(crate) thp_orders: u64,
    /// Whether the address space disabled transparent huge pages
    /// (`PR_GET_THP_DISABLE`), where it decides an answer: [`Self::thp_orders`]
    /// allows an order, or the kernel's answer for a mapping
    /// (`MappingFact::thp_eligible`) is 1; `None` where it was not asked
    /// because neither holds.
    pub(crate) thp_disabled: Option<bool>,
}

/// Linux's `THPeligible` for an anonymous mapping `[start, end)` that no
/// `MADV_HUGEPAGE` or `MADV_NOHUGEPAGE` marked, which may have huge pages of
/// `orders` (`thp_vma_allowable_orders`), in an address space that has
/// (`disabled`) or has not disabled them: the first of those orders, from the
/// largest, for which the mapping holds a naturally aligned block of that
/// size ([`holds_block`]).
fn thp_eligible(orders: u64, disabled: bool, start: u64, end: u64) -> bool {
    if orders == 0 || disabled {
        return false;
    }
    (2..=9)
        .rev()
        .filter(|order| orders & (1 << order) != 0)
        .any(|order| holds_block(start, end, PAGE_SIZE << order))
}

/// Whether `thp_vma_suitable_order` (include/linux/huge_mm.h:220-238 at
/// v7.1.3) finds a block of `size` bytes, aligned to its size, in the mapping
/// `[start, end)`, where `__thp_vma_allowable_orders` looks for it: at `end`
/// less the size, rounded down to the size (mm/huge_memory.c:163). Linux
/// computes both bounds in unsigned arithmetic, which wraps, so a mapping that
/// ends below `size` passes them: on Linux 7.1.3 a 4 KiB anonymous mapping at
/// 0x100000 that `MADV_HUGEPAGE` marked prints `THPeligible: 1` with only the
/// 2 MiB order allowed, where the same mapping high in the address space
/// prints 0.
fn holds_block(start: u64, end: u64, size: u64) -> bool {
    let aligned = end.wrapping_sub(size) & !(size - 1);
    aligned >= start && aligned.wrapping_add(size) <= end
}

/// Whether a mapping that is not anonymous memory, `[start, end)` from page
/// `pgoff` of what backs it, holds a block of one of `orders` aligned to the
/// block's size both in the address space and in the file: the filter
/// `__thp_vma_allowable_orders` (mm/huge_memory.c:158-171 at v7.1.3) applies
/// with `thp_vma_suitable_order`, which for such a mapping also requires the
/// file's offset to be aligned as its address is ([`holds_block`]), from the
/// largest order. Where none fits, Linux's answer is 0 whatever backs the
/// mapping.
fn file_order_fits(orders: u64, start: u64, end: u64, pgoff: u64) -> bool {
    (1..=9)
        .rev()
        .filter(|order| orders & (1 << order) != 0)
        .any(|order| {
            let pages = 1u64 << order;
            (start / PAGE_SIZE).wrapping_sub(pgoff) & (pages - 1) == 0
                && holds_block(start, end, PAGE_SIZE << order)
        })
}

/// Linux's `THPeligible` for `mapping` with the flags `vm_flags` (`None`:
/// the initial stack's derived flags, which hold neither huge page flag), or
/// the refusal that names what decides it and no fact says (the module's
/// docs; `thp_vma_allowable_orders` and `__thp_vma_allowable_orders`,
/// include/linux/huge_mm.h:288-313 and mm/huge_memory.c:111-219 at v7.1.3).
/// `filesystems` is the host's mount table, or why it could not be read.
fn thp_eligibility(
    mapping: &MappingFact,
    vm_flags: Option<u64>,
    facts: &SmapsFacts,
    filesystems: Result<&HostFilesystems, &str>,
) -> Result<bool, Refusal> {
    let flags = vm_flags.unwrap_or(0);
    // No transparent huge pages on the host, or the mapping's own refusal of
    // them (`vma_thp_disabled`).
    let Some(enabled) = facts.thp_enabled else {
        return Ok(false);
    };
    if flags & reverie::VM_NOHUGEPAGE != 0 {
        return Ok(false);
    }
    if flags & reverie::VM_HUGEPAGE != 0 {
        return Err(format!(
            "Linux's smaps prints whether {} may have transparent huge pages, and for a \
             mapping MADV_HUGEPAGE marked that follows host settings Detcore does not read",
            named(mapping)
        ));
    }
    let backing = match &mapping.backing {
        MappingBacking::Anonymous
        | MappingBacking::Stack
        | MappingBacking::Heap
        | MappingBacking::NamedAnonymous(_) => {
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
            return Ok(thp_eligible(
                facts.thp_orders,
                thp_disabled,
                mapping.start,
                mapping.end,
            ));
        }
        // The kernel's own mappings have no file and no `huge_fault`.
        MappingBacking::Vdso | MappingBacking::Vvar | MappingBacking::Special(_) => {
            return Ok(false);
        }
        MappingBacking::File { raw_device, .. } => Some(*raw_device),
        MappingBacking::NamedSharedAnonymous { .. } => None,
        _ => {
            return Err(format!(
                "Linux's smaps prints whether {} may have transparent huge pages, and Detcore \
                 does not know what backs it",
                named(mapping)
            ));
        }
    };
    if !file_order_fits(
        THP_ORDERS_ALL_FILE_DEFAULT,
        mapping.start,
        mapping.end,
        mapping.offset / PAGE_SIZE,
    ) {
        return Ok(false);
    }
    if let Some(eligible) = mapping.thp_eligible {
        // The value the kernel the backend runs on printed for a mapping of
        // its own of the same file and shape (`MappingFact::thp_eligible`),
        // which answers for the mapping alone: the address space's own
        // refusal of huge pages (`PR_SET_THP_DISABLE`, `vma_thp_disabled`)
        // makes it 0, in either of that call's modes, as no `MADV_HUGEPAGE`
        // marked the mapping (refused above).
        return match (eligible, facts.thp_disabled) {
            (false, _) | (true, Some(false)) => Ok(eligible),
            (true, Some(true)) => Ok(false),
            (true, None) => Err(format!(
                "Linux's smaps prints whether {} may have transparent huge pages, and whether \
                 the address space disabled them was not asked",
                named(mapping)
            )),
        };
    }
    let Some(raw_device) = backing else {
        return Err(format!(
            "Linux's smaps prints whether {} may have transparent huge pages, which for shared \
             anonymous memory follow shmem's settings, which Detcore does not model",
            named(mapping)
        ));
    };
    let filesystems = filesystems.map_err(|error| {
        format!(
            "Linux's smaps prints whether {} may have transparent huge pages, which the \
             filesystem of the file decides, and the host's mount table could not be read: \
             {error}",
            named(mapping)
        )
    })?;
    match filesystems.filesystem(raw_device) {
        Some(filesystem) if DISK_FILESYSTEMS.contains(&filesystem) => match enabled {
            ThpEnabled::Madvise | ThpEnabled::Never => Ok(false),
            ThpEnabled::Always => Err(format!(
                "Linux's smaps prints whether {} may have transparent huge pages, which for a \
                 file on {filesystem} under the host's setting always depends on the \
                 filesystem's huge page fault handler and on whether any task holds the file \
                 open for writing, which no fact says",
                named(mapping)
            )),
        },
        Some(filesystem) => Err(format!(
            "Linux's smaps prints whether {} may have transparent huge pages, which for a file \
             on {filesystem} follow rules Detcore does not model (shmem's mount option and the \
             file's size on tmpfs, the filesystem of a layer on overlayfs)",
            named(mapping)
        )),
        None => Err(format!(
            "Linux's smaps prints whether {} may have transparent huge pages, which the \
             filesystem of the file decides, and the host's mount table lists none for its \
             device (the kernel's internal mount backs shared anonymous memory and memfds)",
            named(mapping)
        )),
    }
}

/// `show_smap_vma_flags`'s name for each flag a memory map may give
/// (`reverie::VM_KNOWN_FLAGS`; fs/proc/task_mmu.c:1147-1218 at v7.1.3), in
/// the order of the flags' bits, which is the order Linux prints them in.
const VM_FLAG_NAMES: [(u64, &str); 19] = [
    (reverie::VM_READ, "rd"),
    (reverie::VM_WRITE, "wr"),
    (reverie::VM_EXEC, "ex"),
    (reverie::VM_SHARED, "sh"),
    (reverie::VM_MAYREAD, "mr"),
    (reverie::VM_MAYWRITE, "mw"),
    (reverie::VM_MAYEXEC, "me"),
    (reverie::VM_MAYSHARE, "ms"),
    (reverie::VM_GROWSDOWN, "gd"),
    (reverie::VM_MAYBE_GUARD, "gu"),
    (reverie::VM_LOCKED, "lo"),
    (reverie::VM_SEQ_READ, "sr"),
    (reverie::VM_RAND_READ, "rr"),
    (reverie::VM_ACCOUNT, "ac"),
    (reverie::VM_NORESERVE, "nr"),
    (reverie::VM_SYNC, "sf"),
    (reverie::VM_HUGEPAGE, "hg"),
    (reverie::VM_NOHUGEPAGE, "nh"),
    (reverie::VM_MERGEABLE, "mg"),
];

/// The `VmFlags` line Linux prints for a mapping with the flags `vm_flags`
/// (`show_smap_vma_flags`, fs/proc/task_mmu.c:1221-1229 at v7.1.3): the name
/// of each flag, in the order of its bit, with the space after it. `None`
/// where a flag has no name here, which no memory map gives
/// (`reverie::MappingFact::validate`).
fn vm_flags_line(vm_flags: u64) -> Option<String> {
    let named = VM_FLAG_NAMES.iter().fold(0, |all, (bit, _)| all | bit);
    if vm_flags & !named != 0 {
        return None;
    }
    let mut line = String::from("VmFlags: ");
    for (bit, name) in VM_FLAG_NAMES {
        if vm_flags & bit != 0 {
            line.push_str(name);
            line.push(' ');
        }
    }
    line.push('\n');
    Some(line)
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
/// `facts` and the host's mount table `filesystems` (or why it could not be
/// read), or the refusal that names why Detcore cannot give one of them.
pub(crate) fn render_smaps(
    map: &MemoryMap,
    facts: &SmapsFacts,
    filesystems: Result<&HostFilesystems, &str>,
) -> Result<Vec<u8>, Refusal> {
    refuse_empty_incomplete(map, "smaps")?;
    let mut out = Vec::new();
    for mapping in &map.mappings {
        // The initial stack's flags Detcore derives where the memory map
        // gives none (the module's docs).
        let vm_flags = match (mapping.vm_flags, &mapping.backing) {
            (Some(vm_flags), _) => Some(vm_flags),
            (None, MappingBacking::Stack) => None,
            (None, _) => {
                return Err(format!(
                    "Linux's smaps prints the flags it keeps on each mapping, and for {} \
                     they are not in the backend's memory map",
                    named(mapping)
                ));
            }
        };
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
        let eligible = thp_eligibility(mapping, vm_flags, facts, filesystems)?;
        let flags_line = match vm_flags {
            Some(vm_flags) => vm_flags_line(vm_flags).ok_or_else(|| {
                format!(
                    "the backend's memory map gives {} a flag Detcore has no name for: \
                     {vm_flags:#x}",
                    named(mapping)
                )
            })?,
            None => stack_vm_flags(mapping.prot),
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
        out.extend_from_slice(format!("THPeligible:    {:>8}\n", u8::from(eligible)).as_bytes());
        if facts.protection_keys_enabled {
            out.extend_from_slice(format!("ProtectionKey:  {:>8}\n", 0).as_bytes());
        }
        out.extend_from_slice(flags_line.as_bytes());
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
        thp_enabled: Some(ThpEnabled::Madvise),
        thp_orders: 0,
        thp_disabled: None,
    };

    /// [`render_smaps`] of `map` with `facts`, where the host's mount table
    /// lists nothing.
    fn render(map: &MemoryMap, facts: &SmapsFacts) -> Result<Vec<u8>, Refusal> {
        render_smaps(map, facts, Ok(&HostFilesystems::default()))
    }

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
        let rendered = render(&map, &HOST).unwrap();
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
            crate::procfs::sanitize_smaps(&render(&map, &HOST).unwrap(), &BTreeMap::new());
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
            String::from_utf8(render(&map, facts).unwrap()).unwrap()
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
        // A mapping that ends below the block's size holds it in Linux's
        // wrapping arithmetic ([`holds_block`]), as measured on Linux 7.1.3;
        // one that ends above it but holds no aligned block does not.
        assert!(thp_eligible(pmd, false, 0x10_0000, 0x10_1000));
        assert!(!thp_eligible(pmd, false, 0x20_0000, 0x20_1000));
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
        let text = String::from_utf8(render(&map, &facts).unwrap()).unwrap();
        assert!(text.contains("THPeligible:           1\n"), "{text}");
        let unasked = SmapsFacts {
            thp_disabled: None,
            ..facts
        };
        assert!(render(&map, &unasked).is_err());
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
                enabled: ThpEnabled::Madvise,
                always: 1 << 4,
                inherit: 1 << 9,
            }
        );
        assert_eq!(thp.anonymous_orders(), 1 << 4);
        std::fs::write(path.join("enabled"), "[always] madvise never\n").unwrap();
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(thp.enabled, ThpEnabled::Always);
        assert_eq!(thp.anonymous_orders(), 1 << 4 | 1 << 9);
        std::fs::write(path.join("enabled"), "always madvise [never]\n").unwrap();
        let thp = HostThp::read(&path).unwrap().unwrap();
        assert_eq!(thp.enabled, ThpEnabled::Never);
        assert_eq!(thp.anonymous_orders(), 1 << 4);
        std::fs::write(path.join("enabled"), "always madvise never\n").unwrap();
        assert!(HostThp::read(&path).is_err());
        std::fs::write(path.join("enabled"), "always [sometimes] never\n").unwrap();
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
        let refusal = render(&split, &HOST).unwrap_err();
        assert!(refusal.contains("7fffff7f0000-7fffff800000"), "{refusal}");
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
            let refusal = render(&lone, &facts).unwrap_err();
            assert!(refusal.contains("[stack]"), "{refusal}");
        }
        // A key matters only where Linux prints keys.
        render(
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
        assert!(render(&none, &HOST).is_err());
        let empty = MemoryMap::new(Vec::new(), true).unwrap();
        assert_eq!(render(&empty, &HOST).unwrap(), b"");
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
    /// `smaps`, is what Detcore renders from its `maps` line with this
    /// host's facts, through the sanitizer: the derived fields
    /// (`THPeligible`, `ProtectionKey`, `VmFlags`) are the kernel's. Another thread of the process may change its mappings
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
            thp_enabled: thp.map(|thp| thp.enabled),
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
        let map = MemoryMap::new(vec![fact], false).unwrap();
        let table = BTreeMap::new();
        assert_eq!(
            String::from_utf8(crate::procfs::sanitize_smaps(
                &render(&map, &facts).unwrap(),
                &table
            ))
            .unwrap(),
            String::from_utf8(crate::procfs::sanitize_smaps(kernel.as_bytes(), &table)).unwrap()
        );
    }

    /// `READ` and the three `MAY` flags every mapping of these tests has.
    const READ_ONLY: u64 =
        reverie::VM_READ | reverie::VM_MAYREAD | reverie::VM_MAYWRITE | reverie::VM_MAYEXEC;

    /// A mapping made from its `maps` line, written with one space before
    /// the name, with the flags `vm_flags`. Linux ends the header with a
    /// space, and pads it to column 72 and adds one more before a name
    /// (`show_vma_header_prefix` and `seq_pad`).
    fn with_flags(maps_line: &str, vm_flags: u64) -> MappingFact {
        let fields: Vec<&str> = maps_line.splitn(6, ' ').collect();
        let header = format!("{} ", fields[..5].join(" "));
        let line = match fields.get(5) {
            Some(name) => format!("{header:<72} {name}\n"),
            None => format!("{header}\n"),
        };
        MemoryMap::from_maps(line.as_bytes())
            .unwrap()
            .mappings
            .remove(0)
            .with_vm_flags(vm_flags)
            .unwrap()
    }

    /// The blocks of the `smaps` text `text`, one per mapping, each from its
    /// header through its `VmFlags` line.
    fn blocks(text: &str) -> Vec<String> {
        let mut blocks: Vec<String> = Vec::new();
        for line in text.split_inclusive('\n') {
            let header = line
                .split_once(' ')
                .and_then(|(range, _)| range.split_once('-'))
                .is_some_and(|(start, end)| {
                    u64::from_str_radix(start, 16).is_ok() && u64::from_str_radix(end, 16).is_ok()
                });
            match blocks.last_mut() {
                Some(block) if !header => block.push_str(line),
                _ => blocks.push(line.to_string()),
            }
        }
        blocks
    }

    /// The flags a `smaps` block's `VmFlags` line names, or `None` where it
    /// names one no memory map gives.
    fn named_flags(block: &str) -> Option<u64> {
        let line = block
            .lines()
            .find_map(|line| line.strip_prefix("VmFlags:"))?;
        line.split_whitespace().try_fold(0, |flags, name| {
            VM_FLAG_NAMES
                .iter()
                .find(|(_, known)| *known == name)
                .map(|(bit, _)| flags | bit)
        })
    }

    /// Each flag a memory map may give has Linux's name, in the order of its
    /// bit, and a line names each flag a mapping has as Linux 7.1.3 printed
    /// lines of the same flags on this repository's test host.
    #[test]
    fn vm_flags_are_named_in_the_order_of_their_bits() {
        let bits: Vec<u64> = VM_FLAG_NAMES.iter().map(|(bit, _)| *bit).collect();
        assert!(bits.windows(2).all(|pair| pair[0] < pair[1]), "{bits:?}");
        assert!(bits.iter().all(|bit| bit.is_power_of_two()), "{bits:?}");
        assert_eq!(
            bits.iter().fold(0, |all, bit| all | bit),
            reverie::VM_KNOWN_FLAGS
        );
        for (flags, linux) in [
            (READ_ONLY, "VmFlags: rd mr mw me \n"),
            (READ_ONLY | reverie::VM_EXEC, "VmFlags: rd ex mr mw me \n"),
            (
                READ_ONLY | reverie::VM_ACCOUNT,
                "VmFlags: rd mr mw me ac \n",
            ),
            (
                READ_ONLY | reverie::VM_WRITE | reverie::VM_ACCOUNT,
                "VmFlags: rd wr mr mw me ac \n",
            ),
            (
                READ_ONLY | reverie::VM_WRITE | reverie::VM_GROWSDOWN | reverie::VM_ACCOUNT,
                "VmFlags: rd wr mr mw me gd ac \n",
            ),
            (
                reverie::VM_READ | reverie::VM_MAYREAD | reverie::VM_MAYEXEC | reverie::VM_MAYSHARE,
                "VmFlags: rd mr me ms \n",
            ),
        ] {
            assert_eq!(vm_flags_line(flags).as_deref(), Some(linux));
        }
        assert_eq!(
            vm_flags_line(reverie::VM_KNOWN_FLAGS).as_deref(),
            Some("VmFlags: rd wr ex sh mr mw me ms gd gu lo sr rr ac nr sf hg nh mg \n")
        );
        // VM_PFNMAP (`pf`), which no memory map gives, has no name here.
        assert_eq!(vm_flags_line(reverie::VM_READ | 1 << 10), None);
    }

    /// A mapping of each kind a memory map gives flags for renders them: a
    /// file's on btrfs, the heap, anonymous memory, a thread's stack (whose
    /// `MAP_STACK` set `nh`) and the initial stack. A mapping without flags
    /// refuses the file by name, but for the initial stack, whose flags are
    /// derived.
    #[test]
    fn a_mapping_of_each_kind_renders_its_flags() {
        let filesystems = HostFilesystems::parse(
            "34825 34824 0:32 / / rw,relatime shared:1 - btrfs /dev/nvme1n1p4 rw\n",
        )
        .unwrap();
        let written = READ_ONLY | reverie::VM_WRITE | reverie::VM_ACCOUNT;
        let map = MemoryMap::new(
            vec![
                with_flags(
                    "00400000-00401000 r-xp 00000000 00:20 41 /p",
                    READ_ONLY | reverie::VM_EXEC,
                ),
                with_flags("00402000-00423000 rw-p 00000000 00:00 0 [heap]", written),
                with_flags("7f0000000000-7f0000010000 rw-p 00000000 00:00 0", written),
                with_flags(
                    "7f0000200000-7f0000a00000 rw-p 00000000 00:00 0",
                    written | reverie::VM_NOHUGEPAGE,
                ),
                stack(0x7ffc_ec80_f000, 0x7ffc_ec83_3000, read_write()),
            ],
            true,
        )
        .unwrap();
        // An `always` 2 MiB order, which the thread's stack would hold but for
        // its `nh`.
        let facts = SmapsFacts {
            thp_orders: 1 << 9,
            thp_disabled: Some(false),
            ..HOST
        };
        let text =
            String::from_utf8(render_smaps(&map, &facts, Ok(&filesystems)).unwrap()).unwrap();
        let tails: Vec<String> = blocks(&text)
            .iter()
            .map(|block| {
                let at = block.find("THPeligible:").unwrap();
                block[at..].to_string()
            })
            .collect();
        assert_eq!(
            tails,
            [
                "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd ex mr mw me \n",
                "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me ac \n",
                "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me ac \n",
                "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me ac nh \n",
                "THPeligible:           0\nProtectionKey:         0\nVmFlags: rd wr mr mw me gd ac \n",
            ],
            "{text}"
        );
        assert!(
            text.starts_with(
                "00400000-00401000 r-xp 00000000 00:20 41                                 /p\n\
                 Size:                  4 kB\n"
            ),
            "{text}"
        );
        // A mapping whose flags the memory map does not give.
        let flagless = MappingFact::new(
            0x7f00_0000_0000,
            0x7f00_0001_0000,
            (libc::PROT_READ | libc::PROT_WRITE) as u32,
            false,
            0,
            MappingBacking::Anonymous,
        )
        .unwrap();
        let refusal = render_smaps(
            &MemoryMap::new(vec![flagless], true).unwrap(),
            &facts,
            Ok(&filesystems),
        )
        .unwrap_err();
        assert!(
            refusal.contains("7f0000000000-7f0000010000")
                && refusal.contains("they are not in the backend's memory map"),
            "{refusal}"
        );
    }

    /// A file's `THPeligible` where an order fits it: 0 on a disk filesystem
    /// under the host's `madvise` and `never`, refused under `always`; refused
    /// on tmpfs, on overlayfs and on a device the mount table does not list;
    /// 0 on a host without transparent huge pages; and refused for a mapping
    /// `MADV_HUGEPAGE` marked. Shared anonymous memory is refused too.
    #[test]
    fn a_files_thp_eligibility_follows_its_filesystem_and_the_hosts_setting() {
        let filesystems = HostFilesystems::parse(
            "1 0 0:32 / / rw - btrfs /dev/nvme1n1p4 rw\n\
             2 1 8:1 / /data rw - ext4 /dev/sda1 rw\n\
             3 1 259:5 / /scratch rw - xfs /dev/nvme0n1 rw\n\
             4 1 0:24 / /dev/shm rw - tmpfs tmpfs rw,huge=within_size\n\
             5 1 0:47 / /image rw - overlay overlay rw,lowerdir=/l\n",
        )
        .unwrap();
        // 2 MiB at a 2 MiB boundary, from file offset 0: every order fits.
        let eligible = |device: &str, enabled: Option<ThpEnabled>, flags: u64| {
            let mapping = with_flags(
                &format!("7f0000000000-7f0000200000 r--p 00000000 {device} 7 /f"),
                flags,
            );
            let facts = SmapsFacts {
                thp_enabled: enabled,
                ..HOST
            };
            render_smaps(
                &MemoryMap::new(vec![mapping], true).unwrap(),
                &facts,
                Ok(&filesystems),
            )
            .map(|text| {
                String::from_utf8(text)
                    .unwrap()
                    .contains("THPeligible:           1\n")
            })
        };
        // btrfs 0:32, ext4 8:1 and XFS 259:5, as `maps` prints them in hex.
        for device in ["00:20", "08:01", "103:05"] {
            for enabled in [ThpEnabled::Madvise, ThpEnabled::Never] {
                assert_eq!(eligible(device, Some(enabled), READ_ONLY), Ok(false));
            }
            let refusal = eligible(device, Some(ThpEnabled::Always), READ_ONLY).unwrap_err();
            assert!(refusal.contains("open for writing"), "{refusal}");
        }
        for (device, filesystem) in [("00:18", "tmpfs"), ("00:2f", "overlay")] {
            let refusal = eligible(device, Some(ThpEnabled::Madvise), READ_ONLY).unwrap_err();
            assert!(
                refusal.contains(&format!("a file on {filesystem}")),
                "{refusal}"
            );
        }
        let refusal = eligible("00:01", Some(ThpEnabled::Madvise), READ_ONLY).unwrap_err();
        assert!(refusal.contains("lists none for its device"), "{refusal}");
        assert_eq!(eligible("00:18", None, READ_ONLY), Ok(false));
        let refusal = eligible(
            "00:20",
            Some(ThpEnabled::Madvise),
            READ_ONLY | reverie::VM_HUGEPAGE,
        )
        .unwrap_err();
        assert!(refusal.contains("MADV_HUGEPAGE"), "{refusal}");
        assert_eq!(
            eligible(
                "00:18",
                Some(ThpEnabled::Madvise),
                READ_ONLY | reverie::VM_NOHUGEPAGE
            ),
            Ok(false)
        );
        // A mount table that could not be read.
        let mapping = with_flags(
            "7f0000000000-7f0000200000 r--p 00000000 00:20 7 /f",
            READ_ONLY,
        );
        let refusal = render_smaps(
            &MemoryMap::new(vec![mapping], true).unwrap(),
            &HOST,
            Err("no mountinfo"),
        )
        .unwrap_err();
        assert!(refusal.contains("no mountinfo"), "{refusal}");
        // Shared anonymous memory with a name, which shmem backs.
        let shared = MappingFact::new(
            0x7f00_0000_0000,
            0x7f00_0020_0000,
            libc::PROT_READ as u32,
            true,
            0,
            MappingBacking::NamedSharedAnonymous {
                raw_device: libc::makedev(0, 1),
                raw_inode: 3,
                name: b"pool".to_vec(),
            },
        )
        .unwrap()
        .with_vm_flags(READ_ONLY | reverie::VM_SHARED | reverie::VM_MAYSHARE)
        .unwrap();
        let refusal = render_smaps(
            &MemoryMap::new(vec![shared], true).unwrap(),
            &HOST,
            Ok(&filesystems),
        )
        .unwrap_err();
        assert!(refusal.contains("shared anonymous memory"), "{refusal}");
    }

    /// Where the backend asked the host kernel (`MappingFact::thp_eligible`),
    /// a file's `THPeligible` is the kernel's answer, on overlayfs, on tmpfs
    /// and for shared anonymous memory alike, which are refused without it;
    /// the address space's own refusal of huge pages makes it 0, and is
    /// refused unasked. Where the host has none, the mapping refuses them
    /// (`MADV_NOHUGEPAGE`) or no order fits, it stays 0 whatever the kernel
    /// said.
    #[test]
    fn a_files_thp_eligibility_is_the_kernels_answer_where_the_backend_asked() {
        let filesystems = HostFilesystems::parse(
            "4 1 0:24 / /dev/shm rw - tmpfs tmpfs rw,huge=within_size\n\
             5 1 0:47 / /image rw - overlay overlay rw,lowerdir=/l\n",
        )
        .unwrap();
        let render_one = |mapping: MappingFact, facts: &SmapsFacts| {
            render_smaps(
                &MemoryMap::new(vec![mapping], true).unwrap(),
                facts,
                Ok(&filesystems),
            )
            .map(|text| {
                String::from_utf8(text)
                    .unwrap()
                    .contains("THPeligible:           1\n")
            })
        };
        let file = |line: &str, flags: u64, eligible: bool| {
            with_flags(line, flags).with_thp_eligible(eligible)
        };
        let allowed = SmapsFacts {
            thp_disabled: Some(false),
            ..HOST
        };
        let disabled = SmapsFacts {
            thp_disabled: Some(true),
            ..HOST
        };
        // tmpfs 0:24 and overlayfs 0:47, as `maps` prints them in hex; 2 MiB
        // at a 2 MiB boundary, from file offset 0.
        for device in ["00:18", "00:2f"] {
            let line = format!("7f0000000000-7f0000200000 r--p 00000000 {device} 7 /f");
            assert_eq!(render_one(file(&line, READ_ONLY, true), &allowed), Ok(true));
            assert_eq!(
                render_one(file(&line, READ_ONLY, false), &allowed),
                Ok(false)
            );
            assert_eq!(
                render_one(file(&line, READ_ONLY, true), &disabled),
                Ok(false)
            );
            assert_eq!(render_one(file(&line, READ_ONLY, false), &HOST), Ok(false));
            let refusal = render_one(file(&line, READ_ONLY, true), &HOST).unwrap_err();
            assert!(refusal.contains("was not asked"), "{refusal}");
            let no_thp = SmapsFacts {
                thp_enabled: None,
                ..allowed
            };
            assert_eq!(render_one(file(&line, READ_ONLY, true), &no_thp), Ok(false));
            assert_eq!(
                render_one(
                    file(&line, READ_ONLY | reverie::VM_NOHUGEPAGE, true),
                    &allowed
                ),
                Ok(false)
            );
            // Offset 4 KiB into the file: no block is aligned in both.
            let misaligned = format!("7f0000000000-7f0000200000 r--p 00001000 {device} 7 /f");
            assert_eq!(
                render_one(file(&misaligned, READ_ONLY, true), &allowed),
                Ok(false)
            );
        }
        let shared = MappingFact::new(
            0x7f00_0000_0000,
            0x7f00_0020_0000,
            libc::PROT_READ as u32,
            true,
            0,
            MappingBacking::NamedSharedAnonymous {
                raw_device: libc::makedev(0, 1),
                raw_inode: 3,
                name: b"pool".to_vec(),
            },
        )
        .unwrap()
        .with_vm_flags(READ_ONLY | reverie::VM_SHARED | reverie::VM_MAYSHARE)
        .unwrap()
        .with_thp_eligible(true);
        assert_eq!(render_one(shared, &allowed), Ok(true));
    }

    /// Where no order a file may have fits a mapping, a block aligned to its
    /// size both in the address space and in the file, `THPeligible` is 0
    /// whatever backs the mapping, even on tmpfs; where one fits, as on the
    /// 8 KiB tmpfs mapping Linux 7.1.3 printed 1 for on this repository's
    /// test host, what backs it decides.
    #[test]
    fn a_mapping_no_file_order_fits_is_not_eligible_on_any_filesystem() {
        let fits = |start: u64, end: u64, pgoff: u64| {
            file_order_fits(THP_ORDERS_ALL_FILE_DEFAULT, start, end, pgoff)
        };
        let base = 0x7f00_0000_0000;
        // One page holds no block; two aligned pages hold an order-1 one.
        assert!(!fits(base, base + 0x1000, 0));
        assert!(fits(base, base + 0x2000, 0));
        // Aligned in the address space but not in the file, and the reverse.
        assert!(!fits(base, base + 0x2000, 1));
        assert!(!fits(base + 0x1000, base + 0x3000, 1));
        // Aligned in both, past an odd first page.
        assert!(fits(base + 0x1000, base + 0x5000, 1));
        // The largest file order, 2 MiB.
        assert!(fits(base, base + 0x20_0000, 0));
        assert!(fits(base + 0x1000, base + 0x40_0000, 0x201));
        // A mapping that ends below a block's size, its offset aligned as its
        // address is, holds the block in Linux's wrapping arithmetic
        // ([`holds_block`]); the same mapping from offset 0 holds none.
        assert!(fits(0x1_0000, 0x1_1000, 0x10));
        assert!(!fits(0x1_0000, 0x1_1000, 0));
        let filesystems =
            HostFilesystems::parse("4 1 0:24 / /dev/shm rw - tmpfs tmpfs rw,huge=within_size\n")
                .unwrap();
        let render_one = |line: &str| {
            render_smaps(
                &MemoryMap::new(vec![with_flags(line, READ_ONLY)], true).unwrap(),
                &HOST,
                Ok(&filesystems),
            )
        };
        let text = String::from_utf8(
            render_one("7f0000001000-7f0000002000 r--p 00000000 00:18 9 /dev/shm/f").unwrap(),
        )
        .unwrap();
        assert!(text.contains("THPeligible:           0\n"), "{text}");
        assert!(render_one("7f0000000000-7f0000002000 r--p 00000000 00:18 9 /dev/shm/f").is_err());
    }

    /// The mount table names the filesystem of each superblock it lists, by
    /// its device, through optional fields and escaped mount points, and
    /// refuses a line that is not one Linux prints.
    #[test]
    fn the_mount_table_names_each_superblocks_filesystem() {
        let table = HostFilesystems::parse(
            "34825 34824 0:32 / / rw,relatime shared:1 - btrfs /dev/nvme1n1p4 rw,ssd\n\
             34827 34825 0:24 / /dev/shm rw,nosuid,nodev shared:3 master:7 - tmpfs tmpfs rw\n\
             35029 34825 0:47 /tmpvol /tmp\\040dir rw,noatime - btrfs /dev/md0 rw\n\
             40000 34825 259:5 / /data rw - xfs /dev/nvme0n1 rw\n",
        )
        .unwrap();
        assert_eq!(table.filesystem(libc::makedev(0, 32)), Some("btrfs"));
        assert_eq!(table.filesystem(libc::makedev(0, 24)), Some("tmpfs"));
        assert_eq!(table.filesystem(libc::makedev(0, 47)), Some("btrfs"));
        assert_eq!(table.filesystem(libc::makedev(259, 5)), Some("xfs"));
        assert_eq!(table.filesystem(libc::makedev(0, 1)), None);
        assert!(HostFilesystems::parse("1 0 0:32 / / rw shared:1 btrfs /dev/a rw\n").is_err());
        assert!(HostFilesystems::parse("1 0 x / / rw - btrfs /dev/a rw\n").is_err());
        // This process's own table reads.
        let own = HostFilesystems::parse(&std::fs::read_to_string("/proc/self/mountinfo").unwrap())
            .unwrap();
        assert!(!own.types.is_empty());
    }

    /// This test process's own mappings, as the host kernel prints them in
    /// `smaps`, are what Detcore renders from their `maps` lines and the
    /// kernel's own flags, as a backend that establishes them gives them,
    /// through the sanitizer: the derived fields (`THPeligible`,
    /// `ProtectionKey`, `VmFlags`) are the kernel's for every mapping Detcore
    /// renders. A mapping it refuses is one whose huge pages a rule it does
    /// not model decides (a file on tmpfs or overlayfs, a host setting
    /// `always`), and a mapping with a flag no memory map gives is left out.
    /// At least one anonymous mapping is compared, and one file mapping
    /// (this test's own binary, on a disk filesystem) where the host's
    /// setting is not `always`.
    #[test]
    fn this_process_mappings_render_as_the_host_kernel_prints_them() {
        let thp = HostThp::read(Path::new("/sys/kernel/mm/transparent_hugepage")).unwrap();
        // SAFETY: PR_GET_THP_DISABLE reads a flag of this process.
        let disabled = unsafe { libc::prctl(libc::PR_GET_THP_DISABLE, 0, 0, 0, 0) };
        assert!(disabled >= 0);
        let facts = SmapsFacts {
            protection_keys_enabled: host_protection_keys_enabled().unwrap(),
            protection_keys_used: false,
            mapping_flags_changed: false,
            thp_enabled: thp.map(|thp| thp.enabled),
            thp_orders: thp.map_or(0, |thp| thp.anonymous_orders()),
            thp_disabled: Some(disabled != 0),
        };
        let filesystems =
            HostFilesystems::parse(&std::fs::read_to_string("/proc/self/mountinfo").unwrap())
                .unwrap();
        let kernel = String::from_utf8(std::fs::read("/proc/self/smaps").unwrap()).unwrap();
        let table = BTreeMap::new();
        let (mut files, mut anonymous) = (0, 0);
        for block in blocks(&kernel) {
            let header = block.lines().next().unwrap();
            let Some(flags) = named_flags(&block) else {
                continue;
            };
            let Ok(mut map) = MemoryMap::from_maps(format!("{header}\n").as_bytes()) else {
                continue;
            };
            let Ok(mapping) = map.mappings.remove(0).with_vm_flags(flags) else {
                continue;
            };
            let kind = mapping.backing.clone();
            match render_smaps(
                &MemoryMap::new(vec![mapping], false).unwrap(),
                &facts,
                Ok(&filesystems),
            ) {
                Ok(rendered) => {
                    assert_eq!(
                        String::from_utf8(crate::procfs::sanitize_smaps(&rendered, &table))
                            .unwrap(),
                        String::from_utf8(crate::procfs::sanitize_smaps(block.as_bytes(), &table))
                            .unwrap()
                    );
                    match kind {
                        MappingBacking::File { .. } => files += 1,
                        MappingBacking::Anonymous
                        | MappingBacking::Heap
                        | MappingBacking::Stack
                        | MappingBacking::NamedAnonymous(_) => anonymous += 1,
                        _ => {}
                    }
                }
                Err(refusal) => {
                    assert!(
                        refusal.contains("may have transparent huge pages"),
                        "{header}: {refusal}"
                    );
                }
            }
        }
        assert!(anonymous >= 1, "no anonymous mapping compared:\n{kernel}");
        if facts.thp_enabled != Some(ThpEnabled::Always) {
            assert!(files >= 1, "no file mapping compared:\n{kernel}");
        }
    }
}
