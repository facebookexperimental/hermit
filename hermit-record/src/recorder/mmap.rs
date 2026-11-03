/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::HashSet;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::MutexGuard;
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
use crate::event::SharedMapping;
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
    /// Whether this is shared anonymous memory (`MAP_SHARED|MAP_ANONYMOUS` or
    /// `/dev/zero`, which Linux backs with a deleted `/dev/zero` inode, or
    /// System V shared memory). Their pages outlive advice that drops a
    /// mapping's pages, as the pages of replay's stand-ins do (live shared
    /// memory, or a shared view of a backing file), so they need no refill.
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

/// `/dev/zero`'s device number: Linux opens minor 5 of the memory devices'
/// major 1 with the operations whose `mmap` is `mmap_zero`.
const DEV_ZERO: (u32, u32) = (1, 5);

/// The name of the shmem object Linux makes for a shared mapping of
/// `/dev/zero` or a `MAP_SHARED | MAP_ANONYMOUS` one (`shmem_zero_setup`), as
/// `/proc` shows it. Only the kernel names files on its internal shmem mount
/// ([`shmem_device`]); a memfd there is `/memfd:<name> (deleted)`.
const ZERO_OBJECT_NAME: &str = "/dev/zero (deleted)";

/// File systems whose regular files can have an `mmap` of their own, which
/// maps kernel or device memory rather than file pages: a PCI device's
/// `resourceN` in sysfs or its `/proc/bus/pci` file, and debugfs and tracefs
/// files. By `statfs` magic.
const OWN_MMAP_FILE_SYSTEMS: [(i64, &str); 4] = [
    (0x6265_6572, "sysfs"),
    (0x6462_6720, "debugfs"),
    (0x7472_6163, "tracefs"),
    (0x9fa0, "procfs"),
];

/// The shmem objects of the `/dev/zero` mappings this recording recorded, by
/// device and inode: a later mapping of one of them through its own file
/// (`/proc/<pid>/map_files`) is recorded as a mapping of that file, which
/// replay backs with the same memory.
fn recorded_zero_objects() -> MutexGuard<'static, HashSet<(u64, u64)>> {
    static OBJECTS: OnceLock<Mutex<HashSet<(u64, u64)>>> = OnceLock::new();
    OBJECTS
        .get_or_init(Default::default)
        .lock()
        .expect("should not be poisoned: no code panics while holding it")
}

/// What a `MAP_SHARED` mapping shares, as recording sees it before the call.
#[derive(Debug, PartialEq, Eq)]
enum Sharing {
    /// The file whose pages the mapping maps, for replay to share them again.
    File(SharedMapping),
    /// `/dev/zero` at offset 0 through a descriptor opened with `access`:
    /// the call maps a new object of its own, which `/proc/<tid>/maps` names
    /// once it is made ([`object_at`]).
    Zero { access: i32 },
    /// Memory replay cannot reproduce, and why. A successful mapping of it
    /// marks the recording as one replay refuses
    /// (`detcore::detlog::record_replay_refusal`), and the guest runs on.
    Refused(String),
}

/// What a `MAP_SHARED` mapping of descriptor `fd` of thread `tid`, at
/// `offset`, shares: for a regular file, the file and the descriptor's access
/// mode. `/proc/<tid>` shows that thread's own descriptor table, which is not
/// the leader's for a thread cloned without `CLONE_FILES`.
fn shared_mapping(flags: MapFlags, tid: i32, fd: i32, offset: i64) -> io::Result<Option<Sharing>> {
    shared_mapping_with(flags, tid, fd, offset, |path| std::fs::read_to_string(path))
}

fn shared_mapping_with(
    flags: MapFlags,
    tid: i32,
    fd: i32,
    offset: i64,
    read_fdinfo: impl FnOnce(&str) -> io::Result<String>,
) -> io::Result<Option<Sharing>> {
    if !flags.contains(MapFlags::MAP_SHARED) {
        return Ok(None);
    }
    let path = format!("/proc/{tid}/fd/{fd}");
    let metadata = std::fs::metadata(&path)?;
    let file = MappedFileFacts {
        mode: metadata.mode(),
        rdev: metadata.rdev(),
        device: metadata.dev(),
        inode: metadata.ino(),
        fs_type: file_system_type(&path)?,
        target: std::fs::read_link(&path)?,
    };
    let offset = u64::try_from(offset)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let read_access = move || {
        let fdinfo = read_fdinfo(&format!("/proc/{tid}/fdinfo/{fd}"))?;
        descriptor_access(&fdinfo).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fdinfo has no valid access flags",
            )
        })
    };
    let refused = |what: &str| {
        Sharing::Refused(format!(
            "a MAP_SHARED mapping of {} ({what}): replay reproduces the memory of \
             shared mappings of regular files and of /dev/zero at offset 0 only",
            file.target.display()
        ))
    };
    let kind = mapped_file(&file, shmem_device(), |object| {
        recorded_zero_objects().contains(&object)
    });
    Ok(Some(match kind {
        MappedFile::Regular => Sharing::File(SharedMapping {
            device: file.device,
            inode: file.inode,
            offset,
            access: read_access()?,
        }),
        // The new object is as long as the mapping, and its pages at or past
        // that length fault SIGBUS: at offset 0 the mapping has none, as a
        // replay mapping of a backing at least as long has none.
        MappedFile::Zero if offset == 0 => Sharing::Zero {
            access: read_access()?,
        },
        MappedFile::Zero => refused(&format!(
            "at offset {offset}, whose pages past the mapping's length fault SIGBUS"
        )),
        MappedFile::Other(what) => refused(&what),
    }))
}

/// The `statfs` magic of the file system that holds the file `path` names.
fn file_system_type(path: &str) -> io::Result<i64> {
    let path = std::ffi::CString::new(path).expect("a /proc path holds no NUL");
    // SAFETY: `statfs` is plain old data, valid when zeroed.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: the path is a valid C string and `buf` is writable.
    if unsafe { libc::statfs(path.as_ptr(), &mut buf) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf.f_type)
}

/// What `stat`, `statfs` and `/proc/<tid>/fd` say of a mapped file.
#[derive(Debug)]
struct MappedFileFacts {
    /// `st_mode`.
    mode: u32,
    /// `st_rdev`.
    rdev: u64,
    /// `st_dev`.
    device: u64,
    /// `st_ino`.
    inode: u64,
    /// `f_type` of its file system.
    fs_type: i64,
    /// The name `/proc/<tid>/fd` gives it.
    target: std::path::PathBuf,
}

/// What a `MAP_SHARED` mapping maps, by its file's type and device number.
#[derive(Debug, PartialEq, Eq)]
enum MappedFile {
    /// A file whose pages every shared mapping of it maps: a regular file, or
    /// the shmem object of a recorded `/dev/zero` mapping.
    Regular,
    /// `/dev/zero`, of which every shared mapping is a new shmem object.
    Zero,
    /// Any other file, described.
    Other(String),
}

/// What `file` is to a `MAP_SHARED` mapping. Linux opens a character device
/// by its number, whatever its name, so `/dev/zero` is any node of
/// [`DEV_ZERO`]. `shmem_device` is the kernel's internal shmem mount, and
/// `recorded_zero_object` says whether this recording recorded a
/// `/dev/zero` mapping of an object by device and inode.
fn mapped_file(
    file: &MappedFileFacts,
    shmem_device: Option<Device>,
    recorded_zero_object: impl FnOnce((u64, u64)) -> bool,
) -> MappedFile {
    // The files `anon_inode_getfile` makes share one inode, so its identity
    // names no file of its own. Linux reports an anonymous inode with no type
    // (`anon_inode_getattr`); its name says what it is whatever its reported
    // type.
    let anonymous = file
        .target
        .as_os_str()
        .as_encoded_bytes()
        .starts_with(b"anon_inode:");
    let device = (libc::major(file.rdev), libc::minor(file.rdev));
    match file.mode & libc::S_IFMT {
        _ if anonymous => {
            MappedFile::Other("an anonymous inode, which such files share".to_owned())
        }
        libc::S_IFREG => {
            if let Some((_, name)) = OWN_MMAP_FILE_SYSTEMS
                .iter()
                .find(|(magic, _)| *magic == file.fs_type)
            {
                return MappedFile::Other(format!(
                    "a file on {name}, whose mmap can map kernel or device memory"
                ));
            }
            let on_shmem_mount = shmem_device.is_some()
                && shmem_device == Some((libc::major(file.device), libc::minor(file.device)));
            if on_shmem_mount
                && file.target == Path::new(ZERO_OBJECT_NAME)
                && !recorded_zero_object((file.device, file.inode))
            {
                // Its first mapping was MAP_SHARED | MAP_ANONYMOUS, which runs
                // live in record and replay with no event, so replay has no
                // backing a mapping of this file could share.
                return MappedFile::Other(
                    "the shmem object of a MAP_SHARED | MAP_ANONYMOUS mapping, \
                     whose memory replay cannot name"
                        .to_owned(),
                );
            }
            MappedFile::Regular
        }
        libc::S_IFCHR if device == DEV_ZERO => MappedFile::Zero,
        libc::S_IFCHR => MappedFile::Other(format!(
            "a character device {}:{}, whose driver maps memory of its own",
            device.0, device.1
        )),
        libc::S_IFBLK => MappedFile::Other("a block device".to_owned()),
        0 => MappedFile::Other("a pseudo-file with no file type".to_owned()),
        kind => MappedFile::Other(format!("a file of type {kind:o}")),
    }
}

/// Names the new `/dev/zero` mapping at `address` of thread `tid`, mapped
/// through a descriptor opened with `access`, by the object Linux made for it
/// as `/proc/<tid>/maps` shows it, and remembers the object, so that a later
/// mapping of the object's own file is recorded as sharing its memory. The
/// mapping exists, so a listing without it cannot be recorded.
fn name_zero_mapping(tid: i32, address: usize, access: i32) -> SharedMapping {
    let maps_path = format!("/proc/{tid}/maps");
    let (device, inode) = std::fs::read_to_string(&maps_path)
        .ok()
        .and_then(|maps| object_at(&maps, address))
        .unwrap_or_else(|| {
            panic!(
                "Cannot record MAP_SHARED mmap of /dev/zero: \
                 {maps_path} names no object at {address:#x}"
            )
        });
    recorded_zero_objects().insert((device, inode));
    SharedMapping {
        device,
        inode,
        offset: 0,
        access,
    }
}

/// The file a `/proc/<pid>/maps` listing shows mapped at `address`, the start
/// of a mapping, by device and inode.
fn object_at(maps: &str, address: usize) -> Option<(u64, u64)> {
    maps.lines().find_map(|line| {
        let mut fields = line.split_ascii_whitespace();
        let (start, _) = fields.next()?.split_once('-')?;
        if usize::from_str_radix(start, 16).ok()? != address {
            return None;
        }
        let (_perms, _offset) = (fields.next()?, fields.next()?);
        let (major, minor) = parse_device(fields.next()?)?;
        let inode = fields.next()?.parse().ok()?;
        Some((libc::makedev(major, minor), inode))
    })
}

/// A failed mmap needs no sharing identity. A successful one cannot be
/// recorded without it: returning the capture error to the guest would hide
/// its live mapping, and writing `None` would let replay replace shared pages
/// with a private copy.
fn captured_mmap_result(
    result: Result<i64, Errno>,
    shared: io::Result<Option<Sharing>>,
) -> Result<(i64, Option<Sharing>), Errno> {
    let addr = result?;
    let shared = shared.unwrap_or_else(|error| {
        panic!(
            "Cannot record MAP_SHARED mmap: cannot capture file identity and access mode: {error}"
        )
    });
    Ok((addr, shared))
}

/// The access mode in a `/proc/<pid>/fdinfo/<fd>` listing: the `O_ACCMODE`
/// bits of its octal `flags:` line.
fn descriptor_access(fdinfo: &str) -> Option<i32> {
    let flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))?
        .trim();
    Some(i32::from_str_radix(flags, 8).ok()? & libc::O_ACCMODE)
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
        // Read before the call: the descriptor names the same file then, and a
        // sibling thread could not have closed it, as the call holds the turn.
        let shared = shared_mapping(
            syscall.flags(),
            guest.tid().as_raw(),
            syscall.fd(),
            syscall.offset(),
        );

        // Do the injection. We need to record the pointer the mapping is at and
        // all of the bytes contained in the mapping. This is very inefficient
        // for large mappings and should be replaced by just letting the mmap
        // through to the real file.
        let result = guest.inject(syscall).await;

        self.record_event(
            guest,
            match captured_mmap_result(result, shared) {
                Ok((addr, sharing)) => {
                    let addr = Addr::from_raw(addr as usize).ok_or(Errno::EINVAL)?;
                    // Replay fills a file's backing from recorded bytes. A
                    // new /dev/zero object holds zeros, as a new backing does,
                    // and a refused mapping is never replayed, so their bytes
                    // are not read: reading them through /proc/<pid>/mem
                    // would fault in the new object's pages, or call a
                    // device driver's fault or access handler, during the
                    // recorded run.
                    let (shared, read_bytes) = match sharing {
                        None => (None, true),
                        Some(Sharing::File(shared)) => (Some(shared), true),
                        Some(Sharing::Zero { access }) => (
                            Some(name_zero_mapping(
                                guest.tid().as_raw(),
                                addr.as_raw(),
                                access,
                            )),
                            false,
                        ),
                        Some(Sharing::Refused(reason)) => {
                            tracing::error!("Cannot replay this recording: {reason}");
                            detcore::detlog::record_replay_refusal(&reason);
                            (None, false)
                        }
                    };
                    let buf = if read_bytes {
                        // NOTE: We can't use `read_exact` here to slurp up the
                        // memory map bytes. The memory size may be larger than
                        // the physical file size for dynamic libraries. The
                        // following `read` will read up to the physical file
                        // length, not the length specified on the memory map.
                        // The left over bytes that extend past the end of the
                        // physical file should be set to zeros when we replay
                        // this `mmap`.
                        let mut buf = vec![0u8; len];
                        let physical_length = guest.memory().read(addr, &mut buf)?;

                        // Don't store more bytes than we need to. When we
                        // create the anonymous map later, the extra bytes will
                        // be initialized to zero automatically.
                        buf.truncate(physical_length);
                        buf
                    } else {
                        Vec::new()
                    };

                    Ok(SyscallEvent::Mmap(MmapEvent {
                        addr: addr.as_raw(),
                        buf,
                        shared,
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

    fn failed_fdinfo_capture(file: &std::fs::File) -> io::Result<Option<Sharing>> {
        let tid = std::process::id() as i32;
        let fd = file.as_raw_fd();
        let mut read_attempted = false;
        let capture = shared_mapping_with(MapFlags::MAP_SHARED, tid, fd, 0, |path| {
            assert_eq!(path, format!("/proc/{tid}/fdinfo/{fd}"));
            read_attempted = true;
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        });
        assert!(read_attempted, "metadata must succeed before fdinfo fails");
        assert_eq!(
            capture.as_ref().unwrap_err().raw_os_error(),
            Some(libc::EMFILE)
        );
        capture
    }

    /// A tracer can run out of descriptors while the guest's mmap succeeds.
    /// The capture error must reach the actual result-to-event boundary.
    #[test]
    #[should_panic(
        expected = "Cannot record MAP_SHARED mmap: cannot capture file identity and access mode"
    )]
    fn a_successful_shared_mmap_refuses_a_failed_fdinfo_capture() {
        let file = tempfile::tempfile().unwrap();
        let capture = failed_fdinfo_capture(&file);
        let _ = captured_mmap_result(Ok(0x1000), capture);
    }

    #[test]
    fn a_failed_mmap_keeps_its_errno_when_fdinfo_capture_fails() {
        let file = tempfile::tempfile().unwrap();
        for errno in [Errno::EINVAL, Errno::EBADF, Errno::EACCES] {
            let capture = failed_fdinfo_capture(&file);
            assert_eq!(captured_mmap_result(Err(errno), capture), Err(errno));
        }
    }

    #[test]
    fn private_mmaps_do_not_read_shared_file_identity() {
        let capture = shared_mapping_with(MapFlags::MAP_PRIVATE, -1, -1, 0, |_| {
            panic!("a private mapping must not read fdinfo")
        });
        assert_eq!(
            captured_mmap_result(Ok(0x1000), capture),
            Ok((0x1000, None))
        );
    }

    #[test]
    fn shared_mmaps_keep_the_captured_identity_and_access() {
        let file = tempfile::tempfile().unwrap();
        let metadata = file.metadata().unwrap();
        let expected = SharedMapping {
            device: metadata.dev(),
            inode: metadata.ino(),
            offset: 4096,
            access: libc::O_RDWR,
        };
        for flags in [
            MapFlags::MAP_SHARED,
            MapFlags::from_bits_retain(libc::MAP_SHARED_VALIDATE),
        ] {
            let capture = shared_mapping(flags, std::process::id() as i32, file.as_raw_fd(), 4096);
            assert_eq!(
                captured_mmap_result(Ok(0x1000), capture),
                Ok((0x1000, Some(Sharing::File(expected))))
            );
        }
    }

    /// Linux gives every `MAP_SHARED` mapping of `/dev/zero` a new shmem
    /// object of its own (`mmap_zero`, `shmem_zero_setup`), so its descriptor's
    /// identity names nothing two mappings share: the mapping is named after
    /// the call, by its object. At a nonzero offset the object's pages past
    /// the mapping's length fault SIGBUS, which replay cannot reproduce.
    #[test]
    fn dev_zero_mappings_are_named_after_the_call() {
        let zero = std::fs::File::open("/dev/zero").unwrap();
        let tid = std::process::id() as i32;
        let capture = |offset| {
            shared_mapping_with(
                MapFlags::MAP_SHARED,
                tid,
                zero.as_raw_fd(),
                offset,
                |path| std::fs::read_to_string(path),
            )
            .unwrap()
        };
        assert_eq!(
            capture(0),
            Some(Sharing::Zero {
                access: libc::O_RDONLY
            })
        );
        match capture(4096) {
            Some(Sharing::Refused(reason)) => assert!(
                reason.contains("of /dev/zero (at offset 4096") && reason.contains("SIGBUS"),
                "{reason}"
            ),
            other => panic!("a /dev/zero mapping at an offset was not refused: {other:?}"),
        }
    }

    /// The object a mapping maps is read from `/proc/<pid>/maps` by its start.
    #[test]
    fn the_object_at_a_mapping_is_its_device_and_inode() {
        let maps = "7f0000000000-7f0000001000 rw-s 00000000 00:01 4224641 /dev/zero (deleted)\n\
                    7f0000001000-7f0000002000 r--s 00000000 00:06 5 /dev/zero\n\
                    7f0000002000-7f0000003000 rw-p 00000000 00:00 0\n";
        assert_eq!(
            object_at(maps, 0x7f00_0000_0000),
            Some((libc::makedev(0, 1), 4224641))
        );
        assert_eq!(
            object_at(maps, 0x7f00_0000_1000),
            Some((libc::makedev(0, 6), 5))
        );
        assert_eq!(object_at(maps, 0x7f00_0000_0800), None);
    }

    fn map(fd: i32, flags: i32, prot: i32) -> usize {
        // SAFETY: a new mapping the kernel places; the result is checked.
        let address = unsafe { libc::mmap(std::ptr::null_mut(), 4096, prot, flags, fd, 0) };
        assert_ne!(address, libc::MAP_FAILED, "{}", io::Error::last_os_error());
        address as usize
    }

    fn own_object_at(address: usize) -> (u64, u64) {
        object_at(
            &std::fs::read_to_string("/proc/self/maps").unwrap(),
            address,
        )
        .expect("no mapping at the address")
    }

    /// Each read-write mapping of `/dev/zero` is a new object on the kernel's
    /// shmem mount, named `/dev/zero (deleted)`. A read-only one is private
    /// zero memory that can never be written (`mmap_zero` sees no
    /// `VM_SHARED`), and the mapping keeps the device's own file.
    #[test]
    fn each_shared_dev_zero_mapping_maps_an_object_of_its_own() {
        let rw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/zero")
            .unwrap();
        let rw_prot = libc::PROT_READ | libc::PROT_WRITE;
        let tid = std::process::id() as i32;
        let name = |address| {
            let named = name_zero_mapping(tid, address, libc::O_RDWR);
            assert_eq!((named.device, named.inode), own_object_at(address));
            assert_eq!((named.offset, named.access), (0, libc::O_RDWR));
            assert!(recorded_zero_objects().contains(&(named.device, named.inode)));
            (named.device, named.inode)
        };
        let first = name(map(rw.as_raw_fd(), libc::MAP_SHARED, rw_prot));
        let second = name(map(rw.as_raw_fd(), libc::MAP_SHARED, rw_prot));
        assert_ne!(first, second);
        let shmem = shmem_device().expect("a memfd names the shmem mount");
        for (device, _) in [first, second] {
            assert_eq!((libc::major(device), libc::minor(device)), shmem);
        }
        let ro = std::fs::File::open("/dev/zero").unwrap();
        let node = ro.metadata().unwrap();
        let read_only = own_object_at(map(ro.as_raw_fd(), libc::MAP_SHARED, libc::PROT_READ));
        assert_eq!(read_only, (node.dev(), node.ino()));
    }

    /// A mapping's own file, opened through `/proc/<pid>/map_files`, is the
    /// object it maps: a regular file on the shmem mount, `/dev/zero
    /// (deleted)`, with the inode `/proc/<pid>/maps` shows. The object of a
    /// recorded `/dev/zero` mapping is recorded as that file, so replay
    /// shares the two mappings' memory; the object of a MAP_SHARED |
    /// MAP_ANONYMOUS mapping, which replay makes live with no name, refuses
    /// replay. Opening the file needs CAP_SYS_ADMIN or CAP_CHECKPOINT_RESTORE
    /// in the initial user namespace (`proc_map_files_get_link`), which a
    /// Hermit guest's container never has, so this checks the kernel's side
    /// only where the test holds one.
    #[test]
    fn a_reopened_zero_object_is_shared_only_when_its_mapping_was_recorded() {
        let rw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/zero")
            .unwrap();
        let prot = libc::PROT_READ | libc::PROT_WRITE;
        let zero = map(rw.as_raw_fd(), libc::MAP_SHARED, prot);
        let anonymous = map(-1, libc::MAP_SHARED | libc::MAP_ANONYMOUS, prot);
        let reopen = |address: usize| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(
                    "/proc/self/map_files/{address:x}-{:x}",
                    address + 4096
                ))
        };
        let zero_file = match reopen(zero) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                eprintln!(
                    "skipped: opening /proc/self/map_files needs CAP_SYS_ADMIN or \
                     CAP_CHECKPOINT_RESTORE in the initial user namespace"
                );
                return;
            }
            Err(error) => panic!("{error}"),
        };
        let anonymous_file = reopen(anonymous).unwrap();
        let tid = std::process::id() as i32;
        let capture = |file: &std::fs::File| {
            shared_mapping(MapFlags::MAP_SHARED, tid, file.as_raw_fd(), 0).unwrap()
        };
        let named = name_zero_mapping(tid, zero, libc::O_RDWR);
        let object = (named.device, named.inode);
        let metadata = zero_file.metadata().unwrap();
        assert_eq!((metadata.dev(), metadata.ino()), object);
        assert_eq!(
            capture(&zero_file),
            Some(Sharing::File(SharedMapping {
                device: object.0,
                inode: object.1,
                offset: 0,
                access: libc::O_RDWR,
            }))
        );
        match capture(&anonymous_file) {
            Some(Sharing::Refused(reason)) => {
                assert!(reason.contains("MAP_SHARED | MAP_ANONYMOUS"), "{reason}")
            }
            other => panic!("a reopened anonymous object was not refused: {other:?}"),
        }
    }

    fn facts(mode: u32, rdev: u64, device: u64, fs_type: i64, target: &str) -> MappedFileFacts {
        MappedFileFacts {
            mode,
            rdev,
            device,
            inode: 77,
            fs_type,
            target: target.into(),
        }
    }

    /// The kind of a mapped file is its type, file system and, for a
    /// character device, its number. A file an anonymous inode stands for is
    /// never a regular file whatever type it reports, and a `/dev/zero`
    /// object on the shmem mount is one only when its mapping was recorded.
    #[test]
    fn a_mapped_files_kind_is_its_type_and_device_number() {
        const TMPFS: i64 = 0x0102_1994;
        let shmem = Some((0, 1));
        let shmem_dev = libc::makedev(0, 1);
        let kind = |file: &MappedFileFacts, recorded: bool| mapped_file(file, shmem, |_| recorded);
        let other = |file: MappedFileFacts| match kind(&file, false) {
            MappedFile::Other(what) => what,
            regular => panic!("{file:?} is {regular:?}"),
        };
        let regular = libc::S_IFREG | 0o600;
        let memfd = facts(regular, 0, shmem_dev, TMPFS, "/memfd:x (deleted)");
        assert_eq!(kind(&memfd, false), MappedFile::Regular);
        let object = facts(regular, 0, shmem_dev, TMPFS, ZERO_OBJECT_NAME);
        assert_eq!(kind(&object, true), MappedFile::Regular);
        assert!(other(object).contains("MAP_SHARED | MAP_ANONYMOUS"));
        // A file of that name elsewhere is an ordinary file.
        let elsewhere = facts(regular, 0, libc::makedev(8, 1), TMPFS, ZERO_OBJECT_NAME);
        assert_eq!(kind(&elsewhere, false), MappedFile::Regular);
        let node = |rdev, target| facts(libc::S_IFCHR | 0o666, rdev, 6, TMPFS, target);
        assert_eq!(
            kind(&node(libc::makedev(1, 5), "/dev/zero"), false),
            MappedFile::Zero
        );
        assert_eq!(
            kind(&node(libc::makedev(1, 5), "/x/zero-node"), false),
            MappedFile::Zero
        );
        assert!(other(node(libc::makedev(1, 1), "/dev/mem")).contains("character device 1:1"));
        assert!(other(node(libc::makedev(29, 0), "/dev/fb0")).contains("character device 29:0"));
        assert!(other(facts(regular, 0, 9, 0, "anon_inode:[eventfd]")).contains("anonymous inode"));
        assert!(other(facts(0o600, 0, 9, 0, "anon_inode:[eventfd]")).contains("anonymous inode"));
        assert!(other(facts(0o600, 0, 9, 0, "/dmabuf:")).contains("no file type"));
        assert!(
            other(facts(
                libc::S_IFBLK | 0o660,
                libc::makedev(8, 0),
                6,
                TMPFS,
                "/dev/sda"
            ))
            .contains("block")
        );
        assert!(
            other(facts(libc::S_IFSOCK | 0o777, 0, 8, 0, "socket:[7]")).contains("type 140000")
        );
        for (magic, name) in OWN_MMAP_FILE_SYSTEMS {
            let file = facts(regular, 0, 20, magic, "/sys/bus/pci/devices/x/resource0");
            assert!(other(file).contains(&format!("a file on {name}")));
        }
    }

    /// A shared mapping of any other file that is not a regular one, or of a
    /// regular file on a file system whose files can map memory of their
    /// own, refuses replay by name: a character device's driver maps memory
    /// of its own, and the files an anonymous inode stands for share it, so
    /// no identity could make replay share what Linux shared.
    #[test]
    fn shared_mappings_of_other_files_refuse_replay() {
        let tid = std::process::id() as i32;
        let null = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: eventfd has no memory-safety preconditions; the result is
        // checked and owned below.
        let eventfd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(eventfd >= 0, "{}", io::Error::last_os_error());
        // SAFETY: eventfd returned a new descriptor that nothing else owns.
        let eventfd = unsafe { OwnedFd::from_raw_fd(eventfd) };
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let status = std::fs::File::open("/proc/self/status").unwrap();
        for (fd, what) in [
            (null.as_raw_fd(), "a character device 1:3"),
            (eventfd.as_raw_fd(), "an anonymous inode"),
            (socket.as_raw_fd(), "a file of type 140000"),
            (status.as_raw_fd(), "a file on procfs"),
        ] {
            let capture = shared_mapping_with(MapFlags::MAP_SHARED, tid, fd, 0, |_| {
                panic!("a mapping replay refuses must not read fdinfo")
            });
            match captured_mmap_result(Ok(0x1000), capture) {
                Ok((0x1000, Some(Sharing::Refused(reason)))) => assert!(
                    reason.starts_with("a MAP_SHARED mapping of ") && reason.contains(what),
                    "{reason}"
                ),
                other => panic!("{what} was not refused: {other:?}"),
            }
        }
    }

    #[test]
    fn shared_mmap_capture_reports_missing_identity_and_invalid_access() {
        let tid = std::process::id() as i32;
        let missing = shared_mapping_with(MapFlags::MAP_SHARED, tid, -1, 0, |_| {
            panic!("a missing descriptor must fail before fdinfo is read")
        });
        assert_eq!(missing.unwrap_err().raw_os_error(), Some(libc::ENOENT));
        let file = tempfile::tempfile().unwrap();
        for fdinfo in ["pos:\t0\n", "flags:\tinvalid\n"] {
            let capture =
                shared_mapping_with(MapFlags::MAP_SHARED, tid, file.as_raw_fd(), 0, |_| {
                    Ok(fdinfo.to_owned())
                });
            assert_eq!(capture.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }

    /// fdinfo's `flags:` line is octal; its `O_ACCMODE` bits are the access
    /// mode, whatever else (`O_CLOEXEC`, `O_LARGEFILE`) is set.
    #[test]
    fn a_descriptors_access_mode_is_read_from_its_fdinfo() {
        let fdinfo = |flags: &str| format!("pos:\t0\nflags:\t{flags}\nmnt_id:\t25\nino:\t7\n");
        assert_eq!(descriptor_access(&fdinfo("02100002")), Some(libc::O_RDWR));
        assert_eq!(descriptor_access(&fdinfo("0100000")), Some(libc::O_RDONLY));
        assert_eq!(descriptor_access(&fdinfo("02000001")), Some(libc::O_WRONLY));
        assert_eq!(descriptor_access("pos:\t0\n"), None);
    }

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
