/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Signal producers whose signal Linux sends at a moment host timing
//! decides, which an in-guest run refuses (A5 step 3c slice 0b; design
//! sections 16.30, D25, D28 and D28a of
//! <https://github.com/rrnewton/hermit/issues/4485>).
//!
//! While the in-guest runtime runs its own code, ordinary signals are blocked,
//! and Linux keeps a blocked signal pending even when the guest ignores it, so
//! a signal the kernel sends at a host-timed moment could become pending
//! whatever the guest's action and mask. So on the in-guest backends' default
//! route (`Config::in_guest_refuses_host_timed_producers`) Detcore refuses the
//! one operation that completes such a producer, and Hermit refuses a launch
//! whose guest would inherit one already complete.
//!
//! A descriptor's SIGIO and SIGURG producers (Linux v6.12):
//! - SIGIO reaches a descriptor's owner only through the descriptor's fasync
//!   list (`kill_fasync_rcu`, `fs/fcntl.c:1116-1142`, sends it with `send_sigio`),
//!   which setting `O_ASYNC` populates: `F_SETFL` calls the file's `->fasync`
//!   when `O_ASYNC` changes (`setfl`, `fs/fcntl.c:76-82`), and so does
//!   `FIOASYNC` (`ioctl_fioasync`, `fs/ioctl.c:365-385`). `open(O_ASYNC)` keeps
//!   the flag but never calls `->fasync` (`do_dentry_open`, `fs/open.c:974`),
//!   and a later `F_SETFL` that leaves the flag set calls nothing either. A
//!   descriptor whose status flags show `O_ASYNC` is treated as armed here
//!   anyway, which can only refuse more.
//! - A socket's owner gets SIGURG for out-of-band data without `O_ASYNC`
//!   (`sk_send_sigurg`, `net/core/sock.c:3503-3508`, calls `send_sigurg`,
//!   `fs/fcntl.c:941-978`, which reads only the owner).
//! - `F_SETLEASE` with a read or write lease, and `F_NOTIFY` with a nonzero
//!   mask, make the caller the owner themselves (`lease_setup`,
//!   `fs/locks.c:563-577`; `fcntl_dirnotify`, `fs/notify/dnotify/dnotify.c:372`)
//!   and are refused outright.
//!
//! An owner is named by `F_SETOWN` (`f_setown`, `fs/fcntl.c:154-189`),
//! `F_SETOWN_EX` (`f_setown_ex`, `fs/fcntl.c:217-259`), and on a socket
//! `FIOSETOWN` and `SIOCSPGRP` (`sock_ioctl`, `net/socket.c:1284-1289`); an
//! owner of 0 (a pid of 0 for `F_SETOWN_EX`) removes it.

use std::os::fd::RawFd;

use nix::errno::Errno;

/// `F_SETOWN_EX`'s owner types (Linux's `include/uapi/asm-generic/fcntl.h`;
/// the pinned libc lacks them).
pub const F_OWNER_TID: libc::c_int = 0;
/// See [`F_OWNER_TID`].
pub const F_OWNER_PID: libc::c_int = 1;
/// See [`F_OWNER_TID`].
pub const F_OWNER_PGRP: libc::c_int = 2;
/// `fcntl`'s command that reads a descriptor's owner and its type (Linux's
/// `include/uapi/asm-generic/fcntl.h`; the pinned libc lacks it).
pub const F_GETOWN_EX: libc::c_int = 16;

/// Whether `F_GETOWN_EX`'s answer, owner type `kind` and pid `pid`, names an
/// owner the guest could receive a signal from.
///
/// A nonzero pid is an owner. `f_getown_ex` reports pid 0 both for no owner
/// and for an owner the reader's PID namespace cannot see (`pid_vnr`,
/// `fs/fcntl.c:261-301`). An unseen process or thread is never a guest
/// process, since every guest process is a descendant of the reader in its
/// namespace. That holds only while the reader sees every guest process:
/// Hermit's launch reads from the container's PID namespace, where every
/// guest process lives, and Detcore reads from the caller's, which on the
/// in-guest backends (the only ones that read this) is that same namespace,
/// since they admit no nested PID namespace (they refuse every `clone` but a
/// plain fork, and Detcore refuses `unshare` and `setns` on every backend,
/// `syscall_classification::is_mount_ns_admin_refused_syscall`). A backend that admitted one would have to read from the
/// container's namespace instead. `f_getown_ex` also reports pid 0 when
/// the owner's pid has no task of the owner's type (`pid_task`), and one case
/// of that is a live owner: an `F_SETOWN` or `F_OWNER_PID` owner naming a
/// thread that is not its process's leader, which `send_sigio` still
/// signals. That needs a guest process with a second thread, and the
/// in-guest backends refuse thread creation (`clone_args_are_plain_fork`), so
/// no guest process has one: a second precondition beside the PID
/// namespace's, and one a backend that admitted threads would have to meet
/// another way. An unseen process group can still hold
/// the guest, which keeps
/// the group it starts in, so an owner of type `F_OWNER_PGRP` counts as an
/// owner whatever its pid: that can only refuse more (the one ambiguous
/// disarm is `F_SETOWN_EX` of a process group with pid 0).
pub fn names_an_owner(kind: libc::c_int, pid: libc::c_int) -> bool {
    pid != 0 || kind == F_OWNER_PGRP
}

/// The owner an owner-setting call names, as the kernel looks it up: `who`,
/// a pid number, and whether it names a process group (`group`): a negative
/// `F_SETOWN`, `FIOSETOWN` or `SIOCSPGRP` owner, or `F_OWNER_PGRP`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerLookup {
    /// The pid number the kernel looks up (`find_vpid`).
    pub who: libc::c_int,
    /// Whether the owner is a process group.
    pub group: bool,
}

impl OwnerLookup {
    /// The owner `f_setown` looks up for `owner`, the argument of `F_SETOWN`,
    /// `FIOSETOWN` or `SIOCSPGRP`: a negative one names the process group
    /// `-owner` (`fs/fcntl.c:162-170`). `owner` is neither 0 nor `INT_MIN`.
    pub fn of_setown(owner: libc::c_int) -> Self {
        Self {
            who: owner.abs(),
            group: owner < 0,
        }
    }
}

/// The line Hermit logs once a run's launch check has passed (D25: the
/// guest's physical `RLIMIT_CPU`), kept until the run ends
/// ([`defer_run_end_log`]).
type RunEndLog = Box<dyn FnOnce() + Send>;

static RUN_END_LOG: std::sync::Mutex<Option<RunEndLog>> = std::sync::Mutex::new(None);

/// Keeps `log` to run once at the end of this process's run
/// ([`emit_run_end_log`]): after the scheduler shuts down on the ordinary
/// path, and just before any of the scheduler's stopping refusals exits the
/// process, so the line is logged on every exit path, at the same point of
/// each path in every run. A later call replaces an earlier one.
pub fn defer_run_end_log(log: impl FnOnce() + Send + 'static) {
    *RUN_END_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::new(log));
}

/// Runs the line [`defer_run_end_log`] kept, if any, once.
pub fn emit_run_end_log() {
    let log = RUN_END_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some(log) = log {
        log();
    }
}

/// What a descriptor already arms toward a host-timed signal producer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProducerState {
    /// Its status flags show `O_ASYNC`.
    pub o_async: bool,
    /// It has an owner ([`names_an_owner`]).
    pub owner: bool,
    /// It is a socket.
    pub socket: bool,
}

impl ProducerState {
    /// Whether the descriptor is already a complete producer: an owner with
    /// `O_ASYNC` (SIGIO) or on a socket (SIGURG).
    pub fn is_complete(self) -> bool {
        self.owner && (self.o_async || self.socket)
    }
}

/// An operation on a descriptor that can complete a host-timed signal
/// producer (D28). `call` names the call and command, as the WARN does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProducerArming {
    /// A nonzero owner by `F_SETOWN` or `F_SETOWN_EX`.
    SetOwner(&'static str),
    /// A nonzero owner by `FIOSETOWN` or `SIOCSPGRP`, which only a socket
    /// handles (`sock_ioctl`, `net/socket.c:1284-1289`); on any other file
    /// the call reaches no handler and fails with ENOTTY, arming nothing.
    SetSocketOwner(&'static str),
    /// `O_ASYNC` set: `F_SETFL`, `FIOASYNC`.
    SetAsync(&'static str),
    /// `F_SETLEASE` with `F_RDLCK` or `F_WRLCK`.
    Lease,
    /// `F_NOTIFY` with a nonzero mask.
    Notify,
}

impl ProducerArming {
    /// The call and command, as the WARN names them.
    pub fn call(self) -> &'static str {
        match self {
            Self::SetOwner(call) | Self::SetSocketOwner(call) | Self::SetAsync(call) => call,
            Self::Lease => "fcntl(F_SETLEASE)",
            Self::Notify => "fcntl(F_NOTIFY)",
        }
    }

    /// Whether this operation on a descriptor in `state` completes a
    /// producer, so is refused: an owner for a socket or for a descriptor
    /// with `O_ASYNC`, `O_ASYNC` for a descriptor with an owner, and every
    /// lease or directory notification.
    pub fn completes(self, state: ProducerState) -> bool {
        match self {
            Self::SetOwner(_) => state.socket || state.o_async,
            Self::SetSocketOwner(_) => state.socket,
            Self::SetAsync(_) => state.owner,
            Self::Lease | Self::Notify => true,
        }
    }

    /// What the kernel would then send, and why, as the WARN says it.
    fn producer(self, state: ProducerState) -> &'static str {
        match self {
            Self::SetOwner(_) | Self::SetSocketOwner(_) if state.socket => {
                "the descriptor is a socket, whose owner Linux sends SIGURG when out-of-band data \
                 arrives"
            }
            Self::SetOwner(_) | Self::SetSocketOwner(_) => {
                "the descriptor has O_ASYNC, so Linux would send its owner SIGIO when it becomes \
                 ready"
            }
            Self::SetAsync(_) => {
                "the descriptor has an owner, which Linux would send SIGIO when it becomes ready"
            }
            Self::Lease => {
                "a lease makes the caller the owner, which Linux sends SIGIO when another process \
                 opens the file"
            }
            Self::Notify => {
                "a directory notification makes the caller the owner, which Linux sends SIGIO when \
                 the directory changes"
            }
        }
    }
}

/// The WARN for an operation that would complete a host-timed signal
/// producer on descriptor `fd`, which Detcore refused with EPERM.
pub(crate) fn producer_refused_warning(
    arming: ProducerArming,
    state: ProducerState,
    fd: RawFd,
    caller: i32,
) -> String {
    format!(
        "HERMIT_HOST_TIMED_PRODUCER_REFUSED call={} fd={fd} caller={caller}: received EPERM and \
         nothing was armed: {}, at a moment host timing decides. A named refusal, not Linux's \
         behaviour (A5 step 3c, https://github.com/rrnewton/hermit/issues/4485).",
        arming.call(),
        arming.producer(state),
    )
}

/// The WARN for a parent-death signal Detcore cannot hold, which it refused
/// with EPERM: a stop signal, SIGALRM or a realtime signal.
pub(crate) fn parent_death_signal_refused_warning(signum: i32, caller: i32) -> String {
    format!(
        "HERMIT_HOST_TIMED_PRODUCER_REFUSED call=prctl(PR_SET_PDEATHSIG) signum={signum} \
         caller={caller}: received EPERM and nothing was armed: Detcore holds only a standard \
         signal other than SIGKILL, SIGSTOP and SIGALRM, and the kernel would send any other when \
         the parent thread exits, at a moment host timing decides. A named refusal, not Linux's \
         behaviour (A5 step 3c, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The WARN for a change to the CPU-time limit, which Detcore refused with
/// EPERM as it does on every backend: the limit Detcore reports is virtual,
/// and a physical one would send SIGXCPU at a moment host CPU time decides.
pub(crate) fn rlimit_cpu_refused_warning(call: &str, caller: i32) -> String {
    format!(
        "HERMIT_HOST_TIMED_PRODUCER_REFUSED call={call}(RLIMIT_CPU) caller={caller}: received \
         EPERM and nothing was changed: the CPU-time limit is virtual and stays unlimited, \
         because a physical one would send SIGXCPU at a moment host CPU time decides \
         (A5 step 3c, https://github.com/rrnewton/hermit/issues/4485)."
    )
}

/// The state of descriptor `fd` of the calling process, read from the host,
/// with whether it holds a lease (`F_GETLEASE`): what Hermit checks before an
/// in-guest launch in each descriptor the guest would inherit (D25). An
/// `O_PATH` descriptor arms nothing (`check_fcntl_cmd` refuses every
/// arming command on one), so it reads as arming nothing. A directory
/// notification cannot be read back (there is no `F_GETNOTIFY`, and fdinfo
/// does not show dnotify), so it is not reported.
pub fn host_descriptor_state(fd: RawFd) -> Result<(ProducerState, bool), Errno> {
    // SAFETY: fcntl and fstat on a descriptor number write only the
    // structures passed to them.
    let flags = Errno::result(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    if flags & libc::O_PATH != 0 {
        return Ok((ProducerState::default(), false));
    }
    let mut owner = [0 as libc::c_int; 2];
    Errno::result(unsafe { libc::fcntl(fd, F_GETOWN_EX, owner.as_mut_ptr()) })?;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    Errno::result(unsafe { libc::fstat(fd, &mut stat) })?;
    let lease = Errno::result(unsafe { libc::fcntl(fd, libc::F_GETLEASE) })?;
    Ok((
        ProducerState {
            o_async: flags & libc::O_ASYNC != 0,
            owner: names_an_owner(owner[0], owner[1]),
            socket: stat.st_mode & libc::S_IFMT == libc::S_IFSOCK,
        },
        lease != libc::F_UNLCK,
    ))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    const ALL_STATES: [ProducerState; 8] = {
        let mut states = [ProducerState {
            o_async: false,
            owner: false,
            socket: false,
        }; 8];
        let mut i = 0;
        while i < 8 {
            states[i] = ProducerState {
                o_async: i & 1 != 0,
                owner: i & 2 != 0,
                socket: i & 4 != 0,
            };
            i += 1;
        }
        states
    };

    /// A pid names an owner; pid 0 does only for a process group, which an
    /// unseen group's owner reads as.
    #[test]
    fn an_owner_is_a_pid_or_a_process_group() {
        for kind in [F_OWNER_TID, F_OWNER_PID, F_OWNER_PGRP] {
            assert!(names_an_owner(kind, 7), "{kind}");
            assert!(names_an_owner(kind, -7), "{kind}");
        }
        assert!(!names_an_owner(F_OWNER_TID, 0));
        assert!(!names_an_owner(F_OWNER_PID, 0));
        assert!(names_an_owner(F_OWNER_PGRP, 0));
    }

    /// Each refused operation (D28): an owner on a socket or on an O_ASYNC
    /// descriptor, O_ASYNC on a descriptor with an owner, every lease and
    /// notification. Each allowed one: an owner on a descriptor that is
    /// neither, and O_ASYNC with no owner.
    #[test]
    fn only_the_operation_that_completes_a_producer_is_refused() {
        for state in ALL_STATES {
            let owner = ProducerArming::SetOwner("fcntl(F_SETOWN)");
            assert_eq!(
                owner.completes(state),
                state.socket || state.o_async,
                "{state:?}"
            );
            let socket_owner = ProducerArming::SetSocketOwner("ioctl(FIOSETOWN)");
            assert_eq!(socket_owner.completes(state), state.socket, "{state:?}");
            let o_async = ProducerArming::SetAsync("fcntl(F_SETFL)");
            assert_eq!(o_async.completes(state), state.owner, "{state:?}");
            assert!(ProducerArming::Lease.completes(state), "{state:?}");
            assert!(ProducerArming::Notify.completes(state), "{state:?}");
        }
        // The cases the brief names, one by one.
        let pipe = ProducerState::default();
        let socket = ProducerState {
            socket: true,
            ..pipe
        };
        let o_async = ProducerState {
            o_async: true,
            ..pipe
        };
        let owned = ProducerState {
            owner: true,
            ..pipe
        };
        let set_owner = ProducerArming::SetOwner("fcntl(F_SETOWN)");
        let set_async = ProducerArming::SetAsync("fcntl(F_SETFL)");
        assert!(!set_owner.completes(pipe), "an owner on a plain pipe");
        assert!(set_owner.completes(socket), "an owner on a socket");
        assert!(set_owner.completes(o_async), "an owner with O_ASYNC");
        assert!(!set_async.completes(pipe), "O_ASYNC with no owner");
        assert!(!set_async.completes(socket), "O_ASYNC on an unowned socket");
        assert!(set_async.completes(owned), "O_ASYNC with an owner");
        let set_socket_owner = ProducerArming::SetSocketOwner("ioctl(SIOCSPGRP)");
        assert!(set_socket_owner.completes(socket), "SIOCSPGRP on a socket");
        assert!(
            !set_socket_owner.completes(o_async),
            "SIOCSPGRP on an O_ASYNC pipe is ENOTTY"
        );
    }

    /// A complete producer (D25): an owner with O_ASYNC or on a socket.
    #[test]
    fn a_complete_producer_has_an_owner_and_a_way_to_signal_it() {
        for state in ALL_STATES {
            assert_eq!(
                state.is_complete(),
                state.owner && (state.o_async || state.socket),
                "{state:?}"
            );
        }
    }

    /// `F_SETOWN`'s owner names a process when positive and the process
    /// group `-owner` when negative, as `f_setown` looks it up.
    #[test]
    fn a_negative_setown_owner_is_a_process_group() {
        assert_eq!(
            OwnerLookup::of_setown(7),
            OwnerLookup {
                who: 7,
                group: false
            }
        );
        assert_eq!(
            OwnerLookup::of_setown(-7),
            OwnerLookup {
                who: 7,
                group: true
            }
        );
        assert_eq!(
            OwnerLookup::of_setown(libc::c_int::MAX).who,
            libc::c_int::MAX
        );
        assert_eq!(
            OwnerLookup::of_setown(-libc::c_int::MAX).who,
            libc::c_int::MAX
        );
    }

    /// The WARNs name the call, the descriptor or signal, EPERM and the
    /// issue, and never a backend.
    #[test]
    fn the_warnings_name_the_call_and_the_descriptor() {
        let socket = ProducerState {
            socket: true,
            ..ProducerState::default()
        };
        let text = producer_refused_warning(
            ProducerArming::SetSocketOwner("ioctl(SIOCSPGRP)"),
            socket,
            5,
            3,
        );
        assert!(
            text.starts_with(
                "HERMIT_HOST_TIMED_PRODUCER_REFUSED call=ioctl(SIOCSPGRP) fd=5 caller=3: \
                 received EPERM"
            ),
            "{text}"
        );
        assert!(text.contains("SIGURG"), "{text}");
        let lease = producer_refused_warning(ProducerArming::Lease, socket, 4, 3);
        assert!(lease.contains("call=fcntl(F_SETLEASE) fd=4"), "{lease}");
        let notify = producer_refused_warning(ProducerArming::Notify, socket, 4, 3);
        assert!(notify.contains("call=fcntl(F_NOTIFY) fd=4"), "{notify}");
        let pdeath = parent_death_signal_refused_warning(libc::SIGSTOP, 3);
        assert!(
            pdeath.starts_with(
                "HERMIT_HOST_TIMED_PRODUCER_REFUSED call=prctl(PR_SET_PDEATHSIG) signum=19 \
                 caller=3: received EPERM"
            ),
            "{pdeath}"
        );
        let cpu = rlimit_cpu_refused_warning("setrlimit", 3);
        assert!(
            cpu.starts_with(
                "HERMIT_HOST_TIMED_PRODUCER_REFUSED call=setrlimit(RLIMIT_CPU) caller=3: \
                 received EPERM"
            ),
            "{cpu}"
        );
        for text in [&text, &lease, &notify, &pdeath, &cpu] {
            assert!(
                text.contains("https://github.com/rrnewton/hermit/issues/4485"),
                "{text}"
            );
            for backend in ["liteinst", "LiteInst", "in-guest-trap", "ptrace"] {
                assert!(!text.contains(backend), "{text}");
            }
        }
    }

    /// The host read sees what this process set: a pipe with an owner and
    /// O_ASYNC is complete, the same pipe without O_ASYNC is not, a socket
    /// with an owner is, and an O_PATH descriptor arms nothing.
    #[test]
    fn the_host_read_sees_owner_flags_and_sockets() {
        let (read, write) = nix::unistd::pipe().unwrap();
        let fd = read.as_raw_fd();
        let (state, lease) = host_descriptor_state(fd).unwrap();
        assert_eq!(state, ProducerState::default());
        assert!(!lease);
        // SAFETY: plain fcntl calls on a descriptor this test owns.
        let pid = unsafe { libc::getpid() };
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETOWN, pid) }, 0);
        let (state, _) = host_descriptor_state(fd).unwrap();
        assert!(state.owner && !state.o_async && !state.socket, "{state:?}");
        assert!(!state.is_complete());
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_ASYNC) },
            0
        );
        let (state, _) = host_descriptor_state(fd).unwrap();
        assert!(state.is_complete(), "{state:?}");
        // Disarm before the pipe could ever be written.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETOWN, 0) }, 0);
        assert_eq!(
            host_descriptor_state(fd).unwrap().0,
            ProducerState::default()
        );
        drop(write);

        let (left, _right) = nix::sys::socket::socketpair(
            nix::sys::socket::AddressFamily::Unix,
            nix::sys::socket::SockType::Stream,
            None,
            nix::sys::socket::SockFlag::SOCK_CLOEXEC,
        )
        .unwrap();
        let fd = left.as_raw_fd();
        assert_eq!(
            host_descriptor_state(fd).unwrap().0,
            ProducerState {
                socket: true,
                ..ProducerState::default()
            }
        );
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETOWN, pid) }, 0);
        assert!(host_descriptor_state(fd).unwrap().0.is_complete());
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETOWN, 0) }, 0);

        let path = std::ffi::CString::new("/").unwrap();
        let o_path = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        assert!(o_path >= 0);
        assert_eq!(
            host_descriptor_state(o_path).unwrap(),
            (ProducerState::default(), false)
        );
        unsafe { libc::close(o_path) };
    }
}
