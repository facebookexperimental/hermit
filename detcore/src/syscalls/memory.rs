/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic policies for memory-management advice.

use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;

use crate::Detcore;
use crate::RecordOrReplay;

const PAGE_SIZE: usize = 4096;
// Added in Linux 6.13 and not yet exposed by the pinned libc crate.
const MADV_GUARD_INSTALL: i32 = 102;
const MADV_GUARD_REMOVE: i32 = 103;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MadviseAction {
    ForwardHint,
    ForwardSemantic,
    Ignore,
    Reject(Errno),
    Unknown,
}

const fn madvise_action(advice: i32) -> MadviseAction {
    match advice {
        // Pure access-pattern and prefetch hints have no required memory-content
        // side effect. Backends without native madvise support accept them as no-ops.
        libc::MADV_NORMAL | libc::MADV_RANDOM | libc::MADV_SEQUENTIAL | libc::MADV_WILLNEED => {
            MadviseAction::ForwardHint
        }

        // Operations with guest-visible memory, fork, backing-store, dump, or guard
        // semantics must reach a backend that implements native madvise behavior.
        libc::MADV_DONTNEED
        | libc::MADV_DONTFORK
        | libc::MADV_DOFORK
        | libc::MADV_DONTDUMP
        | libc::MADV_DODUMP
        | libc::MADV_WIPEONFORK
        | libc::MADV_KEEPONFORK
        | libc::MADV_DONTNEED_LOCKED
        | MADV_GUARD_INSTALL
        | MADV_GUARD_REMOVE => MadviseAction::ForwardSemantic,

        // These are optional reclaim or asynchronous VM-policy controls. Their host
        // effects depend on memory pressure, KSM, and THP activity. Hermit accepts
        // them as fixed no-ops after deterministic argument validation; it deliberately
        // does not reproduce each advice's host- and mapping-specific EINVAL cases.
        libc::MADV_FREE
        | libc::MADV_MERGEABLE
        | libc::MADV_UNMERGEABLE
        | libc::MADV_HUGEPAGE
        | libc::MADV_NOHUGEPAGE
        | libc::MADV_COLD
        | libc::MADV_PAGEOUT => MadviseAction::Ignore,

        // Hole punching mutates backing storage and every mapping alias. Refuse it
        // until Detcore can update file resources and replay all affected aliases.
        libc::MADV_REMOVE => MadviseAction::Reject(Errno::EINVAL),

        // Successful population and collapse promise synchronous, resource-
        // dependent work. Report the same deterministic error Linux uses when
        // an advice value is unsupported so callers can take their fallback.
        libc::MADV_POPULATE_READ | libc::MADV_POPULATE_WRITE | libc::MADV_COLLAPSE => {
            MadviseAction::Reject(Errno::EINVAL)
        }

        // Never let a guest inject host memory failures, even if the container
        // unexpectedly has enough privilege to make these operations succeed.
        libc::MADV_HWPOISON | libc::MADV_SOFT_OFFLINE => MadviseAction::Reject(Errno::EPERM),

        _ => MadviseAction::Unknown,
    }
}

fn validate_common_args(call: syscalls::Madvise) -> Result<(), Error> {
    let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
    if !start.is_multiple_of(PAGE_SIZE) {
        return Err(Errno::EINVAL.into());
    }
    if call.len() == 0 {
        return Ok(());
    }

    let end = start.checked_add(call.len()).ok_or(Errno::EINVAL)?;
    end.checked_add(PAGE_SIZE - 1).ok_or(Errno::EINVAL)?;
    Ok(())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Apply a deterministic policy to madvise(2).
    ///
    /// Ptrace/DBT forward hints and supported advice with guest-visible semantics.
    /// Record/replay accepts pure hints as no-ops and passes guest-semantic advice
    /// to the recorder and replayer, which record and restore the effects that
    /// differ because replay replaces file mappings with anonymous mappings.
    /// Reclaim and asynchronous VM-policy advice receives fixed success without exposing host memory pressure. Resource-
    /// dependent, backing-store, and hardware-failure operations receive fixed errors.
    /// KVM accepts pure hints as no-ops, implements MADV_DONTNEED in its executor
    /// (`supports_madv_dontneed`), and reports ENOSYS for the other guest-visible
    /// semantics it cannot provide.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#548): Recheck advice policy and record/replay boundaries.
    pub async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Madvise,
    ) -> Result<i64, Error> {
        let advice = call.advice();
        let action = madvise_action(advice);
        validate_common_args(call)?;

        if call.len() == 0 {
            return match action {
                MadviseAction::Unknown => Err(Errno::EINVAL.into()),
                _ => Ok(0),
            };
        }
        if self.cfg.recordreplay_modes && action == MadviseAction::ForwardHint {
            crate::detlog!(
                "[dtid {}] madvise hint {} accepted as record/replay no-op",
                guest.thread_state().dettid,
                advice,
            );
            return Ok(0);
        }

        match action {
            MadviseAction::ForwardHint if self.cfg.backend.supports_madvise => {
                Ok(self.record_or_replay(guest, call).await?)
            }
            MadviseAction::ForwardHint => {
                crate::detlog!(
                    "[dtid {}] madvise hint {} accepted as backend no-op",
                    guest.thread_state().dettid,
                    advice,
                );
                Ok(0)
            }
            MadviseAction::ForwardSemantic
                if self.cfg.backend.supports_madvise
                    || (advice == libc::MADV_DONTNEED
                        && self.cfg.backend.supports_madv_dontneed) =>
            {
                Ok(self.record_or_replay(guest, call).await?)
            }
            MadviseAction::ForwardSemantic => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} is unsupported by this backend",
                    guest.thread_state().dettid,
                    advice,
                );
                Err(Errno::ENOSYS.into())
            }
            MadviseAction::Ignore => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} accepted as deterministic no-op",
                    guest.thread_state().dettid,
                    advice,
                );
                Ok(0)
            }
            MadviseAction::Reject(errno) => {
                crate::detlog!(
                    "[dtid {}] madvise advice {} rejected with {}",
                    guest.thread_state().dettid,
                    advice,
                    errno,
                );
                Err(errno.into())
            }
            MadviseAction::Unknown => {
                crate::detlog!(
                    "[dtid {}] unknown madvise advice {} rejected with EINVAL",
                    guest.thread_state().dettid,
                    advice,
                );
                Err(Errno::EINVAL.into())
            }
        }
    }

    /// Deterministic `mincore(2)`.
    ///
    /// Real page residency reflects host memory pressure and is therefore
    /// nondeterministic, which is why mincore was previously classified as an
    /// unsupported syscall. GNU grep (and other glibc consumers) invoke mincore
    /// under the KVM backend on a code path the ptrace backend does not take, so
    /// leaving it unsupported aborts the guest under `--strict`.
    ///
    /// Inject the call first so the backend preserves Linux pointer and mapping
    /// validation, then report every mapped page as resident. The residency
    /// vector is only an advisory hint, so replacing those nondeterministic bits
    /// with a constant answer remains bitwise-identical across runs.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#775): Review deterministic mincore residency emulation.
    pub async fn handle_mincore<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Mincore,
    ) -> Result<i64, Error> {
        // Linux rejects a start address that is not page-aligned with EINVAL.
        let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        if !start.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL.into());
        }
        let len = call.len();
        if len == 0 {
            return Ok(0);
        }
        // Match madvise's overflow guard on the requested range.
        start.checked_add(len).ok_or(Errno::EINVAL)?;
        let page_count = len.div_ceil(PAGE_SIZE);
        let vec = call.vec().ok_or(Errno::EFAULT)?;
        guest.inject(syscalls::Syscall::from(call)).await?;
        let residency = vec![1u8; page_count];
        guest.memory().write_exact(vec, &residency)?;
        Ok(0)
    }
}

/// The page size `brk(2)` rounds the break to.
const BRK_PAGE: u64 = 4096;

fn brk_page_align(address: u64) -> Option<u64> {
    Some(address.checked_add(BRK_PAGE - 1)? & !(BRK_PAGE - 1))
}

/// Linux's default `stack_guard_gap`: 256 pages kept free below a mapping
/// that grows down.
const STACK_GUARD_GAP: u64 = 256 * BRK_PAGE;

/// One mapping of a `/proc/<pid>/smaps` text: its range, and the gap Linux
/// keeps free below it (`stack_guard_start_gap`): `stack_guard_gap` below a
/// mapping that grows down (`gd` among its `VmFlags`), a page below a
/// shadow stack (`ss`), none below any other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Vma {
    start: u64,
    end: u64,
    start_gap: u64,
}

/// The mappings of a `/proc/<pid>/smaps` text, in address order, parsed as
/// they are asked for: nothing is allocated, so where the tool shares the
/// guest's process its allocator maps nothing between the read and the
/// injected call that relies on it.
fn smaps_vmas(smaps: &[u8]) -> SmapsVmas<'_> {
    SmapsVmas { rest: smaps }
}

#[derive(Clone)]
struct SmapsVmas<'a> {
    rest: &'a [u8],
}

impl<'a> SmapsVmas<'a> {
    /// The next line, without consuming it.
    fn peek(&self) -> Option<&'a [u8]> {
        (!self.rest.is_empty()).then(|| {
            self.rest
                .split(|&byte| byte == b'\n')
                .next()
                .unwrap_or_default()
        })
    }

    fn advance(&mut self) {
        self.rest = match self.rest.iter().position(|&byte| byte == b'\n') {
            Some(newline) => &self.rest[newline + 1..],
            None => &[],
        };
    }
}

/// A mapping's range, from the first field of its header line; `None` for
/// any other line. Bytes, not text: the pathname that follows is whatever
/// bytes the file was named with.
fn smaps_range(line: &[u8]) -> Option<(u64, u64)> {
    let range = line.split(|&byte| byte == b' ').next()?;
    let (start, end) = std::str::from_utf8(range).ok()?.split_once('-')?;
    Some((
        u64::from_str_radix(start, 16).ok()?,
        u64::from_str_radix(end, 16).ok()?,
    ))
}

impl Iterator for SmapsVmas<'_> {
    type Item = Vma;

    fn next(&mut self) -> Option<Vma> {
        let (start, end) = loop {
            let line = self.peek()?;
            self.advance();
            if let Some(range) = smaps_range(line) {
                break range;
            }
        };
        let mut vma = Vma {
            start,
            end,
            start_gap: 0,
        };
        // The mapping's fields, up to the next mapping's header.
        while let Some(line) = self.peek() {
            if smaps_range(line).is_some() {
                break;
            }
            self.advance();
            if let Some(flags) = line.strip_prefix(b"VmFlags:") {
                let mut flags = flags.split(|byte| byte.is_ascii_whitespace());
                vma.start_gap = if flags.clone().any(|flag| flag == b"gd") {
                    STACK_GUARD_GAP
                } else if flags.any(|flag| flag == b"ss") {
                    BRK_PAGE
                } else {
                    0
                };
            }
        }
        Some(vma)
    }
}

/// Whether Linux's `brk` grows a break whose page-aligned end is `old_end`
/// to `new_end`: no mapping may meet `[old_end, new_end + PAGE)`, nor a
/// mapping that grows down have its guard gap there
/// (`vma_find(&vmi, newbrk + PAGE_SIZE + stack_guard_gap)` and
/// `vm_start_gap`).
fn brk_may_grow(mut vmas: impl Iterator<Item = Vma>, old_end: u64, new_end: u64) -> bool {
    let Some(limit) = new_end.checked_add(BRK_PAGE) else {
        return false;
    };
    let search_end = limit.saturating_add(STACK_GUARD_GAP);
    let Some(next) = vmas.find(|vma| vma.end > old_end && vma.start < search_end) else {
        return true;
    };
    // `vm_start_gap`, which gives 0 where the gap would pass it.
    let gap_start = next.start.saturating_sub(next.start_gap);
    limit <= gap_start
}

/// Whether Linux's `brk` shrinks a break whose page-aligned end is `old_end`
/// to `new_end`: only where a mapping meets `[new_end, old_end)`
/// (`vma_find(&vmi, oldbrk)` from `newbrk`); otherwise it keeps the old
/// break.
fn brk_may_shrink(mut vmas: impl Iterator<Item = Vma>, new_end: u64, old_end: u64) -> bool {
    vmas.any(|vma| vma.end > new_end && vma.start < old_end)
}

impl<T: RecordOrReplay> Detcore<T> {
    /// `brk(addr)` in a run that declares a launcher
    /// (`Detcore::declares_launcher`), where the kernel's program break is the
    /// launcher's: answered from Detcore's model of the guest's break
    /// ([`crate::memory::BrkModel`]), as Linux's `brk` (mm/mmap.c) answers
    /// from `mm->brk`. A request below the start, one past `RLIMIT_DATA`,
    /// growth that would meet another mapping or its guard gap, or a shrink
    /// over a range with no mapping leaves the break where it is; the call
    /// returns the break in effect afterwards, never an error. Growth maps
    /// exactly the new anonymous zero pages (`MAP_FIXED_NOREPLACE`, so an
    /// existing mapping is never replaced) and shrinking unmaps them.
    ///
    /// Refused by name, where an answer would be a plausible wrong one: a
    /// brk without a model for the image (with the reason there is none),
    /// and a request at or above the launcher's break but below the model's
    /// start. Only a libc that cached the launcher's break before Detcore's
    /// first callback asks for that, and it would take Linux's answer, the
    /// break in effect above its request, for a grant at its stale address.
    pub(crate) async fn handle_brk<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Syscall,
    ) -> Result<i64, Error> {
        let (model, unmodeled) = {
            let metadata = guest
                .thread_state()
                .memory_metadata
                .lock()
                .expect("memory metadata mutex poisoned");
            (metadata.brk_model, metadata.brk_unmodeled.clone())
        };
        let Some(model) = model else {
            let reason = format!(
                "unsupported: brk where the kernel's program break is the launcher's and Detcore has no model of the guest's: {}",
                unmodeled
                    .as_deref()
                    .unwrap_or("the image was never modeled"),
            );
            return self
                .refuse_launched_exe_because(guest, syscalls::Sysno::brk, reason)
                .await;
        };
        let requested = syscalls::SyscallInfo::into_parts(call).1.arg0 as u64;
        let granted = match self.granted_brk(guest, &model, requested).await {
            Ok(granted) => granted,
            Err(reason) => {
                return self
                    .refuse_launched_exe_because(guest, syscalls::Sysno::brk, reason.to_owned())
                    .await;
            }
        };
        guest
            .thread_state()
            .memory_metadata
            .lock()
            .expect("memory metadata mutex poisoned")
            .brk_model = Some(crate::memory::BrkModel {
            current: granted,
            ..model
        });
        Ok(granted as i64)
    }

    /// The break `brk(requested)` leaves in effect, from `model`, mapping or
    /// unmapping the pages between the old and the new break; an error where
    /// Detcore could not keep the address space and the model in step (a
    /// mapping it could neither place nor remove).
    async fn granted_brk<G: Guest<Self>>(
        &self,
        guest: &mut G,
        model: &crate::memory::BrkModel,
        requested: u64,
    ) -> Result<u64, &'static str> {
        let current = model.current;
        if requested < model.start {
            if requested >= model.launcher_break {
                return Err(
                    "unsupported: brk from the launcher's break, which the guest's libc read before Detcore modeled the guest's",
                );
            }
            return Ok(current);
        }
        let limit = guest
            .thread_state()
            .resource_limits
            .lock()
            .unwrap()
            .get(libc::RLIMIT_DATA)
            .map_or(libc::RLIM64_INFINITY, |limit| limit.current);
        let used = (requested - model.start).saturating_add(model.data_span);
        if limit != libc::RLIM64_INFINITY && used > limit {
            return Ok(current);
        }
        let (Some(new_end), Some(old_end)) = (brk_page_align(requested), brk_page_align(current))
        else {
            return Ok(current);
        };
        if new_end == old_end {
            return Ok(requested);
        }
        // The address space as Linux's brk inspects it, read with raw
        // syscalls and no allocation between reads where the tool shares the
        // guest's process; the guest's other threads are held by the
        // sequentialized schedule.
        let smaps = self
            .guest_task_directory(guest)
            .and_then(|directory| crate::util::raw_read_listing(&directory.join("smaps")).ok())
            .ok_or("unsupported: brk where Detcore cannot read the guest's mappings")?;
        if requested <= current {
            if !brk_may_shrink(smaps_vmas(&smaps), new_end, old_end) {
                return Ok(current);
            }
            // As Linux, which keeps the old break where the unmap fails.
            let unmapped = guest
                .inject_with_retry(syscalls::Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(syscalls::Addr::from_raw(new_end as usize))
                        .with_len((old_end - new_end) as usize),
                ))
                .await;
            return Ok(if unmapped.is_ok() { requested } else { current });
        }
        if !brk_may_grow(smaps_vmas(&smaps), old_end, new_end) {
            return Ok(current);
        }
        let length = new_end - old_end;
        let mapped = guest
            .inject_with_retry(syscalls::Syscall::Mmap(
                syscalls::Mmap::new()
                    .with_addr(syscalls::Addr::from_raw(old_end as usize))
                    .with_len(length as usize)
                    .with_prot(syscalls::ProtFlags::PROT_READ | syscalls::ProtFlags::PROT_WRITE)
                    .with_flags(
                        syscalls::MapFlags::MAP_PRIVATE
                            | syscalls::MapFlags::MAP_ANONYMOUS
                            | syscalls::MapFlags::MAP_FIXED_NOREPLACE,
                    )
                    .with_fd(-1)
                    .with_offset(0),
            ))
            .await;
        match mapped {
            Ok(address) if address as u64 == old_end => Ok(requested),
            // A kernel without MAP_FIXED_NOREPLACE treats the address as a
            // hint and may map elsewhere: give that mapping back, or refuse
            // where it cannot be.
            Ok(address) => {
                let removed = guest
                    .inject_with_retry(syscalls::Syscall::Munmap(
                        syscalls::Munmap::new()
                            .with_addr(syscalls::Addr::from_raw(address as usize))
                            .with_len(length as usize),
                    ))
                    .await;
                if removed.is_ok() {
                    Ok(current)
                } else {
                    Err(
                        "unsupported: brk growth the kernel mapped elsewhere and Detcore could not remove",
                    )
                }
            }
            Err(_) => Ok(current),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_break_grows_and_shrinks_only_where_linuxs_brk_would() {
        let smaps = "\
00400000-00401000 r-xp 00000000 00:20 1234 /image
Size:                  4 kB
VmFlags: rd ex mr mw me
00406000-00407000 r--p 00000000 00:00 0
VmFlags: rd mr mw me
7ffffff00000-7ffffffff000 rw-p 00000000 00:00 0 [stack]
VmFlags: rd wr mr mw me gd ac
";
        let vmas: Vec<Vma> = smaps_vmas(smaps.as_bytes()).collect();
        assert_eq!(vmas.len(), 3);
        // A pathname that is not UTF-8 does not hide the mappings.
        let mut named =
            b"00500000-00501000 r--p 00000000 00:20 42 /n\xffme\nVmFlags: rd\n".to_vec();
        named.extend_from_slice(smaps.as_bytes());
        let named: Vec<Vma> = smaps_vmas(&named).collect();
        assert_eq!(named.len(), 4);
        assert_eq!(named[1..], vmas[..]);
        assert_eq!(
            vmas[2],
            Vma {
                start: 0x7ffffff00000,
                end: 0x7ffffffff000,
                start_gap: STACK_GUARD_GAP
            }
        );
        // A mapping at 0x406000: growth to 0x405000 needs 0x405000-0x406000
        // free above it, which it is; growth to 0x406000 meets the mapping.
        assert!(brk_may_grow(vmas.iter().copied(), 0x401000, 0x405000));
        assert!(!brk_may_grow(vmas.iter().copied(), 0x401000, 0x406000));
        // Below a stack, the guard gap is kept as well.
        let below_stack = 0x7ffffff00000 - STACK_GUARD_GAP;
        assert!(brk_may_grow(
            vmas.iter().copied(),
            0x500000,
            below_stack - BRK_PAGE
        ));
        assert!(!brk_may_grow(vmas.iter().copied(), 0x500000, below_stack));
        // Below a shadow stack, one page.
        let shadow: Vec<Vma> =
            smaps_vmas(b"00410000-00411000 rw-p 00000000 00:00 0\nVmFlags: rd wr mr mw me ss\n")
                .collect();
        assert_eq!(shadow[0].start_gap, BRK_PAGE);
        assert!(brk_may_grow(shadow.iter().copied(), 0x401000, 0x40e000));
        assert!(!brk_may_grow(shadow.iter().copied(), 0x401000, 0x40f000));
        // Shrinking needs a mapping to unmap in the range.
        assert!(brk_may_shrink(vmas.iter().copied(), 0x405000, 0x407000));
        assert!(!brk_may_shrink(vmas.iter().copied(), 0x402000, 0x406000));
    }

    #[test]
    fn madvise_known_linux_advice_has_an_explicit_policy() {
        for advice in [
            libc::MADV_NORMAL,
            libc::MADV_RANDOM,
            libc::MADV_SEQUENTIAL,
            libc::MADV_WILLNEED,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::ForwardHint);
        }

        for advice in [
            libc::MADV_DONTNEED,
            libc::MADV_DONTFORK,
            libc::MADV_DOFORK,
            libc::MADV_DONTDUMP,
            libc::MADV_DODUMP,
            libc::MADV_WIPEONFORK,
            libc::MADV_KEEPONFORK,
            libc::MADV_DONTNEED_LOCKED,
            MADV_GUARD_INSTALL,
            MADV_GUARD_REMOVE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::ForwardSemantic);
        }

        for advice in [
            libc::MADV_FREE,
            libc::MADV_MERGEABLE,
            libc::MADV_UNMERGEABLE,
            libc::MADV_HUGEPAGE,
            libc::MADV_NOHUGEPAGE,
            libc::MADV_COLD,
            libc::MADV_PAGEOUT,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Ignore);
        }

        for advice in [
            libc::MADV_REMOVE,
            libc::MADV_POPULATE_READ,
            libc::MADV_POPULATE_WRITE,
            libc::MADV_COLLAPSE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EINVAL));
        }
        for advice in [libc::MADV_HWPOISON, libc::MADV_SOFT_OFFLINE] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EPERM));
        }
        assert_eq!(madvise_action(i32::MAX), MadviseAction::Unknown);
    }

    #[test]
    fn common_argument_validation_is_host_independent() {
        let aligned = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(0x1000) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(aligned))
                    .with_len(0)
                    .with_advice(libc::MADV_FREE),
            )
            .is_ok()
        );

        let unaligned = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(0x1001) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(unaligned))
                    .with_len(0)
                    .with_advice(libc::MADV_FREE),
            )
            .is_err()
        );

        let near_end =
            unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(usize::MAX & !(PAGE_SIZE - 1)) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(near_end))
                    .with_len(PAGE_SIZE)
                    .with_advice(libc::MADV_FREE),
            )
            .is_err()
        );
    }
}
