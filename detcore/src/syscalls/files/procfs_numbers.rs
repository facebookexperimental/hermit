/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A `/proc` path that names a process or thread by the guest's number
//! reaches the kernel's directory for it where the kernel numbers processes
//! otherwise than Detcore (DBT; slice B3a-4 of
//! <https://github.com/rrnewton/hermit/issues/4266>; see
//! [`crate::procfs::ids`]).
//!
//! The guest's call goes to the kernel with each such path spelled with the
//! kernel's numbers instead, staged on a transient page, as the calls for a
//! name outside the `--proc` level are (see [`super::procfs_levels`]). Every
//! check before the call, and every decision Detcore takes about the path,
//! sees the guest's spelling; only the kernel sees the other. A number that
//! names no process or thread `/proc` lists is spelled `0`, which names
//! nothing, so the kernel's process of that number, if any, is never reached.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::OsString;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use reverie::Guest;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::procfs_levels::Lookups;
use super::procfs_levels::looked_up;
use super::procfs_levels::map_transient;
use super::procfs_levels::raw_args;
use super::procfs_levels::unmap_transient;
use crate::procfs::ids;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// A call whose paths are respelled with the kernel's numbers, and the
/// transient page holding them, which the caller unmaps once the call
/// returns.
pub(crate) struct TranslatedCall {
    pub(crate) call: Syscall,
    pub(crate) page: AddrMut<'static, u8>,
    pub(crate) length: usize,
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether this guest thread's `/proc` paths may need translating: where
    /// its backend registered the thread's kernel ids (DBT, and an in-guest
    /// native exec, whose numberings agree, so its paths come back
    /// unchanged), outside record and replay (whose replayer answers for the
    /// recorded paths), a nested PID namespace (whose numbers are not the
    /// container's) and a guest `chroot` (after which `/proc` need not be
    /// procfs).
    pub(crate) fn translates_procfs_numbers<G: Guest<Self>>(guest: &G) -> bool {
        let state = guest.thread_state();
        state.physical_tid.is_some()
            && !guest.config().recordreplay_modes
            && !state.nested_pid_namespace
            && !state.changed_root.load(Ordering::SeqCst)
    }

    /// The directory the guest's relative spelling, looked up from `dirfd`,
    /// starts at, resolved: the kernel's name for the directory `dirfd` is
    /// open on, or for the working directory with `AT_FDCWD`, which the
    /// tool, on the guest's own thread where numbers are translated (DBT),
    /// reads as the guest's. It is spelled with the kernel's numbers, and
    /// names no link. Only the number of its components, and whether it is
    /// the init's, matter ([`ids::Lookup`]). `None` where the tool has no
    /// view of the guest's descriptors, and for a directory that was
    /// removed.
    fn relative_base<G: Guest<Self>>(guest: &G, dirfd: i32) -> Option<PathBuf> {
        if !guest.config().backend.tool_shares_guest_descriptor_table {
            return None;
        }
        let link = if dirfd == libc::AT_FDCWD {
            PathBuf::from("/proc/thread-self/cwd")
        } else {
            PathBuf::from(format!("/proc/thread-self/fd/{dirfd}"))
        };
        let base = std::fs::read_link(link).ok()?;
        (base.is_absolute() && !base.as_os_str().as_bytes().ends_with(b" (deleted)"))
            .then_some(base)
    }

    /// The guest's `spelling`, absolute, or relative to `dirfd` (or the
    /// working directory for `AT_FDCWD`), whose resolved name is `base`, with
    /// each number it names as a process or thread replaced in place by the
    /// kernel's number for it (see the module documentation). Every other
    /// component is kept, and a relative spelling stays relative, so the
    /// kernel looks the path up as it would the guest's: from the same
    /// directory, through the same links, counted against the same limit.
    ///
    /// Where the meaning of a component depends on where a link before it
    /// leads (a number, or a `..` a number follows, past `cwd`, `root`,
    /// `exe`, a descriptor's link, or a directory outside `/proc`), the
    /// tool, on the guest's own thread, opens the components before it as
    /// the kernel looks them up (`O_PATH`, from the same directory), and
    /// walks the rest from the kernel's name for what it opened. That only
    /// decides which components are numbers: what the tool opens is closed
    /// again. Where it cannot be opened, the kernel's lookup of the same
    /// components fails too, so the rest stays as it is. `None` where no
    /// number changes.
    async fn kernel_spelling<G: Guest<Self>>(
        guest: &mut G,
        dirfd: i32,
        base: Option<PathBuf>,
        spelling: &Path,
    ) -> Option<PathBuf> {
        let names = ids::components(spelling);
        let shares = guest.config().backend.tool_shares_guest_descriptor_table;
        let mut replaced: Vec<(usize, OsString)> = Vec::new();
        let mut base = base;
        let mut offset = 0;
        // The directory the components from `offset` on are looked up from,
        // where a resolution opened one.
        let mut directory: Option<OwnedFd> = None;
        while offset < names.len() {
            let absolute = offset == 0 && spelling.is_absolute();
            let rest = if absolute {
                spelling.to_path_buf()
            } else {
                ids::rebuilt_like(names[offset..].to_vec(), Path::new(""))
            };
            let lookup = ids::Lookup::new(base.as_deref(), &rest);
            let named = lookup
                .ids()
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let map = if named.is_empty() {
                BTreeMap::new()
            } else {
                let kernel = crate::tool_global::procfs_ids(guest, true, named.clone()).await;
                named.into_iter().zip(kernel).collect::<BTreeMap<_, _>>()
            };
            replaced.extend(
                lookup
                    .translations(|id| map.get(&id).copied().flatten())
                    .into_iter()
                    .map(|(position, name)| (offset + position, name)),
            );
            // Only the tool on the guest's own thread looks up as the guest.
            let Some(at) = lookup.stop().filter(|_| shares) else {
                break;
            };
            let mut prefix = names[offset..offset + at].to_vec();
            for (position, name) in &replaced {
                if (offset..offset + at).contains(position) {
                    prefix[position - offset] = name.clone();
                }
            }
            let from = directory.as_ref().map_or(dirfd, |fd| fd.as_raw_fd());
            let Some(opened) = open_path(from, prefix, absolute) else {
                break;
            };
            directory = Some(opened);
            offset += at;
            base = directory.as_ref().and_then(resolved_name);
            // A removed directory has no name to walk from, and no entries:
            // only `.` and `..` lead on from it.
            while base.is_none() && offset < names.len() {
                let name = &names[offset];
                if name != "." && name != ".." {
                    break;
                }
                let from = directory.as_ref().map_or(dirfd, |fd| fd.as_raw_fd());
                let Some(opened) = open_path(from, vec![name.clone()], false) else {
                    break;
                };
                directory = Some(opened);
                offset += 1;
                base = directory.as_ref().and_then(resolved_name);
            }
            if base.is_none() {
                break;
            }
        }
        if replaced.is_empty() {
            return None;
        }
        let mut names = names;
        for (position, name) in replaced {
            names[position] = name;
        }
        Some(ids::rebuilt_like(names, spelling))
    }

    /// `call` with each path it looks up that names a process or thread by
    /// the guest's number spelled with the kernel's numbers instead (see the
    /// module documentation and [`Self::kernel_spelling`]): `Ok(None)` where
    /// no path changes, and an error, never the untranslated call, where the
    /// respelled paths cannot be staged. A path is respelled where it is
    /// absolute, or relative to a directory the tool can resolve
    /// ([`Self::relative_base`]).
    pub(crate) async fn procfs_translated_call<G: Guest<Self>>(
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Option<TranslatedCall>, Errno> {
        if !Self::translates_procfs_numbers(guest) {
            return Ok(None);
        }
        let (sysno, args) = call.into_parts();
        let Some(Lookups::Paths(operands)) = looked_up(sysno) else {
            return Ok(None);
        };
        let mut values = [
            args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
        ];
        let mut respelled = Vec::new();
        for (dirfd, argument) in operands {
            let Some(spelling) = PathPtr::from_ptr(values[argument] as *const libc::c_char)
                .and_then(|address| address.read(&guest.memory()).ok())
            else {
                continue;
            };
            // Only a spelling that can name a process or a thread is looked
            // at further.
            if !spelling
                .as_os_str()
                .as_bytes()
                .iter()
                .any(u8::is_ascii_digit)
            {
                continue;
            }
            let base = if spelling.is_absolute() {
                None
            } else {
                let dirfd = dirfd.map_or(libc::AT_FDCWD, |position| values[position] as i32);
                match Self::relative_base(guest, dirfd) {
                    Some(base) => Some(base),
                    None => continue,
                }
            };
            let dirfd = dirfd.map_or(libc::AT_FDCWD, |position| values[position] as i32);
            if let Some(translated) = Self::kernel_spelling(guest, dirfd, base, &spelling).await {
                let mut bytes = translated.into_os_string().into_vec();
                bytes.push(0);
                respelled.push((argument, bytes));
            }
        }
        if respelled.is_empty() {
            return Ok(None);
        }
        let length: usize = respelled.iter().map(|(_, bytes)| bytes.len()).sum();
        // With no room for the respelled paths, the call fails rather than
        // reaching the kernel's process of the guest's number.
        let page = map_transient(guest, length).await.ok_or(Errno::ENOMEM)?;
        let mut contents = Vec::with_capacity(length);
        for (argument, bytes) in &respelled {
            values[*argument] = page.as_raw() + contents.len();
            contents.extend_from_slice(bytes);
        }
        if guest.memory().write_exact(page, &contents).is_err() {
            unmap_transient(guest, page, length).await;
            return Err(Errno::ENOMEM);
        }
        Ok(Some(TranslatedCall {
            call: Syscall::from_raw(sysno, raw_args(values)),
            page,
            length,
        }))
    }

    /// Inject `call` directly, with its `/proc` paths translated as
    /// [`Self::procfs_translated_call`] translates them, for the handlers and
    /// the lookups Detcore makes that inject a call rather than hand it to
    /// the record-or-replay tool.
    pub(crate) async fn inject_translated<G: Guest<Self>>(
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Errno> {
        let translated = Self::procfs_translated_call(guest, &call).await?;
        let result = guest
            .inject(
                translated
                    .as_ref()
                    .map_or(call, |translated| translated.call),
            )
            .await;
        if let Some(translated) = translated {
            Self::finish_translated_call(guest, translated).await;
        }
        result
    }

    /// Unmap the page of a [`TranslatedCall`] once its call returned.
    pub(crate) async fn finish_translated_call<G: Guest<Self>>(
        guest: &mut G,
        translated: TranslatedCall,
    ) {
        unmap_transient(guest, translated.page, translated.length).await;
    }

    /// The kernel's number for the guest's process or thread number `id`
    /// where numbers are translated (see [`Self::translates_procfs_numbers`]),
    /// `id` itself elsewhere, and `None` for a number of no process or thread
    /// `/proc` lists.
    pub(crate) async fn kernel_procfs_number<G: Guest<Self>>(
        guest: &mut G,
        id: i32,
    ) -> Option<i32> {
        if !Self::translates_procfs_numbers(guest) {
            return Some(id);
        }
        crate::tool_global::procfs_ids(guest, true, vec![id])
            .await
            .into_iter()
            .next()
            .flatten()
    }

    /// The guest's spelling of the `/proc` directory open as `fd`, with the
    /// kernel's numbers in the kernel's name for it replaced by the guest's
    /// (see [`Self::translates_procfs_numbers`]): bound when the descriptor
    /// was opened, as the kernel binds it, so a `/proc/self/task` a child
    /// inherited still names its opener's threads. `None` for a descriptor
    /// not below `/proc`, or one that names a process or thread `/proc` does
    /// not list for the guest. The tool shares the guest's descriptor table
    /// there (DBT), so its own view of the descriptor is the guest's.
    pub(crate) async fn guest_procfs_spelling<G: Guest<Self>>(
        guest: &mut G,
        fd: std::os::fd::RawFd,
    ) -> Option<PathBuf> {
        if !Self::translates_procfs_numbers(guest)
            || !guest.config().backend.tool_shares_guest_descriptor_table
        {
            return None;
        }
        let kernel = std::fs::read_link(format!("/proc/thread-self/fd/{fd}")).ok()?;
        let held = ids::named_ids(&kernel);
        if held.is_empty() {
            return kernel.starts_with("/proc").then_some(kernel);
        }
        let numbers = Self::guest_procfs_numbers(guest, &held).await?;
        if held.iter().any(|kernel| !numbers.contains_key(kernel)) {
            return None;
        }
        Some(ids::translate(&kernel, |id| numbers.get(&id).copied()).unwrap_or(kernel))
    }

    /// The guest's number for each kernel process or thread number in `ids`,
    /// where numbers are translated (see [`Self::translates_procfs_numbers`]),
    /// as `/proc` spells it; `None` for a number of no process or thread
    /// `/proc` lists, and for every number elsewhere.
    pub(crate) async fn guest_procfs_numbers<G: Guest<Self>>(
        guest: &mut G,
        kernel: &[i32],
    ) -> Option<BTreeMap<i32, i32>> {
        if !Self::translates_procfs_numbers(guest) || kernel.is_empty() {
            return None;
        }
        let guest_ids = crate::tool_global::procfs_ids(guest, false, kernel.to_vec()).await;
        Some(
            kernel
                .iter()
                .zip(guest_ids)
                .filter_map(|(kernel, guest_id)| Some((*kernel, guest_id?)))
                .collect(),
        )
    }
}

/// Open the components `names`, absolute or relative to `from`, as the
/// kernel looks up a directory on the way to a path's last component
/// (following every link), for its name only (`O_PATH`). `"."` for none.
/// `None` where the kernel's lookup fails.
fn open_path(from: RawFd, names: Vec<OsString>, absolute: bool) -> Option<OwnedFd> {
    let mut path = if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };
    path.extend(names);
    if path.as_os_str().is_empty() {
        path.push(".");
    }
    let path = CString::new(path.into_os_string().into_vec()).ok()?;
    // SAFETY: a valid, NUL-terminated path; the result is a new descriptor
    // or -1.
    let fd = unsafe {
        libc::openat(
            from,
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    // SAFETY: a descriptor openat just returned, owned by nothing else.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The kernel's name for the directory open as `fd`: absolute, with no link
/// among its components and the kernel's numbers. `None` for a directory
/// that was removed.
fn resolved_name(fd: &OwnedFd) -> Option<PathBuf> {
    let name = std::fs::read_link(format!("/proc/thread-self/fd/{}", fd.as_raw_fd())).ok()?;
    (name.is_absolute() && !name.as_os_str().as_bytes().ends_with(b" (deleted)")).then_some(name)
}
