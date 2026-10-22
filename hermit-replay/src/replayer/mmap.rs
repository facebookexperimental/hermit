/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `mmap` is a tricky case. When using `MAP_ANONYMOUS` with a file descriptor of
//! -1, we are effectively allocating memory. If ASLR is turned off, this should
//! be deterministic and the mmap can be let through in this case. However, when
//! mapping a file descriptor, things get tricky. Memory writes from one thread
//! can affect the memory reads from another thread. During recording, we should
//! record the entire contents of the buffer. During replay, we need some way to
//! allocate the same size buffer at the same address. We do this by injecting
//!
//! ```ignore
//! mmap(desired_addr, length, flags | MAP_ANONYMOUS, -1, 0)
//! ```
//!
//! in order to get a blank memory map of the right size. We can then fill this
//! with the previously recorded bytes.

use std::os::unix::fs::FileExt;

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;
use reverie::syscalls::Mprotect;
use reverie::syscalls::ProtFlags;

use super::Replayer;
use crate::event::MadviseRefill;

const PAGE_SIZE: usize = 4096;

impl Replayer {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        let flags = syscall.flags();

        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if flags.contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject_with_retry(syscall).await;
        }

        // Get the event. We need to know what pointer to use for the map.
        let event = next_event!(guest, Mmap)?;

        // This is safe since we only record non-NULL pointers.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(event.addr) };

        let len = syscall.len();
        let prot = syscall.prot();

        // On replay, we can't actually map the original file because it may not
        // exist. Instead, change this to be an anonymous mapping and write the
        // bytes that we recorded to it. This has the same effect, but without
        // requiring to map a real file.
        //
        // Write permission is also needed to be able to write the recorded
        // bytes to the mapping. After the data has been written, it can be
        // reset to the original protection value with a call to `mprotect`.
        let ptr = guest
            .inject_with_retry(
                syscall
                    .with_addr(Some(addr.cast::<libc::c_void>().into()))
                    .with_prot(prot | ProtFlags::PROT_WRITE)
                    .with_flags(flags | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0),
            )
            .await?;

        // Make sure we got the pointer we wanted.
        assert_eq!(
            ptr as usize, event.addr,
            "Failed to inject mmap at desired address"
        );

        // Fill in the memory map.
        guest.memory().write_exact(addr.cast(), &event.buf).unwrap();

        // Reset the page protection to the original value (if it didn't already
        // have PROT_WRITE) so that we still correctly mimic the page protection
        // of the original mapping.
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(
                    Mprotect::new()
                        .with_addr(Some(addr.cast()))
                        .with_len(len)
                        .with_protection(prot),
                )
                .await?;
        }

        Ok(ptr)
    }

    /// Replays a guest-semantic `madvise` recorded by the recorder's
    /// `handle_madvise`: applies the advice live to the recorded prefix, then
    /// restores the file-backed contents the recording observed.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3589): Audit the madvise refill and WIPEONFORK prefix.
    pub(super) async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Madvise,
    ) -> Result<i64, Errno> {
        let event = next_event!(guest, Madvise)?;
        let len = syscall.len();
        assert!(
            event.live_len <= len,
            "Recorded madvise prefix {} exceeds the guest length {}",
            event.live_len,
            len
        );

        if event.live_len == len {
            // Some recorded outcomes depend on the mapping type, which replay's
            // anonymous stand-ins do not preserve: MADV_GUARD_INSTALL on a
            // private file mapping fails with EINVAL before Linux 6.15, and
            // advice on device or hugetlbfs mappings can fail where anonymous
            // memory accepts it. Those fail the assertion below loudly.
            let result = guest.inject_with_retry(syscall).await;
            assert_eq!(
                result,
                event.result,
                "Replayed madvise({:?}, {}, {}) diverged from the recording; replay \
                 cannot reproduce this outcome",
                syscall.addr(),
                len,
                syscall.advice()
            );
        } else if event.live_len > 0 {
            // The recording failed at a file mapping after this prefix, so the
            // recorded error is the result. Before reaching it, Linux may have
            // passed holes (ENOMEM) or a shared mapping (EINVAL); anything else
            // means the prefix was not advised as it was when recording.
            let result = guest
                .inject_with_retry(syscall.with_len(event.live_len))
                .await;
            assert!(
                matches!(result, Ok(0) | Err(Errno::ENOMEM) | Err(Errno::EINVAL)),
                "Replayed madvise prefix ({:?}, {}, {}) returned {:?}; replay cannot \
                 reproduce the recorded {:?}",
                syscall.addr(),
                event.live_len,
                syscall.advice(),
                result,
                event.result
            );
        }

        if !event.refills.is_empty() {
            let mem_path = format!("/proc/{}/mem", guest.tid().as_raw());
            let mem = std::fs::File::open(&mem_path)
                .unwrap_or_else(|error| panic!("Cannot replay madvise: {mem_path}: {error}"));
            for refill in &event.refills {
                // Only write pages that differ: the range may include mappings
                // replay maps from the real file, such as executables, where a
                // write would create private copies the recording did not have.
                let mut current = vec![0u8; refill.bytes.len()];
                let readable = read_prefix(&mem, refill.addr, &mut current);
                for (offset, run) in differing_pages(&current[..readable], &refill.bytes) {
                    self.write_refill(guest, refill, offset, run).await;
                }
            }
        }

        event.result
    }
}

impl Replayer {
    /// Writes `refill.bytes[offset..offset + len]`, lifting write protection
    /// for the duration of the write.
    ///
    /// The advice has already been applied, so a failure here panics rather
    /// than return an error the recording never saw.
    async fn write_refill<G: Guest<Self>>(
        &self,
        guest: &mut G,
        refill: &MadviseRefill,
        offset: usize,
        len: usize,
    ) {
        let start = refill.addr + offset;
        let prot = ProtFlags::from_bits_truncate(refill.prot);
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot | ProtFlags::PROT_WRITE))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot unprotect madvise refill at {start:#x}: {err}")
                });
        }
        // This is safe since the recorder only records mapped addresses.
        let addr = unsafe { AddrMut::<u8>::from_raw_unchecked(start) };
        guest
            .memory()
            .write_exact(addr, &refill.bytes[offset..offset + len])
            .unwrap();
        if !prot.contains(ProtFlags::PROT_WRITE) {
            guest
                .inject_with_retry(protection(start, len, prot))
                .await
                .unwrap_or_else(|err| {
                    panic!("Cannot reprotect madvise refill at {start:#x}: {err}")
                });
        }
    }
}

fn protection(start: usize, len: usize, protection: ProtFlags) -> Mprotect {
    // This is safe since the recorder only records mapped addresses.
    let addr = unsafe { AddrMut::<libc::c_void>::from_raw_unchecked(start) };
    Mprotect::new()
        .with_addr(Some(addr))
        .with_len(len)
        .with_protection(protection)
}

/// Reads guest memory at `addr` through `/proc/<tid>/mem`, which also reads
/// mappings without `PROT_READ`, and returns how many leading bytes it read.
fn read_prefix(mem: &std::fs::File, addr: usize, buf: &mut [u8]) -> usize {
    let mut done = 0;
    while done < buf.len() {
        match mem.read_at(&mut buf[done..], (addr + done) as u64) {
            Ok(n) if n > 0 => done += n,
            _ => break,
        }
    }
    done
}

/// Page-aligned `(offset, len)` runs where `recorded` differs from `current`.
/// Bytes past the end of `current` (unreadable in replay) always differ.
fn differing_pages(current: &[u8], recorded: &[u8]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for offset in (0..recorded.len()).step_by(PAGE_SIZE) {
        let end = (offset + PAGE_SIZE).min(recorded.len());
        if current.get(offset..end) == Some(&recorded[offset..end]) {
            continue;
        }
        match runs.last_mut() {
            Some((start, len)) if *start + *len == offset => *len = end - *start,
            _ => runs.push((offset, end - offset)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn differing_pages_merges_adjacent_changed_pages() {
        let recorded = vec![1u8; 4 * PAGE_SIZE];
        let mut current = recorded.clone();
        current[PAGE_SIZE + 7] = 0;
        current[2 * PAGE_SIZE] = 0;
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE)]
        );
        assert!(differing_pages(&recorded, &recorded).is_empty());
    }

    #[test]
    fn differing_pages_treats_unread_bytes_as_changed() {
        let recorded = vec![1u8; 3 * PAGE_SIZE + 10];
        let current = recorded[..PAGE_SIZE].to_vec();
        assert_eq!(
            differing_pages(&current, &recorded),
            vec![(PAGE_SIZE, 2 * PAGE_SIZE + 10)]
        );
    }
}
