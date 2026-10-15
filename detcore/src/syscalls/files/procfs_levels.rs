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

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

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
const MSGHDR_SIZE: usize = std::mem::size_of::<libc::msghdr>();
/// The longest pathname an `AF_UNIX` address holds, with its NUL.
const SUN_PATH_MAX: usize = 108;

/// What a call names that a lookup may find, by argument position.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lookups {
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
fn looked_up(sysno: Sysno) -> Option<Lookups> {
    let paths = match sysno {
        Sysno::open
        | Sysno::creat
        | Sysno::stat
        | Sysno::lstat
        | Sysno::access
        | Sysno::readlink
        | Sysno::execve
        | Sysno::chdir
        | Sysno::chroot
        | Sysno::mkdir
        | Sysno::mknod
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::truncate
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::utime
        | Sysno::utimes
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::removexattr
        | Sysno::lremovexattr
        | Sysno::statfs => vec![(None, 0)],
        Sysno::openat
        | Sysno::newfstatat
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
        | Sysno::utimensat
        | Sysno::futimesat => vec![(Some(0), 1)],
        Sysno::symlink => vec![(None, 1)],
        Sysno::symlinkat => vec![(Some(1), 2)],
        Sysno::inotify_add_watch => vec![(None, 1)],
        Sysno::rename | Sysno::link => vec![(None, 0), (None, 1)],
        Sysno::renameat | Sysno::renameat2 | Sysno::linkat => {
            vec![(Some(0), 1), (Some(2), 3)]
        }
        Sysno::bind | Sysno::connect => {
            return Some(Lookups::SocketAddress {
                address: 1,
                length: 2,
                send: false,
            });
        }
        Sysno::sendto => {
            return Some(Lookups::SocketAddress {
                address: 4,
                length: 5,
                send: true,
            });
        }
        Sysno::sendmsg => return Some(Lookups::MessageAddress),
        _ => return None,
    };
    Some(Lookups::Paths(paths))
}

/// The prefix of the absolute `path` that ends at the first component the
/// lookup finds absent at `level`, walking its normal components from the
/// left, or `None` where it finds none absent. The walk stops, deciding
/// nothing, at a `..` or at a per-process name (whose links the kernel
/// resolves), and where a component it reaches, the absent one included, is
/// longer than `NAME_MAX` (the kernel's ENAMETOOLONG comes first).
fn absent_prefix(path: &Path, level: detcore_model::config::ProcLevel) -> Option<PathBuf> {
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
        match procfs_name(&prefix) {
            None => {}
            Some(ProcfsName::Undecided) => return None,
            Some(name) if name.absent_at(level) => return Some(prefix),
            Some(_) => {}
        }
    }
    None
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
async fn map_transient<T: RecordOrReplay, G: Guest<Detcore<T>>>(
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
async fn unmap_transient<T: RecordOrReplay, G: Guest<Detcore<T>>>(
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
fn raw_args(values: [usize; 6]) -> syscalls::SyscallArgs {
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
    /// The path is a decided system-wide procfs name (not per-process).
    system_wide: bool,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The kernel's answer for a call that looks up a `/proc` name outside
    /// the run's level (see the module documentation), as the guest spelled
    /// it: absolute, relative to its working directory, or relative to a
    /// directory descriptor Detcore knows the path of. A spelling Detcore
    /// cannot place, or one with `..` or a per-process name before the
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
        let _ = rendered_procfs::proc_device();
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
        // Whether a refusal for permission can only be the KVM executor's.
        let mut confined = true;
        let replacements = match lookups {
            Lookups::Paths(operands) => {
                let mut replacements = Vec::new();
                for (dirfd, path) in operands {
                    let decision = match PathPtr::from_ptr(values[path] as *const libc::c_char)
                        .and_then(|address| address.read(&guest.memory()).ok())
                    {
                        Some(spelling) => {
                            self.decide(guest, dirfd.map(|p| values[p] as i32), &spelling, &root)
                        }
                        None => Decision::default(),
                    };
                    confined &= decision.system_wide;
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
                        self.substituted_socket_address(guest, name, length as usize, true, &root)
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
        let in_view = match root {
            None => observed.clone(),
            Some(root) => root.join(observed.strip_prefix("/").unwrap_or(&observed)),
        };
        let system_wide = procfs_name(&in_view).is_some_and(|name| name != ProcfsName::Undecided);
        let substituted = absent_prefix(&in_view, self.cfg.proc_level).and_then(|prefix| {
            let parent = prefix.parent()?;
            let on_procfs = std::fs::metadata(parent)
                .ok()
                .is_some_and(|metadata| Some(metadata.dev()) == rendered_procfs::proc_device());
            if !on_procfs {
                return None;
            }
            // Back in the guest's view.
            let parent = match root {
                None => parent.to_path_buf(),
                Some(root) => Path::new("/").join(parent.strip_prefix(root).ok()?),
            };
            let name_length = prefix.file_name()?.len();
            let mut substituted = parent.as_os_str().as_bytes().to_vec();
            if !substituted.ends_with(b"/") {
                substituted.push(b'/');
            }
            substituted.resize(substituted.len() + name_length, b'-');
            // What follows the omitted name stays: the kernel's walk ends at
            // it, before them, but which name is the last decides how a call
            // treats it (rename looks up the last one's directory first).
            let rest = in_view.strip_prefix(&prefix).ok()?;
            if !rest.as_os_str().is_empty() {
                substituted.push(b'/');
                substituted.extend_from_slice(rest.as_os_str().as_bytes());
            }
            // So does a trailing slash (O_CREAT then fails with EISDIR).
            if bytes.ends_with(b"/") {
                substituted.push(b'/');
            }
            Some(substituted)
        });
        Decision {
            substituted,
            system_wide,
        }
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
        let substituted = self.decide(guest, None, &path, root).substituted?;
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
    /// Where every path the call looks up is a decided system-wide procfs
    /// name (`confined`), a refusal for permission is ENOENT: Linux's procfs
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
                if matches!(sysno, Sysno::open | Sysno::openat | Sysno::creat) {
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

    /// Whether the socket `fd` is a datagram socket, as the socket says
    /// (`getsockopt(SO_TYPE)`, into a transient page): only such a socket
    /// looks up the address a send gives. `None` where there is no room for
    /// the question. Asked only for a send whose address reaches an omitted
    /// name, outside record and replay.
    async fn socket_is_datagram<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> Option<bool> {
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

    /// The guest's root in the tool's view: `Some(None)` where they are the
    /// same (no `chroot`), `Some(Some(root))` after a `chroot`, and `None`
    /// where it cannot be known alike in record and replay (a `chroot` under
    /// record or replay, whose replay root is the replayer's own), or at all
    /// (the tool's `/proc` unreadable from inside the new root).
    fn guest_root_in_view<G: Guest<Self>>(&self, guest: &G) -> Option<Option<PathBuf>> {
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
            _ => self.descriptor_is_on_procfs(guest, fd).await,
        };
        if !on_procfs {
            return false;
        }
        match visible.then(|| std::fs::read_link(&link)) {
            Some(Ok(path)) if path.is_absolute() => {
                procfs_name(&path).is_some_and(|name| name.absent_at(self.cfg.proc_level))
            }
            _ => true,
        }
    }

    /// Whether the descriptor `fd` is on procfs, as the descriptor says
    /// (`fstatfs`, into a transient page); `false` where it cannot say.
    async fn descriptor_is_on_procfs<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) -> bool {
        const LENGTH: usize = std::mem::size_of::<libc::statfs>();
        let Some(page) = map_transient(guest, LENGTH).await else {
            return false;
        };
        let mut contents = [0u8; LENGTH];
        let answered = guest
            .inject(Syscall::from_raw(
                Sysno::fstatfs,
                raw_args([fd as usize, page.as_raw(), 0, 0, 0, 0]),
            ))
            .await
            .is_ok()
            && guest.memory().read_exact(page, &mut contents).is_ok();
        unmap_transient(guest, page, LENGTH).await;
        // `f_type` is the first field.
        answered
            && i64::from_ne_bytes(contents[..8].try_into().expect("8 bytes"))
                == libc::PROC_SUPER_MAGIC
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
        match guest.inject(probe).await {
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
            Some(None) => call
                .path()
                .and_then(|path| path.read(&guest.memory()).ok())
                .and_then(|path| Self::observed_path(guest, call.dirfd(), &path).ok())
                .filter(|observed| observed.is_absolute())
                .and_then(|observed| resolve_links(&observed)),
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
            guest
                .inject(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(page.as_raw() as *const libc::c_char))
                        .with_flags(o_path),
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

#[cfg(test)]
mod tests {
    use detcore_model::config::ProcLevel;

    use super::*;

    #[test]
    fn every_path_operand_is_found() {
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
        // A `..` or a per-process name first decides nothing.
        assert_eq!(at("/proc/version/../kallsyms"), None);
        assert_eq!(at("/proc/self/../kallsyms"), None);
        assert_eq!(at("/proc/net/../status"), None);
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
}
