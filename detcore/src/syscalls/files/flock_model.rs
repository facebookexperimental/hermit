/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Detcore's side of its `flock(2)` model (the kvm lane's slice SP2), whose
//! state the lock ledger keeps (`procfs::locks::LockLedger::flock`).
//!
//! Linux keeps an inode's flock locks in one list, each owned by an open file
//! description (`flock_make_lock`, `flock_lock_inode` in fs/locks.c). Detcore
//! keeps the same list per inode, keyed by the device and inode numbers of a
//! recorded `fstat`, follows each description's references as the ledger
//! follows them, and decides every call through a description it can follow
//! in the caller's turn, on every backend:
//!
//! - a call that takes, converts or releases a lock without a conflict, and
//!   one that finds a conflict with `LOCK_NB` (`EWOULDBLOCK`), are answered at
//!   once;
//! - a blocking call that finds a conflict waits in Detcore's schedule, as a
//!   pipe read does (`retry_nonblocking_syscall`), asking the model at each of
//!   its turns ([`Detcore::flock_wait_attempt`]); its request is blocked in
//!   Linux's tree of blocked requests, and a release grants nothing but wakes
//!   the requests blocked under the lock it deleted, each of which retries in
//!   its own call's next turn, taking the lock or blocking again, as Linux's
//!   woken waiter retries in its own run; a signal ends a blocked wait with
//!   `ERESTARTSYS`, as Linux's `wait_event_interruptible` does.
//!
//! On every backend but KVM the guest's descriptor is a host description with
//! the guest's own lifetime, so each decision is mirrored on it, in the turn
//! of the call that made it: `flock(fd, mode | LOCK_NB)` after a grant,
//! `LOCK_UN` after a release ([`Detcore::flock_mirror`]). The kernel's
//! conflict test is the model's and every lock the model holds is mirrored, so
//! a mirror fails only on a host lock the model does not know (a process
//! outside the run, a description the guest inherited locked): that call is
//! refused by name, never granted against the host's lock and never left
//! waiting on host timing. KVM's host descriptions do not follow guest
//! lifetimes, so it keeps no mirror: processes outside the run neither see nor
//! affect the guest's locks there (a named limitation,
//! <https://github.com/rrnewton/hermit/issues/5043>).
//!
//! Detcore leaves to the kernel, as before the model, on the backends with
//! host descriptions, and refuses by name on KVM:
//!
//! - a description whose references Detcore cannot all follow
//!   (`DetFd::references_escaped`: shared with another process before its
//!   first lock call, or not made by a call Detcore saw), after which the
//!   file is the kernel's for the rest of the run, so that the model and the
//!   kernel never both decide one file;
//! - a file whose filesystem's own `f_op->flock` decides the call
//!   (`flock_lock_inode` never runs, fs/locks.c `sys_flock`): NFS, SMB, FUSE,
//!   Ceph, GFS2, OCFS2, 9p, AFS;
//! - a file Detcore cannot name (its `fstat` failed).

use std::ops::RangeInclusive;
use std::os::unix::io::RawFd;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Sysno;

use super::StatFailure;
use crate::Detcore;
use crate::fd::DetFd;
use crate::fd::FlockIdentity;
use crate::procfs::locks::FlockInodeKey;
use crate::procfs::locks::FlockMode;
use crate::procfs::locks::FlockOutcome;
use crate::procfs::locks::FlockPoll;
use crate::procfs::locks::FlockRequest;
use crate::procfs_inode;
use crate::record_or_replay::RecordOrReplay;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::syscalls::helpers::NonblockableSyscall;
use crate::types::DetPid;
use crate::types::OpenFileId;

/// `LOCK_MAND`, which Linux ignores, answering 0 before it looks at anything
/// else (`sys_flock`).
const LOCK_MAND: i32 = 32;

/// How long a grant's mirror after a release by an exit waits at most for the
/// host teardown of the process that held the lock: then it mirrors anyway
/// where the backend reports teardowns, and refuses the call by name on DBT,
/// whose retried mirror kept failing. A backstop, not the mechanism
/// ([`Detcore::flock_mirror_after_teardown`]).
const TEARDOWN_BACKSTOP: Duration = Duration::from_secs(30);

/// How often such a mirror looks again.
const TEARDOWN_POLL: Duration = Duration::from_millis(1);

/// The filesystems whose own `f_op->flock` decides a flock, by `fstatfs`
/// `f_type` (include/uapi/linux/magic.h; GFS2's from
/// include/uapi/linux/gfs2_ondisk.h), with the name a refusal gives: NFS
/// (fs/nfs/file.c:947), SMB (fs/smb/client/cifsfs.c:1539), FUSE
/// (fs/fuse/file.c:3081), Ceph (fs/ceph/file.c:3173), GFS2
/// (fs/gfs2/file.c:1586), OCFS2 (fs/ocfs2/file.c:2814), 9p
/// (fs/9p/vfs_file.c:534) and AFS (fs/afs/file.c:43), Linux v6.17.
const OWN_FLOCK_FILESYSTEMS: [(u64, &str); 11] = [
    (0x6969, "NFS"),
    (0x517B, "SMB"),
    (0xFF53_4D42, "SMB"),
    (0xFE53_4D42, "SMB"),
    (0x6573_5546, "FUSE"),
    (0x00C3_6400, "Ceph"),
    (0x0116_1970, "GFS2"),
    (0x7461_636F, "OCFS2"),
    (0x0102_1997, "9p"),
    (0x5346_414F, "AFS"),
    (0x6B41_4653, "AFS"),
];

/// Why a call through a description whose references Detcore cannot all
/// follow is refused where no host lock can decide the file (KVM).
const ESCAPED_REFUSAL: &str = "this open file description was shared with another process before \
     its first flock (a fork copied it, it was passed over SCM_RIGHTS, or Detcore did not see it \
     made: the launcher's standard streams, or a descriptor found in the table), so Detcore \
     cannot follow its references to place the lock's release, and on this backend no host lock \
     can decide the file";

/// Why a call that waited is refused when its retry changed the lock of the
/// description it took (a grant, or the release of the other mode), which
/// another reference keeps, and no descriptor of the caller's table names
/// that description any longer (`Detcore::flock_mirror_waited`).
const WAITER_DESCRIPTOR_GONE_REFUSAL: &str = "the waiting call's descriptor was closed or \
     replaced while it waited, and another reference (another process's table, a mapping, a \
     copy in flight) keeps the open file description the call took, whose lock the call's retry \
     changed: the caller's process has no descriptor of it to change the lock on the host \
     through";

/// Why a call that waited is refused when its file left the model meanwhile
/// and no descriptor of the caller's table names the description the call
/// took any longer, so the kernel cannot be asked to decide it
/// (`Detcore::flock_wait_attempt`).
const WAITER_DESCRIPTOR_GONE_KERNEL_REFUSAL: &str = "the waiting call's descriptor was closed \
     or replaced while it waited, and its file left Detcore's model meanwhile: the kernel would \
     decide the call, but the caller's process has no descriptor of the open file description \
     the call took to ask it through";

/// Why a waiting call a job-control stop interrupted is refused
/// (`Detcore::flock_wait`).
const STOPPED_WAITER_REFUSAL: &str = "a job-control stop interrupted this waiting flock: Linux \
     takes the waiter out of the queue for the stop and queues the call again only once SIGCONT \
     continues it, but Detcore does not represent job-control stops \
     (https://github.com/rrnewton/hermit/issues/4944), so the stopped thread would restart the \
     call, and retry for the lock, while Linux keeps it stopped";

/// The mode `description` now holds, in its cache of the kernel's
/// (`DetFd::set_flock_mode`), which the vfork guards read.
fn note_flock_mode(description: Option<&DetFd>, mode: Option<FlockMode>) {
    let held = mode.map(|mode| match mode {
        FlockMode::Shared => libc::LOCK_SH,
        FlockMode::Exclusive => libc::LOCK_EX,
    });
    if let Some(description) = description {
        description.set_flock_mode(held);
    }
}

/// What a mirror after a holder's exit does next
/// (`Detcore::flock_mirror_after_teardown`, [`TeardownWait`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TeardownStep {
    /// Ask whether the host teardown of the lagging processes is still to be
    /// reported.
    AskPending,
    /// Make the mirror once more.
    Attempt,
    /// Wait [`TEARDOWN_POLL`] before the next step.
    Pause,
    /// The mirror's answer: `EWOULDBLOCK` past the backstop.
    Done(Result<i64, Errno>),
}

/// The waiting of a mirror after a holder's exit, apart from its calls and
/// its clock, so that a test can drive both: where the backend reports the
/// host teardown (`reported`), it asks until the teardown is reported, or
/// `deadline` passes, and then mirrors once, whatever the answer; where it
/// does not (DBT), it mirrors again until the mirror stops answering
/// `EWOULDBLOCK`, at most until `deadline`, after which the answer is
/// `EWOULDBLOCK`.
struct TeardownWait {
    reported: bool,
    deadline: Instant,
    /// How many pauses the wait made.
    polls: u32,
}

impl TeardownWait {
    fn new(reported: bool, deadline: Instant) -> Self {
        Self {
            reported,
            deadline,
            polls: 0,
        }
    }

    fn first(&self) -> TeardownStep {
        if self.reported {
            TeardownStep::AskPending
        } else {
            TeardownStep::Attempt
        }
    }

    fn after_pending(&self, pending: bool, now: Instant) -> TeardownStep {
        if !pending || now >= self.deadline {
            TeardownStep::Attempt
        } else {
            TeardownStep::Pause
        }
    }

    fn after_attempt(&self, answer: Result<i64, Errno>, now: Instant) -> TeardownStep {
        if self.reported || answer != Err(Errno::EWOULDBLOCK) {
            TeardownStep::Done(answer)
        } else if now >= self.deadline {
            TeardownStep::Done(Err(Errno::EWOULDBLOCK))
        } else {
            TeardownStep::Pause
        }
    }

    fn after_pause(&mut self) -> TeardownStep {
        self.polls += 1;
        self.first()
    }
}

/// The name of the filesystem `f_type` names, where its own `f_op->flock`
/// decides a flock ([`OWN_FLOCK_FILESYSTEMS`]).
// GNU declares `f_type` as i64, musl as u64: its bits are compared.
#[allow(clippy::unnecessary_cast)]
fn own_flock_filesystem(f_type: i64) -> Option<&'static str> {
    OWN_FLOCK_FILESYSTEMS
        .iter()
        .find(|(magic, _)| *magic == f_type as u64 & 0xFFFF_FFFF)
        .map(|(_, name)| *name)
}

/// A file as `/proc/locks` spells it, `MAJOR:MINOR:INODE` (`%02x:%02x:%lu`,
/// `lock_get_status`), from its `st_dev` and `st_ino`.
fn flock_object(dev: u64, ino: u64) -> String {
    format!("{:02x}:{:02x}:{ino}", libc::major(dev), libc::minor(dev))
}

/// Where `Detcore::handle_flock` takes a call.
pub(crate) enum FlockRoute {
    /// Detcore's model decided it, with this result.
    Decided(Result<i64, Error>),
    /// The kernel decides it, as before the model
    /// (`Detcore::flock_through_the_kernel`).
    Kernel,
}

/// A `flock` waiting in Detcore's model is retried as a pipe read is
/// (`retry_nonblocking_syscall`), but each attempt asks the model
/// ([`Detcore::flock_wait_attempt`]) rather than the kernel: the call itself
/// is never made nonblocking.
#[async_trait]
impl NonblockableSyscall for syscalls::Flock {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        (self, None)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EWOULDBLOCK)
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Whether this backend's guest descriptors are host descriptions with
    /// the guest's own lifetime, on which Detcore mirrors its decisions and
    /// the kernel can decide a file the model leaves: wherever guest threads
    /// are host tasks (`procfs_inode::tool_can_name_guest_descriptors`), so
    /// every backend but KVM. No backend capability states that fact itself:
    /// one would be a field of Reverie's `BackendCapabilities`. Until it
    /// exists this reads the fact Detcore's procfs reads guest descriptors
    /// by, whose only backend without it, KVM, runs guest threads that are
    /// not host tasks (`provides_process_signal_control`).
    pub(crate) fn flock_mirrors(&self) -> bool {
        procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend)
    }

    /// Decide `call` with the flock model where it can (see the module
    /// documentation), or say that the kernel decides it.
    pub(crate) async fn flock_through_the_model<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
    ) -> Result<FlockRoute, Error> {
        let (fd, operation) = (call.fd(), call.operation());
        let mirrors = self.flock_mirrors();
        // Without sequentialized threads (a relaxation) no turn orders the
        // calls: the kernel decides where it can, as before the model.
        if !self.cfg.sequentialize_threads && mirrors {
            return Ok(FlockRoute::Kernel);
        }
        // Linux's order (`sys_flock`): `LOCK_MAND` is ignored, then the
        // command decides `EINVAL`, then the descriptor `EBADF`.
        if operation & LOCK_MAND != 0 {
            return Ok(FlockRoute::Decided(Ok(0)));
        }
        let mode = match operation & !libc::LOCK_NB {
            libc::LOCK_SH => Some(FlockMode::Shared),
            libc::LOCK_EX => Some(FlockMode::Exclusive),
            libc::LOCK_UN => None,
            _ => return Ok(FlockRoute::Decided(Err(Errno::EINVAL.into()))),
        };
        let nonblocking = operation & libc::LOCK_NB != 0;
        // A promoted description (DBT's shared model of a timerfd or a random
        // device, `DetFd::promote`) spans processes by construction: escaped.
        let Ok((open_file, status_flags, escaped, cached, taken)) =
            guest.thread_state().with_detfd(fd, |detfd| {
                (
                    detfd.open_file_id(),
                    detfd.status_flags(),
                    detfd.references_escaped() || detfd.is_shared(),
                    detfd.flock_identity(),
                    detfd.clone(),
                )
            })
        else {
            // Detcore keeps no description of `fd`: the kernel answers, as
            // before the model (`EBADF` for a number that is not open).
            return Ok(FlockRoute::Kernel);
        };
        // `fdget` finds no file for an `O_PATH` descriptor, and a lock needs
        // read or write access (`sys_flock`).
        if status_flags & libc::O_PATH != 0
            || (mode.is_some() && status_flags & libc::O_ACCMODE == libc::O_ACCMODE)
        {
            return Ok(FlockRoute::Decided(Err(Errno::EBADF.into())));
        }
        let identity = match cached {
            Some(identity) => identity,
            None => match self.flock_identity(guest, fd).await? {
                Ok(identity) => {
                    guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.set_flock_identity(identity.clone()))?;
                    identity
                }
                Err(why) => {
                    if mirrors {
                        return Ok(FlockRoute::Kernel);
                    }
                    let reason = format!("Detcore cannot name the file it locks: {why}");
                    return Ok(FlockRoute::Decided(
                        self.refuse_flock(guest, call, &reason).await,
                    ));
                }
            },
        };
        if let Some(filesystem) = &identity.own_flock {
            if mirrors {
                return Ok(FlockRoute::Kernel);
            }
            let reason = format!(
                "the file is on {filesystem}, whose own flock implementation decides the call, \
                 which Detcore does not model"
            );
            return Ok(FlockRoute::Decided(
                self.refuse_flock(guest, call, &reason).await,
            ));
        }
        let key = FlockInodeKey {
            dev: identity.dev,
            ino: identity.ino,
        };
        let object = flock_object(identity.dev, identity.ino);
        if escaped {
            if !mirrors {
                return Ok(FlockRoute::Decided(
                    self.refuse_flock(guest, call, ESCAPED_REFUSAL).await,
                ));
            }
            // The kernel decides this file for the rest of the run.
            crate::tool_global::flock_host_decided(guest, key, object).await;
            return Ok(FlockRoute::Kernel);
        }
        let request = FlockRequest {
            inode: key,
            object,
            open_file,
            slots: guest.thread_state().slots_naming(open_file),
            mode,
            nonblocking,
            mapped: guest.thread_state().maps_open_file(open_file),
            host_fallback: mirrors,
        };
        let outcome = crate::tool_global::flock(guest, request).await;
        if !matches!(
            outcome,
            FlockOutcome::HostDecided | FlockOutcome::Refused(_)
        ) {
            // The ledger follows the description's references from now on:
            // every close or replacement of a slot naming it is reported
            // (`DetFd::is_lock_tracked`), a waiting call's sibling's included,
            // and its mappings matter (`Detcore::note_flock_mapping`).
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.set_lock_tracked();
                detfd.set_flock_modelled();
            })?;
        }
        Ok(match outcome {
            FlockOutcome::Granted { lagging } => {
                FlockRoute::Decided(self.flock_mirror(guest, call, mode, lagging).await)
            }
            FlockOutcome::Released => {
                FlockRoute::Decided(self.flock_mirror(guest, call, None, Vec::new()).await)
            }
            FlockOutcome::WouldBlock { dropped } => {
                // A conversion deleted the description's lock of the other
                // mode before it found the conflict (`flock_lock_inode`).
                if dropped
                    && let Err(error) = self.flock_mirror(guest, call, None, Vec::new()).await
                {
                    return Ok(FlockRoute::Decided(Err(error)));
                }
                FlockRoute::Decided(Err(Errno::EWOULDBLOCK.into()))
            }
            FlockOutcome::Queued { dropped } => {
                if dropped
                    && let Err(error) = self.flock_mirror(guest, call, None, Vec::new()).await
                {
                    crate::tool_global::flock_cancel(guest).await;
                    return Ok(FlockRoute::Decided(Err(error)));
                }
                if !self.cfg.sequentialize_threads {
                    // KVM without sequentialized threads: no turns to wait in.
                    crate::tool_global::flock_cancel(guest).await;
                    let reason = "another open file description holds a conflicting lock, and \
                                  without sequentialized threads Detcore has no turns to wait for \
                                  its release in";
                    return Ok(FlockRoute::Decided(
                        self.refuse_flock(guest, call, reason).await,
                    ));
                }
                FlockRoute::Decided(self.flock_wait(guest, call, taken).await)
            }
            FlockOutcome::HostDecided => FlockRoute::Kernel,
            FlockOutcome::Refused(reason) => {
                FlockRoute::Decided(self.refuse_flock(guest, call, &reason).await)
            }
        })
    }

    /// The inode a `flock` through `fd` is kept on ([`FlockIdentity`]), or
    /// why it cannot be named: the device and inode of an `fstat`, and
    /// whether its filesystem decides flock itself, from an `fstatfs`, both
    /// through the recording, so that record and replay decide alike.
    async fn flock_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Result<FlockIdentity, String>, Error> {
        let stat = match self.recorded_fstat(guest, fd).await {
            Ok(stat) => stat,
            Err(StatFailure::Scratch(errno)) if self.cfg.recordreplay_modes => {
                return Err(Error::Tool(anyhow::anyhow!(
                    "cannot record flock(fd={fd}) under record and replay: no buffer for its \
                     fstat ({errno})"
                )));
            }
            Err(failure) => return Ok(Err(format!("its fstat failed ({})", failure.errno()))),
        };
        let own_flock = match self.recorded_fstatfs_type(guest, fd).await {
            Ok(f_type) => own_flock_filesystem(f_type).map(str::to_owned),
            Err(errno) => return Ok(Err(format!("its fstatfs failed ({errno})"))),
        };
        Ok(Ok(FlockIdentity {
            dev: stat.st_dev,
            ino: stat.st_ino,
            own_flock,
            modelled: false,
        }))
    }

    /// The filesystem type (`f_type`) of `fd`, by one `fstatfs` through the
    /// recording, whose buffer is in the guest's stack scratch, with the
    /// guest's bytes there saved first and put back after, as
    /// `Detcore::inject_stat_on_stack` does. Each attempt a signal stops
    /// before it runs is an event, so replay retries as the recording did.
    // GNU declares `f_type` as i64, musl as u64: its bits are kept.
    #[allow(clippy::unnecessary_cast)]
    async fn recorded_fstatfs_type<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<i64, Errno> {
        let mut stack = guest.stack().await;
        let buf: AddrMut<libc::statfs> = stack.reserve();
        let mut saved = [0u8; std::mem::size_of::<libc::statfs>()];
        guest.memory().read_exact(buf.cast(), &mut saved)?;
        let guard = match stack.commit() {
            Ok(guard) => guard,
            Err(errno) => {
                let _ = guest.memory().write_exact(buf.cast(), &saved);
                return Err(errno);
            }
        };
        let _ = guest.memory().write_exact(buf.cast(), &saved);
        let call = syscalls::Fstatfs::new().with_fd(fd).with_buf(Some(buf));
        let mut answer = Err(Errno::ERESTARTSYS);
        for _ in 0..64 {
            answer = self.record_or_replay(guest, call).await;
            if !matches!(answer, Err(Errno::EINTR | Errno::ERESTARTSYS)) {
                break;
            }
        }
        let f_type = answer.and_then(|_| {
            guest
                .memory()
                .read_value(buf)
                .map(|statfs: libc::statfs| statfs.f_type as i64)
        });
        let restored = guest.memory().write_exact(buf.cast(), &saved);
        drop(guard);
        let f_type = f_type?;
        restored?;
        Ok(f_type)
    }

    /// Mirror a decision of the model on the guest's own host description,
    /// where the backend has one ([`Self::flock_mirrors`]): a grant of `mode`
    /// as `flock(fd, mode | LOCK_NB)`, a release (`None`) as `LOCK_UN`,
    /// through the recording. `lagging` names the processes whose exit
    /// released a lock on the file and whose host teardown the backend has
    /// not reported: the host keeps their locks until then, which a grant's
    /// mirror waits for ([`Self::flock_mirror_after_teardown`]). A mirror that
    /// fails found a host lock the model does not know, and the call is
    /// refused by name.
    async fn flock_mirror<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
        mode: Option<FlockMode>,
        lagging: Vec<DetPid>,
    ) -> Result<i64, Error> {
        let description = guest
            .thread_state()
            .with_detfd(call.fd(), |detfd| detfd.clone())
            .ok();
        self.flock_mirror_on(guest, call, call.fd(), description, mode, lagging)
            .await
    }

    /// [`Self::flock_mirror`] on descriptor `fd`, which names the description
    /// the call took (`description`), whose cache of the kernel's mode it
    /// updates: the call's own descriptor, or, for a call that waited, another
    /// descriptor of the caller's table that names that description
    /// ([`Self::flock_mirror_waited`]). Where the refusal of a failed grant
    /// mirror lets the run go on (`--allow-unsupported-syscalls`: the guest
    /// gets `ENOLCK`, holding no lock), the model lets the grant go too
    /// ([`Self::flock_revoke`]), so that the guest, the model and the host
    /// agree that the description holds none.
    async fn flock_mirror_on<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
        fd: RawFd,
        description: Option<DetFd>,
        mode: Option<FlockMode>,
        lagging: Vec<DetPid>,
    ) -> Result<i64, Error> {
        if !self.flock_mirrors() {
            // KVM keeps no mirror: its host descriptions do not follow guest
            // lifetimes (https://github.com/rrnewton/hermit/issues/5043).
            note_flock_mode(description.as_ref(), mode);
            return Ok(0);
        }
        let operation = match mode {
            Some(FlockMode::Shared) => libc::LOCK_SH | libc::LOCK_NB,
            Some(FlockMode::Exclusive) => libc::LOCK_EX | libc::LOCK_NB,
            None => libc::LOCK_UN,
        };
        let mirror = call.with_fd(fd).with_operation(operation);
        let result = if mode.is_some() && !lagging.is_empty() {
            self.flock_mirror_after_teardown(guest, mirror, lagging)
                .await
        } else {
            self.flock_mirror_attempt(guest, mirror).await
        };
        let reason = match result {
            Ok(_) => {
                note_flock_mode(description.as_ref(), mode);
                return Ok(0);
            }
            Err(Errno::EWOULDBLOCK) => "Detcore granted the lock, and a lock its model does not \
                                        know holds the file on the host: a process outside the \
                                        run, or an open file description the guest inherited \
                                        locked"
                .to_owned(),
            Err(errno) => format!(
                "Detcore decided the call, and mirroring the decision on the host failed \
                 ({errno})"
            ),
        };
        let refused = self.refuse_flock(guest, call, &reason).await;
        if mode.is_some() && matches!(refused, Err(Error::Errno(_))) {
            self.flock_revoke(guest, description).await;
        }
        refused
    }

    /// The model lets go of the lock it granted `description`, as a `LOCK_UN`
    /// through it would, after the guest got `ENOLCK` for the call that took
    /// it ([`Self::flock_mirror_on`]); the requests blocked under the lock are
    /// woken. Nothing where Detcore never named the description's file.
    async fn flock_revoke<G: Guest<Self>>(&self, guest: &mut G, description: Option<DetFd>) {
        let Some(description) = description else {
            return;
        };
        let Some(identity) = description.flock_identity() else {
            return;
        };
        let open_file = description.open_file_id();
        let request = FlockRequest {
            inode: FlockInodeKey {
                dev: identity.dev,
                ino: identity.ino,
            },
            object: flock_object(identity.dev, identity.ino),
            open_file,
            slots: guest.thread_state().slots_naming(open_file),
            mode: None,
            nonblocking: false,
            mapped: guest.thread_state().maps_open_file(open_file),
            host_fallback: self.flock_mirrors(),
        };
        let _ = crate::tool_global::flock(guest, request).await;
        note_flock_mode(Some(&description), None);
    }

    /// A change the model made, in a waiting call's own turn, to the lock of
    /// the description the call took at its entry (`taken`): a grant of
    /// `mode`, or the release of the lock of the other mode its retry deleted
    /// (`None`). It is mirrored on that description, through a descriptor of
    /// the caller's table that still names it, as Linux's call changes the
    /// lock of the `struct file` it took, whatever a sibling did to its
    /// descriptor meanwhile. Where none does any longer, a grant may have been
    /// to the description's last reference, the call's own, which the model
    /// released with the call ([`crate::procfs::locks::LockLedger::flock_poll`]),
    /// so the host has no lock to take (the call returns 0, as Linux's does
    /// before its last `fput` releases the lock). Otherwise another reference
    /// (another process's table, a mapping, a copy in flight) keeps the
    /// description, whose lock changed, and the caller has no descriptor to
    /// change it on the host through: the call is refused by name. (The model
    /// reports a release only while such a reference remains, as with none the
    /// host's description, and its lock, went with its last descriptor.)
    async fn flock_mirror_waited<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
        taken: DetFd,
        mode: Option<FlockMode>,
        lagging: Vec<DetPid>,
    ) -> Result<i64, Error> {
        let open_file = taken.open_file_id();
        if let Some(fd) = guest.thread_state().slot_naming(open_file, call.fd()) {
            return self
                .flock_mirror_on(guest, call, fd, Some(taken), mode, lagging)
                .await;
        }
        if !self.flock_mirrors() {
            note_flock_mode(Some(&taken), mode);
            return Ok(0);
        }
        if mode.is_some()
            && crate::tool_global::flock_held_by(guest, open_file)
                .await
                .is_none()
        {
            note_flock_mode(Some(&taken), None);
            return Ok(0);
        }
        let refused = self
            .refuse_flock(guest, call, WAITER_DESCRIPTOR_GONE_REFUSAL)
            .await;
        if mode.is_some() && matches!(refused, Err(Error::Errno(_))) {
            self.flock_revoke(guest, Some(taken)).await;
        }
        refused
    }

    /// One mirror, through the recording, made again while a signal stops it
    /// before it runs: every attempt is an event, so replay makes as many.
    async fn flock_mirror_attempt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mirror: syscalls::Flock,
    ) -> Result<i64, Errno> {
        let mut last = Errno::ERESTARTSYS;
        for _ in 0..64 {
            match self.record_or_replay(guest, mirror).await {
                Err(errno @ (Errno::EINTR | Errno::ERESTARTSYS)) => last = errno,
                answer => return answer,
            }
        }
        Err(last)
    }

    /// A grant's mirror while processes whose exit released a lock on the
    /// file (`lagging`) may not be torn down on the host yet: Detcore's
    /// logical exit precedes the host's. Holding the caller's turn, so that no
    /// other guest thread runs meanwhile and the host's timing decides only
    /// how long this lasts, never which turn is next, it waits for that
    /// teardown as an event where the backend reports it to the ledger
    /// (ptrace and e9patch with ptrace when the tracer consumes the final
    /// status, LiteInst and in-guest-trap at the physical exit, whose
    /// scheduler also holds every turn until then), or for
    /// [`TEARDOWN_BACKSTOP`] at most, and then mirrors once: so a recording,
    /// which only ptrace makes, holds that one mirror, made after the
    /// teardown, and no attempt whose answer was the host's timing. DBT
    /// reports no teardown, and SaBRe reports it to the scheduler alone,
    /// which holds every turn until it: there the mirror is made, and made
    /// again while it answers `EWOULDBLOCK`, at most for
    /// [`TEARDOWN_BACKSTOP`], after which the answer is `EWOULDBLOCK`. A
    /// mirror that fails is refused by name.
    async fn flock_mirror_after_teardown<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mirror: syscalls::Flock,
        lagging: Vec<DetPid>,
    ) -> Result<i64, Errno> {
        // The backends that report each host teardown to the ledger
        // (`GlobalTool::on_backend_process_exited`, `LockLedger::flock_torn_down`).
        // SaBRe's ptrace supervisor reports physical exits to the scheduler
        // alone, which holds every turn until one completes
        // (`reports_physical_process_exits`): a later grant finds the holder
        // torn down, and its mirror is made as DBT's is.
        let backend = &self.cfg.backend;
        let reported =
            backend.reports_child_exit_publication || backend.process_exits_complete_asynchronously;
        let in_guest = backend.tool_shares_guest_descriptor_table
            || backend.runs_guest_code_between_injections;
        let started = Instant::now();
        let mut wait = TeardownWait::new(reported, started + TEARDOWN_BACKSTOP);
        let mut step = wait.first();
        let answer = loop {
            step = match step {
                TeardownStep::AskPending => {
                    let pending =
                        crate::tool_global::flock_teardown_pending(guest, lagging.clone()).await;
                    wait.after_pending(pending, Instant::now())
                }
                TeardownStep::Attempt => {
                    let answer = self.flock_mirror_attempt(guest, mirror).await;
                    wait.after_attempt(answer, Instant::now())
                }
                TeardownStep::Pause => {
                    // The ptrace tracer serves every guest task from one Tokio
                    // thread, so only this task may wait there; an in-guest
                    // tool blocks its own thread, as
                    // `race_probe::delay_on_any_backend` does.
                    if !in_guest && tokio::runtime::Handle::try_current().is_ok() {
                        tokio::time::sleep(TEARDOWN_POLL).await;
                    } else {
                        std::thread::sleep(TEARDOWN_POLL);
                    }
                    wait.after_pause()
                }
                TeardownStep::Done(answer) => break answer,
            };
        };
        // How long the host's teardown took, at debug level only: it is host
        // time, which no INFO record may carry.
        tracing::debug!(
            "[detcore] flock mirror after the host teardown of {lagging:?}: {answer:?} after {:?} \
             and {} poll(s) ({})",
            started.elapsed(),
            wait.polls,
            if reported {
                "teardown event"
            } else {
                "retried mirror"
            }
        );
        answer
    }

    /// A blocking `call` the model queued: it waits in Detcore's schedule as
    /// a pipe read does, a guest-state wait (only a guest's release can end
    /// it), asking the model at each turn ([`Self::flock_wait_attempt`]),
    /// which retries the call once a release woke it, until a retry takes the
    /// lock (0), the file leaves the model, or a signal ends the wait while
    /// the call is blocked (`ERESTARTSYS`, as Linux's
    /// `wait_event_interruptible` ends it): the attempt of that turn is made
    /// first, as Linux's woken waiter retries before it acts on the signal,
    /// and a call that is still blocked leaves the queue.
    async fn flock_wait<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
        taken: DetFd,
    ) -> Result<i64, Error> {
        let mut rsrc = Resources::new(guest.thread_state().dettid);
        rsrc.insert(ResourceID::InternalIOPolling, Permission::W);
        rsrc.fyi("flock");
        rsrc.polls_guest_state_only = true;
        // The description the call took at its entry, which each attempt's
        // grant is mirrored on (`ThreadState::flock_waiting_description`).
        guest.thread_state_mut().flock_waiting_description = Some(taken);
        let result =
            crate::syscalls::helpers::retry_nonblocking_syscall(guest, call, rsrc, Some(self))
                .await;
        guest.thread_state_mut().flock_waiting_description = None;
        if result.is_ok() {
            return result;
        }
        // The wait ended without a lock: the caller's request leaves the
        // model, if a refusal or the file's leaving it did not take it out
        // already.
        let interrupted = matches!(&result, Err(Error::Errno(Errno::ERESTARTSYS)));
        crate::tool_global::flock_cancel(guest).await;
        if interrupted && self.flock_wait_stopped(guest) {
            return self.refuse_flock(guest, call, STOPPED_WAITER_REFUSAL).await;
        }
        result
    }

    /// Whether a default job-control stop is pending for the caller, whose
    /// wait a signal just ended without a lock: `SIGSTOP`, or `SIGTSTP`,
    /// `SIGTTIN` or `SIGTTOU` while their disposition is `SIG_DFL`, read from
    /// the kernel's signal state of the thread where guest threads are host
    /// tasks. The scheduler ended the wait for that signal in this turn, and a
    /// guest's own call alone changes a pending bit Detcore recorded the send
    /// of, so the read decides as the scheduler did.
    fn flock_wait_stopped<G: Guest<Self>>(&self, guest: &G) -> bool {
        let Some((pid, tid)) = crate::syscalls::helpers::guest_signal_state_task(guest) else {
            return false;
        };
        let Ok(state) = crate::syscalls::read_thread_signal_state(
            reverie::Pid::from_raw(pid),
            reverie::Pid::from_raw(tid),
        ) else {
            return false;
        };
        let stops =
            crate::syscalls::kernel_sigset_bit(libc::SIGSTOP) | state.default_job_control_stops();
        state.pending & !state.blocked & stops != 0
    }

    /// One turn of a waiting `call` ([`Self::flock_wait`]), made in place of
    /// the kernel attempt `retry_nonblocking_syscall` makes for a pipe: the
    /// model's answer to the call's retry where a release woke it
    /// ([`crate::procfs::locks::LockLedger::flock_poll`]), with its mirror, so
    /// that a lock is placed on the host in the turn that takes it;
    /// `EWOULDBLOCK` while the call waits; the kernel's path where the file
    /// left the model; a refusal by name where the model refuses it.
    pub(crate) async fn flock_wait_attempt<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
    ) -> Result<i64, Error> {
        // The description the call took at its entry (`Self::flock_wait`).
        let taken = guest.thread_state().flock_waiting_description.clone();
        match crate::tool_global::flock_poll(guest).await {
            FlockPoll::Granted { lagging } => {
                let mode = match call.operation() & !libc::LOCK_NB {
                    libc::LOCK_SH => FlockMode::Shared,
                    _ => FlockMode::Exclusive,
                };
                match taken {
                    Some(taken) => {
                        self.flock_mirror_waited(guest, call, taken, Some(mode), lagging)
                            .await
                    }
                    None => self.flock_mirror(guest, call, Some(mode), lagging).await,
                }
            }
            FlockPoll::Waiting { dropped } => {
                // The retry deleted the description's lock of the other mode
                // before it found its conflict, as a conversion does: the
                // host's goes in this turn too.
                if dropped {
                    let mirrored = match taken {
                        Some(taken) => {
                            self.flock_mirror_waited(guest, call, taken, None, Vec::new())
                                .await
                        }
                        None => self.flock_mirror(guest, call, None, Vec::new()).await,
                    };
                    if let Err(error) = mirrored {
                        crate::tool_global::flock_cancel(guest).await;
                        return Err(error);
                    }
                }
                Err(Errno::EWOULDBLOCK.into())
            }
            FlockPoll::HostDecided => {
                // The kernel decides the call from now on, through a
                // descriptor that names the description the call took.
                let fd = match &taken {
                    Some(taken) => guest
                        .thread_state()
                        .slot_naming(taken.open_file_id(), call.fd()),
                    None => Some(call.fd()),
                };
                match fd {
                    Some(fd) => self.flock_through_the_kernel(guest, call.with_fd(fd)).await,
                    None => {
                        self.refuse_flock(guest, call, WAITER_DESCRIPTOR_GONE_KERNEL_REFUSAL)
                            .await
                    }
                }
            }
            FlockPoll::Refused(reason) => self.refuse_flock(guest, call, &reason).await,
        }
    }

    /// Refuse `call` by name, for `reason`, through the unsupported-operation
    /// policy: the run ends, or with `--allow-unsupported-syscalls` the guest
    /// gets `ENOLCK`.
    async fn refuse_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: syscalls::Flock,
        reason: &str,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        super::report_flock_refusal(&format!(
            "[dtid {dettid}] flock(fd={}, operation={:#x}) refused: {reason}",
            call.fd(),
            call.operation()
        ));
        if !self.cfg.panic_on_unsupported_syscalls {
            crate::tool_global::report_unsupported_syscall(guest, Sysno::flock).await;
        }
        self.refuse_unserviceable_operation_because(guest, Sysno::flock, Errno::ENOLCK, reason)
            .await
    }

    /// A successful file-backed `mmap` through `fd` mapped its description,
    /// which keeps it, and a flock lock it holds, after its last descriptor
    /// goes: noted in this process's table (`ThreadState::maps_open_file`),
    /// and reported to the model only for a description it decided a call
    /// through, so that no other `mmap` costs a request.
    pub(crate) async fn note_flock_mapping<G: Guest<Self>>(&self, guest: &mut G, fd: RawFd) {
        let Ok((open_file, modelled)) = guest.thread_state().with_detfd(fd, |detfd| {
            (detfd.open_file_id(), detfd.is_flock_modelled())
        }) else {
            return;
        };
        guest.thread_state().note_mapped_open_file(open_file);
        if modelled {
            crate::tool_global::flock_mapped(guest, open_file).await;
        }
    }

    /// A successful send passed the descriptors `fds` over `SCM_RIGHTS`: a
    /// receipt may hold each description under an identity Detcore cannot
    /// join to it, or out of the guest, so that Detcore can no longer follow
    /// its references to the end, except for the copies the lock ledger
    /// counted in flight (`followed`), which it follows to their receipt
    /// ([`Self::note_flock_receipts_unjoined`] takes over where a receipt does
    /// not join). Each other description is marked so
    /// (`DetFd::mark_references_escaped`), and the inode of a lock or waiter
    /// the model keeps for one the ledger follows leaves the model
    /// (`procfs::locks::LockLedger::flock_unfollowable`). Only a send of a
    /// description the ledger follows costs a request.
    pub(crate) async fn note_flock_descriptors_sent<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fds: &[RawFd],
        followed: &[OpenFileId],
    ) {
        let mut escaped = Vec::new();
        for fd in fds {
            if let Ok(Some(open_file)) = guest.thread_state().with_detfd(*fd, |detfd| {
                let open_file = detfd.open_file_id();
                if followed.contains(&open_file) {
                    return None;
                }
                detfd.mark_references_escaped();
                detfd.is_lock_tracked().then_some(open_file)
            }) {
                escaped.push(open_file);
            }
        }
        escaped.sort();
        escaped.dedup();
        if !escaped.is_empty() {
            crate::tool_global::flock_unfollowable(guest, escaped).await;
        }
    }

    /// A receive took queued copies of these descriptions (`lock_copies`, the
    /// copies the lock ledger counted in flight) without joining each to the
    /// description its sender's table modeled: the receiver holds it under an
    /// identity of its own, or Detcore could not match the message. The
    /// ledger lets the copies go with the message, so the model can no longer
    /// place their descriptions' release: their inodes leave it
    /// (`procfs::locks::LockLedger::flock_unfollowable`).
    pub(crate) async fn note_flock_receipts_unjoined<G: Guest<Self>>(
        &self,
        guest: &mut G,
        mut lock_copies: Vec<OpenFileId>,
    ) {
        lock_copies.sort();
        lock_copies.dedup();
        if !lock_copies.is_empty() {
            crate::tool_global::flock_unfollowable(guest, lock_copies).await;
        }
    }

    /// Where no host lock can decide a file (KVM), the refusal of a close or
    /// replacement of the slots `fds` of this thread's table (by `sysno`)
    /// that would drop the last reference of a description holding a lock
    /// the model decides while a mapping of it may remain: another process's,
    /// or this process's own, which its memory map still shows or cannot be
    /// read to rule out (`procfs::locks::LockLedger::flock_close_refusal`).
    /// Linux keeps such a lock until the last mapping goes, which Detcore
    /// cannot place there. `None` elsewhere, and when the close may go on.
    pub(crate) async fn refuse_flock_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        fds: RangeInclusive<RawFd>,
    ) -> Option<Result<i64, Error>> {
        if self.flock_mirrors() {
            return None;
        }
        let mut closing: Vec<(OpenFileId, u32, u64)> = Vec::new();
        let numbers = guest.thread_state().open_fds();
        for fd in numbers.into_iter().filter(|fd| fds.contains(fd)) {
            let Ok(Some((open_file, ino))) = guest.thread_state().with_detfd(fd, |detfd| {
                detfd.is_flock_modelled().then(|| {
                    (
                        detfd.open_file_id(),
                        detfd.flock_identity().map(|id| id.ino),
                    )
                })
            }) else {
                continue;
            };
            match closing.iter_mut().find(|(known, _, _)| *known == open_file) {
                Some((_, slots, _)) => *slots += 1,
                None => closing.push((open_file, 1, ino.unwrap_or_default())),
            }
        }
        for (open_file, closed, ino) in closing {
            let remaining = guest
                .thread_state()
                .slots_naming(open_file)
                .saturating_sub(closed);
            if remaining > 0 {
                continue;
            }
            let mapped = guest.thread_state().maps_open_file(open_file);
            let maps_inode = mapped && self.memory_map_may_show(guest, ino);
            if let Some(reason) = crate::tool_global::flock_close_check(
                guest, open_file, remaining, mapped, maps_inode,
            )
            .await
            {
                let dettid = guest.thread_state().dettid;
                super::report_flock_refusal(&format!("[dtid {dettid}] {sysno} refused: {reason}"));
                return Some(
                    self.refuse_unserviceable_operation_because(
                        guest,
                        sysno,
                        Errno::EOPNOTSUPP,
                        &reason,
                    )
                    .await,
                );
            }
        }
        None
    }

    /// Whether this thread's memory map may show a mapping of the file with
    /// inode number `ino`: it lists one, or it is not complete, or the
    /// backend cannot give it, so that no missing entry can rule one out.
    fn memory_map_may_show<G: Guest<Self>>(&self, guest: &G, ino: u64) -> bool {
        match guest.memory_map(guest.tid()) {
            Ok(map) if map.complete => map.mappings.iter().any(|mapping| match &mapping.backing {
                reverie::MappingBacking::File { raw_inode, .. }
                | reverie::MappingBacking::NamedSharedAnonymous { raw_inode, .. } => {
                    *raw_inode == ino
                }
                _ => false,
            }),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_flock_filesystems_are_named_by_their_magic() {
        assert_eq!(own_flock_filesystem(0x6969), Some("NFS"));
        assert_eq!(own_flock_filesystem(0xFF53_4D42), Some("SMB"));
        assert_eq!(own_flock_filesystem(0x6573_5546), Some("FUSE"));
        // ext4, tmpfs, btrfs, xfs, overlayfs: the kernel's own flock.
        for local in [0xEF53, 0x0102_1994, 0x9123_683E, 0x5846_5342, 0x794C_7630] {
            assert_eq!(own_flock_filesystem(local), None);
        }
    }

    #[test]
    fn a_flock_object_is_spelled_as_proc_locks_spells_it() {
        assert_eq!(flock_object(libc::makedev(0, 0x2a), 1001), "00:2a:1001");
        assert_eq!(flock_object(libc::makedev(259, 3), 7), "103:03:7");
    }

    /// Drive a [`TeardownWait`] to its answer with scripted calls: `pending`
    /// answers each question whether the teardown is still to be reported,
    /// `attempts` each mirror, and every pause moves the clock a millisecond.
    /// Returns the answer, the number of mirrors made and the pauses.
    fn drive(
        reported: bool,
        deadline_ms: u64,
        mut pending: impl FnMut() -> bool,
        mut attempts: impl FnMut() -> Result<i64, Errno>,
    ) -> (Result<i64, Errno>, u32, u32) {
        let start = Instant::now();
        let mut now = start;
        let mut wait = TeardownWait::new(reported, start + Duration::from_millis(deadline_ms));
        let mut step = wait.first();
        let mut mirrors = 0;
        loop {
            step = match step {
                TeardownStep::AskPending => wait.after_pending(pending(), now),
                TeardownStep::Attempt => {
                    mirrors += 1;
                    wait.after_attempt(attempts(), now)
                }
                TeardownStep::Pause => {
                    now += Duration::from_millis(1);
                    wait.after_pause()
                }
                TeardownStep::Done(answer) => return (answer, mirrors, wait.polls),
            };
        }
    }

    /// DBT reports no teardown: a mirror the host's lingering lock refuses is
    /// made again, a pause apart, until it succeeds (the 90 hand-offs the
    /// campaign measured never needed a second mirror; Codex review of the kvm
    /// lane's slice SP2).
    #[test]
    fn without_a_teardown_report_the_mirror_is_retried_until_it_succeeds() {
        let mut answers = vec![Err(Errno::EWOULDBLOCK), Err(Errno::EWOULDBLOCK), Ok(0)].into_iter();
        assert_eq!(
            drive(false, 30_000, || unreachable!(), || answers.next().unwrap()),
            (Ok(0), 3, 2)
        );
        // Another errno ends the wait at once: the mirror failed otherwise.
        assert_eq!(
            drive(false, 30_000, || unreachable!(), || Err(Errno::EBADF)),
            (Err(Errno::EBADF), 1, 0)
        );
    }

    /// The backstop: a mirror that keeps failing ends with `EWOULDBLOCK` at the
    /// deadline, which `Detcore::flock_mirror` refuses by name. Where the
    /// teardown is never reported, the mirror is made once at the deadline,
    /// so a host lock already gone grants the call rather than a refusal.
    #[test]
    fn a_mirror_that_keeps_failing_stops_at_the_backstop() {
        let (answer, mirrors, polls) =
            drive(false, 5, || unreachable!(), || Err(Errno::EWOULDBLOCK));
        assert_eq!(answer, Err(Errno::EWOULDBLOCK));
        assert_eq!((mirrors, polls), (6, 5));
        for (host, expected) in [
            (Err(Errno::EWOULDBLOCK), Err(Errno::EWOULDBLOCK)),
            (Ok(0), Ok(0)),
        ] {
            let mut asked = 0;
            let (answer, mirrors, _) = drive(
                true,
                5,
                || {
                    asked += 1;
                    true
                },
                || host,
            );
            assert_eq!(answer, expected);
            assert_eq!(mirrors, 1);
            assert_eq!(asked, 6);
        }
    }

    /// Where the backend reports the teardown to the ledger (ptrace,
    /// LiteInst, in-guest-trap), the wait asks until it is reported and
    /// mirrors once, whatever that mirror answers.
    #[test]
    fn with_a_teardown_report_the_mirror_waits_for_it_and_is_made_once() {
        let mut pending = vec![true, true, false].into_iter();
        assert_eq!(
            drive(
                true,
                30_000,
                || pending.next().unwrap(),
                || Err(Errno::EWOULDBLOCK)
            ),
            (Err(Errno::EWOULDBLOCK), 1, 2)
        );
        assert_eq!(drive(true, 30_000, || false, || Ok(0)), (Ok(0), 1, 0));
    }
}
