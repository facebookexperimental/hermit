/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Descriptors that would make a guest the server another guest's exit waits
//! for, refused while process exits complete asynchronously.
//!
//! With `BackendCapabilities::process_exits_complete_asynchronously`, Detcore
//! selects no turn between a process's exit grant and its physical exit. A native exit that needs another guest to act therefore
//! never completes. Coord rulings A and D.1 (dev-hermit
//! ai_docs/transient/liteinst-inguest-exit-control-design-20261006.md) refuse,
//! by name, the two descriptors that make a guest such a server:
//!
//! - a FUSE or CUSE device (`/dev/fuse` or `/dev/cuse`): the exiting guest's
//!   close of a file that guest serves sends a synchronous flush to it.
//!   Recognised by the character device's number (misc major 10, minors 229
//!   and 203), never by path. A plain file on a FUSE filesystem is not
//!   refused: when no guest holds the device, the server is outside the guest
//!   set.
//! - a seccomp user-notification listener: an exit-time system call the
//!   listener's filter traps waits for the listener's reply. Recognised by the
//!   descriptor's anon inode, `anon_inode:seccomp notify`, never by a name.
//!   Creating one (`SECCOMP_FILTER_FLAG_NEW_LISTENER`) is already refused,
//!   because guests cannot install filters; an inherited filter with no
//!   guest-held listener stays allowed.
//!
//! A descriptor is checked where it arrives: from an open, through
//! `SCM_RIGHTS` (`recvmsg`, `recvmmsg`) or through `pidfd_getfd`
//! (`Detcore::refuse_exit_dependencies`). The in-guest runtime checks the
//! descriptors a process starts with ([`descriptor_exit_dependency`] on
//! `/proc/self/fd`). userfaultfd is not refused (ruling D.2): the reference
//! backend supports it, and a stuck exit is caught by the backend's watchdog.
//!
//! Detcore's refusal names the capability and the descriptor, never a backend.
//! The backend layer, which knows which backend runs and what to try instead,
//! adds that through [`set_backend_advice`].

use std::os::fd::RawFd;
use std::path::Path;

use detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
use reverie::Guest;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

use crate::record_or_replay::RecordOrReplay;
use crate::syscalls::scm_rights::delivered_descriptors;
use crate::tool_global::unrecoverable_shutdown;
use crate::tool_local::Detcore;

/// What the backend layer adds to a refusal; see [`set_backend_advice`].
static BACKEND_ADVICE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Sets the sentence a refusal ends with: which backend refused, and what to
/// run instead. The backend layer calls it once, before Detcore runs; a later
/// call is ignored.
pub fn set_backend_advice(advice: &'static str) {
    let _ = BACKEND_ADVICE.set(advice);
}

/// The refusal for a guest holding `what` at descriptor `fd`, which arrived as
/// `how`.
fn refusal(what: &str, fd: RawFd, how: &str) -> String {
    let mut refusal = format!(
        "hermit: refusing a guest that holds {what} (descriptor {fd}, {how}): this backend \
         completes process exits asynchronously, so no other guest runs until an exit is \
         complete, and a guest serving that descriptor could make another guest's exit wait \
         forever."
    );
    if let Some(advice) = BACKEND_ADVICE.get() {
        refusal.push(' ');
        refusal.push_str(advice);
    }
    refusal
}

/// The misc character devices' major number.
const MISC_MAJOR: u64 = 10;
/// `/dev/fuse` (`FUSE_MINOR` in the kernel's `miscdevice.h`).
const FUSE_MINOR: u64 = 229;
/// `/dev/cuse` (`CUSE_MINOR`).
const CUSE_MINOR: u64 = 203;

/// What `readlink` reports for a seccomp user-notification listener.
pub const SECCOMP_NOTIFY_LINK: &str = "anon_inode:seccomp notify";

/// The major and minor numbers of a `dev_t`, as glibc's `major` and `minor`.
fn device_numbers(rdev: u64) -> (u64, u64) {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    (major, minor)
}

/// What a character device numbered `rdev` would make its holder serve, or
/// `None`.
pub fn device_exit_dependency(rdev: u64) -> Option<&'static str> {
    match device_numbers(rdev) {
        (MISC_MAJOR, FUSE_MINOR) => Some("a FUSE device (/dev/fuse)"),
        (MISC_MAJOR, CUSE_MINOR) => Some("a CUSE device (/dev/cuse)"),
        _ => None,
    }
}

/// What the descriptor at `fd_link`, a `/proc/<pid>/fd/<n>` entry, would make
/// its holder serve, or `None`. A descriptor that cannot be read is not
/// refused.
pub fn descriptor_exit_dependency(fd_link: &Path) -> Option<&'static str> {
    // Raw readlinkat and newfstatat (see crate::util::raw_syscall): the
    // in-guest runtime runs this inside the guest before Detcore installs.
    descriptor_exit_dependency_with(fd_link, |number, args| {
        // SAFETY: the shared inspection below supplies its live Tool buffers
        // for these synchronous read-only calls.
        let result =
            unsafe { crate::util::raw_syscall(number as libc::c_long, args.map(|arg| arg as u64)) };
        reverie::Errno::from_ret(result as usize)
    })
}

/// The same inspection for startup and callbacks. Only the instruction used
/// for the two Tool-side syscalls differs; neither is a guest injection.
fn descriptor_exit_dependency_with(
    fd_link: &Path,
    mut execute: impl FnMut(Sysno, [usize; 6]) -> Result<usize, reverie::Errno>,
) -> Option<&'static str> {
    use std::os::unix::ffi::OsStrExt;

    let link = std::ffi::CString::new(fd_link.as_os_str().as_bytes()).ok()?;
    let mut target = [0_u8; 64];
    let length = execute(
        Sysno::readlinkat,
        [
            libc::AT_FDCWD as usize,
            link.as_ptr() as usize,
            target.as_mut_ptr() as usize,
            target.len(),
            0,
            0,
        ],
    );
    if let Ok(length) = length
        && &target[..length] == SECCOMP_NOTIFY_LINK.as_bytes()
    {
        return Some("a seccomp user-notification listener");
    }
    // Follows the descriptor to the file it refers to.
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    let status = execute(
        crate::arch::FSTATAT,
        [
            libc::AT_FDCWD as usize,
            link.as_ptr() as usize,
            (&raw mut metadata) as usize,
            0,
            0,
            0,
        ],
    );
    if status != Ok(0) || metadata.st_mode & libc::S_IFMT != libc::S_IFCHR {
        return None;
    }
    device_exit_dependency(metadata.st_rdev)
}

/// The first descriptor listed in `fd_directory` (a `/proc/<pid>/fd`
/// directory) that would make its holder serve another guest's exit: its name
/// in the directory and what it is. The in-guest runtime checks
/// `/proc/self/fd` with it before installing Detcore. A directory that cannot
/// be listed is an error, never a pass. Listed with
/// [`crate::util::find_in_directory`]: this runs inside the guest, where
/// `std::fs::read_dir` would leave the listing in the guest's heap.
pub fn held_exit_dependency(
    fd_directory: &Path,
) -> std::io::Result<Option<(String, &'static str)>> {
    crate::util::find_in_directory(fd_directory, |name| {
        descriptor_exit_dependency(&fd_directory.join(name))
            .map(|what| (name.to_string_lossy().into_owned(), what))
    })
}

impl<T: RecordOrReplay> Detcore<T> {
    /// [`Self::refuse_exit_dependencies`] with the call's memory as the
    /// kernel's view of it holds it (`EntryImport::reinstate_returned`), when
    /// another thread has rewritten it since (`ThreadState::entry_import`),
    /// and that rewrite restored after: it reads a received message's
    /// descriptors from the call's header. Fails only if the rewrite cannot
    /// be restored.
    pub(crate) async fn refuse_exit_dependencies_in_view<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        ret: i64,
    ) -> Result<(), reverie::Error> {
        let entry = crate::entry_view(guest, call);
        let view = entry
            .as_deref()
            .map(|entry| (entry, entry.reinstate_returned(&mut guest.memory())));
        self.refuse_exit_dependencies(guest, call, ret).await;
        crate::restore_entry_view(guest, view)
    }

    /// Ends the run, by name, when `call`, which returned `ret`, gave the guest
    /// a descriptor that would make it serve another guest's exit; see the
    /// module documentation. Does nothing unless process exits complete
    /// asynchronously.
    pub(crate) async fn refuse_exit_dependencies<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        ret: i64,
    ) {
        if !self.cfg.backend.process_exits_complete_asynchronously || ret < 0 {
            return;
        }
        let (arrived, how): (Vec<RawFd>, &str) = match call {
            Syscall::Openat(_) => (vec![ret as RawFd], "opened"),
            #[cfg(target_arch = "x86_64")]
            Syscall::Open(_) => (vec![ret as RawFd], "opened"),
            _ if call.number() == Sysno::openat2 => (vec![ret as RawFd], "opened"),
            _ if call.number() == Sysno::pidfd_getfd => {
                (vec![ret as RawFd], "received through pidfd_getfd")
            }
            Syscall::Recvmsg(_) | Syscall::Recvmmsg(_) => {
                let Some(received) = delivered_descriptors(&guest.memory(), call, ret) else {
                    return;
                };
                (received, "received through SCM_RIGHTS")
            }
            _ => return,
        };
        let pid = guest.pid().as_raw();
        for fd in arrived {
            let Some(what) = descriptor_exit_dependency_with(
                Path::new(&format!("/proc/{pid}/fd/{fd}")),
                |number, args| {
                    // SAFETY: readlinkat/newfstatat borrow only this inspection's
                    // live Tool-owned path and output buffers. Their operands
                    // name the same native procfs entry as the startup path;
                    // they retain no pointers and return on the original stack.
                    unsafe { guest.tool_syscall(number, args) }
                },
            ) else {
                continue;
            };
            eprintln!("{}", refusal(what, fd, how));
            unrecoverable_shutdown(guest, HERMIT_POLICY_REFUSAL_EXIT).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;
    use crate::syscalls::scm_rights::received_descriptors;

    /// glibc's `makedev`.
    fn makedev(major: u64, minor: u64) -> u64 {
        ((major & 0xfff) << 8) | ((major & !0xfff) << 32) | (minor & 0xff) | ((minor & !0xff) << 12)
    }

    /// A transport error does not change the inspection policy: a failed
    /// readlink still permits the stat check, and neither call is retried.
    #[test]
    fn descriptor_inspection_keeps_operands_results_and_failure_order() {
        use reverie::Errno;

        let cases = [
            (
                Ok(SECCOMP_NOTIFY_LINK.as_bytes()),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 229),
                Some("a seccomp user-notification listener"),
                1,
            ),
            (
                Ok(b"/dev/ordinary".as_slice()),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 229),
                Some("a FUSE device (/dev/fuse)"),
                2,
            ),
            (
                Ok(b"/dev/ordinary".as_slice()),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 203),
                Some("a CUSE device (/dev/cuse)"),
                2,
            ),
            (
                Err(Errno::EACCES),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 229),
                Some("a FUSE device (/dev/fuse)"),
                2,
            ),
            (
                Err(Errno::EINTR),
                Ok(0),
                libc::S_IFREG,
                makedev(10, 229),
                None,
                2,
            ),
            (
                Ok(b"anon_inode:seccomp notify suffix".as_slice()),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 232),
                None,
                2,
            ),
            (
                Ok(b"".as_slice()),
                Err(Errno::EINTR),
                libc::S_IFCHR,
                makedev(10, 229),
                None,
                2,
            ),
            (
                Err(Errno::ENOENT),
                Err(Errno::EBADF),
                libc::S_IFCHR,
                makedev(10, 229),
                None,
                2,
            ),
            (
                Ok(b"/dev/ordinary".as_slice()),
                Ok(1),
                libc::S_IFCHR,
                makedev(10, 229),
                None,
                2,
            ),
            (
                Ok(&[b'x'; 64]),
                Ok(0),
                libc::S_IFCHR,
                makedev(10, 203),
                Some("a CUSE device (/dev/cuse)"),
                2,
            ),
        ];
        for (link_result, stat_result, mode, device, expected, expected_calls) in cases {
            let mut calls = 0;
            let mut path_address = None;
            let actual =
                descriptor_exit_dependency_with(Path::new("/proc/123/fd/456"), |number, args| {
                    calls += 1;
                    assert_eq!(args[0], libc::AT_FDCWD as usize);
                    assert_eq!(&args[4..], &[0, 0]);
                    // SAFETY: the helper lends a live NUL-terminated path to
                    // this synchronous executor, just as to the kernel.
                    let path = unsafe { std::ffi::CStr::from_ptr(args[1] as *const libc::c_char) };
                    assert_eq!(path.to_bytes(), b"/proc/123/fd/456");
                    if let Some(address) = path_address {
                        assert_eq!(args[1], address);
                    } else {
                        path_address = Some(args[1]);
                    }
                    match number {
                        Sysno::readlinkat => {
                            assert_eq!(calls, 1);
                            assert_eq!(args[3], 64);
                            let bytes = link_result?;
                            // SAFETY: the helper lends its 64-byte output,
                            // and every case fits. No pointer is retained.
                            unsafe {
                                std::ptr::copy_nonoverlapping(
                                    bytes.as_ptr(),
                                    args[2] as *mut u8,
                                    bytes.len(),
                                );
                            }
                            Ok(bytes.len())
                        }
                        crate::arch::FSTATAT => {
                            assert_eq!(calls, 2);
                            assert_eq!(args[3], 0, "follow the descriptor's target");
                            // Also fill on failure: a failed stat must not
                            // accidentally classify stale output as success.
                            // SAFETY: the helper lends a valid aligned stat.
                            unsafe {
                                let output = &mut *(args[2] as *mut libc::stat);
                                output.st_mode = mode;
                                output.st_rdev = device;
                            }
                            stat_result
                        }
                        other => panic!("unexpected Tool syscall {other:?}"),
                    }
                });
            assert_eq!(actual, expected);
            assert_eq!(calls, expected_calls);
        }
    }

    #[test]
    fn an_invalid_descriptor_path_executes_no_inspection() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"/proc/self/fd/1\0suffix"));
        assert_eq!(
            descriptor_exit_dependency_with(path, |_, _| panic!("invalid path reached executor")),
            None
        );
    }

    #[test]
    fn only_the_fuse_and_cuse_devices_are_exit_dependencies() {
        assert_eq!(
            device_exit_dependency(makedev(10, 229)),
            Some("a FUSE device (/dev/fuse)")
        );
        assert_eq!(
            device_exit_dependency(makedev(10, 203)),
            Some("a CUSE device (/dev/cuse)")
        );
        // Other misc devices: /dev/kvm, /dev/net/tun, a dynamic minor such as
        // /dev/userfaultfd's, and the same minors under another major.
        for (major, minor) in [
            (10, 232),
            (10, 200),
            (10, 125),
            (1, 229),
            (4, 203),
            (259, 229),
        ] {
            assert_eq!(
                device_exit_dependency(makedev(major, minor)),
                None,
                "{major}:{minor}"
            );
        }
        // Large numbers survive the split encoding.
        assert_eq!(device_numbers(makedev(4095, 1 << 19)), (4095, 1 << 19));
    }

    #[test]
    fn ordinary_descriptors_are_not_exit_dependencies() {
        let file = tempfile::tempfile().unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        let (left, _right) = std::os::unix::net::UnixStream::pair().unwrap();
        for fd in [file.as_raw_fd(), null.as_raw_fd(), left.as_raw_fd()] {
            assert_eq!(
                descriptor_exit_dependency(Path::new(&format!("/proc/self/fd/{fd}"))),
                None,
                "fd {fd}"
            );
        }
        // A closed descriptor reads nothing and is not refused.
        assert_eq!(
            descriptor_exit_dependency(Path::new("/proc/self/fd/987654")),
            None
        );
    }

    #[test]
    fn a_received_seccomp_listener_is_recognised_by_its_anon_inode() {
        // A child installs an allow-everything filter with a listener and sends
        // the listener over SCM_RIGHTS; the filter stays in the child. Only raw
        // system calls on buffers prepared before the fork run in the child.
        const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
        const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
        const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
        let (sender, receiver) = std::os::unix::net::UnixDatagram::pair().unwrap();
        let filter = [libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: SECCOMP_RET_ALLOW,
        }];
        let program = libc::sock_fprog {
            len: 1,
            filter: filter.as_ptr().cast_mut(),
        };
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u64; 3];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = 24;
        // SAFETY: the child makes only raw system calls and writes into
        // buffers that exist before the fork, then _exits.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork: {}", std::io::Error::last_os_error());
        if child == 0 {
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(2);
                }
                let listener = libc::syscall(
                    libc::SYS_seccomp,
                    SECCOMP_SET_MODE_FILTER,
                    SECCOMP_FILTER_FLAG_NEW_LISTENER,
                    &program as *const libc::sock_fprog,
                );
                if listener < 0 {
                    libc::_exit(3);
                }
                let header = control.as_mut_ptr().cast::<libc::cmsghdr>();
                (*header).cmsg_len = 20;
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                *control.as_mut_ptr().add(2).cast::<libc::c_int>() = listener as libc::c_int;
                if libc::sendmsg(sender.as_raw_fd(), &message, 0) != 1 {
                    libc::_exit(4);
                }
                libc::_exit(0);
            }
        }
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
        assert_eq!(libc::WEXITSTATUS(status), 0, "child status {status:#x}");
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 64];
        let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        // This fixture calls the host libc wrapper, whose length is u32 on
        // musl and usize on GNU; production guest buffers use the kernel ABI.
        #[cfg(target_env = "musl")]
        {
            message.msg_controllen = control.len().try_into().unwrap();
        }
        #[cfg(not(target_env = "musl"))]
        {
            message.msg_controllen = control.len();
        }
        let received =
            unsafe { libc::recvmsg(receiver.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        assert_eq!(received, 1, "recvmsg: {}", std::io::Error::last_os_error());
        #[cfg(target_env = "musl")]
        let control_len = usize::try_from(message.msg_controllen).unwrap();
        #[cfg(not(target_env = "musl"))]
        let control_len = message.msg_controllen;
        let descriptors = received_descriptors(&control[..control_len]);
        assert_eq!(descriptors.len(), 1, "{descriptors:?}");
        let listener = unsafe {
            <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(descriptors[0])
        };
        assert_eq!(
            descriptor_exit_dependency(Path::new(&format!(
                "/proc/self/fd/{}",
                listener.as_raw_fd()
            ))),
            Some("a seccomp user-notification listener")
        );
    }

    #[test]
    fn a_fuse_device_descriptor_is_recognised_by_its_number() {
        let Ok(fuse) = std::fs::File::open("/dev/fuse") else {
            eprintln!("skipping: /dev/fuse cannot be opened on this host");
            return;
        };
        assert_eq!(
            descriptor_exit_dependency(Path::new(&format!("/proc/self/fd/{}", fuse.as_raw_fd()))),
            Some("a FUSE device (/dev/fuse)")
        );
        // The startup scan finds it among this process's descriptors.
        assert_eq!(
            held_exit_dependency(Path::new("/proc/self/fd")).unwrap(),
            Some((fuse.as_raw_fd().to_string(), "a FUSE device (/dev/fuse)"))
        );
    }

    #[test]
    fn the_refusal_names_the_capability_and_ends_with_the_backend_advice() {
        let text = refusal("a FUSE device (/dev/fuse)", 5, "opened");
        assert!(
            text.starts_with(
                "hermit: refusing a guest that holds a FUSE device (/dev/fuse) (descriptor 5, \
                 opened): this backend completes process exits asynchronously"
            ),
            "{text}"
        );
        set_backend_advice("Backend X refused it; try backend Y.");
        assert!(
            refusal("a FUSE device (/dev/fuse)", 5, "opened")
                .ends_with("wait forever. Backend X refused it; try backend Y."),
        );
    }

    #[test]
    fn the_startup_scan_refuses_an_unlistable_directory() {
        assert!(held_exit_dependency(Path::new("/nonexistent-fd-directory")).is_err());
    }
}
