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
use tracing::warn;

use crate::Detcore;
use crate::RecordOrReplay;

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
        libc::MADV_POPULATE_READ
        | libc::MADV_POPULATE_WRITE
        | reverie::syscalls::linux_abi::MADV_COLLAPSE => MadviseAction::Reject(Errno::EINVAL),

        // Never let a guest inject host memory failures, even if the container
        // unexpectedly has enough privilege to make these operations succeed.
        libc::MADV_HWPOISON | libc::MADV_SOFT_OFFLINE => MadviseAction::Reject(Errno::EPERM),

        _ => MadviseAction::Unknown,
    }
}

/// Whether `advice`, once the backend runs it, may set or clear a flag Linux
/// keeps on the mappings it covers (`madvise_vma_behavior`, mm/madvise.c in
/// v6.17), which `smaps` prints (`VmFlags`) and the backend's memory map does
/// not list (`MemoryMetadata::mapping_flags_changed`): the access-pattern
/// advice (VM_SEQ_READ, VM_RAND_READ) and the fork and dump advice
/// (VM_DONTCOPY, VM_DONTDUMP, VM_WIPEONFORK). The huge-page and KSM advice
/// change flags too, but Detcore answers it itself and never passes it on
/// ([`MadviseAction::Ignore`]); the rest acts on pages, not on flags.
const fn advice_changes_mapping_flags(advice: i32) -> bool {
    matches!(
        advice,
        libc::MADV_NORMAL
            | libc::MADV_RANDOM
            | libc::MADV_SEQUENTIAL
            | libc::MADV_DONTFORK
            | libc::MADV_DOFORK
            | libc::MADV_DONTDUMP
            | libc::MADV_DODUMP
            | libc::MADV_WIPEONFORK
            | libc::MADV_KEEPONFORK
    )
}

/// Whether `advice`, once the backend runs it, may leave a mapping it covers
/// without the pages it held (`madvise_dontneed_free` and
/// `madvise_guard_install`, mm/madvise.c in v6.17): `MADV_DONTNEED` and
/// `MADV_DONTNEED_LOCKED` zap them, `MADV_GUARD_INSTALL` zaps them and
/// installs guard markers, and `MADV_WIPEONFORK` leaves a later fork's copy
/// empty. `numa_maps` prints `kernelpagesize_kB` only for a mapping with a
/// resident page (`MemoryMetadata::pages_discarded`). `MADV_FREE`,
/// `MADV_COLD` and `MADV_PAGEOUT` would too, but Detcore answers them itself
/// and never passes them on ([`MadviseAction::Ignore`]).
const fn advice_discards_pages(advice: i32) -> bool {
    matches!(
        advice,
        libc::MADV_DONTNEED
            | libc::MADV_DONTNEED_LOCKED
            | MADV_GUARD_INSTALL
            | libc::MADV_WIPEONFORK
    )
}

/// Whether a forwarded madvise call that returned `result` may have changed
/// mapping flags or pages: every outcome but ENOSYS, which says the backend
/// ran nothing (a backend that does not implement the advice). Linux applies
/// advice to the mapped part of a range it then reports ENOMEM for, and may
/// fail with EINVAL at a later mapping after changing earlier ones.
fn madvise_may_have_run(result: &Result<i64, Errno>) -> bool {
    !matches!(result, Err(Errno::ENOSYS))
}

fn validate_common_args(call: syscalls::Madvise) -> Result<(), Error> {
    let page = crate::page::page_size() as usize;
    let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
    if !start.is_multiple_of(page) {
        return Err(Errno::EINVAL.into());
    }
    if call.len() == 0 {
        return Ok(());
    }

    let end = start.checked_add(call.len()).ok_or(Errno::EINVAL)?;
    end.checked_add(page - 1).ok_or(Errno::EINVAL)?;
    Ok(())
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Apply a deterministic policy to madvise(2).
    ///
    /// Hints and advice with guest-visible semantics are forwarded to the
    /// backend, on every backend. A backend that does not implement an advice
    /// answers ENOSYS with the typed refusal `MemoryAdvice`, which is logged as a
    /// warning naming the advice; the guest sees the ENOSYS.
    /// Record/replay accepts pure hints as no-ops and passes guest-semantic advice
    /// to the recorder and replayer, which record and restore the effects that
    /// differ because replay replaces file mappings with anonymous mappings.
    /// Reclaim and asynchronous VM-policy advice receives fixed success without exposing host memory pressure. Resource-
    /// dependent, backing-store, and hardware-failure operations receive fixed errors.
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
            MadviseAction::ForwardHint | MadviseAction::ForwardSemantic => {
                let advised = (
                    call.addr().map_or(0, |addr| addr.as_raw() as u64),
                    call.len() as u64,
                );
                let result = self.record_or_replay(guest, call).await;
                // The backend's memory map lists neither the flags this advice
                // sets nor the pages it discards, so a `smaps` or `numa_maps`
                // Detcore renders from that map can no longer derive them for
                // a listed mapping the advice reached
                // (`MemoryMetadata::mapping_flags_changed`, `pages_discarded`).
                let changes_flags = advice_changes_mapping_flags(advice);
                let discards_pages = advice_discards_pages(advice);
                if (changes_flags || discards_pages)
                    && madvise_may_have_run(&result)
                    && self.renders_address_space_files()
                    && Self::range_reaches_listed_mapping(guest, advised.0, advised.1)
                {
                    if changes_flags {
                        guest.thread_state().note_mapping_flags_changed();
                    }
                    if discards_pages {
                        guest.thread_state().note_pages_discarded();
                    }
                }
                if result.is_err()
                    && let Some(refusal) = guest.take_unsupported_refusal().filter(|refusal| {
                        refusal.operation == reverie::UnsupportedOperation::MemoryAdvice
                    })
                {
                    warn!(
                        "[dtid {}] madvise advice {} refused by the backend: {}",
                        guest.thread_state().dettid,
                        advice,
                        refusal.diagnostic,
                    );
                }
                Ok(result?)
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
        let page = crate::page::page_size() as usize;
        let start = call.addr().map(AddrMut::as_raw).unwrap_or(0);
        if !start.is_multiple_of(page) {
            return Err(Errno::EINVAL.into());
        }
        let len = call.len();
        if len == 0 {
            return Ok(0);
        }
        // Match madvise's overflow guard on the requested range.
        start.checked_add(len).ok_or(Errno::EINVAL)?;
        let page_count = len.div_ceil(page);
        let vec = call.vec().ok_or(Errno::EFAULT)?;
        guest.inject(syscalls::Syscall::from(call)).await?;
        let residency = vec![1u8; page_count];
        guest.memory().write_exact(vec, &residency)?;
        Ok(0)
    }
}

/// `address` rounded up to a `page`-byte boundary, as `brk(2)` rounds the
/// break (`PAGE_ALIGN`).
fn brk_page_align(address: u64, page: u64) -> Option<u64> {
    Some(address.checked_add(page - 1)? & !(page - 1))
}

/// Linux's default `stack_guard_gap` with `page`-byte pages: 256 pages kept
/// free below a mapping that grows down.
fn stack_guard_gap(page: u64) -> u64 {
    256 * page
}

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
fn smaps_vmas(smaps: &[u8], page: u64) -> SmapsVmas<'_> {
    SmapsVmas { rest: smaps, page }
}

#[derive(Clone)]
struct SmapsVmas<'a> {
    rest: &'a [u8],
    /// The host's page size, which sizes the guard gaps.
    page: u64,
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
                    stack_guard_gap(self.page)
                } else if flags.any(|flag| flag == b"ss") {
                    self.page
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
fn brk_may_grow(
    mut vmas: impl Iterator<Item = Vma>,
    old_end: u64,
    new_end: u64,
    page: u64,
) -> bool {
    let Some(limit) = new_end.checked_add(page) else {
        return false;
    };
    let search_end = limit.saturating_add(stack_guard_gap(page));
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

// Bounded DEBUG diagnostic: no new procfs read and no formatting between
// the metadata capture and a mapping operation. Only the first rejected moving
// request in this process is retained. Fork inherits the diagnostic latch.
struct BrkDiagnostic<'a> {
    dettid: i32,
    model: &'a crate::memory::BrkModel,
    requested: u64,
}

struct BrkDiagnosticHex<'a>(&'a [u8]);

impl std::fmt::Display for BrkDiagnosticHex<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl BrkDiagnostic<'_> {
    fn rejected(
        &self,
        branch: &'static str,
        limit: Option<u64>,
        smaps: Option<&[u8]>,
        kernel_result: &dyn std::fmt::Debug,
    ) {
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;
        static CAPTURED: AtomicBool = AtomicBool::new(false);
        const MAX_BYTES: usize = 256 * 1024;
        if self.requested == 0
            || self.requested == self.model.current
            || !tracing::enabled!(target: "detcore::brk_diagnostic", tracing::Level::DEBUG)
            || CAPTURED.swap(true, Ordering::Relaxed)
        {
            return;
        }
        let complete = smaps.is_some_and(|bytes| bytes.len() <= MAX_BYTES);
        tracing::debug!(
            target: "detcore::brk_diagnostic",
            dettid = self.dettid,
            requested = self.requested,
            start = self.model.start,
            current = self.model.current,
            data_span = self.model.data_span,
            launcher_break = self.model.launcher_break,
            ?limit,
            branch,
            ?kernel_result,
            smaps_len = ?smaps.map(<[u8]>::len),
            complete,
            "BRK_DENIAL_V1 begin",
        );
        if complete {
            for (index, chunk) in smaps.unwrap().chunks(1024).enumerate() {
                tracing::debug!(
                    target: "detcore::brk_diagnostic",
                    dettid = self.dettid,
                    index,
                    bytes = %BrkDiagnosticHex(chunk),
                    "BRK_DENIAL_V1 chunk",
                );
            }
        }
        tracing::debug!(
            target: "detcore::brk_diagnostic",
            dettid = self.dettid,
            complete,
            "BRK_DENIAL_V1 end",
        );
    }
}

/// The mapping-dependent part of a modeled program-break request. Endpoints
/// are page-aligned; resource limits and the actual mapping operation stay
/// with the caller, independently of where the smaps bytes were captured.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize
)]
pub enum BrkMappingCheck {
    /// Check new pages and Linux's guard gap above the old break.
    Grow { old_end: u64, new_end: u64 },
    /// Check that some mapping overlaps the pages being removed.
    Shrink { new_end: u64, old_end: u64 },
}

impl BrkMappingCheck {
    pub(crate) fn allows(self, smaps: &[u8]) -> bool {
        self.allows_with_page(smaps, crate::page::page_size())
    }

    /// [`Self::allows`] on a host whose pages are `page` bytes.
    fn allows_with_page(self, smaps: &[u8], page: u64) -> bool {
        match self {
            Self::Grow { old_end, new_end } => {
                brk_may_grow(smaps_vmas(smaps, page), old_end, new_end, page)
            }
            Self::Shrink { new_end, old_end } => {
                brk_may_shrink(smaps_vmas(smaps, page), new_end, old_end)
            }
        }
    }
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
        let diagnostic = BrkDiagnostic {
            dettid: guest.thread_state().dettid.as_raw(),
            model,
            requested,
        };
        if requested < model.start {
            if requested >= model.launcher_break {
                diagnostic.rejected("below_start_launcher", None, None, &"not_injected");
                return Err(
                    "unsupported: brk from the launcher's break, which the guest's libc read before Detcore modeled the guest's",
                );
            }
            diagnostic.rejected("below_start", None, None, &"not_injected");
            return Ok(current);
        }
        let limit = guest
            .thread_state()
            .resource_limits
            .lock()
            .unwrap()
            .get(reverie::syscalls::linux_abi::RLIMIT_DATA)
            .map_or(libc::RLIM64_INFINITY, |limit| limit.current);
        let used = (requested - model.start).saturating_add(model.data_span);
        if limit != libc::RLIM64_INFINITY && used > limit {
            diagnostic.rejected("rlimit_data", Some(limit), None, &"not_injected");
            return Ok(current);
        }
        let page = crate::page::page_size();
        let (Some(new_end), Some(old_end)) = (
            brk_page_align(requested, page),
            brk_page_align(current, page),
        ) else {
            diagnostic.rejected("page_overflow", Some(limit), None, &"not_injected");
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
            .and_then(|directory| crate::util::raw_read_listing(&directory.join("smaps")).ok());
        let check = if requested <= current {
            BrkMappingCheck::Shrink { new_end, old_end }
        } else {
            BrkMappingCheck::Grow { old_end, new_end }
        };
        let allowed = match smaps.as_deref() {
            Some(smaps) => Some(check.allows(smaps)),
            None if guest
                .thread_state()
                .changed_root
                .load(std::sync::atomic::Ordering::SeqCst) =>
            {
                crate::tool_global::check_brk_mappings(guest, check)
            }
            None => None,
        }
        .ok_or_else(|| {
            diagnostic.rejected("smaps_unavailable", Some(limit), None, &"not_injected");
            "unsupported: brk where Detcore cannot read the guest's mappings"
        })?;
        if !allowed {
            diagnostic.rejected(
                if requested <= current {
                    "shrink_predicate"
                } else {
                    "grow_predicate"
                },
                Some(limit),
                smaps.as_deref(),
                &"not_injected",
            );
            return Ok(current);
        }
        if requested <= current {
            // As Linux, which keeps the old break where the unmap fails.
            let unmapped = guest
                .inject_with_retry(syscalls::Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(syscalls::Addr::from_raw(new_end as usize))
                        .with_len((old_end - new_end) as usize),
                ))
                .await;
            if unmapped.is_err() {
                diagnostic.rejected("munmap_error", Some(limit), smaps.as_deref(), &unmapped);
            }
            return Ok(if unmapped.is_ok() { requested } else { current });
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
        match &mapped {
            Ok(address) if *address as u64 == old_end => Ok(requested),
            // A kernel without MAP_FIXED_NOREPLACE treats the address as a
            // hint and may map elsewhere: give that mapping back, or refuse
            // where it cannot be.
            Ok(address) => {
                let removed = guest
                    .inject_with_retry(syscalls::Syscall::Munmap(
                        syscalls::Munmap::new()
                            .with_addr(syscalls::Addr::from_raw(*address as usize))
                            .with_len(length as usize),
                    ))
                    .await;
                diagnostic.rejected(
                    "mmap_wrong_address",
                    Some(limit),
                    smaps.as_deref(),
                    &(&mapped, &removed),
                );
                if removed.is_ok() {
                    Ok(current)
                } else {
                    Err(
                        "unsupported: brk growth the kernel mapped elsewhere and Detcore could not remove",
                    )
                }
            }
            Err(_) => {
                diagnostic.rejected("mmap_error", Some(limit), smaps.as_deref(), &mapped);
                Ok(current)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page size the fixtures below are written for.
    const PAGE: u64 = 4096;

    #[test]
    fn a_shared_brk_check_uses_the_existing_mapping_policy() {
        let smaps = b"00500000-00501000 rw-p 00000000 00:00 0\nVmFlags: rd wr\n";
        let grow = |new_end| BrkMappingCheck::Grow {
            old_end: 0x400000,
            new_end,
        };
        assert!(grow(0x4ff000).allows_with_page(smaps, PAGE));
        assert!(!grow(0x500000).allows_with_page(smaps, PAGE));
        assert!(!grow(u64::MAX - PAGE + 1).allows_with_page(smaps, PAGE));
        assert!(
            BrkMappingCheck::Shrink {
                new_end: 0x500000,
                old_end: 0x501000,
            }
            .allows_with_page(smaps, PAGE)
        );
        assert!(
            !BrkMappingCheck::Shrink {
                new_end: 0x400000,
                old_end: 0x500000,
            }
            .allows_with_page(smaps, PAGE)
        );
        assert!(grow(0x500000).allows_with_page(b"", PAGE));
        assert!(
            !BrkMappingCheck::Shrink {
                new_end: 0x500000,
                old_end: 0x501000,
            }
            .allows_with_page(b"", PAGE)
        );
    }

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
        let vmas: Vec<Vma> = smaps_vmas(smaps.as_bytes(), PAGE).collect();
        assert_eq!(vmas.len(), 3);
        // A pathname that is not UTF-8 does not hide the mappings.
        let mut named =
            b"00500000-00501000 r--p 00000000 00:20 42 /n\xffme\nVmFlags: rd\n".to_vec();
        named.extend_from_slice(smaps.as_bytes());
        let named: Vec<Vma> = smaps_vmas(&named, PAGE).collect();
        assert_eq!(named.len(), 4);
        assert_eq!(named[1..], vmas[..]);
        assert_eq!(
            vmas[2],
            Vma {
                start: 0x7ffffff00000,
                end: 0x7ffffffff000,
                start_gap: stack_guard_gap(PAGE)
            }
        );
        // A mapping at 0x406000: growth to 0x405000 needs 0x405000-0x406000
        // free above it, which it is; growth to 0x406000 meets the mapping.
        assert!(brk_may_grow(vmas.iter().copied(), 0x401000, 0x405000, PAGE));
        assert!(!brk_may_grow(
            vmas.iter().copied(),
            0x401000,
            0x406000,
            PAGE
        ));
        // Below a stack, the guard gap is kept as well.
        let below_stack = 0x7ffffff00000 - stack_guard_gap(PAGE);
        assert!(brk_may_grow(
            vmas.iter().copied(),
            0x500000,
            below_stack - PAGE,
            PAGE
        ));
        assert!(!brk_may_grow(
            vmas.iter().copied(),
            0x500000,
            below_stack,
            PAGE
        ));
        // Below a shadow stack, one page.
        let shadow: Vec<Vma> = smaps_vmas(
            b"00410000-00411000 rw-p 00000000 00:00 0\nVmFlags: rd wr mr mw me ss\n",
            PAGE,
        )
        .collect();
        assert_eq!(shadow[0].start_gap, PAGE);
        assert!(brk_may_grow(
            shadow.iter().copied(),
            0x401000,
            0x40e000,
            PAGE
        ));
        assert!(!brk_may_grow(
            shadow.iter().copied(),
            0x401000,
            0x40f000,
            PAGE
        ));
        // Shrinking needs a mapping to unmap in the range.
        assert!(brk_may_shrink(vmas.iter().copied(), 0x405000, 0x407000));
        assert!(!brk_may_shrink(vmas.iter().copied(), 0x402000, 0x406000));
    }

    /// With 64 KiB pages the break, the page above it and the stack guard
    /// gap are each counted in 64 KiB pages.
    #[test]
    fn a_break_on_a_host_with_large_pages_keeps_large_page_gaps() {
        const LARGE: u64 = 0x10000;
        let smaps = "\
00400000-00410000 r-xp 00000000 00:20 1234 /image
VmFlags: rd ex mr mw me
00460000-00470000 r--p 00000000 00:00 0
VmFlags: rd mr mw me
7ffffff00000-7fffffff0000 rw-p 00000000 00:00 0 [stack]
VmFlags: rd wr mr mw me gd ac
";
        let vmas: Vec<Vma> = smaps_vmas(smaps.as_bytes(), LARGE).collect();
        assert_eq!(vmas[2].start_gap, 256 * LARGE);
        // The page above the new break must be free as well: growth to
        // 0x450000 leaves 0x450000-0x460000 free, growth to 0x460000 does not.
        assert!(brk_may_grow(
            vmas.iter().copied(),
            0x410000,
            0x450000,
            LARGE
        ));
        assert!(!brk_may_grow(
            vmas.iter().copied(),
            0x410000,
            0x460000,
            LARGE
        ));
        // A break requested at 0x45f000: with 4 KiB pages it ends there and
        // fits; with 64 KiB pages it rounds up to 0x460000 and meets the
        // mapping's page.
        let requested = 0x45f000;
        let small = brk_page_align(requested, PAGE).unwrap();
        let large = brk_page_align(requested, LARGE).unwrap();
        assert_eq!((small, large), (0x45f000, 0x460000));
        assert!(brk_may_grow(vmas.iter().copied(), 0x410000, small, PAGE));
        assert!(!brk_may_grow(vmas.iter().copied(), 0x410000, large, LARGE));
        // Below the stack, the guard gap is 256 large pages.
        let below_stack = 0x7ffffff00000 - 256 * LARGE;
        assert!(brk_may_grow(
            vmas.iter().copied(),
            0x500000,
            below_stack - LARGE,
            LARGE
        ));
        assert!(!brk_may_grow(
            vmas.iter().copied(),
            0x500000,
            below_stack,
            LARGE
        ));
        assert_eq!(brk_page_align(0x421001, LARGE), Some(0x430000));
        assert_eq!(brk_page_align(0x430000, LARGE), Some(0x430000));
        assert_eq!(brk_page_align(0x421001, PAGE), Some(0x422000));
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
            reverie::syscalls::linux_abi::MADV_COLLAPSE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EINVAL));
        }
        for advice in [libc::MADV_HWPOISON, libc::MADV_SOFT_OFFLINE] {
            assert_eq!(madvise_action(advice), MadviseAction::Reject(Errno::EPERM));
        }
        assert_eq!(madvise_action(i32::MAX), MadviseAction::Unknown);
    }

    /// The advice that sets or clears a flag `smaps` prints is all advice
    /// Detcore passes on, but for advice that acts on pages alone; the advice
    /// that discards pages is `MADV_DONTNEED`, `MADV_DONTNEED_LOCKED`,
    /// `MADV_GUARD_INSTALL` and `MADV_WIPEONFORK`, all passed on.
    #[test]
    fn flag_advice_is_forwarded_advice_that_does_not_act_on_pages_alone() {
        let forwarded = (-1..=200).filter(|advice| {
            matches!(
                madvise_action(*advice),
                MadviseAction::ForwardHint | MadviseAction::ForwardSemantic
            )
        });
        let pages_alone = [
            libc::MADV_WILLNEED,
            libc::MADV_DONTNEED,
            libc::MADV_DONTNEED_LOCKED,
            MADV_GUARD_INSTALL,
            MADV_GUARD_REMOVE,
        ];
        let discarding = [
            libc::MADV_DONTNEED,
            libc::MADV_DONTNEED_LOCKED,
            MADV_GUARD_INSTALL,
            libc::MADV_WIPEONFORK,
        ];
        for advice in forwarded {
            assert_eq!(
                advice_changes_mapping_flags(advice),
                !pages_alone.contains(&advice),
                "{advice}"
            );
            assert_eq!(
                advice_discards_pages(advice),
                discarding.contains(&advice),
                "{advice}"
            );
        }
        for advice in (-1..=200).filter(|advice| advice_discards_pages(*advice)) {
            assert_eq!(
                madvise_action(advice),
                MadviseAction::ForwardSemantic,
                "{advice}"
            );
        }
        for advice in [
            libc::MADV_HUGEPAGE,
            libc::MADV_NOHUGEPAGE,
            libc::MADV_MERGEABLE,
        ] {
            assert_eq!(madvise_action(advice), MadviseAction::Ignore);
        }
    }

    /// A memory map that lists one stack window, `[0x7000_0000, 0x7002_0000)`,
    /// for the tests below.
    fn stack_window() -> reverie::MemoryMap {
        let stack = reverie::MappingFact::new(
            0x7000_0000,
            0x7002_0000,
            (libc::PROT_READ | libc::PROT_WRITE) as u32,
            false,
            0,
            reverie::MappingBacking::Stack,
        )
        .unwrap();
        reverie::MemoryMap::new(vec![stack], false).unwrap()
    }

    /// Flag advice marks the address space once the backend may have run it:
    /// a success, and a failure Linux can return after changing a mapping;
    /// not ENOSYS (the backend ran nothing), advice that acts on pages alone,
    /// or advice Detcore answers itself. So does advice that discards pages,
    /// in its own record. Each only where Detcore renders the files from the
    /// memory map (KVM's capabilities), and only for a range that reaches a
    /// mapping the map lists, or where the backend gives no map.
    #[test]
    fn advice_that_reaches_a_listed_mapping_marks_the_address_space() {
        use crate::record_or_replay::NoopTool;
        use crate::syscall_hot_path_tests::InjectGuest;

        futures::executor::block_on(async {
            let page = crate::page::page_size() as usize;
            for (advice, reply, flags, discarded) in [
                (libc::MADV_RANDOM, Ok(0), true, false),
                (libc::MADV_SEQUENTIAL, Err(Errno::ENOMEM), true, false),
                (libc::MADV_DONTFORK, Err(Errno::EINVAL), true, false),
                (libc::MADV_NORMAL, Ok(0), true, false),
                (libc::MADV_WIPEONFORK, Ok(0), true, true),
                (libc::MADV_DONTDUMP, Err(Errno::ENOSYS), false, false),
                (libc::MADV_WILLNEED, Ok(0), false, false),
                (libc::MADV_DONTNEED, Ok(0), false, true),
                (libc::MADV_DONTNEED_LOCKED, Err(Errno::EINVAL), false, true),
                (MADV_GUARD_INSTALL, Ok(0), false, true),
                (libc::MADV_DONTNEED, Err(Errno::ENOSYS), false, false),
                (libc::MADV_HUGEPAGE, Ok(0), false, false),
            ] {
                for (backend, map, at, reached) in [
                    // The stack window's first page, its last, a page past it,
                    // and the first page with no map to tell.
                    (reverie::BackendCapabilities::KVM, true, 0x7000_0000, true),
                    (reverie::BackendCapabilities::KVM, true, 0x7001_f000, true),
                    (reverie::BackendCapabilities::KVM, true, 0x7002_0000, false),
                    (reverie::BackendCapabilities::KVM, false, 0x7002_0000, true),
                    // Where the kernel's files are served, nothing reads it.
                    (
                        reverie::BackendCapabilities::PTRACE,
                        true,
                        0x7000_0000,
                        false,
                    ),
                ] {
                    let mut tool = crate::syscall_hot_path_tests::detcore::<NoopTool>();
                    tool.cfg.recordreplay_modes = false;
                    tool.cfg.backend = backend;
                    let mut guest = InjectGuest::<NoopTool>::new(reply);
                    if map {
                        guest = guest.with_memory_map(stack_window());
                    }
                    let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(at) };
                    let call = syscalls::Madvise::new()
                        .with_addr(Some(addr))
                        .with_len(page)
                        .with_advice(advice);
                    let _ = tool.handle_madvise(&mut guest, call).await;
                    let state = guest.thread_state();
                    let what = format!("{advice} {reply:?} at {at:#x} map {map}");
                    assert_eq!(
                        state.mapping_flags_may_have_changed(),
                        flags && reached,
                        "flags: {what}"
                    );
                    assert_eq!(
                        state.pages_may_have_been_discarded(),
                        discarded && reached,
                        "pages: {what}"
                    );
                }
            }
        })
    }

    /// A memory lock marks the address space once the backend may have run
    /// it, a part-way failure too; not ENOSYS, the answer of a backend that
    /// runs none (KVM's LinuxEmu). Only where Detcore renders the files from
    /// the memory map, and only for a lock whose range reaches a listed
    /// mapping; `mlockall` reaches every mapping.
    #[test]
    fn a_memory_lock_that_reaches_a_listed_mapping_marks_the_address_space() {
        use reverie::syscalls::Syscall;
        use reverie::syscalls::SyscallArgs;
        use reverie::syscalls::Sysno;

        use crate::record_or_replay::NoopTool;
        use crate::syscall_hot_path_tests::InjectGuest;

        futures::executor::block_on(async {
            for (backend, sysno, reply, at, marked) in [
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlock,
                    Ok(0),
                    0x7000_0000,
                    true,
                ),
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlock2,
                    Err(Errno::ENOMEM),
                    0x7001_ff00,
                    true,
                ),
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlock,
                    Ok(0),
                    0x7002_0000,
                    false,
                ),
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlockall,
                    Ok(0),
                    0,
                    true,
                ),
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlock,
                    Err(Errno::ENOSYS),
                    0x7000_0000,
                    false,
                ),
                (
                    reverie::BackendCapabilities::KVM,
                    Sysno::mlockall,
                    Err(Errno::ENOSYS),
                    0,
                    false,
                ),
                (
                    reverie::BackendCapabilities::PTRACE,
                    Sysno::mlockall,
                    Ok(0),
                    0,
                    false,
                ),
            ] {
                let mut tool = crate::syscall_hot_path_tests::detcore::<NoopTool>();
                tool.cfg.backend = backend;
                let mut guest = InjectGuest::<NoopTool>::new(reply).with_memory_map(stack_window());
                let call = Syscall::from_raw(sysno, SyscallArgs::new(at, 4096, 0, 0, 0, 0));
                let _ = tool.handle_memory_lock_call(&mut guest, call).await;
                assert_eq!(
                    guest.thread_state().mapping_flags_may_have_changed(),
                    marked,
                    "{sysno} {reply:?} at {at:#x}"
                );
            }
        })
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

        let page = crate::page::page_size() as usize;
        let near_end =
            unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(usize::MAX & !(page - 1)) };
        assert!(
            validate_common_args(
                syscalls::Madvise::new()
                    .with_addr(Some(near_end))
                    .with_len(page)
                    .with_advice(libc::MADV_FREE),
            )
            .is_err()
        );
    }
}
