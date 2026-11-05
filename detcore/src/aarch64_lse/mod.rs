/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Rewriting aarch64 load/store-exclusive retry loops to FEAT_LSE atomics.
//!
//! On aarch64 an `LDXR`/`STXR` pair retries whenever the local exclusive
//! monitor is cleared between the two, which any exception does: a timer tick,
//! an IPI, a page fault, a PMU overflow, a ptrace stop. Each retry retires the
//! loop's branches again, so the retired-branch count of identical guest work
//! depends on host interrupt timing, and Hermit's branch-counted schedule
//! cannot be replayed exactly. A single LSE atomic cannot retry, so a program
//! whose exclusive loops have been rewritten retires a fixed number of
//! branches for the same work.
//!
//! [`insn`] holds the decoder and the encoders; [`rewrite`] holds the idioms
//! and the in-place replacement; [`elf`] applies them to the code of an ELF
//! file's executable sections, which [`mapping`] tells from the data those
//! sections can also hold. All four are pure functions of their input and
//! compile on every architecture, so their tests run everywhere.
//! `hermit-cli` writes rewritten files with them; Detcore applies the same
//! rewrite to a guest's mapped pages.

pub mod elf;
pub mod insn;
pub mod mapping;
pub mod rewrite;

/// Bumped whenever the rewrite's output for the same input can change. It is
/// part of the overlay cache key, so a cached artifact from an older
/// rewriter is never reused, and it is the algorithm identity a preemption
/// record states for its run's rewrite (`crate::preemptions`), so a replay
/// under another version is refused rather than retiring other
/// instructions. 3: only code is rewritten, as [`mapping`] tells it from
/// data; none of the changes since 2 had bumped it, though the canonical
/// shapes the refusal census added, the conditional compare-and-swap
/// refusal, the direct-entry decoding and the positive allow-list each
/// changed what the rewrite writes. 4: a sequence a known entry (the
/// loader's entry, a defined function symbol or an FDE start) points into
/// is refused, as is a sub-word compare whose head is one. 5: literal loads
/// of an FDE entry protect its instruction bytes from rewriting; only ADR
/// address-taking can use the function-entry exemption. 6: literal readers
/// in unknown executable ranges protect the bytes they reference, including
/// bytes in other sections; only proven data is excluded from that scan.
/// 7: so do the reads through an `ADRP`'s page or an `ADR`'s address: an
/// access the instruction bounds protects those bytes, and any other use,
/// or the address held where the scan stops, every executable byte.
pub const REWRITE_FORMAT_VERSION: u32 = 7;

/// Whether this host executes the FEAT_LSE atomics the rewrite emits.
///
/// The rewritten image is a program for the machine that is about to run
/// it: without FEAT_LSE every rewritten site would raise SIGILL.
pub fn host_has_lse_atomics() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        // HWCAP_ATOMICS, Linux Documentation/arch/arm64/elf_hwcaps.rst.
        const HWCAP_ATOMICS: libc::c_ulong = 1 << 8;
        // SAFETY: getauxval reads the process's own auxiliary vector.
        (unsafe { libc::getauxval(libc::AT_HWCAP) } & HWCAP_ATOMICS) != 0
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        false
    }
}
