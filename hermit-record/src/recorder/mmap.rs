/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::Guest;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Madvise;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Mmap;

use super::Recorder;
use crate::event::MadviseEvent;
use crate::event::MadviseRefill;
use crate::event::MmapEvent;
use crate::event::SyscallEvent;

const PAGE_SIZE: usize = 4096;
// Added in Linux 6.13 and not yet exposed by the pinned libc crate.
const MADV_GUARD_REMOVE: i32 = 103;

/// One line of `/proc/<pid>/maps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapping {
    start: usize,
    end: usize,
    /// `PROT_*` bits from the permission column.
    prot: i32,
    private: bool,
    /// Whether the mapping has a backing inode. Replay represents recorded
    /// file mappings as anonymous memory, so these are the ranges whose
    /// contents can differ after advice that drops pages.
    file_backed: bool,
    /// Whether this is shared anonymous memory (`MAP_SHARED|MAP_ANONYMOUS`,
    /// which Linux backs with a deleted `/dev/zero` inode, or System V
    /// shared memory). Replay maps these live, so they need no refill.
    anonymous_shmem: bool,
}

/// `(major, minor)` of a device as `/proc/<pid>/maps` prints it.
type Device = (u32, u32);

/// The device of the kernel's internal shmem mount, found from a memfd,
/// which Linux creates on that mount.
///
/// Shared anonymous memory and System V segments live on the same mount, and
/// their names there (`/dev/zero`, `/SYSV<key>`) are chosen by the kernel:
/// nothing can create a named file on it. A file elsewhere with one of those
/// names is on another device. `None` if no memfd could be made, in which
/// case every file-backed mapping is refilled.
fn shmem_device() -> Option<Device> {
    static DEVICE: OnceLock<Option<Device>> = OnceLock::new();
    *DEVICE.get_or_init(|| {
        // SAFETY: the name is a valid C string; the result is checked.
        let fd = unsafe { libc::memfd_create(c"hermit-shmem-device".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return None;
        }
        // SAFETY: memfd_create returned a new descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let dev = std::fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd()))
            .ok()?
            .dev();
        Some((libc::major(dev), libc::minor(dev)))
    })
}

fn parse_device(field: &str) -> Option<Device> {
    let (major, minor) = field.split_once(':')?;
    Some((
        u32::from_str_radix(major, 16).ok()?,
        u32::from_str_radix(minor, 16).ok()?,
    ))
}

fn parse_maps(text: &str, shmem_device: Option<Device>) -> Vec<Mapping> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_ascii_whitespace();
            let (start, end) = fields.next()?.split_once('-')?;
            let perms = fields.next()?.as_bytes();
            let _offset = fields.next()?;
            let device = parse_device(fields.next()?);
            let inode = fields.next()?;
            let path = fields.collect::<Vec<_>>().join(" ");
            if perms.len() < 4 {
                return None;
            }
            let mut prot = libc::PROT_NONE;
            for (flag, bit) in [
                (b'r', libc::PROT_READ),
                (b'w', libc::PROT_WRITE),
                (b'x', libc::PROT_EXEC),
            ] {
                if perms[..3].contains(&flag) {
                    prot |= bit;
                }
            }
            Some(Mapping {
                start: usize::from_str_radix(start, 16).ok()?,
                end: usize::from_str_radix(end, 16).ok()?,
                prot,
                private: perms[3] == b'p',
                file_backed: inode != "0",
                anonymous_shmem: shmem_device.is_some()
                    && device == shmem_device
                    && (path == "/dev/zero (deleted)" || path.starts_with("/SYSV")),
            })
        })
        .collect()
}

/// Reads the guest's mappings. Recording cannot continue without them: an
/// error returned to the guest would leave no event for replay to consume.
fn read_mappings(path: &str) -> Vec<Mapping> {
    let maps = std::fs::read(path)
        .unwrap_or_else(|error| panic!("Cannot record madvise: {path}: {error}"));
    parse_maps(&String::from_utf8_lossy(&maps), shmem_device())
}

/// The parts of `[start, end)` covered by mappings that replay represents
/// with an anonymous stand-in, each with the protection of its mapping.
///
/// After their pages are dropped these can read back differently in replay:
/// a private file mapping refaults from the file here but reads zeros there,
/// and a shared file mapping refaults from the page cache here, including
/// writes made through a descriptor since it was mapped, which replay's
/// stand-in never saw. Shared anonymous memory is mapped live in replay and
/// keeps its contents in both.
fn refill_ranges(mappings: &[Mapping], start: usize, end: usize) -> Vec<(usize, usize, i32)> {
    mappings
        .iter()
        .filter(|mapping| mapping.file_backed && !mapping.anonymous_shmem)
        .filter_map(|mapping| {
            let lo = mapping.start.max(start);
            let hi = mapping.end.min(end);
            (lo < hi).then_some((lo, hi, mapping.prot))
        })
        .collect()
}

/// Offset of the first private file-backed mapping in `[start, end)`.
///
/// Linux refuses `MADV_WIPEONFORK` with EINVAL at the first VMA that has a
/// file, after applying the advice to the VMAs before it. Replay's anonymous
/// replacement would accept the advice, so replay applies it only to this
/// prefix. Shared mappings stay shared in replay and fail there too.
fn wipeonfork_live_len(mappings: &[Mapping], start: usize, end: usize) -> Option<usize> {
    mappings
        .iter()
        .find(|mapping| {
            mapping.file_backed && mapping.private && mapping.start < end && start < mapping.end
        })
        .map(|mapping| mapping.start.max(start) - start)
}

/// Reads `[start, end)` of the guest through `/proc/<tid>/mem`, which also
/// reads mappings without `PROT_READ`. Pages that cannot be read (guard pages,
/// pages beyond the end of the file) are skipped; the readable runs are
/// returned with their addresses.
fn read_readable_runs(mem: &std::fs::File, start: usize, end: usize) -> Vec<(usize, Vec<u8>)> {
    let mut runs: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut pos = start;
    let mut run_open = false;
    while pos < end {
        let mut buf = vec![0u8; end - pos];
        match mem.read_at(&mut buf, pos as u64) {
            Ok(n) if n > 0 => {
                buf.truncate(n);
                if run_open {
                    runs.last_mut().unwrap().1.extend_from_slice(&buf);
                } else {
                    runs.push((pos, buf));
                    run_open = true;
                }
                pos += n;
            }
            _ => {
                run_open = false;
                pos = (pos & !(PAGE_SIZE - 1)) + PAGE_SIZE;
            }
        }
    }
    runs
}

impl Recorder {
    pub(super) async fn handle_mmap<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Mmap,
    ) -> Result<i64, Errno> {
        // Let anonymous mappings through. This should already be deterministic
        // because ASLR is disabled.
        if syscall.flags().contains(MapFlags::MAP_ANONYMOUS) {
            return guest.inject(syscall).await;
        }

        let len = syscall.len();

        // Do the injection. We need to record the pointer the mapping is at and
        // all of the bytes contained in the mapping. This is very inefficient
        // for large mappings and should be replaced by just letting the mmap
        // through to the real file.
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            match result {
                Ok(addr) => {
                    let addr = Addr::from_raw(addr as usize).ok_or(Errno::EINVAL)?;

                    // NOTE: We can't use `read_exact` here to slurp up the
                    // memory map bytes. The memory size may be larger than the
                    // physical file size for dynamic libraries. The following
                    // `read` will read up to the physical file length, not the
                    // length specified on the memory map. The left over bytes
                    // that extend past the end of the physical file should be
                    // set to zeros when we replay this `mmap`.
                    let mut buf = vec![0u8; len];
                    let physical_length = guest.memory().read(addr, &mut buf)?;

                    // Don't store more bytes than we need to. When we create
                    // the anonymous map later, the extra bytes will be
                    // initialized to zero automatically.
                    buf.truncate(physical_length);

                    Ok(SyscallEvent::Mmap(MmapEvent {
                        addr: addr.as_raw(),
                        buf,
                    }))
                }
                Err(errno) => Err(errno),
            },
        );

        result
    }

    /// Records a guest-semantic `madvise`.
    ///
    /// The advice runs live in both recording and replay. Replay's anonymous
    /// stand-ins for file mappings differ in two guest-visible ways, which the
    /// event carries: pages dropped from a file mapping read back as file
    /// contents here but as zeros in replay, and `MADV_WIPEONFORK` fails at a
    /// private file mapping here but would succeed in replay.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(https://github.com/rrnewton/hermit/pull/3589): Audit the madvise refill and WIPEONFORK prefix.
    pub(super) async fn handle_madvise<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Madvise,
    ) -> Result<i64, Errno> {
        let advice = syscall.advice();
        let start = syscall.addr().map(AddrMut::as_raw).unwrap_or(0);
        let len = syscall.len();
        // Detcore has already rejected unaligned or overflowing ranges.
        let end = start.saturating_add(len).next_multiple_of(PAGE_SIZE);

        let drops_pages = matches!(
            advice,
            libc::MADV_DONTNEED | libc::MADV_DONTNEED_LOCKED | MADV_GUARD_REMOVE
        );
        // The thread's own maps: the thread-group leader's is empty once it has
        // exited.
        let maps_path = format!("/proc/{}/maps", guest.tid().as_raw());

        let wipeonfork_prefix = if advice == libc::MADV_WIPEONFORK {
            let mappings = read_mappings(&maps_path);
            wipeonfork_live_len(&mappings, start, end)
        } else {
            None
        };

        let result = guest.inject(syscall).await;

        // Linux reports EINVAL at the first private file mapping and returns
        // without visiting the rest of the range. Replay reproduces that by
        // applying the advice only up to that mapping. Any other outcome
        // means Linux accepted or failed independently of the file mappings,
        // so replay applies the whole range and must reproduce the result.
        let live_len = match (wipeonfork_prefix, result) {
            (Some(prefix), Err(Errno::EINVAL)) => prefix,
            _ => len,
        };

        let mut refills = Vec::new();
        if drops_pages {
            let mappings = read_mappings(&maps_path);
            let ranges = refill_ranges(&mappings, start, end);
            if !ranges.is_empty() {
                let mem_path = format!("/proc/{}/mem", guest.tid().as_raw());
                // The advice has already taken effect, so failing the guest
                // call now would leave the recording without its event.
                let mem = std::fs::File::open(&mem_path)
                    .unwrap_or_else(|error| panic!("Cannot record madvise: {mem_path}: {error}"));
                for (lo, hi, prot) in ranges {
                    for (addr, bytes) in read_readable_runs(&mem, lo, hi) {
                        refills.push(MadviseRefill { addr, bytes, prot });
                    }
                }
            }
        }

        self.record_event(
            guest,
            Ok(SyscallEvent::Madvise(MadviseEvent {
                result,
                live_len,
                refills,
            })),
        );

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAPS: &str = "\
00400000-00401000 r-xp 00000000 08:01 1234 /usr/bin/guest
7f0000000000-7f0000002000 rw-p 00000000 00:00 0
7f0000002000-7f0000003000 rw-p 00001000 08:01 1234 /usr/bin/guest
7f0000003000-7f0000004000 ---s 00000000 00:01 77 /dev/zero (deleted)
7f0000004000-7f0000005000 rw-s 00000000 00:01 88 /memfd:shared state (deleted)
7f0000005000-7f0000006000 rw-s 00000000 00:01 2 /SYSV00000000 (deleted)
7f0000006000-7f0000007000 rw-s 00000000 08:01 99 /SYSV00000000 (deleted)
7f0000007000-7f0000008000 rw-s 00000000 00:05 98 /dev/zero (deleted)
7ffffffde000-7ffffffff000 rw-p 00000000 00:00 0 [stack]
";

    /// The shmem device in `MAPS`.
    const SHMEM: Option<Device> = Some((0, 1));

    #[test]
    fn parses_maps_lines() {
        let mappings = parse_maps(MAPS, SHMEM);
        assert_eq!(mappings.len(), 9);
        assert_eq!(
            mappings[0],
            Mapping {
                start: 0x400000,
                end: 0x401000,
                prot: libc::PROT_READ | libc::PROT_EXEC,
                private: true,
                file_backed: true,
                anonymous_shmem: false,
            }
        );
        assert!(!mappings[1].file_backed);
        assert_eq!(mappings[3].prot, libc::PROT_NONE);
        assert!(!mappings[3].private);
        assert!(mappings[3].file_backed);
        assert!(mappings[3].anonymous_shmem);
        assert!(!mappings[4].anonymous_shmem);
        assert!(mappings[5].anonymous_shmem);
    }

    #[test]
    fn refill_ranges_clip_to_the_advised_range() {
        let mappings = parse_maps(MAPS, SHMEM);
        assert_eq!(
            refill_ranges(&mappings, 0x7f0000001000, 0x7f0000004000),
            vec![(
                0x7f0000002000,
                0x7f0000003000,
                libc::PROT_READ | libc::PROT_WRITE
            )]
        );
        assert!(refill_ranges(&mappings, 0x7f0000000000, 0x7f0000002000).is_empty());
    }

    #[test]
    fn refill_ranges_cover_shared_files_but_not_shared_anonymous_memory() {
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        // The memfd and the two files merely named like shared anonymous
        // memory on other devices are refilled.
        let mappings = parse_maps(MAPS, SHMEM);
        assert_eq!(
            refill_ranges(&mappings, 0x7f0000003000, 0x7f0000008000),
            vec![
                (0x7f0000004000, 0x7f0000005000, rw),
                (0x7f0000006000, 0x7f0000007000, rw),
                (0x7f0000007000, 0x7f0000008000, rw),
            ]
        );
        // Without a known shmem device, everything file-backed is refilled.
        let mappings = parse_maps(MAPS, None);
        assert!(mappings.iter().all(|mapping| !mapping.anonymous_shmem));
        assert_eq!(
            refill_ranges(&mappings, 0x7f0000003000, 0x7f0000008000).len(),
            5
        );
    }

    #[test]
    fn this_hosts_shared_anonymous_memory_is_on_the_shmem_device() {
        // SAFETY: a fresh anonymous mapping, unmapped below.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(addr, libc::MAP_FAILED);
        let mappings = read_mappings("/proc/self/maps");
        let mapping = mappings
            .iter()
            .find(|mapping| mapping.start == addr as usize)
            .expect("the new mapping is listed");
        assert!(mapping.file_backed && mapping.anonymous_shmem);
        assert!(refill_ranges(&mappings, mapping.start, mapping.end).is_empty());
        // SAFETY: mapped above and no longer referenced.
        assert_eq!(unsafe { libc::munmap(addr, PAGE_SIZE) }, 0);
    }

    #[test]
    fn wipeonfork_prefix_stops_at_the_first_private_file_mapping() {
        let mappings = parse_maps(MAPS, SHMEM);
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000000000, 0x7f0000004000),
            Some(0x2000)
        );
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000002000, 0x7f0000003000),
            Some(0)
        );
        // A shared mapping fails in replay too, so it needs no prefix.
        assert_eq!(
            wipeonfork_live_len(&mappings, 0x7f0000003000, 0x7f0000004000),
            None
        );
    }
}
