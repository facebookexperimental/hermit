/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `/proc` names outside the run's `--proc` level do not exist, on every
//! backend (see [`crate::procfs::levels`]).
//!
//! A call that looks one up is answered by the kernel for a name procfs does
//! not have: Detcore issues the same call with the path replaced by an
//! absolute one that ends where the omitted name is, with that name replaced
//! by as many dashes, a name no procfs directory holds. Every argument the
//! kernel checks first, the order in which it looks names up, and how it
//! compares mounts are then Linux's own, and the call fails as Linux fails it
//! where the name is missing. The directory the substituted name is in is
//! checked to be procfs, which creates nothing, so such a call cannot
//! succeed or change anything.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::syscalls;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MapFlags;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ProtFlags;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use super::rendered_procfs;
use crate::procfs::levels::ProcfsName;
use crate::procfs::levels::procfs_name;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// The longest path the kernel accepts, with its NUL (`PATH_MAX`).
const PATH_MAX: usize = libc::PATH_MAX as usize;
/// The longest name a path component may have (`NAME_MAX`).
const NAME_MAX: usize = 255;
/// The most symbolic links one resolution follows (`MAXSYMLINKS`).
const MAXSYMLINKS: usize = 40;
/// The size of `struct msghdr`, whose first two fields are `msg_name` and
/// `msg_namelen`.
const MSGHDR_SIZE: usize = std::mem::size_of::<reverie::syscalls::linux_abi::Msghdr>();
/// The longest pathname an `AF_UNIX` address holds, with its NUL.
const SUN_PATH_MAX: usize = 108;

/// What a call names that a lookup may find, by argument position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Lookups {
    /// Paths: each the position of its directory descriptor (`None` for
    /// `AT_FDCWD`) and of its path pointer.
    Paths(Vec<(Option<usize>, usize)>),
    /// A socket address and its length, at these positions, for the socket
    /// in argument 0. A pathname `AF_UNIX` address is looked up. Only a
    /// datagram socket looks up the address a send gives (a stream socket
    /// refuses it, a sequenced-packet socket ignores it).
    SocketAddress {
        address: usize,
        length: usize,
        send: bool,
    },
    /// The address in the `msghdr` in argument 1, for `sendmsg`.
    MessageAddress,
}

/// What `sysno` looks up.
///
/// Calls Detcore refuses whatever their path names (`mount` and its family,
/// `pivot_root`, `swapon`, `quotactl`, `acct`, `uselib`, `fanotify_mark`,
/// `name_to_handle_at`; see `classify_syscall`) keep that refusal. Calls
/// newer than the pinned `syscalls` crate (`setxattrat` and its siblings,
/// `file_getattr`, `open_tree_attr`) are unknown to Detcore and refused as
/// such.
pub(super) fn looked_up(sysno: Sysno) -> Option<Lookups> {
    if let Some(paths) = path_operands(sysno) {
        return Some(Lookups::Paths(paths.to_vec()));
    }
    socket_lookups(sysno)
}

/// What a socket call numbered `sysno` looks up: the address it binds,
/// connects or sends to ([`looked_up`] for a call with no path operands).
fn socket_lookups(sysno: Sysno) -> Option<Lookups> {
    match sysno {
        Sysno::bind | Sysno::connect => Some(Lookups::SocketAddress {
            address: 1,
            length: 2,
            send: false,
        }),
        Sysno::sendto => Some(Lookups::SocketAddress {
            address: 4,
            length: 5,
            send: true,
        }),
        Sysno::sendmsg => Some(Lookups::MessageAddress),
        _ => None,
    }
}

/// Whether a call numbered `sysno` looks anything up ([`looked_up`] is
/// `Some`), without allocating: the calls
/// `Detcore::refuse_procfs_name_outside_level` can act on. Part of
/// `may_name_file_or_descriptor`.
pub(crate) fn looks_up_names(sysno: Sysno) -> bool {
    path_operands(sysno).is_some() || socket_lookups(sysno).is_some()
}

/// Take the device of the guest's procfs mount now, if it was not taken
/// yet, as `Detcore::refuse_procfs_name_outside_level` does first for every
/// call: it must be taken before the guest's first call can change its root,
/// so a call that skips the check takes it here.
pub(crate) fn take_proc_device() {
    let _ = rendered_procfs::proc_device();
}

/// Whether `sysno` opens a new descriptor for its path.
fn creates_descriptor(sysno: Sysno) -> bool {
    match sysno {
        Sysno::openat => true,
        #[cfg(target_arch = "x86_64")]
        Sysno::open | Sysno::creat => true,
        _ => false,
    }
}

/// The path operands of a call that looks up paths ([`Lookups::Paths`] of
/// [`looked_up`]), without allocating; `None` for every other call.
pub(super) fn path_operands(sysno: Sysno) -> Option<&'static [(Option<usize>, usize)]> {
    let paths: &'static [(Option<usize>, usize)] = match sysno {
        Sysno::execve
        | Sysno::chdir
        | Sysno::chroot
        | Sysno::truncate
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::removexattr
        | Sysno::lremovexattr
        | Sysno::statfs => &[(None, 0)],
        #[cfg(target_arch = "x86_64")]
        Sysno::open
        | Sysno::creat
        | Sysno::stat
        | Sysno::lstat
        | Sysno::access
        | Sysno::readlink
        | Sysno::mkdir
        | Sysno::mknod
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::utime
        | Sysno::utimes => &[(None, 0)],
        Sysno::openat
        | Sysno::statx
        | Sysno::faccessat
        | Sysno::faccessat2
        | Sysno::readlinkat
        | Sysno::execveat
        | Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::unlinkat
        | Sysno::fchmodat
        | Sysno::fchmodat2
        | Sysno::fchownat
        | Sysno::utimensat => &[(Some(0), 1)],
        #[cfg(target_arch = "x86_64")]
        Sysno::newfstatat | Sysno::futimesat => &[(Some(0), 1)],
        #[cfg(target_arch = "aarch64")]
        Sysno::fstatat => &[(Some(0), 1)],
        #[cfg(target_arch = "x86_64")]
        Sysno::symlink => &[(None, 1)],
        Sysno::symlinkat => &[(Some(1), 2)],
        Sysno::inotify_add_watch => &[(None, 1)],
        #[cfg(target_arch = "x86_64")]
        Sysno::rename | Sysno::link => &[(None, 0), (None, 1)],
        Sysno::renameat | Sysno::renameat2 | Sysno::linkat => &[(Some(0), 1), (Some(2), 3)],
        _ => return None,
    };
    Some(paths)
}

/// The prefix of the absolute `path` that ends at the first component the
/// lookup finds absent at `level`, walking its normal components from the
/// left, or `None` where it finds none absent. A component the table leaves
/// undecided (a process's or a thread's directory, an entry of `fd`) decides
/// nothing but does not stop the walk: the names in a process's directory
/// are decided whichever link (`self`, `thread-self`) reached it, and below
/// a link out of `/proc` (`fd/N`, `cwd`, `root`, `exe`) every name stays
/// undecided. The walk stops, deciding nothing, at a `..` (the kernel's to
/// resolve, through any link before it), and where a component it reaches,
/// the absent one included, is longer than `NAME_MAX` (the kernel's
/// ENAMETOOLONG comes first).
pub(super) fn absent_prefix(
    path: &Path,
    level: detcore_model::config::ProcLevel,
) -> Option<PathBuf> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return None;
    }
    let mut prefix = PathBuf::from("/");
    for component in components {
        let name = match component {
            Component::Normal(name) => name,
            Component::CurDir => continue,
            _ => return None,
        };
        if name.len() > NAME_MAX {
            return None;
        }
        prefix.push(name);
        if procfs_name(&prefix).is_some_and(|name| name.absent_at(level)) {
            return Some(prefix);
        }
    }
    None
}

/// The spelling the kernel is given in place of `named`, whose component
/// ending `prefix` names nothing: `parent` (`prefix`'s directory, in the
/// guest's view), then that component replaced by as many dashes, then the
/// rest of `named`. What follows the omitted name stays: the kernel's walk
/// ends at it, before them, but which name is the last decides how a call
/// treats it (rename looks up the last one's directory first). So does a
/// trailing slash in the guest's spelling `bytes` (O_CREAT then fails with
/// EISDIR).
pub(super) fn dashed_spelling(
    parent: &Path,
    prefix: &Path,
    named: &Path,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    let name_length = prefix.file_name()?.len();
    let mut substituted = parent.as_os_str().as_bytes().to_vec();
    if !substituted.ends_with(b"/") {
        substituted.push(b'/');
    }
    substituted.resize(substituted.len() + name_length, b'-');
    let rest = named.strip_prefix(prefix).ok()?;
    if !rest.as_os_str().is_empty() {
        substituted.push(b'/');
        substituted.extend_from_slice(rest.as_os_str().as_bytes());
    }
    if bytes.ends_with(b"/") {
        substituted.push(b'/');
    }
    Some(substituted)
}

/// Whether the directory `path`, in the tool's view, is on sysfs: not a
/// user bind or another file system mounted over part of `/sys`, whose
/// `cpuN` and `nodeN` names are the guest's own.
pub(super) fn is_on_sysfs(path: &Path) -> bool {
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buffer = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `buffer` has room for a `statfs`,
    // which the call fills when it returns 0.
    if unsafe { libc::statfs(path.as_ptr(), buffer.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: filled by the successful call above.
    let buffer = unsafe { buffer.assume_init() };
    buffer.f_type as u64 == libc::SYSFS_MAGIC as u64
}

/// The pathname in an `AF_UNIX` socket address, as its bytes; `None` for
/// any other address: another family, an unnamed or abstract address, or one
/// the kernel refuses before it looks anything up (too short or too long).
fn unix_address_path(address: &[u8]) -> Option<&[u8]> {
    let family_length = std::mem::size_of::<libc::sa_family_t>();
    if address.len() <= family_length
        || address.len() > std::mem::size_of::<libc::sockaddr_un>()
        || libc::sa_family_t::from_ne_bytes(address[..family_length].try_into().ok()?)
            != libc::AF_UNIX as libc::sa_family_t
    {
        return None;
    }
    let path = &address[family_length..];
    if path[0] == 0 {
        return None;
    }
    let end = path
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(path.len());
    Some(&path[..end])
}

/// An `AF_UNIX` address holding `path`, NUL-terminated, or `None` if it does
/// not fit.
fn unix_address(path: &[u8]) -> Option<Vec<u8>> {
    if path.len() + 1 > SUN_PATH_MAX {
        return None;
    }
    let mut address = (libc::AF_UNIX as libc::sa_family_t).to_ne_bytes().to_vec();
    address.extend_from_slice(path);
    address.push(0);
    Some(address)
}

/// `path`, absolute in the tool's view, with its symbolic links resolved as
/// the kernel resolves them, and `.` and `..` folded in. Inside `/proc` only
/// the links into a process's directory are followed (`self`,
/// `thread-self`, `net`, `mounts`), to the directory of an arbitrary
/// process: the table decides nothing below one. Every other procfs link is
/// per-process and left unresolved, so the result there decides nothing
/// either. `None` after `MAXSYMLINKS` links.
fn resolve_links(path: &Path) -> Option<PathBuf> {
    let mut pending = path.components().map(owned).collect::<VecDeque<_>>();
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        match component {
            Owned::Root => resolved = PathBuf::from("/"),
            Owned::Current => {}
            Owned::Parent => {
                resolved.pop();
            }
            Owned::Name(name) => {
                let next = resolved.join(&name);
                if next.starts_with("/proc") {
                    resolved = match next.to_str() {
                        Some("/proc/self") => PathBuf::from("/proc/1"),
                        Some("/proc/thread-self") => PathBuf::from("/proc/1/task/1"),
                        Some("/proc/net") => PathBuf::from("/proc/1/net"),
                        Some("/proc/mounts") => PathBuf::from("/proc/1/mounts"),
                        _ => next,
                    };
                    continue;
                }
                match std::fs::read_link(&next) {
                    Ok(target) => {
                        links += 1;
                        if links > MAXSYMLINKS {
                            return None;
                        }
                        for component in target.components().rev() {
                            pending.push_front(owned(component));
                        }
                    }
                    Err(_) => resolved = next,
                }
            }
        }
    }
    Some(resolved)
}

/// Whether a call numbered `sysno` is an access probe: `access`, `faccessat`
/// or `faccessat2` (`Detcore::handle_access_probe`).
pub(crate) fn is_access_probe(sysno: Sysno) -> bool {
    #[cfg(target_arch = "x86_64")]
    if sysno == Sysno::access {
        return true;
    }
    matches!(sysno, Sysno::faccessat | Sysno::faccessat2)
}

/// Whether a lookup of `spelling` follows a symbolic link that its last name
/// names, whatever the call's flags: a spelling that ends in `/`, or whose
/// last name is `.` or `..`, which Linux resolves through what comes before.
fn follows_last_name(spelling: &Path) -> bool {
    let bytes = spelling.as_os_str().as_bytes();
    bytes.ends_with(b"/")
        || bytes.rsplit(|byte| *byte == b'/').next() == Some(b".")
        || bytes.rsplit(|byte| *byte == b'/').next() == Some(b"..")
}

/// The kernel's path for what `named`, an absolute path in the tool's view,
/// reaches, where that is on the guest's sysfs mount
/// ([`rendered_procfs::sys_device`]): with every symbolic link on the way
/// resolved as the kernel resolves them, the one its last name names only
/// where `follow`, and otherwise that name kept as it is, below its resolved
/// directory. Where `named` reaches nothing, the longest prefix of it that
/// reaches something, so resolved, with the rest of its names after it, where
/// that is on sysfs: the deepest object a lookup that failed below it can
/// have reached. `None` elsewhere.
fn resolved_on_sysfs(named: &Path, follow: bool) -> Option<PathBuf> {
    let sysfs = rendered_procfs::sys_device()?;
    let on_sysfs =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.dev() == sysfs);
    let whole = if follow {
        crate::util::canonicalize(named).ok()
    } else {
        named
            .file_name()
            .zip(named.parent())
            .and_then(|(name, directory)| {
                Some(crate::util::canonicalize(directory).ok()?.join(name))
            })
    };
    if let Some(whole) = whole
        && std::fs::symlink_metadata(&whole).is_ok()
    {
        return on_sysfs(&whole).then_some(whole);
    }
    for prefix in named.ancestors().skip(1) {
        if let Ok(resolved) = crate::util::canonicalize(prefix) {
            let rest = named.strip_prefix(prefix).ok()?;
            return on_sysfs(&resolved).then(|| resolved.join(rest));
        }
    }
    None
}

/// Whether Linux reports the failure `errno` of a call that looks up a path
/// only once its lookup has found an object, or has gone through one: a
/// refusal for permission or of a write, a name that is no directory or a
/// link too many, and the like. Not a failure Linux reports before its lookup
/// (`EINVAL`, `EFAULT`, `EBADF`, `EMFILE`, `ENAMETOOLONG`), nor `ENOENT`,
/// which is the answer for an absent object already.
fn fails_after_lookup(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::EACCES
            | Errno::EPERM
            | Errno::EROFS
            | Errno::ENOTDIR
            | Errno::ELOOP
            | Errno::EISDIR
            | Errno::ETXTBSY
            | Errno::ENXIO
            | Errno::ENODEV
            | Errno::EOPNOTSUPP
            | Errno::EEXIST
            | Errno::EBUSY
            | Errno::EOVERFLOW
            | Errno::EFBIG
            | Errno::EAGAIN
    )
}

/// A path a call looked up, as the guest spelled it: relative to `dirfd`,
/// following a symbolic link its last name names where `follow`.
pub(crate) struct LookedUpPath {
    pub(crate) dirfd: i32,
    pub(crate) spelling: PathBuf,
    pub(crate) follow: bool,
}

/// A path component, owned.
enum Owned {
    Root,
    Current,
    Parent,
    Name(std::ffi::OsString),
}

fn owned(component: Component<'_>) -> Owned {
    match component {
        Component::RootDir | Component::Prefix(_) => Owned::Root,
        Component::CurDir => Owned::Current,
        Component::ParentDir => Owned::Parent,
        Component::Normal(name) => Owned::Name(name.to_owned()),
    }
}

/// A replacement for one argument: bytes for a transient page, and the
/// argument that takes their address (`None` for the name a copied
/// `msghdr`, the replacement before it, points to).
struct Replacement {
    argument: Option<usize>,
    bytes: Vec<u8>,
}

/// A transient anonymous page of at least `length` bytes in the guest, or
/// `None` if the guest's address space has no room.
pub(crate) async fn map_transient<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    length: usize,
) -> Option<AddrMut<'static, u8>> {
    guest
        .inject_with_retry(Syscall::Mmap(
            syscalls::Mmap::new()
                .with_addr(None)
                .with_len(length)
                .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                .with_fd(-1)
                .with_offset(0),
        ))
        .await
        .ok()
        .and_then(|mapped| usize::try_from(mapped).ok())
        .and_then(AddrMut::<u8>::from_raw)
}

/// Unmap a page from [`map_transient`]: a mapping left behind would be
/// visible to the guest.
pub(crate) async fn unmap_transient<T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
    page: AddrMut<'_, u8>,
    length: usize,
) {
    let unmapped = guest
        .inject_with_retry(Syscall::Munmap(
            syscalls::Munmap::new()
                .with_addr(Some(page.cast::<libc::c_void>().into()))
                .with_len(length),
        ))
        .await;
    if let Err(errno) = unmapped {
        tracing::warn!("[detcore] could not unmap the transient procfs page: {errno}");
    }
}

/// Raw syscall arguments from six values.
pub(super) fn raw_args(values: [usize; 6]) -> syscalls::SyscallArgs {
    syscalls::SyscallArgs::new(
        values[0], values[1], values[2], values[3], values[4], values[5],
    )
}

/// What Detcore decided about one path a call looks up.
#[derive(Debug, Default)]
struct Decision {
    /// The absolute path, in the guest's view, to give the kernel instead:
    /// up to the omitted name, which is replaced by dashes.
    substituted: Option<Vec<u8>>,
    /// The path is a procfs name the table decides (not one it leaves to the
    /// kernel, such as a process's directory or an entry of `fd`).
    decided: bool,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The kernel's answer for a call that looks up a `/proc` name outside
    /// the run's level (see the module documentation), as the guest spelled
    /// it: absolute, relative to its working directory, or relative to a
    /// directory descriptor Detcore knows the path of. A spelling Detcore
    /// cannot place, or one with `..` or a link out of `/proc` before the
    /// omitted one, is the kernel's to resolve; an open that so reaches such
    /// a name is refused once the kernel names what it opened, or, if it
    /// refused the open, once Detcore resolves the spelling (see
    /// `handle_openat`).
    pub(crate) async fn refuse_procfs_name_outside_level<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Option<i64>, Error> {
        // Taken before the guest's first call can change its root.
        take_proc_device();
        let (sysno, args) = call.into_parts();
        let Some(lookups) = looked_up(sysno) else {
            return Ok(None);
        };
        let Some(root) = self.guest_root_in_view(guest) else {
            return Ok(None);
        };
        let mut values = [
            args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
        ];
        // A relative spelling below a descriptor Detcore has no recorded path
        // for (one received over `SCM_RIGHTS`) is judged as the name the kernel will
        // look up: the directory the descriptor is open on, joined with it
        // (`Detcore::unrecorded_procfs_directory`). Otherwise a received
        // `/proc` descriptor would reach a name the level omits. Recovered
        // first, so that the task numbers the recovered directory names are
        // translated with the rest (DBT, `parent_numbers`): a received
        // `/proc/self` directory names its task by the guest's number.
        let mut recovered = Vec::new();
        if let Lookups::Paths(operands) = &lookups {
            for (dirfd, path) in operands {
                let spelling = PathPtr::from_ptr(values[*path] as *const libc::c_char)
                    .and_then(|address| address.read(&guest.memory()).ok());
                let directory = match (dirfd, &spelling) {
                    (Some(position), Some(spelling)) if !spelling.is_absolute() => {
                        self.unrecorded_procfs_directory(guest, values[*position] as i32)
                            .await
                    }
                    _ => None,
                };
                recovered.push(directory);
            }
        }
        let recovered_paths = match &lookups {
            Lookups::Paths(operands) => operands
                .iter()
                .zip(&recovered)
                .filter_map(|((_, path), directory)| {
                    let spelling = PathPtr::from_ptr(values[*path] as *const libc::c_char)
                        .and_then(|address| address.read(&guest.memory()).ok())?;
                    Some(directory.as_ref()?.join(spelling))
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let numbers = self
            .parent_numbers(guest, &lookups, &values, &recovered_paths)
            .await;
        // A spelling relative to the working directory is decided from the
        // guest's working directory, which the tool may first have to read
        // from the guest (`Detcore::prepare_guest_cwd`: KVM), here, before
        // the decisions, which do not await.
        if !crate::procfs_inode::tool_can_read_guest_cwd(&self.cfg.backend) {
            for (dirfd, spelling) in Self::looked_up_spellings(guest, &lookups, &values) {
                Self::prepare_guest_cwd(guest, dirfd, &spelling).await;
            }
        }
        // Whether a refusal for permission can only be the KVM executor's.
        let mut confined = true;
        let replacements = match lookups {
            Lookups::Paths(operands) => {
                let mut replacements = Vec::new();
                for (index, (dirfd, path)) in operands.into_iter().enumerate() {
                    let decision = match PathPtr::from_ptr(values[path] as *const libc::c_char)
                        .and_then(|address| address.read(&guest.memory()).ok())
                    {
                        Some(spelling) => match recovered.get(index).cloned().flatten() {
                            Some(directory) => self.decide(
                                guest,
                                None,
                                &directory.join(&spelling),
                                &root,
                                numbers.as_ref(),
                            ),
                            None => self.decide(
                                guest,
                                dirfd.map(|p| values[p] as i32),
                                &spelling,
                                &root,
                                numbers.as_ref(),
                            ),
                        },
                        None => Decision::default(),
                    };
                    confined &= decision.decided;
                    if let Some(bytes) = decision.substituted {
                        replacements.push(Replacement {
                            argument: Some(path),
                            bytes,
                        });
                    }
                }
                replacements
            }
            Lookups::SocketAddress {
                address,
                length,
                send,
            } => {
                // The length is an int.
                let Ok(length_value) = usize::try_from(values[length] as u32 as i32) else {
                    return Ok(None);
                };
                let Some(bytes) = self.substituted_socket_address(
                    guest,
                    values[address] as u64,
                    length_value,
                    send,
                    &root,
                    numbers.as_ref(),
                ) else {
                    return Ok(None);
                };
                if send {
                    match self.socket_is_datagram(guest, values[0] as RawFd).await {
                        Some(false) => return Ok(None),
                        Some(true) => {}
                        // No room for the question: refused without asking.
                        None => return Err(syscalls::Errno::ENOENT.into()),
                    }
                }
                values[length] = bytes.len();
                confined = false;
                vec![Replacement {
                    argument: Some(address),
                    bytes,
                }]
            }
            Lookups::MessageAddress => {
                // `msg_name` and `msg_namelen` begin `struct msghdr`.
                let mut header = vec![0u8; MSGHDR_SIZE];
                let Some(start) = Addr::<u8>::from_raw(values[1]) else {
                    return Ok(None);
                };
                if guest.memory().read_exact(start, &mut header).is_err() {
                    return Ok(None);
                }
                let name = u64::from_ne_bytes(header[..8].try_into().expect("8 bytes"));
                let length = u32::from_ne_bytes(header[8..12].try_into().expect("4 bytes"));
                let bytes = (name != 0)
                    .then(|| {
                        self.substituted_socket_address(
                            guest,
                            name,
                            length as usize,
                            true,
                            &root,
                            numbers.as_ref(),
                        )
                    })
                    .flatten();
                let Some(bytes) = bytes else {
                    return Ok(None);
                };
                match self.socket_is_datagram(guest, values[0] as RawFd).await {
                    Some(false) => return Ok(None),
                    Some(true) => {}
                    None => return Err(syscalls::Errno::ENOENT.into()),
                }
                header[8..12].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
                confined = false;
                // The header is copied; its `msg_name` takes the address of
                // the substituted name after it.
                vec![
                    Replacement {
                        argument: Some(1),
                        bytes: header,
                    },
                    Replacement {
                        argument: None,
                        bytes,
                    },
                ]
            }
        };
        if replacements.is_empty() {
            return Ok(None);
        }
        self.issue_substituted(guest, sysno, values, replacements, confined)
            .await
            .map(Some)
    }

    /// The kernel's number for each guest process or thread number in the
    /// paths `lookups` names, as [`Self::decide`] places them (DBT;
    /// `procfs_numbers`), and in `recovered`, the absolute spellings of
    /// relative names below received procfs directories
    /// (`Detcore::unrecorded_procfs_directory`): the tool looks at an omitted
    /// name's directory by them ([`Self::parent_in_tool_view`]). `None` where
    /// numbers are not translated.
    async fn parent_numbers<G: Guest<Self>>(
        &self,
        guest: &mut G,
        lookups: &Lookups,
        values: &[usize; 6],
        recovered: &[PathBuf],
    ) -> Option<BTreeMap<i32, Option<i32>>> {
        if !Self::translates_procfs_numbers(guest) {
            return None;
        }
        let mut spellings = recovered
            .iter()
            .map(|path| (libc::AT_FDCWD, path.clone()))
            .collect::<Vec<_>>();
        spellings.extend(Self::looked_up_spellings(guest, lookups, values));
        let named = spellings
            .iter()
            .filter_map(|(dirfd, spelling)| Self::observed_path(guest, *dirfd, spelling).ok())
            .flat_map(|path| crate::procfs::ids::named_ids(&path))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if named.is_empty() {
            return Some(BTreeMap::new());
        }
        let kernel = crate::tool_global::procfs_ids(guest, true, named.clone()).await;
        Some(named.into_iter().zip(kernel).collect())
    }

    /// The spellings `lookups` names in a call's argument `values`, each with
    /// the directory descriptor it is looked up from (`AT_FDCWD` for the
    /// working directory): the path operands, or the pathname a socket
    /// address names.
    fn looked_up_spellings<G: Guest<Self>>(
        guest: &G,
        lookups: &Lookups,
        values: &[usize; 6],
    ) -> Vec<(i32, PathBuf)> {
        let mut spellings = Vec::new();
        match lookups {
            Lookups::Paths(operands) => {
                for (dirfd, path) in operands {
                    if let Some(spelling) = PathPtr::from_ptr(values[*path] as *const libc::c_char)
                        .and_then(|address| address.read(&guest.memory()).ok())
                    {
                        let dirfd =
                            dirfd.map_or(libc::AT_FDCWD, |position| values[position] as i32);
                        spellings.push((dirfd, spelling));
                    }
                }
            }
            Lookups::SocketAddress {
                address, length, ..
            } => {
                spellings.extend(
                    socket_address_path(guest, values[*address], values[*length] as u32 as usize)
                        .map(|path| (libc::AT_FDCWD, path)),
                );
            }
            Lookups::MessageAddress => {
                // `msg_name` and `msg_namelen` begin `struct msghdr`.
                let mut header = [0u8; 12];
                if let Some(start) = Addr::<u8>::from_raw(values[1])
                    && guest.memory().read_exact(start, &mut header).is_ok()
                {
                    let name = u64::from_ne_bytes(header[..8].try_into().expect("8 bytes"));
                    let length = u32::from_ne_bytes(header[8..12].try_into().expect("4 bytes"));
                    spellings.extend(
                        socket_address_path(guest, name as usize, length as usize)
                            .map(|path| (libc::AT_FDCWD, path)),
                    );
                }
            }
        }
        spellings
    }

    /// What Detcore decides about `spelling`, relative to `dirfd` (`None`
    /// for the working directory). It decides nothing for an empty or
    /// overlong spelling, one it cannot place (a working directory outside
    /// the root, a directory descriptor whose path it does not know, or, after
    /// a `chroot`, any directory descriptor: its recorded path may be in the
    /// old root's view), and one whose omitted name is not in a procfs
    /// directory (a user bind over part of /proc).
    fn decide<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: Option<i32>,
        spelling: &Path,
        root: &Option<PathBuf>,
        numbers: Option<&BTreeMap<i32, Option<i32>>>,
    ) -> Decision {
        let bytes = spelling.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.len() >= PATH_MAX {
            return Decision::default();
        }
        let dirfd = dirfd.unwrap_or(libc::AT_FDCWD);
        if root.is_some() && dirfd != libc::AT_FDCWD && !spelling.is_absolute() {
            return Decision::default();
        }
        let Some(observed) = Self::observed_path(guest, dirfd, spelling)
            .ok()
            .filter(|observed| observed.is_absolute())
        else {
            return Decision::default();
        };
        // The path Linux's walk reaches: through the caller's descriptor
        // links to the `/proc` directories Detcore serves on placeholders,
        // and back through `..` over the directories every procfs has, so
        // that no spelling a placeholder's respelling or a `..` makes of an
        // omitted name reaches it (`descriptor_directory`). Not from a
        // descriptor whose recorded path is a descriptor link's spelling,
        // which need not name the file it is open on
        // (`Detcore::starts_at_a_recorded_link`).
        let observed = if Self::starts_at_a_recorded_link(guest, dirfd, spelling) {
            observed
        } else {
            let observed = match Self::decided_path(guest, &observed) {
                Ok((decided, _)) => decided,
                Err(_) => observed,
            };
            // A task the spelling names on the way that does not exist ends
            // the kernel's walk with ENOENT, as every name this refuses does.
            super::descriptor_directory::resolved_procfs_dotdot(&observed)
                .map_or(observed, |(resolved, _)| resolved)
        };
        let in_view = match root {
            None => observed.clone(),
            Some(root) => root.join(observed.strip_prefix("/").unwrap_or(&observed)),
        };
        // The path the table judges, with procfs at `/proc`: the tool's view
        // of the guest's path (also after a `chroot` into `/proc`), or, after
        // a `chroot` into another root, the guest's own path, whose `/proc` is
        // that root's (checked below to be procfs).
        let guest_view = root.is_some() && procfs_name(&in_view).is_none();
        let named = if guest_view { &observed } else { &in_view };
        let to_tool = |path: &Path| match root {
            Some(root) if guest_view => root.join(path.strip_prefix("/").unwrap_or(path)),
            _ => path.to_path_buf(),
        };
        let decided = procfs_name(named).is_some_and(|name| name != ProcfsName::Undecided);
        let substituted = absent_prefix(named, self.cfg.proc_level).and_then(|prefix| {
            let parent = prefix.parent()?;
            let checked = self.parent_in_tool_view(guest, &to_tool(parent), root, numbers);
            let on_procfs = std::fs::metadata(&checked)
                .ok()
                .is_some_and(|metadata| Some(metadata.dev()) == rendered_procfs::proc_device());
            if !on_procfs {
                return None;
            }
            // Back in the guest's view.
            let parent = match root {
                Some(root) if !guest_view => Path::new("/").join(parent.strip_prefix(root).ok()?),
                _ => parent.to_path_buf(),
            };
            dashed_spelling(&parent, &prefix, named, bytes)
        });
        // A CPU or node that the virtual CPUs and single node leave out, below
        // the sysfs directories that list them
        // (`crate::procfs::absent_cpu_entry_prefix`), as the guest named it
        // before any `chroot`: the listings leave it out, so a lookup finds
        // nothing either. With native metadata or under record and replay,
        // the listings keep every entry (`Detcore::drop_absent_entries`).
        let substituted = substituted.or_else(|| {
            if !self.cfg.virtualize_metadata || root.is_some() || self.cfg.recordreplay_modes {
                return None;
            }
            let prefix =
                crate::procfs::absent_cpu_entry_prefix(&observed, self.cfg.virtual_cpu_count())?;
            let parent = prefix.parent()?;
            if !is_on_sysfs(parent) {
                return None;
            }
            dashed_spelling(parent, &prefix, &observed, bytes)
        });
        // A device's `numa_node` that the kernel without NUMA does not
        // register (`crate::procfs::is_absent_device_attribute`), by whatever
        // spelling reaches the device: the listings leave it out too, under
        // the same conditions.
        let substituted = substituted.or_else(|| {
            if !self.cfg.virtualize_metadata || root.is_some() || self.cfg.recordreplay_modes {
                return None;
            }
            let prefix = crate::procfs::absent_device_attribute_prefix(&observed)?;
            let parent = prefix.parent()?;
            if !is_on_sysfs(parent) {
                return None;
            }
            dashed_spelling(parent, &prefix, &observed, bytes)
        });
        Decision {
            substituted,
            decided,
        }
    }

    /// The directory the tool looks at to tell whether `parent`, the
    /// directory of an omitted name in the tool's view of the guest's path,
    /// is procfs (and not, say, a user bind over part of `/proc`). A process's
    /// or a thread's directory is named for the guest: the tool's own `self`
    /// is not the guest's, so `self` and `thread-self` become the calling
    /// guest thread's pid and tid where those name host tasks the tool can
    /// see. Where the tool runs on the guest's own thread (DBT), its view is
    /// the guest's. Where the guest's threads are no host tasks (KVM), the
    /// guest's process directory has no name in the tool's view, and the
    /// guest's `/proc` itself is looked at. After a guest `chroot` to `root`,
    /// the guest's `/proc` is `root`'s.
    fn parent_in_tool_view<G: Guest<Self>>(
        &self,
        guest: &G,
        parent: &Path,
        root: &Option<PathBuf>,
        numbers: Option<&BTreeMap<i32, Option<i32>>>,
    ) -> PathBuf {
        let proc_root = match root {
            None => PathBuf::from("/proc"),
            Some(root) => root.join("proc"),
        };
        let Ok(below) = parent.strip_prefix(&proc_root) else {
            return parent.to_path_buf();
        };
        let mut components = below.components();
        let Some(first) = components.next() else {
            return parent.to_path_buf();
        };
        let first = first.as_os_str().as_bytes();
        let is_process =
            first == b"self" || first == b"thread-self" || first.iter().all(u8::is_ascii_digit);
        if !is_process {
            return parent.to_path_buf();
        }
        // The tool on the guest's own thread (DBT) sees the guest's /proc,
        // by the kernel's numbers where they differ (`procfs_numbers`).
        if self.cfg.backend.tool_shares_guest_descriptor_table {
            return numbers
                .and_then(|numbers| {
                    crate::procfs::ids::translate(parent, |id| numbers.get(&id).copied().flatten())
                })
                .unwrap_or_else(|| parent.to_path_buf());
        }
        if !crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            return proc_root;
        }
        let process = match first {
            b"self" => proc_root.join(guest.pid().as_raw().to_string()),
            b"thread-self" => proc_root.join(format!(
                "{}/task/{}",
                guest.pid().as_raw(),
                guest.tid().as_raw()
            )),
            _ => proc_root.join(OsStr::from_bytes(first)),
        };
        process.join(components.as_path())
    }

    /// An `AF_UNIX` address naming the substituted path, if the address at
    /// `address`, `length` bytes long, is a pathname address that reaches a
    /// `/proc` name outside the level. A send's address is looked up only by
    /// a datagram socket, whose type the caller asks the socket for, so a
    /// send is checked only outside record and replay (whose replayed
    /// descriptor is not the socket); the caller refuses one whose
    /// substituted address does not fit.
    fn substituted_socket_address<G: Guest<Self>>(
        &self,
        guest: &G,
        address: u64,
        length: usize,
        send: bool,
        root: &Option<PathBuf>,
        numbers: Option<&BTreeMap<i32, Option<i32>>>,
    ) -> Option<Vec<u8>> {
        if length > std::mem::size_of::<libc::sockaddr_un>()
            || (send && self.cfg.recordreplay_modes)
        {
            return None;
        }
        let mut bytes = vec![0; length];
        guest
            .memory()
            .read_exact(Addr::<u8>::from_raw(address as usize)?, &mut bytes)
            .ok()?;
        let path = PathBuf::from(OsStr::from_bytes(unix_address_path(&bytes)?));
        let substituted = self.decide(guest, None, &path, root, numbers).substituted?;
        unix_address(&substituted)
    }

    /// Issue `sysno` with `values`, each replacement's bytes on a transient
    /// page in the guest and its argument their address, through the
    /// recording, and return what the kernel answered. The call names a
    /// missing name in a procfs directory, so it fails; were it to succeed,
    /// it would be refused with ENOENT (a descriptor it made closed), as
    /// nothing it did is Detcore's. With no room for the page, the call is
    /// refused with ENOENT unasked.
    ///
    /// Where every path the call looks up is a procfs name the table decides
    /// (`confined`), a refusal for permission is ENOENT: Linux's procfs
    /// directories are searchable, and its procfs lookup of a missing name
    /// fails with ENOENT before any permission is checked, so the refusal is
    /// the KVM executor's, which refuses what reaches procfs.
    async fn issue_substituted<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        mut values: [usize; 6],
        mut replacements: Vec<Replacement>,
        confined: bool,
    ) -> Result<i64, Error> {
        let length: usize = replacements
            .iter()
            .map(|replacement| replacement.bytes.len() + 1)
            .sum();
        let Some(page) = map_transient(guest, length).await else {
            return Err(syscalls::Errno::ENOENT.into());
        };
        let base = page.as_raw();
        // Lay the replacements out, NUL-terminated, and point their
        // arguments (or the copied header's `msg_name`) at them.
        let mut offsets = Vec::with_capacity(replacements.len());
        let mut offset = 0;
        for replacement in &replacements {
            offsets.push(offset);
            offset += replacement.bytes.len() + 1;
        }
        for index in 1..replacements.len() {
            if replacements[index].argument.is_none() {
                let name = (base + offsets[index]) as u64;
                replacements[index - 1].bytes[..8].copy_from_slice(&name.to_ne_bytes());
            }
        }
        let mut contents = Vec::with_capacity(length);
        for (replacement, offset) in replacements.iter().zip(&offsets) {
            contents.extend_from_slice(&replacement.bytes);
            contents.push(0);
            if let Some(argument) = replacement.argument {
                values[argument] = base + offset;
            }
        }
        let result = match guest.memory().write_exact(page, &contents) {
            Ok(()) => {
                self.record_or_replay(guest, Syscall::from_raw(sysno, raw_args(values)))
                    .await
            }
            Err(_) => Err(syscalls::Errno::ENOENT),
        };
        unmap_transient(guest, page, length).await;
        match result {
            Ok(value) => {
                if creates_descriptor(sysno) {
                    let _ = self
                        .record_or_replay(guest, syscalls::Close::new().with_fd(value as RawFd))
                        .await;
                }
                Err(syscalls::Errno::ENOENT.into())
            }
            Err(syscalls::Errno::EACCES | syscalls::Errno::EPERM) if confined => {
                Err(syscalls::Errno::ENOENT.into())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The answer to the call `sysno`, its argument `values` but for the path
    /// at argument `path`, which is `spelling`, a name procfs does not have
    /// ([`dashed_spelling`]): the kernel's refusal of the call's other
    /// arguments, which Linux checks before its walk, if it makes one, and
    /// otherwise ENOENT, where the walk ends ([`Self::issue_substituted`]).
    pub(super) async fn answer_for_absent_name<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        values: [usize; 6],
        path: usize,
        spelling: Vec<u8>,
    ) -> Result<i64, Error> {
        let replacement = Replacement {
            argument: Some(path),
            bytes: spelling,
        };
        self.issue_substituted(guest, sysno, values, vec![replacement], true)
            .await
    }

    /// Whether the socket `fd` is a datagram socket, as the socket says
    /// (`getsockopt(SO_TYPE)`, into a transient page): only such a socket
    /// looks up the address a send gives. `None` where there is no room for
    /// the question. Asked only for a send whose address reaches an omitted
    /// name, or leads below a `/proc` directory Detcore serves on a
    /// placeholder (`Detcore::refuse_lookup_below_placeholder`), outside
    /// record and replay.
    pub(super) async fn socket_is_datagram<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<bool> {
        const LENGTH: usize = 8;
        let page = map_transient(guest, LENGTH).await?;
        let base = page.as_raw();
        // The type, an int, then its length, a socklen_t.
        let mut contents = [0u8; LENGTH];
        contents[4..].copy_from_slice(&4u32.to_ne_bytes());
        let answered = guest.memory().write_exact(page, &contents).is_ok()
            && guest
                .inject(Syscall::from_raw(
                    Sysno::getsockopt,
                    raw_args([
                        fd as usize,
                        libc::SOL_SOCKET as usize,
                        libc::SO_TYPE as usize,
                        base,
                        base + 4,
                        0,
                    ]),
                ))
                .await
                .is_ok()
            && guest.memory().read_exact(page, &mut contents).is_ok();
        unmap_transient(guest, page, LENGTH).await;
        Some(
            answered
                && i32::from_ne_bytes(contents[..4].try_into().expect("4 bytes"))
                    == libc::SOCK_DGRAM,
        )
    }

    /// Whether the socket `fd` can pass descriptors over `SCM_RIGHTS`: an
    /// `AF_UNIX` socket, as the socket says (`getsockopt(SO_DOMAIN)`, into a
    /// transient page). `false` where the socket does not answer because `fd`
    /// is no socket or no descriptor, where Linux's send fails before it reads
    /// the control data (`sockfd_lookup_light`); `true` where Detcore cannot
    /// ask (no room for the question, or another failure).
    pub(super) async fn socket_carries_descriptors<G: Guest<Self>>(
        guest: &mut G,
        fd: RawFd,
    ) -> bool {
        const LENGTH: usize = 8;
        let Some(page) = map_transient(guest, LENGTH).await else {
            return true;
        };
        let base = page.as_raw();
        // The domain, an int, then its length, a socklen_t.
        let mut contents = [0u8; LENGTH];
        contents[4..].copy_from_slice(&4u32.to_ne_bytes());
        let carries = if guest.memory().write_exact(page, &contents).is_err() {
            true
        } else {
            match guest
                .inject(Syscall::from_raw(
                    Sysno::getsockopt,
                    raw_args([
                        fd as usize,
                        libc::SOL_SOCKET as usize,
                        libc::SO_DOMAIN as usize,
                        base,
                        base + 4,
                        0,
                    ]),
                ))
                .await
            {
                Ok(_) if guest.memory().read_exact(page, &mut contents).is_ok() => {
                    i32::from_ne_bytes(contents[..4].try_into().expect("4 bytes")) == libc::AF_UNIX
                }
                Err(syscalls::Errno::ENOTSOCK | syscalls::Errno::EBADF) => false,
                _ => true,
            }
        };
        unmap_transient(guest, page, LENGTH).await;
        carries
    }

    /// The guest's root in the tool's view: `Some(None)` where they are the
    /// same (no `chroot`), `Some(Some(root))` after a `chroot`, and `None`
    /// where it cannot be known alike in record and replay (a `chroot` under
    /// record or replay, whose replay root is the replayer's own), or at all
    /// (the tool's `/proc` unreadable from inside the new root).
    pub(crate) fn guest_root_in_view<G: Guest<Self>>(&self, guest: &G) -> Option<Option<PathBuf>> {
        if !guest
            .thread_state()
            .changed_root
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Some(None);
        }
        if self.cfg.recordreplay_modes {
            return None;
        }
        std::fs::read_link(format!("{}/root", self.caller_proc_directory(guest)))
            .ok()
            .map(Some)
    }

    /// The tool's `/proc` directory for the calling guest thread, whose
    /// `fd` holds its descriptors (its table may be its own: a clone without
    /// `CLONE_FILES`) and whose `root` is its root. Where the tool runs on
    /// the guest's own thread (DBT, which knows the thread by Detcore's
    /// identities rather than the kernel's), `/proc/thread-self`.
    pub(crate) fn caller_proc_directory<G: Guest<Self>>(&self, guest: &G) -> String {
        if self.cfg.backend.tool_shares_guest_descriptor_table {
            String::from("/proc/thread-self")
        } else {
            format!("/proc/{}", guest.tid().as_raw())
        }
    }

    /// Whether the descriptor `fd` the kernel just opened is a `/proc` name
    /// outside the run's level. It is one if it is on procfs and its kernel
    /// path is outside the level, or cannot be read at all (after a `chroot`
    /// into a root without `/proc`, where the tool runs inside the guest): a
    /// procfs file Detcore cannot name is not one it serves.
    ///
    /// Whether it is on procfs: by its device, `identity`'s (the caller's
    /// stat, with its inode) or the tool's own stat of it, against the
    /// guest's proc device; where neither is known, by asking the descriptor
    /// (`fstatfs`). A tool-side entry whose identity is not the descriptor's
    /// (the KVM executor's guest has no host task) decides nothing. Not under
    /// record and replay, whose replayed descriptor is not the file (see
    /// `handle_openat`'s recorded path).
    pub(crate) async fn opened_procfs_name_outside_level<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        identity: Option<(u64, u64)>,
    ) -> bool {
        if self.cfg.recordreplay_modes {
            return false;
        }
        let link = format!("{}/fd/{fd}", self.caller_proc_directory(guest));
        // Under KVM the link is an unrelated host task's, if any: neither look
        // at it, and leave the answer to the caller's identity, as when the
        // link is unreadable. DBT and SaBRe read their own `thread-self`.
        let visible = crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend);
        let entry = visible
            .then(|| std::fs::metadata(&link).ok())
            .flatten()
            .map(|metadata| (metadata.dev(), metadata.ino()));
        if let (Some(identity), Some(entry)) = (identity, entry)
            && identity != entry
        {
            return false;
        }
        let device = identity.or(entry).map(|(dev, _)| dev);
        let on_procfs = match (device, rendered_procfs::proc_device()) {
            (Some(device), Some(proc_device)) => device == proc_device,
            _ => self
                .descriptor_is_on_filesystem(guest, fd, libc::PROC_SUPER_MAGIC)
                .await
                .unwrap_or(false),
        };
        if !on_procfs {
            return false;
        }
        match visible.then(|| std::fs::read_link(&link)) {
            Some(Ok(path)) if path.is_absolute() => {
                let absent = |path: &Path| {
                    procfs_name(path).is_some_and(|name| name.absent_at(self.cfg.proc_level))
                };
                // After a `chroot` into another root, the kernel names the
                // file in the tool's view, below that root.
                absent(&path)
                    || matches!(self.guest_root_in_view(guest), Some(Some(root))
                    if path.strip_prefix(&root).is_ok_and(|below| {
                        absent(&Path::new("/").join(below))
                    }))
            }
            _ => true,
        }
    }

    /// Whether Detcore checks the sysfs object a call reached
    /// ([`Self::is_absent_sysfs_object`]) once the call has run: where the
    /// lookup check leaves the same objects out by their spelling
    /// ([`Self::decide`]) and the listings drop them
    /// (`Detcore::drop_absent_entries`), in run mode with virtualized
    /// metadata, where the guest's `/sys` is a sysfs mount
    /// ([`rendered_procfs::sys_device`]), and, as there, only before any
    /// `chroot`, as the tool reads the object's path in its own root.
    fn checks_reached_sysfs_objects<G: Guest<Self>>(&self, guest: &G) -> bool {
        self.cfg.virtualize_metadata
            && !self.cfg.recordreplay_modes
            && rendered_procfs::sys_device().is_some()
            && self.guest_root_in_view(guest) == Some(None)
    }

    /// Whether the sysfs object at `path`, the kernel's path for it in the
    /// tool's view, is one the machine Detcore presents does not have, or is
    /// below one: a CPU or node entry of the directories that list them
    /// ([`crate::procfs::absent_cpu_entry_prefix`]), or a device's NUMA
    /// attribute that a kernel without NUMA does not register
    /// ([`crate::procfs::absent_device_attribute_prefix`]).
    pub(crate) fn is_absent_sysfs_object(&self, path: &Path) -> bool {
        self.absent_sysfs_prefix(path).is_some()
    }

    /// The prefix of `path` that ends at the object
    /// [`Self::is_absent_sysfs_object`] finds absent.
    fn absent_sysfs_prefix(&self, path: &Path) -> Option<PathBuf> {
        crate::procfs::absent_cpu_entry_prefix(path, self.cfg.virtual_cpu_count())
            .or_else(|| crate::procfs::absent_device_attribute_prefix(path))
    }

    /// The kernel's path for the calling thread's descriptor `fd`, in the
    /// tool's view: its link in the tool's `/proc` entry for the caller where
    /// that entry is the caller's
    /// ([`crate::procfs_inode::tool_can_name_guest_descriptors`]), and
    /// elsewhere (KVM) the backend's `link_path` fact
    /// (`Detcore::own_descriptor_link_from_fact`). `None` where neither names
    /// an absolute path.
    pub(crate) fn descriptor_path_in_tool_view<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
    ) -> Option<PathBuf> {
        let path = if crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            std::fs::read_link(format!("{}/fd/{fd}", self.caller_proc_directory(guest))).ok()?
        } else {
            PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(
                    Self::own_descriptor_link_from_fact(guest, fd)??,
                ),
            )
        };
        path.is_absolute().then_some(path)
    }

    /// The kernel's path, in the tool's view, for the sysfs object that the
    /// descriptor `fd` an open just returned is open on, where Detcore checks
    /// such objects ([`Self::checks_reached_sysfs_objects`]) and the open's
    /// `identity` (the descriptor's stat) is on the guest's sysfs mount.
    /// `None` for every other descriptor.
    pub(crate) fn opened_sysfs_object<G: Guest<Self>>(
        &self,
        guest: &G,
        fd: RawFd,
        identity: Option<&libc::stat>,
    ) -> Option<PathBuf> {
        if !self.checks_reached_sysfs_objects(guest)
            || identity.map(|stat| stat.st_dev) != rendered_procfs::sys_device()
        {
            return None;
        }
        self.descriptor_path_in_tool_view(guest, fd)
    }

    /// `spelling`, which a call looks up relative to `dirfd`, as an absolute
    /// path in the tool's view that leads where the guest's lookup does:
    /// below the guest's working directory ([`Self::observed_path`]) or the
    /// descriptor's directory, by the kernel's path for it
    /// ([`Self::descriptor_path_in_tool_view`]). Through the guest's own
    /// `/proc/self` or `/proc/thread-self`: its descriptor's path for
    /// `fd/<n>`, and `/` for its `root`; anything else there by the caller's
    /// numbers where the tool's `/proc` entry for the caller is the caller's,
    /// and not placed where it is not (KVM). `None` where it cannot be
    /// placed.
    fn named_in_tool_view<G: Guest<Self>>(
        &self,
        guest: &G,
        dirfd: i32,
        spelling: &Path,
    ) -> Option<PathBuf> {
        let named = if spelling.is_absolute() || dirfd == libc::AT_FDCWD {
            Self::observed_path(guest, dirfd, spelling).ok()?
        } else {
            self.descriptor_path_in_tool_view(guest, dirfd)?
                .join(spelling)
        };
        if !named.is_absolute() {
            return None;
        }
        let Ok(below) = named.strip_prefix("/proc") else {
            return Some(named);
        };
        let names = below
            .components()
            .map(|component| component.as_os_str().as_bytes())
            .collect::<Vec<_>>();
        let own = matches!(names.first(), Some(&(b"self" | b"thread-self")));
        let visible = crate::procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend);
        match names.as_slice() {
            [_, b"fd", number, ..] if own => {
                let fd = std::str::from_utf8(number).ok()?.parse::<RawFd>().ok()?;
                let rest = below.components().skip(3).collect::<PathBuf>();
                return Some(self.descriptor_path_in_tool_view(guest, fd)?.join(rest));
            }
            [_, b"root", ..] if own => {
                return Some(Path::new("/").join(below.components().skip(2).collect::<PathBuf>()));
            }
            _ if !visible => return None,
            // The tool on the guest's own thread (DBT) has the guest's `self`.
            _ if !own || self.cfg.backend.tool_shares_guest_descriptor_table => {
                return Some(named);
            }
            _ => {}
        }
        let caller = match names.first() {
            Some(&b"self") => format!("/proc/{}", guest.pid().as_raw()),
            _ => format!(
                "/proc/{}/task/{}",
                guest.pid().as_raw(),
                guest.tid().as_raw()
            ),
        };
        Some(Path::new(&caller).join(below.components().skip(1).collect::<PathBuf>()))
    }

    /// The kernel's path, in the tool's view, for the sysfs object the
    /// machine Detcore presents does not have
    /// ([`Self::is_absent_sysfs_object`]) that a lookup of `spelling`
    /// relative to `dirfd` went through, following a symbolic link its last
    /// name names where `follow` (or the spelling itself does:
    /// [`follows_last_name`]), where Detcore checks such objects
    /// ([`Self::checks_reached_sysfs_objects`]). For a call that `succeeded`,
    /// the object it reached, asked first by a stat in the tool's view, so
    /// that a lookup that reached no sysfs object costs one; for a call that
    /// failed, the deepest object its lookup can have reached
    /// ([`resolved_on_sysfs`]). `None` where it went through no such object.
    pub(crate) async fn absent_sysfs_object_reached<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: i32,
        spelling: &Path,
        follow: bool,
        succeeded: bool,
    ) -> Option<PathBuf> {
        if !self.checks_reached_sysfs_objects(guest) {
            return None;
        }
        let follow = follow || follows_last_name(spelling);
        Self::prepare_guest_cwd(guest, dirfd, spelling).await;
        let named = self.named_in_tool_view(guest, dirfd, spelling)?;
        if succeeded {
            let metadata = if follow {
                std::fs::metadata(&named)
            } else {
                std::fs::symlink_metadata(&named)
            };
            if !metadata.is_ok_and(|metadata| Some(metadata.dev()) == rendered_procfs::sys_device())
            {
                return None;
            }
        }
        resolved_on_sysfs(&named, follow).filter(|reached| self.is_absent_sysfs_object(reached))
    }

    /// Whether a stat call whose result is on the device `dev` may have
    /// reached a sysfs object the machine Detcore presents does not have
    /// ([`Self::stat_reached_absent_sysfs_object`] decides): a result on the
    /// guest's sysfs mount, where Detcore checks such objects.
    pub(crate) fn may_reach_absent_sysfs_object<G: Guest<Self>>(
        &self,
        guest: &G,
        dev: u64,
    ) -> bool {
        Some(dev) == rendered_procfs::sys_device() && self.checks_reached_sysfs_objects(guest)
    }

    /// Whether a stat call that looked up `target` went through a sysfs
    /// object the machine Detcore presents does not have
    /// ([`Self::is_absent_sysfs_object`]), by a spelling the lookup check does
    /// not resolve: a symbolic link to it, or to a directory above it. Where
    /// the call's result overwrote its path, the path is `overwritten`, read
    /// before the call. Of a call that succeeded (`outcome` the device of its
    /// result), asked only for a result on the guest's sysfs mount; of one
    /// that failed, only for a failure Linux reports once its lookup found an
    /// object ([`fails_after_lookup`]).
    pub(super) async fn stat_reached_absent_sysfs_object<G: Guest<Self>>(
        &self,
        guest: &mut G,
        outcome: Result<u64, Errno>,
        target: super::StatTarget<'_>,
        overwritten: Option<LookedUpPath>,
    ) -> bool {
        let succeeded = match outcome {
            Ok(dev) if self.may_reach_absent_sysfs_object(guest, dev) => true,
            Err(errno) if fails_after_lookup(errno) => false,
            _ => return false,
        };
        let looked_up = match target {
            super::StatTarget::Descriptor(fd) => {
                return succeeded
                    && self
                        .descriptor_path_in_tool_view(guest, fd)
                        .is_some_and(|path| self.is_absent_sysfs_object(&path));
            }
            super::StatTarget::Cwd => Some(LookedUpPath {
                dirfd: libc::AT_FDCWD,
                spelling: PathBuf::from("."),
                follow: true,
            }),
            super::StatTarget::Path {
                dirfd,
                path,
                follow,
            } => path
                .and_then(|path| path.read(&guest.memory()).ok())
                .map(|spelling| LookedUpPath {
                    dirfd,
                    spelling,
                    follow,
                }),
            super::StatTarget::Overwritten => overwritten,
        };
        let Some(LookedUpPath {
            dirfd,
            spelling,
            follow,
        }) = looked_up
        else {
            return false;
        };
        self.absent_sysfs_object_reached(guest, dirfd, &spelling, follow, succeeded)
            .await
            .is_some()
    }

    /// An access probe (`access`, `faccessat` or `faccessat2`), passed
    /// through, but `ENOENT` where its lookup went through a sysfs object the
    /// machine Detcore presents does not have
    /// ([`Self::absent_sysfs_object_reached`]) by a spelling the lookup check
    /// does not resolve, as a probe that names the object fails: whether the
    /// probe succeeded or failed once its lookup found the object (`W_OK` on
    /// a read-only attribute, say: [`fails_after_lookup`]).
    pub(crate) async fn handle_access_probe<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let result = self.passthrough(guest, call).await;
        let succeeded = match &result {
            Ok(_) => true,
            Err(Error::Errno(errno)) if fails_after_lookup(*errno) => false,
            Err(_) => return result,
        };
        if !self.checks_reached_sysfs_objects(guest) {
            return result;
        }
        let (sysno, args) = call.into_parts();
        // Linux declares the flags `int`.
        let (dirfd, path, flags) = match sysno {
            #[cfg(target_arch = "x86_64")]
            Sysno::access => (libc::AT_FDCWD, args.arg0, 0),
            Sysno::faccessat => (args.arg0 as i32, args.arg1, 0),
            _ => (args.arg0 as i32, args.arg1, args.arg3 as i32),
        };
        let Some(spelling) = PathPtr::from_ptr(path as *const libc::c_char)
            .and_then(|path| path.read(&guest.memory()).ok())
        else {
            return result;
        };
        // An empty path with `AT_EMPTY_PATH` probes the directory itself.
        let (spelling, follow) =
            if spelling.as_os_str().is_empty() && flags & libc::AT_EMPTY_PATH != 0 {
                (PathBuf::from("."), true)
            } else {
                (spelling, flags & libc::AT_SYMLINK_NOFOLLOW == 0)
            };
        match self
            .absent_sysfs_object_reached(guest, dirfd, &spelling, follow, succeeded)
            .await
        {
            Some(_) => Err(Errno::ENOENT.into()),
            None => result,
        }
    }

    /// The answer to the open `call` of `spelling`, which failed with
    /// `errno`, where its lookup went through a sysfs object the machine
    /// Detcore presents does not have
    /// ([`Self::absent_sysfs_object_reached`]) by a spelling the lookup check
    /// does not resolve: as an open of that object's own path fails
    /// ([`Self::answer_open_of_absent_sysfs_object`]), not, say, with the
    /// `EACCES` of a write to it. `None` elsewhere, where `errno` stands. With
    /// `O_NOFOLLOW`, or `O_CREAT` and `O_EXCL`, a link the last name names is
    /// not followed: the open is of the link.
    pub(crate) async fn failed_open_answer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::Openat,
        spelling: &Path,
        errno: Errno,
    ) -> Option<Error> {
        use nix::fcntl::OFlag;
        if !fails_after_lookup(errno) {
            return None;
        }
        let flags = call.flags();
        let follow =
            !flags.contains(OFlag::O_NOFOLLOW) && !flags.contains(OFlag::O_CREAT | OFlag::O_EXCL);
        let object = self
            .absent_sysfs_object_reached(guest, call.dirfd(), spelling, follow, false)
            .await?;
        Some(
            self.answer_open_of_absent_sysfs_object(guest, call, &object)
                .await,
        )
    }

    /// The answer to the open `call` where it went through `object`, a sysfs
    /// object the machine Detcore presents does not have
    /// ([`Self::is_absent_sysfs_object`]), by a spelling the lookup check
    /// does not resolve: that of an open of the object's own path, with the
    /// absent name dashed as the lookup check dashes it ([`dashed_spelling`]),
    /// which is ENOENT, except that with `O_CREAT` the kernel says whether
    /// the name could be made in that sysfs directory (it refuses, as a
    /// read-only mount or for permission), so it is asked
    /// ([`Self::issue_substituted`]).
    pub(crate) async fn answer_open_of_absent_sysfs_object<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::Openat,
        object: &Path,
    ) -> Error {
        let dashed = self.absent_sysfs_prefix(object).and_then(|prefix| {
            dashed_spelling(
                prefix.parent()?,
                &prefix,
                object,
                object.as_os_str().as_bytes(),
            )
        });
        match dashed {
            Some(bytes) if call.flags().contains(nix::fcntl::OFlag::O_CREAT) => {
                let values = [
                    libc::AT_FDCWD as usize,
                    0,
                    call.flags().bits() as usize,
                    call.mode().map_or(0, |mode| mode.bits() as usize),
                    0,
                    0,
                ];
                let replacement = Replacement {
                    argument: Some(1),
                    bytes,
                };
                match self
                    .issue_substituted(guest, Sysno::openat, values, vec![replacement], false)
                    .await
                {
                    Err(error) => error,
                    Ok(_) => Errno::ENOENT.into(),
                }
            }
            _ => Errno::ENOENT.into(),
        }
    }

    /// Whether the descriptor `fd` is on procfs, as the descriptor said in
    /// the recording (`fstatfs` through the recording, into a transient
    /// page), so record and replay decide alike. Where there is no page for
    /// the question, `true`: the caller asks only about a descriptor whose
    /// link names a `/proc` name outside the level, which is then refused
    /// rather than admitted unasked (as a substituted call is).
    /// Under record and replay, where the descriptor's link alone does not
    /// show that it is procfs (a user bind over part of `/proc`).
    pub(crate) async fn recorded_descriptor_is_on_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> bool {
        self.recorded_descriptor_on_procfs(guest, fd)
            .await
            .unwrap_or(true)
    }

    /// [`Self::recorded_descriptor_is_on_procfs`] without its fail-closed
    /// default: `None` where there is no page for the question.
    pub(super) async fn recorded_descriptor_on_procfs<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<bool> {
        const LENGTH: usize = std::mem::size_of::<libc::statfs>();
        let page = map_transient(guest, LENGTH).await?;
        let mut contents = [0u8; LENGTH];
        let answered = self
            .record_or_replay(
                guest,
                Syscall::from_raw(
                    Sysno::fstatfs,
                    raw_args([fd as usize, page.as_raw(), 0, 0, 0, 0]),
                ),
            )
            .await
            .is_ok()
            && guest.memory().read_exact(page, &mut contents).is_ok();
        unmap_transient(guest, page, LENGTH).await;
        // `f_type` is the first field.
        Some(
            answered
                && i64::from_ne_bytes(contents[..8].try_into().expect("8 bytes"))
                    == libc::PROC_SUPER_MAGIC,
        )
    }

    /// The mount `fd` was reached through, its filesystem's device and
    /// the descriptor's mode, as the descriptor said in the recording
    /// (`statx(fd, "", AT_EMPTY_PATH)` through the recording, into a
    /// transient page: the empty path is the fresh page's first, zero,
    /// byte, and the result buffer follows it), so record and replay
    /// decide alike. `None` where there is no page for the question, the
    /// call failed, or it reported no mount ID or type. Under record and
    /// replay, where the descriptor's link names a bind mount at an
    /// ordinary path instead of the object it reaches.
    pub(super) async fn recorded_descriptor_mount_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<(u64, u64, u32)> {
        // `struct statx` (256 bytes) at the page's second half.
        const STATX_SIZE: usize = 256;
        const LENGTH: usize = 2 * STATX_SIZE;
        const MASK_OFFSET: usize = 0;
        const MODE_OFFSET: usize = 28;
        const DEV_MAJOR_OFFSET: usize = 136;
        const DEV_MINOR_OFFSET: usize = 140;
        const MNT_ID_OFFSET: usize = 144;
        let page = map_transient(guest, LENGTH).await?;
        let buffer = page.as_raw() + STATX_SIZE;
        let mut contents = [0u8; STATX_SIZE];
        let answered = self
            .record_or_replay(
                guest,
                Syscall::from_raw(
                    Sysno::statx,
                    raw_args([
                        fd as usize,
                        page.as_raw(),
                        libc::AT_EMPTY_PATH as usize,
                        (libc::STATX_BASIC_STATS | reverie::syscalls::linux_abi::STATX_MNT_ID)
                            as usize,
                        buffer,
                        0,
                    ]),
                ),
            )
            .await
            .is_ok()
            && AddrMut::<u8>::from_raw(buffer)
                .is_some_and(|buffer| guest.memory().read_exact(buffer, &mut contents).is_ok());
        unmap_transient(guest, page, LENGTH).await;
        if !answered {
            return None;
        }
        let field_u32 = |offset: usize| {
            u32::from_ne_bytes(contents[offset..offset + 4].try_into().expect("4 bytes"))
        };
        let field_u64 = |offset: usize| {
            u64::from_ne_bytes(contents[offset..offset + 8].try_into().expect("8 bytes"))
        };
        let field_u16 = |offset: usize| {
            u16::from_ne_bytes(contents[offset..offset + 2].try_into().expect("2 bytes"))
        };
        let mask = field_u32(MASK_OFFSET);
        (mask & reverie::syscalls::linux_abi::STATX_MNT_ID != 0 && mask & libc::STATX_TYPE != 0)
            .then(|| {
                (
                    field_u64(MNT_ID_OFFSET),
                    libc::makedev(field_u32(DEV_MAJOR_OFFSET), field_u32(DEV_MINOR_OFFSET)),
                    u32::from(field_u16(MODE_OFFSET)),
                )
            })
    }

    /// Whether the directory `fd` holds procfs's `root` link, as the
    /// descriptor said in the recording (`statx(fd, "root",
    /// AT_SYMLINK_NOFOLLOW)` through the recording, into a transient
    /// page, consuming no descriptor of the guest's), so record and
    /// replay decide alike. A process's and a thread's directory are
    /// the only procfs directories that hold that link, and `lstat`
    /// answers for it where a `readlink` would want ptrace access, so
    /// on a descriptor already shown to be a procfs directory this
    /// authenticates a task directory reached by any name — in
    /// particular a bind of one that took the guest's last free
    /// descriptor, where reading the mount table, which must open it,
    /// fails `EMFILE` in the recording and names nothing. `None` where
    /// there is no page for the question or it reported no type.
    pub(super) async fn recorded_descriptor_has_root_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Option<bool> {
        // The name `root` in the page's first half, the `struct statx`
        // (256 bytes) in the second, as in
        // [`Self::recorded_descriptor_mount_identity`].
        const STATX_SIZE: usize = 256;
        const LENGTH: usize = 2 * STATX_SIZE;
        const MASK_OFFSET: usize = 0;
        const MODE_OFFSET: usize = 28;
        let page = map_transient(guest, LENGTH).await?;
        let buffer = page.as_raw() + STATX_SIZE;
        let mut contents = [0u8; STATX_SIZE];
        let answered = guest.memory().write_exact(page, b"root\0").is_ok()
            && self
                .record_or_replay(
                    guest,
                    Syscall::from_raw(
                        Sysno::statx,
                        raw_args([
                            fd as usize,
                            page.as_raw(),
                            libc::AT_SYMLINK_NOFOLLOW as usize,
                            libc::STATX_TYPE as usize,
                            buffer,
                            0,
                        ]),
                    ),
                )
                .await
                .is_ok()
            && AddrMut::<u8>::from_raw(buffer)
                .is_some_and(|buffer| guest.memory().read_exact(buffer, &mut contents).is_ok());
        unmap_transient(guest, page, LENGTH).await;
        if !answered {
            return Some(false);
        }
        let mask = u32::from_ne_bytes(
            contents[MASK_OFFSET..MASK_OFFSET + 4]
                .try_into()
                .expect("4 bytes"),
        );
        let mode = u32::from(u16::from_ne_bytes(
            contents[MODE_OFFSET..MODE_OFFSET + 2]
                .try_into()
                .expect("2 bytes"),
        ));
        (mask & libc::STATX_TYPE != 0).then_some(mode & libc::S_IFMT == libc::S_IFLNK)
    }

    /// Whether `fd` was authenticated at its open as a real procfs task
    /// directory under record and replay: a placeholder bound to a process
    /// or thread directory is one by construction, and a real descriptor
    /// carries the authentication its open recorded
    /// (`OpenFileDescription::recordreplay_task_directory`, from the
    /// recorded descriptor link naming a task directory — or, where the
    /// link names a bind of one at an ordinary path, from the
    /// directory's recorded `root` link or its recorded mount identity
    /// taken through the recorded mount table — and the descriptor
    /// itself answering procfs to a recorded `fstatfs`). The identity
    /// comes from the opened object,
    /// not the guest's spelling, so an ordinary symlink or a bind mount
    /// of the directory carries it and a user bind over a task spelling
    /// does not. A link itself and a missing descriptor are not one.
    pub(crate) fn recordreplay_task_directory<G: Guest<Self>>(&self, guest: &G, fd: RawFd) -> bool {
        if !self.cfg.recordreplay_modes {
            return false;
        }
        let Ok((placeholder, link_itself, authenticated)) =
            guest.thread_state().with_detfd(fd, |detfd| {
                (
                    detfd.placeholder(),
                    detfd.procfs_link_itself(),
                    detfd.recordreplay_task_directory(),
                )
            })
        else {
            return false;
        };
        if link_itself {
            return false;
        }
        match placeholder {
            Some(state) => matches!(
                state.directory,
                crate::procfs::fd_directory::PlaceholderDirectory::Task(binding)
                    if matches!(
                        binding.kind,
                        crate::procfs::fd_directory::TaskDirectoryKind::Process
                            | crate::procfs::fd_directory::TaskDirectoryKind::Thread
                    )
            ),
            None => authenticated,
        }
    }

    /// Whether the descriptor `fd` is on the named filesystem, as the
    /// descriptor says (`fstatfs`, into a transient page). A failed probe
    /// remains an error, not evidence of another filesystem. The descriptor
    /// identifies the opened object even when a user mount covers a
    /// conventional procfs or sysfs pathname.
    pub(super) async fn descriptor_is_on_filesystem<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        magic: i64,
    ) -> Result<bool, Errno> {
        const LENGTH: usize = std::mem::size_of::<libc::statfs>();
        let page = map_transient(guest, LENGTH).await.ok_or(Errno::ENOMEM)?;
        let mut contents = [0u8; LENGTH];
        let answered = guest
            .inject_with_retry(Syscall::from_raw(
                Sysno::fstatfs,
                raw_args([fd as usize, page.as_raw(), 0, 0, 0, 0]),
            ))
            .await
            .and_then(|_| guest.memory().read_exact(page, &mut contents));
        unmap_transient(guest, page, LENGTH).await;
        answered?;
        // `f_type` is the first field.
        Ok(i64::from_ne_bytes(contents[..8].try_into().expect("8 bytes")) == magic)
    }

    /// Whether the open `call`, which the kernel refused for permission,
    /// reaches a `/proc` name outside the run's level.
    ///
    /// First an `O_PATH` open of the spelling, which needs no permission on
    /// the file, names what it reaches. Where that is refused too, the
    /// refusal is either Linux's, for a directory on the way the guest
    /// cannot search, or the KVM executor's, which refuses what reaches
    /// procfs. Detcore then resolves the spelling's links itself, and opens
    /// what they reach directly with `O_PATH`: where that is refused as well,
    /// it is the backend that refuses procfs, and an omitted name is refused
    /// with ENOENT; where it opens, the guest's spelling is what is refused,
    /// as Linux refuses it. The tool reads links in its own root, so the
    /// resolution runs only where that is the guest's (no `chroot`).
    pub(crate) async fn refused_open_reaches_procfs_name_outside_level<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &syscalls::Openat,
    ) -> bool {
        if self.cfg.recordreplay_modes {
            return false;
        }
        let o_path = nix::fcntl::OFlag::O_PATH
            | nix::fcntl::OFlag::O_CLOEXEC
            | (call.flags() & nix::fcntl::OFlag::O_NOFOLLOW);
        let probe = syscalls::Openat::new()
            .with_dirfd(call.dirfd())
            .with_path(call.path())
            .with_flags(o_path);
        // With the guest's numbers translated, as its own open is (DBT).
        match Self::inject_translated(guest, Syscall::from(probe)).await {
            Ok(fd) => {
                let fd = fd as RawFd;
                let refused = self.opened_procfs_name_outside_level(guest, fd, None).await;
                let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                return refused;
            }
            Err(syscalls::Errno::EACCES | syscalls::Errno::EPERM) => {}
            Err(_) => return false,
        }
        let level = self.cfg.proc_level;
        let Some(resolved) = (match self.guest_root_in_view(guest) {
            Some(None) => {
                let spelling: Option<PathBuf> =
                    call.path().and_then(|path| path.read(&guest.memory()).ok());
                match spelling {
                    Some(path) => {
                        Self::prepare_guest_cwd(guest, call.dirfd(), &path).await;
                        Self::observed_path(guest, call.dirfd(), &path)
                            .ok()
                            .filter(|observed| observed.is_absolute())
                            .and_then(|observed| resolve_links(&observed))
                    }
                    None => None,
                }
            }
            _ => None,
        }) else {
            return false;
        };
        if !procfs_name(&resolved)
            .is_some_and(|name| name != ProcfsName::Undecided && name.absent_at(level))
        {
            return false;
        }
        let path = resolved.as_os_str().as_bytes();
        let Some(page) = map_transient(guest, path.len() + 1).await else {
            return false;
        };
        let mut contents = path.to_vec();
        contents.push(0);
        let direct = if guest.memory().write_exact(page, &contents).is_ok() {
            Self::inject_translated(
                guest,
                Syscall::from(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(page.as_raw() as *const libc::c_char))
                        .with_flags(o_path),
                ),
            )
            .await
        } else {
            Err(syscalls::Errno::EFAULT)
        };
        unmap_transient(guest, page, path.len() + 1).await;
        match direct {
            Ok(fd) => {
                let _ = guest
                    .inject(syscalls::Close::new().with_fd(fd as RawFd))
                    .await;
                false
            }
            Err(error) => matches!(error, syscalls::Errno::EACCES | syscalls::Errno::EPERM),
        }
    }
}

/// The path of the `AF_UNIX` address of `length` bytes at `address` in the
/// guest's memory, if it is one with a path.
fn socket_address_path<G: Guest<Detcore<T>>, T: RecordOrReplay>(
    guest: &G,
    address: usize,
    length: usize,
) -> Option<PathBuf> {
    if length > std::mem::size_of::<libc::sockaddr_un>() {
        return None;
    }
    let mut bytes = vec![0; length];
    guest
        .memory()
        .read_exact(Addr::<u8>::from_raw(address)?, &mut bytes)
        .ok()?;
    Some(PathBuf::from(OsStr::from_bytes(unix_address_path(&bytes)?)))
}

#[cfg(test)]
mod tests {
    use detcore_model::config::ProcLevel;

    use super::*;

    #[test]
    fn every_path_operand_is_found() {
        // x86_64 only: aarch64 has openat alone.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            looked_up(Sysno::open),
            Some(Lookups::Paths(vec![(None, 0)]))
        );
        assert_eq!(
            looked_up(Sysno::openat),
            Some(Lookups::Paths(vec![(Some(0), 1)]))
        );
        assert_eq!(
            looked_up(Sysno::symlinkat),
            Some(Lookups::Paths(vec![(Some(1), 2)]))
        );
        assert_eq!(
            looked_up(Sysno::renameat2),
            Some(Lookups::Paths(vec![(Some(0), 1), (Some(2), 3)]))
        );
        assert_eq!(
            looked_up(Sysno::sendto),
            Some(Lookups::SocketAddress {
                address: 4,
                length: 5,
                send: true
            })
        );
        assert_eq!(looked_up(Sysno::sendmsg), Some(Lookups::MessageAddress));
        assert!(looked_up(Sysno::read).is_none());
        // The allocation-free form answers the same paths, and only paths.
        assert_eq!(path_operands(Sysno::openat), Some(&[(Some(0), 1)][..]));
        assert_eq!(
            path_operands(Sysno::renameat2),
            Some(&[(Some(0), 1), (Some(2), 3)][..])
        );
        for other in [Sysno::sendto, Sysno::sendmsg, Sysno::bind, Sysno::read] {
            assert!(path_operands(other).is_none(), "{other}");
        }
        // Detcore refuses these whatever their path names.
        for refused in [
            Sysno::mount,
            Sysno::pivot_root,
            Sysno::acct,
            Sysno::fanotify_mark,
        ] {
            assert!(looked_up(refused).is_none(), "{refused}");
        }
    }

    #[test]
    fn a_lookup_fails_at_its_first_absent_component() {
        let level = ProcLevel::Standard;
        let at = |path: &str| {
            absent_prefix(Path::new(path), level).map(|prefix| prefix.display().to_string())
        };
        assert_eq!(at("/proc/version"), None);
        assert_eq!(at("/proc/kallsyms").as_deref(), Some("/proc/kallsyms"));
        assert_eq!(at("/proc/./kallsyms").as_deref(), Some("/proc/kallsyms"));
        assert_eq!(
            at("/proc/sys/crypto/fips_enabled").as_deref(),
            Some("/proc/sys/crypto")
        );
        // Passing through an omitted name is caught where it is reached.
        assert_eq!(at("/proc/sysvipc/shm").as_deref(), Some("/proc/sysvipc"));
        assert_eq!(at("/proc/kallsyms/..").as_deref(), Some("/proc/kallsyms"));
        assert_eq!(
            at("/proc/sysvipc/../version").as_deref(),
            Some("/proc/sysvipc")
        );
        // A `..` first decides nothing.
        assert_eq!(at("/proc/version/../kallsyms"), None);
        assert_eq!(at("/proc/self/../kallsyms"), None);
        assert_eq!(at("/proc/net/../status"), None);
        // The names in a process's or a thread's directory are decided,
        // whichever spelling reaches it; below a link out of /proc, none is.
        assert_eq!(
            at("/proc/self/limits").as_deref(),
            Some("/proc/self/limits")
        );
        assert_eq!(
            at("/proc/thread-self/attr/current").as_deref(),
            Some("/proc/thread-self/attr")
        );
        assert_eq!(
            at("/proc/1/task/1/limits").as_deref(),
            Some("/proc/1/task/1/limits")
        );
        // A name still left to the kernel is not substituted.
        assert_eq!(at("/proc/self/mem"), None);
        assert_eq!(at("/proc/1/task/1/uid_map"), None);
        assert_eq!(
            at("/proc/1/task/1/wchan").as_deref(),
            Some("/proc/1/task/1/wchan")
        );
        assert_eq!(at("/proc/self/status"), None);
        assert_eq!(at("/proc/self/task/1/status"), None);
        assert_eq!(at("/proc/self/fd/3/x"), None);
        assert_eq!(at("/proc/self/root/proc/kallsyms"), None);
        assert_eq!(at("/proc/self/cwd/limits"), None);
        assert_eq!(
            absent_prefix(Path::new("/proc/self/io"), ProcLevel::Minimal),
            Some(PathBuf::from("/proc/self/io"))
        );
        assert_eq!(at("/tmp/x"), None);
        assert_eq!(at("proc/kallsyms"), None);
        // A name longer than NAME_MAX that the lookup reaches first, or the
        // absent one itself, is the kernel's ENAMETOOLONG.
        assert_eq!(at(&format!("/proc/{}", "x".repeat(NAME_MAX + 1))), None);
        // One below an absent directory is never reached.
        assert_eq!(
            at(&format!("/proc/sysvipc/{}", "x".repeat(NAME_MAX + 1))).as_deref(),
            Some("/proc/sysvipc")
        );
        assert_eq!(
            absent_prefix(Path::new("/proc/interrupts"), ProcLevel::Minimal),
            Some(PathBuf::from("/proc/interrupts"))
        );
    }

    #[test]
    fn only_a_pathname_unix_address_names_a_file() {
        let mut address = (libc::AF_UNIX as libc::sa_family_t).to_ne_bytes().to_vec();
        address.extend_from_slice(b"/proc/kallsyms\0");
        assert_eq!(unix_address_path(&address), Some(&b"/proc/kallsyms"[..]));
        // Without the NUL, the length ends it.
        assert_eq!(
            unix_address_path(&address[..address.len() - 1]),
            Some(&b"/proc/kallsyms"[..])
        );
        let mut abstract_address = address.clone();
        abstract_address[2] = 0;
        assert_eq!(unix_address_path(&abstract_address), None);
        assert_eq!(unix_address_path(&address[..2]), None);
        let mut inet = address.clone();
        inet[..2].copy_from_slice(&(libc::AF_INET as libc::sa_family_t).to_ne_bytes());
        assert_eq!(unix_address_path(&inet), None);
        // The substituted address round-trips, and one too long is refused.
        assert_eq!(
            unix_address(b"/proc/--------")
                .as_deref()
                .and_then(unix_address_path),
            Some(&b"/proc/--------"[..])
        );
        assert_eq!(unix_address(&[b'-'; SUN_PATH_MAX]), None);
    }

    #[test]
    fn links_resolve_as_the_kernel_resolves_them() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink("/proc/kallsyms", &link).unwrap();
        assert_eq!(resolve_links(&link), Some(PathBuf::from("/proc/kallsyms")));
        let relative = directory.path().join("relative");
        std::os::unix::fs::symlink("link", &relative).unwrap();
        assert_eq!(
            resolve_links(&relative),
            Some(PathBuf::from("/proc/kallsyms"))
        );
        assert_eq!(
            resolve_links(Path::new("/proc/self/../kallsyms")),
            Some(PathBuf::from("/proc/kallsyms"))
        );
        assert_eq!(
            resolve_links(Path::new("/proc/thread-self/../../../sysvipc")),
            Some(PathBuf::from("/proc/sysvipc"))
        );
        // `net` is a link into the process's directory.
        assert_eq!(
            resolve_links(Path::new("/proc/net/../../kallsyms")),
            Some(PathBuf::from("/proc/kallsyms"))
        );
        assert_eq!(
            resolve_links(Path::new("/proc/net/../limits")),
            Some(PathBuf::from("/proc/1/limits"))
        );
        let looped = directory.path().join("loop");
        std::os::unix::fs::symlink("loop", &looped).unwrap();
        assert_eq!(resolve_links(&looped), None);
    }

    /// A lookup follows a link its last name names where the spelling ends
    /// in `/`, `.` or `..`, whatever the call's flags.
    #[test]
    fn a_spelling_that_ends_in_a_slash_or_a_dot_follows_its_last_link() {
        for (spelling, follows) in [
            ("link", false),
            ("dir/link", false),
            ("/abs/link", false),
            ("link.", false),
            ("link..", false),
            ("link/", true),
            ("link/.", true),
            ("link/..", true),
            (".", true),
            ("..", true),
            ("dir/./link", false),
        ] {
            assert_eq!(
                follows_last_name(Path::new(spelling)),
                follows,
                "{spelling:?}"
            );
        }
    }

    /// The failures Linux reports once its lookup found an object are the
    /// ones a late check of the object looks at; those it reports before its
    /// lookup, and `ENOENT`, are not.
    #[test]
    fn only_a_failure_after_the_lookup_is_looked_at() {
        for errno in [
            Errno::EACCES,
            Errno::EPERM,
            Errno::EROFS,
            Errno::ENOTDIR,
            Errno::ELOOP,
            Errno::EISDIR,
        ] {
            assert!(fails_after_lookup(errno), "{errno}");
        }
        for errno in [
            Errno::ENOENT,
            Errno::EINVAL,
            Errno::EFAULT,
            Errno::EBADF,
            Errno::EMFILE,
            Errno::ENAMETOOLONG,
            Errno::EINTR,
        ] {
            assert!(!fails_after_lookup(errno), "{errno}");
        }
        #[cfg(target_arch = "x86_64")]
        assert!(is_access_probe(Sysno::access));
        assert!(is_access_probe(Sysno::faccessat));
        assert!(is_access_probe(Sysno::faccessat2));
        assert!(!is_access_probe(Sysno::newfstatat));
    }

    /// What a path in the tool's view reaches on the guest's sysfs mount, as
    /// the kernel resolves it: through links, the last one only where
    /// followed; below an object a lookup cannot go into, the object and the
    /// names after it; and nothing for what is not on sysfs. Skipped where
    /// `/sys` is no sysfs mount.
    #[test]
    fn a_path_resolves_to_what_it_reaches_on_sysfs() {
        if rendered_procfs::sys_device().is_none() {
            return;
        }
        let cpu = Path::new("/sys/devices/system/cpu/cpu0");
        assert_eq!(
            resolved_on_sysfs(&cpu.join("topology/core_id"), true).as_deref(),
            Some(cpu.join("topology/core_id").as_path())
        );
        // `/sys/bus/cpu/devices/cpu0` is a link to cpu0's directory.
        let alias = Path::new("/sys/bus/cpu/devices/cpu0");
        assert_eq!(
            resolved_on_sysfs(&alias.join("topology/core_id"), true).as_deref(),
            Some(cpu.join("topology/core_id").as_path())
        );
        assert_eq!(resolved_on_sysfs(alias, true).as_deref(), Some(cpu));
        assert_eq!(resolved_on_sysfs(alias, false).as_deref(), Some(alias));
        assert_eq!(
            resolved_on_sysfs(&cpu.join("topology/core_id/x"), true).as_deref(),
            Some(cpu.join("topology/core_id/x").as_path())
        );
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(cpu, &link).unwrap();
        assert_eq!(resolved_on_sysfs(&link, true).as_deref(), Some(cpu));
        assert_eq!(
            resolved_on_sysfs(&link.join("topology/core_id"), false).as_deref(),
            Some(cpu.join("topology/core_id").as_path())
        );
        // The link itself is no sysfs object, nor is an ordinary file.
        assert_eq!(resolved_on_sysfs(&link, false), None);
        std::fs::write(directory.path().join("file"), "").unwrap();
        assert_eq!(
            resolved_on_sysfs(&directory.path().join("file"), true),
            None
        );
        assert_eq!(
            resolved_on_sysfs(&directory.path().join("file/x"), true),
            None
        );
    }
}
