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

use super::procfs_levels::map_transient;
use super::procfs_levels::path_operands;
use super::procfs_levels::raw_args;
use super::procfs_levels::unmap_transient;
use crate::procfs::ids;
use crate::record_or_replay::RecordOrReplay;
use crate::tool_local::Detcore;

/// A call whose paths are respelled (with the kernel's numbers, or below a
/// descriptor directory), and where they are staged, which the caller
/// releases once the call returns ([`Detcore::finish_translated_call`]).
pub(crate) struct TranslatedCall {
    pub(crate) call: Syscall,
    staging: Staging,
}

/// Where a [`TranslatedCall`]'s respelled paths are staged.
enum Staging {
    /// In the guest stack scratch below the stack pointer, which needs no
    /// release.
    Scratch,
    /// In a transient private page, unmapped once the call returns.
    Page {
        page: AddrMut<'static, u8>,
        length: usize,
    },
}

/// How far below the stack pointer the guest stack scratch for respelled
/// paths starts: past the red zone and the whole of the scratch Reverie
/// hands out (`Guest::stack`: the red zone's 128 bytes and 896 more, on
/// ptrace), so a stack guard alive around the call is never overlapped and
/// none is checked out (a second checkout while one is alive panics,
/// `reverie-ptrace/src/stack.rs`).
const SCRATCH_GAP: usize = 1024 + 64;

/// The most bytes of respelled paths, NULs included, staged in the guest
/// stack scratch; more go to a transient page. A path respelled below a
/// descriptor directory starts with a short spelling of it
/// (`/proc/self/fd/N`).
const SCRATCH_LIMIT: usize = 512;

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

    /// Whether [`Self::procfs_translated_call`] can translate anything in
    /// `call`: numbers are translated on this thread and `call` looks up a
    /// path, or a path of `call` may lead below a descriptor directory
    /// (`Detcore::may_respell_below_descriptor_directory`). Where this is
    /// false, that function answers `Ok(None)` at once, so a caller that asks
    /// first may skip it.
    pub(crate) fn may_translate_procfs_call<G: Guest<Self>>(guest: &G, call: &Syscall) -> bool {
        (Self::translates_procfs_numbers(guest) && path_operands(call.number()).is_some())
            || Self::may_respell_below_descriptor_directory(guest, call)
    }

    /// `call` with each path it looks up that names a process or thread by
    /// the guest's number spelled with the kernel's numbers instead (see the
    /// module documentation and [`Self::kernel_spelling`]): `Ok(None)` where
    /// no path changes, and an error, never the untranslated call, where the
    /// respelled paths cannot be staged. A path is respelled where it is
    /// absolute, or relative to a directory the tool can resolve
    /// ([`Self::relative_base`]).
    ///
    /// A path that leads below a descriptor directory Detcore serves is
    /// respelled as Linux resolves it instead
    /// (`Detcore::respelled_below_descriptor_directory`), and looked up from
    /// the working directory (`AT_FDCWD`) at its absolute spelling. The
    /// respelled paths are staged for this one call alone
    /// ([`Self::stage_translated`]): in the guest stack scratch, or, where
    /// that cannot hold them, a transient private page mapped for the call
    /// and unmapped as it returns ([`Self::finish_translated_call`]); the
    /// guest never learns either address. Every call so translated is one
    /// that holds the turn while it runs (no path call is committed as a
    /// background turn), so no other guest thread runs while they are
    /// staged, outside `--no-sequentialize-threads`.
    pub(crate) async fn procfs_translated_call<G: Guest<Self>>(
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Option<TranslatedCall>, Errno> {
        Self::translated_call_with_scratch(guest, call, true).await
    }

    async fn translated_call_with_scratch<G: Guest<Self>>(
        guest: &mut G,
        call: &Syscall,
        allow_scratch: bool,
    ) -> Result<Option<TranslatedCall>, Errno> {
        if !Self::may_translate_procfs_call(guest, call) {
            return Ok(None);
        }
        let (sysno, args) = call.into_parts();
        let mut values = [
            args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5,
        ];
        if Self::may_respell_below_descriptor_directory(guest, call) {
            // Boxed: the respelling's future is large, and every handler that
            // injects a path call through this one would carry it inline,
            // while a path below a placeholder descriptor is rare.
            let respelled =
                Box::pin(Self::respelled_below_descriptor_directory(guest, call)).await?;
            if !respelled.is_empty() {
                let staged = respelled
                    .into_iter()
                    .map(|operand| {
                        if let Some(dirfd) = operand.dirfd {
                            values[dirfd] = libc::AT_FDCWD as isize as usize;
                        }
                        let mut bytes = operand.bytes;
                        bytes.push(0);
                        (operand.path, bytes)
                    })
                    .collect();
                return Self::stage_translated(guest, sysno, values, staged, allow_scratch)
                    .await
                    .map(Some);
            }
        }
        if !Self::translates_procfs_numbers(guest) {
            return Ok(None);
        }
        let Some(operands) = path_operands(sysno) else {
            return Ok(None);
        };
        let mut respelled = Vec::new();
        for &(dirfd, argument) in operands {
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
        Self::stage_translated(guest, sysno, values, respelled, allow_scratch)
            .await
            .map(Some)
    }

    /// The call `sysno` with arguments `values`, each path argument in
    /// `respelled` (by index, with its NUL) pointing at its bytes, staged as
    /// `Detcore::inject_fstat` stages its buffer
    /// (<https://github.com/rrnewton/hermit/issues/3328>): in the guest stack
    /// scratch where the paths fit in [`SCRATCH_LIMIT`] bytes, the scratch
    /// overlaps no other argument the call may read, and it is writable;
    /// otherwise in a transient private page mapped for the call. The
    /// scratch maps nothing, so the guest's address space and the placement
    /// of its later mappings stay as they were, and nothing is left behind
    /// if the thread is killed inside the call. Only where neither can hold
    /// them does the call fail (ENOMEM) rather than reach a directory other
    /// than the one the guest named.
    async fn stage_translated<G: Guest<Self>>(
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        values: [usize; 6],
        respelled: Vec<(usize, Vec<u8>)>,
        allow_scratch: bool,
    ) -> Result<TranslatedCall, Errno> {
        if allow_scratch
            && let Some(staged) = Self::stage_in_scratch(guest, sysno, values, &respelled).await
        {
            return Ok(staged);
        }
        Self::stage_in_page(guest, sysno, values, respelled).await
    }

    /// [`Self::stage_translated`] in the guest stack scratch, or `None`
    /// where it cannot be: [`SCRATCH_GAP`] below the stack pointer and
    /// down, memory the x86-64 ABI leaves dead (anything below the red zone
    /// may be overwritten at any moment, by a signal frame), written with
    /// the call's paths and read by the call alone, in the caller's turn. A
    /// stack pointer too low, or memory that cannot be written there (a
    /// guard page), falls back to the page.
    async fn stage_in_scratch<G: Guest<Self>>(
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        mut values: [usize; 6],
        respelled: &[(usize, Vec<u8>)],
    ) -> Option<TranslatedCall> {
        let length: usize = respelled.iter().map(|(_, bytes)| bytes.len()).sum();
        if length > SCRATCH_LIMIT {
            return None;
        }
        let rsp = usize::try_from(guest.regs().await.rsp).ok()?;
        let top = rsp.checked_sub(SCRATCH_GAP)?;
        let lowest = top.checked_sub(length)? & !15;
        // Another argument the call reads (a second path, a buffer) must not
        // lie in the scratch, nor in the page below it, where a buffer may
        // start.
        let respelled_arguments = respelled
            .iter()
            .map(|(argument, _)| *argument)
            .collect::<Vec<_>>();
        let overlaps = values.iter().enumerate().any(|(index, value)| {
            !respelled_arguments.contains(&index)
                && (lowest.saturating_sub(4096)..top).contains(value)
        });
        if overlaps {
            return None;
        }
        let mut contents = Vec::with_capacity(length);
        for (argument, bytes) in respelled {
            values[*argument] = lowest + contents.len();
            contents.extend_from_slice(bytes);
        }
        let target = AddrMut::<u8>::from_raw(lowest)?;
        guest.memory().write_exact(target, &contents).ok()?;
        Some(TranslatedCall {
            call: Syscall::from_raw(sysno, raw_args(values)),
            staging: Staging::Scratch,
        })
    }

    /// [`Self::stage_translated`] in a transient private page.
    async fn stage_in_page<G: Guest<Self>>(
        guest: &mut G,
        sysno: reverie::syscalls::Sysno,
        mut values: [usize; 6],
        respelled: Vec<(usize, Vec<u8>)>,
    ) -> Result<TranslatedCall, Errno> {
        let length: usize = respelled.iter().map(|(_, bytes)| bytes.len()).sum();
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
        Ok(TranslatedCall {
            call: Syscall::from_raw(sysno, raw_args(values)),
            staging: Staging::Page { page, length },
        })
    }

    /// Inject `call` directly, with its `/proc` paths translated as
    /// [`Self::procfs_translated_call`] translates them, for the handlers and
    /// the lookups Detcore makes that inject a call rather than hand it to
    /// the record-or-replay tool.
    pub(crate) async fn inject_translated<G: Guest<Self>>(
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Errno> {
        Self::inject_translated_retrying(guest, call, false).await
    }

    /// An auxiliary lookup whose translated paths must not borrow guest
    /// stack scratch: the original syscall may still need bytes that are
    /// absent from this lookup's arguments. Keep the same translation, but
    /// stage changed paths in the existing transient page instead.
    pub(crate) async fn inject_translated_in_page<G: Guest<Self>>(
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Errno> {
        let translated = Self::translated_call_with_scratch(guest, &call, false).await?;
        let injected = translated
            .as_ref()
            .map_or(call, |translated| translated.call);
        let result = guest.inject(injected).await;
        if let Some(translated) = translated {
            Self::finish_translated_call(guest, translated).await;
        }
        result
    }

    /// [`Self::inject_translated`], made again while a signal interrupts it
    /// where `retry` is set (`Guest::inject_with_retry`).
    pub(crate) async fn inject_translated_retrying<G: Guest<Self>>(
        guest: &mut G,
        call: Syscall,
        retry: bool,
    ) -> Result<i64, Errno> {
        let translated = Self::procfs_translated_call(guest, &call).await?;
        let injected = translated
            .as_ref()
            .map_or(call, |translated| translated.call);
        let result = if retry {
            guest.inject_with_retry(injected).await
        } else {
            guest.inject(injected).await
        };
        if let Some(translated) = translated {
            Self::finish_translated_call(guest, translated).await;
        }
        result
    }

    /// Release where a [`TranslatedCall`]'s paths were staged once its call
    /// returned: drop the scratch guard, or unmap the page.
    pub(crate) async fn finish_translated_call<G: Guest<Self>>(
        guest: &mut G,
        translated: TranslatedCall,
    ) {
        match translated.staging {
            Staging::Scratch => {}
            Staging::Page { page, length } => unmap_transient(guest, page, length).await,
        }
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
