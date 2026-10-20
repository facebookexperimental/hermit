/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use detcore::BlockingMode;
use detcore_model::config::MountInfoRootRewrite;
use reverie::process::Command;
use serde::Deserialize;
use serde::Serialize;

use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;

/// Hermit record version. Recorded as part of hermit-record, hermit-replay
/// will check this version and will fail if hermit-record version is newer.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Deserialize,
    Serialize
)]
#[repr(transparent)]
pub struct RecordVersion(u32);
impl RecordVersion {
    /// Check if the recorder/replayer version is compatible with a given
    /// recording (trace).
    pub fn compatible_with(&self, other: &RecordVersion) -> bool {
        self == other
    }
}

/// hermit record/replay version.
// NB: Increase the version number when there are breaking changes, i.e.:
// when new syscalls or event schemas are added.
//
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#2373)
// 0x10b -> 0x10c: flock(2) stopped being a Detcore no-op and now reaches
// `record_or_replay`, so a post-fix run emits a `Return` event per flock call.
// Replay re-issues calls for materialized descriptors and fails closed when it
// cannot reproduce the lock side effect. A 0x10b recording contains NO flock
// event at all -- the old handler returned Ok(0) before ever reaching the
// recorder, even though flock was already classified Determinized -- so
// replaying one under this build would read the next thread event for every
// flock and desynchronize the stream. The version gate must refuse it.
//
// TODO-HUMAN-REVIEW(#2370)
// 0x10d -> 0x10e: record/replay now carries successful exec image/path state and
// an explicit ppoll timeout-pointer shape. Older readers cannot interpret those
// events without desynchronizing the stream, so the format advances once.
//
// TODO-HUMAN-REVIEW(#2272)
// 0x10c -> 0x10d: `Ppoll` becomes its OWN event rather than sharing `Poll`'s,
// because ppoll additionally copies out a timeout and must preserve it on
// EINTR and on a partial EFAULT copy-out; and `Poll` itself now records the
// partial `revents` copy-out an error return leaves behind. The recorded
// stream therefore carries a different event shape for both syscalls.
//
// ONE increment covers BOTH halves deliberately. This branch bumped twice --
// once per format change -- but a reader either understands the new stream or
// it does not, so what matters is that the version differs from every stream
// a different shape was written under. Landing two increments would imply a
// 0x10d recording exists that this build can read and it cannot: no build
// ever wrote one.
//
// ⚠️ THIS BUMP IS WHY THE REBASE COULD NOT SIMPLY TAKE EITHER SIDE. This
// change was authored against 0x10a and bumped to 0x10b; main has since gone
// to 0x10c for the unrelated flock work. Keeping the branch's 0x10b would
// move the version BACKWARDS and let a 0x10c flock recording be replayed by a
// build whose ppoll events have a different shape. Keeping main's 0x10c
// unchanged would be worse: the format would change with no bump at all, so a
// recording made here would claim 0x10c while containing a `Ppoll` event the
// 0x10c reader does not know -- exactly the desynchronization the paragraph
// above exists to prevent. The version must go FORWARD once more.
//
// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#2407)
// 0x10e -> 0x10f: pidfd_getfd(2) is now explicitly subscribed and recorded.
// Earlier builds let the syscall bypass the recorder, so a successful call
// changed the guest descriptor table without a corresponding stream event.
// The replayer now consumes one exact pidfd_getfd event and reissues the
// syscall to reproduce the kernel side effect; accepting a 0x10e stream would
// therefore consume the next event at every pidfd_getfd call.
//
// 0x10f -> 0x110: metadata now carries the recording namespace's proven
// Hermit-owned mount IDs. Detcore applies that map after both the recorder's
// raw read and the replayer's ReadV2 copyout. An older recording has no map and
// would replay different guest-visible mountinfo bytes under this sanitizer,
// so accepting it would violate replay fidelity even though the event enum did
// not change.
//
// 0x110 -> 0x111: metadata now also carries the recording namespace's full
// raw mount-ID order. That makes fdinfo's mnt_id use the same canonical identity
// as mountinfo during replay instead of consulting the unrelated replay
// namespace or collapsing all descriptors to one mount.
// 0x111 -> 0x113: fdinfo mount identities are now keyed by each observed raw
// mount ID rather than broad descriptor classes. Metadata carries only the
// completed namespace's mountinfo row/parent order; pseudo-filesystem IDs that
// are absent from mountinfo are allocated afterward in deterministic guest
// observation order. Replay must rebuild that mapping from recording-time raw
// bytes, or pidfs/nsfs/anon_inodefs can be collapsed or rejected. The draft
// 0x112 format was never shipped.
//
// 0x113 -> 0x114: recordings persist the completed guest's mountinfo order and
// unlisted fdinfo mount-ID order, and event streams use stable recording-local
// thread IDs rather than host PID allocation. An older reader cannot rebuild
// either mapping reliably for a plain public record API recording.
//
// 0x114 -> 0x115: event stream filenames now encode deterministic process-tree
// pedigrees instead of process-local allocation order. Older readers look for
// integer filenames and cannot replay a recording containing child streams.
//
// 0x115 -> 0x116: event stream filenames are fixed-size SHA-256 names and each
// data/debug stream begins with its complete process-tree identity. Older
// readers cannot skip or validate these headers.
// 0x116 -> 0x119: nonpositioned random-device readv now uses Detcore's
// shared canonical cursor and no longer emits or consumes recorder ReadvV2
// events. Old event streams cannot be replayed under this routing.
// 0x117 and 0x118 identify incompatible retained RNG, clock, and network
// recordings from parallel development; do not reinterpret those identities.
// 0x119 -> 0x11a: random-device preadv/preadv2 now use Detcore's canonical
// generator, including the shared cursor for preadv2(-1). They no longer emit
// or consume ReadvV2 events; accepting an older recording would desynchronize
// the event stream and replace its recorded random bytes.
// Early rejection of invalid negative offsets also stops emitting ReadvV2
// error events for non-random descriptors.
// 0x11a -> 0x11b: sysinfo uptime rounds exact elapsed logical time up, removing
// dependence on the calendar epoch's fraction. Detcore recomputes sysinfo during
// recording/replay, even with virtualize_time=false; replaying an older stream
// under the new projection could change guest control flow and event consumption.
// Sysinfo remains determinized rather than gaining a recorded event.
// 0x11b -> 0x11c: /proc/uptime renders the floor of exact elapsed logical time
// instead of the difference of the separately truncated clock and epoch, so a
// fractional epoch no longer moves it (or /proc/stat's idle ticks derived from
// it) by one second. /proc/stat btime is now derived from the fixed boot
// instant rather than from the clock minus uptime. Detcore recomputes these
// fields over the recorded procfs bytes during recording and replay, so an older
// stream replayed under the new projection could show the guest different bytes.
// 0x11c -> 0x11d: with time not virtualized, which is always the case for
// record/replay, detcore passes clock reads to record/replay instead of refusing
// them. Streams now carry Timespec and Timeofday events for clock_gettime,
// gettimeofday and time, and clock_getres gains a recorder event (Timespec, or
// Return for a NULL `res`). An older reader would desynchronize on these events.
// 0x11d -> 0x11e: guest-semantic madvise advice, which detcore used to refuse
// with ENOSYS, now reaches the recorder and replayer and emits a Madvise event.
// 0x11e -> 0x11f: faccessat, faccessat2, chdir, getcwd and the legacy path
// mutations (rename, link, symlink, chmod, chown, lchown, mknod, rmdir) are
// recorded instead of running live. A successful chdir or fchdir carries a
// Bytes event naming the directory it entered. An older reader would
// desynchronize on these events.
// 0x11f -> 0x120: descriptor syncs and metadata changes (fsync, fdatasync,
// syncfs, fchmod, fchown, fsetxattr, fremovexattr) record a Return event, the
// xattr queries (getxattr, lgetxattr, fgetxattr and the listxattr family)
// record a Bytes event (Return for a zero-size query), and zero-count read and
// pread64 calls are recorded instead of running live. An older reader would
// desynchronize on these events.
// 0x120 -> 0x121: splice and tee refuse with EINVAL instead of ENOSYS; vmsplice
// keeps ENOSYS. Record and replay always fail closed, and Detcore computes this
// refusal itself rather than recording it, so replay recomputes the errno. The
// guest can branch on it: GNU grep falls back to read/write on EINVAL but
// reports a read error and exits 2 on ENOSYS. Replaying an older stream under
// the new errno could change guest control flow and event consumption, although
// no event shape changed.
// 0x121 -> 0x122: Detcore numbers raw mount IDs in the order the guest first
// observes them through mountinfo or fdinfo, instead of by row position in the
// run's starting mountinfo. Metadata carries that order as
// `mount_id_assignment_order`, replacing `fdinfo_unlisted_mount_ids`, and
// replay numbers every raw ID from it. An older recording's metadata cannot
// rebuild the mapping its guest saw, so replaying it could change the mount IDs
// the guest reads and its control flow.
// 0x122 -> 0x123: Detcore's inode pool starts at 9000 instead of 1 and every
// numbering request consumes a number, so the deterministic inode numbers
// differ (https://github.com/rrnewton/hermit/issues/2897). Even with
// virtualize_metadata off, as record/replay runs, the procfs sanitizer renders
// pool numbers into /proc/*/fdinfo `ino:` lines and /proc/*/maps and smaps
// inode columns over the recorded raw bytes, during recording and again during
// replay. An older recording replayed under the new numbering could show the
// guest different bytes, although no event shape changed.
// 0x123 -> 0x124: accept and accept4 record an `Accept` event, and bind, listen
// and shutdown record a `Return` event, instead of running live during replay
// (https://github.com/rrnewton/hermit/issues/3864). An older recording has no
// events for these calls, so this replayer would consume another call's event.
// 0x124 -> 0x125: a blocking accept on a listener the container created runs in
// the caller's turn (https://github.com/rrnewton/hermit/issues/3880). It
// records one `Accept` event per nonblocking attempt, where an older recording
// has a single `Accept` event for the backgrounded call. (Its timeout comes
// from Detcore's model of `setsockopt`, not from a recorded event.) Connections accepted under record
// are no longer made nonblocking. An older recording would hand this replayer
// the wrong number of events. A recording also carries a `rejoins` file, the
// turns at which backgrounded calls rejoined the run queue, which replay
// requires.
// 0x125 -> 0x126: in the precise futex mode a FUTEX_WAKE that wakes fewer
// waiters than are waiting takes the longest waiters first, as Linux does,
// instead of the most recent ones, and a FUTEX_WAKE_BITSET leaves the waiters
// it skips in arrival order (https://github.com/rrnewton/hermit/issues/3917).
// Futex waits and wakes are not recorded events; replay recomputes which thread
// each wake takes, so an older recording replayed under the new order could run
// a different schedule, although no event shape changed. Schedule and
// preemption files written by `--record-preemptions-to` carry no version and
// are likewise tied to the old order.
// 0x126 -> 0x127: a vfork barrier ends at the child's release edge (its exec
// or `Exit` grant) whether or not the parent's death has been seen. With a
// live parent, the scheduler then commits no turn until the parent's
// continuation is posted, where the child used to keep running alone for a
// host-timed number of turns (https://github.com/rrnewton/hermit/issues/3984).
// Turn selection is not recorded; replay recomputes it, so an older recording
// containing a vfork replayed under the new rule could run a different
// schedule, although no event shape changed.
// 0x127 -> 0x128: on ptrace, `prctl(PR_SET_CHILD_SUBREAPER)` and
// `PR_GET_CHILD_SUBREAPER` run live in record and replay alike instead of
// failing with ENOSYS, and the scheduler re-parents orphans to a child
// subreaper (https://github.com/rrnewton/hermit/issues/3997). An older
// recording saw ENOSYS for both calls; this replayer would run them and could
// take another branch and a different schedule, although no event shape
// changed.
// 0x128 -> 0x129: record and replay set
// `supports_blocked_wait_signal_interruption`, so the scheduler models
// signal targets as `hermit run` does on ptrace
// (https://github.com/rrnewton/hermit/issues/3963). A thread that a committed
// signal wakes out of a backgrounded call rejoins at the release barrier, a
// precise FUTEX_WAIT or an rt_sigtimedwait ends only for a signal that would end
// it on Linux, and an emulated pause or nanosleep ends for another thread's
// signal. (poll, epoll_wait, select and pselect6 are logged background calls
// here, which the kernel itself ends.) Replay recomputes all of these, so an
// older recording replayed here could run a different schedule, although no
// event shape changed.
// 0x129 -> 0x12a: a SIGCHLD the scheduler sends at a commit point (a child's
// child-exit timer, or a POSIX timer) ends a precise FUTEX_WAIT that catches
// it, as on Linux, where it was held until the call returned
// (https://github.com/rrnewton/hermit/issues/4005). Replay recomputes precise
// futex results, so an older recording replayed here could take the other
// branch, although no event shape changed (Codex review of
// https://github.com/rrnewton/hermit/pull/4039).
// 0x12a -> 0x12b: on ptrace, a clone without CLONE_THREAD and with a
// non-SIGCHLD exit signal is its own process to the Tool, with its own pid and
// its kernel parent (https://github.com/rrnewton/hermit/issues/4012). Detcore
// recomputes calls that name that process (getpid-based prlimit, its exit and
// its parent's notification) from it, so an older recording replayed here
// could take another branch, although no event shape changed.
// 0x12b -> 0x12c: a precise FUTEX_WAIT woken for a signal that is no longer
// pending for it re-reads its word and answers EAGAIN if the word changed
// (EFAULT if unreadable), where it answered 0 as a wakeup; Linux's woken waiter
// reaches the same EAGAIN through -ERESTARTSYS and the restarted wait. A signal
// still pending for it is answered before the word is compared
// (https://github.com/rrnewton/hermit/issues/4033). Replay recomputes
// precise futex results, so an older recording replayed here could see another
// result and take another branch, although no event shape changed.
// 0x12c -> 0x12d: a precise FUTEX_WAIT or FUTEX_WAIT_BITSET checks its timeout
// and bitset before the word, and reads the word with user-mode permissions, as
// Linux's `futex_wait_setup` does: EFAULT for an unmapped or PROT_NONE word,
// where the tracer's read gave EIO or compared a PROT_NONE word's value
// (https://github.com/rrnewton/hermit/issues/4030). Replay recomputes precise
// futex results, so an older recording replayed here could see another errno
// and take another branch, although no event shape changed (Codex review of
// https://github.com/rrnewton/hermit/pull/4061).
// 0x12d -> 0x12e: a pidfd_send_signal to a process with one live thread tells
// the scheduler the signal is pending for that thread, as kill does, so a
// receiver parked in a blocking call is woken to take it
// (https://github.com/rrnewton/hermit/issues/4099). Replay recomputes
// the schedule, so an older recording replayed here could be scheduled
// differently, although no event shape changed.
// 0x12e -> 0x12f: a kill to a zombie (exited, its parent can still wait for
// it) answers 0, as Linux's kill_pid_info does, where it answered ESRCH; a
// kill(pid, 0) to an exited process answers from the same model; and an
// rt_sigaction(SIGCHLD) tells the scheduler whether the process now reaps its
// children automatically (https://github.com/rrnewton/hermit/issues/4091).
// Replay recomputes these answers and the schedule, so an older recording
// replayed here could see another result and take another branch, although
// no event shape changed.
// 0x12f -> 0x130: `epoll_pwait` is recorded with the event `epoll_wait`
// records, and replay serves it from the recording. It used to run live in
// record and replay alike, so an older recording holds no event for it and this
// replayer would desynchronize at the first one.
// 0x130 -> 0x131: an rt_sigqueueinfo to a zombie is forwarded to the kernel,
// which checks the siginfo and then finds the zombie (0), where it answered
// ESRCH (https://github.com/rrnewton/hermit/issues/4091). Replay recomputes
// the answer, so an older recording replayed here could see another result
// and take another branch, although no event shape changed.
// 0x131 -> 0x132: a process-directed signal naming a live non-leader thread's
// id goes to that thread's process, as Linux's kill_pid_info does, where it
// answered ESRCH (https://github.com/rrnewton/hermit/issues/4094). Replay
// recomputes the answer, so an older recording replayed here could see another
// result and take another branch, although no event shape changed.
// 0x132 -> 0x133: record readmits a finished background call while the only
// runnable work queued is threads that yielded and pollers, where it used to
// wait for every yielder to stop (https://github.com/rrnewton/hermit/issues/4092).
// Replay follows the logged readmission turn only once its gate opens, and an
// older replayer's gate stays shut in that state, so it could not consume the
// readmission: it would refuse, or hang behind a yielder that never stops
// (Codex review of https://github.com/rrnewton/hermit/pull/4120).
// 0x133 -> 0x134:
// a masked or zero-timeout ppoll on a stack with no scratch below its red zone
// stages its private timeout and mask in the red zone, where it returned EFAULT
// and recorded nothing, and a masked record/replay pselect6 that replay could
// not deliver an interruption of is refused
// (https://github.com/rrnewton/hermit/issues/4113,
// https://github.com/rrnewton/hermit/issues/4112). An older recording of such a
// ppoll holds no event for the call, so this replayer would desynchronize at it
// (Claude review of https://github.com/rrnewton/hermit/pull/4127).
// 0x134 -> 0x135: an rt_sigqueueinfo to a gone process makes the kernel check
// its siginfo (EFAULT, E2BIG, EPERM) before answering ESRCH, where Detcore
// answered ESRCH first; an rt_sigqueueinfo existence check (signal 0) answers
// from the same exit model (https://github.com/rrnewton/hermit/issues/4124).
// Replay recomputes the answer, so an older recording replayed here could see
// another result and take another branch, although no event shape changed.
// 0x135 -> 0x136: on ptrace, a guest-sent SIGKILL retires its victims at the
// sending turn instead of at their host-timed deregistration
// (https://github.com/rrnewton/hermit/issues/3994). Turn and wait selection
// are not recorded; replay recomputes them, so an older recording replayed
// under the new rule could run a different schedule. The metadata also gains
// `replay_refused`, which an older replayer would not check.
// 0x136 -> 0x137: `epoll_pwait2` is recorded with the event `epoll_wait`
// records, and replay serves it from the recording; it ran live before
// (https://github.com/rrnewton/hermit/issues/4088).
// 0x137 -> 0x138: a guest send of a default-stop signal records SIGHUP and
// SIGCONT as host-timed for every process, which an orphaned process group
// can receive at an exit's physical moment
// (https://github.com/rrnewton/hermit/issues/4111), so no gated wait ends for
// them afterwards. Replay recomputes the schedule, so an older recording
// replayed here could see such a wait end, although no event shape changed.
// Record and replay refuse the newly admitted signal to the sender's own
// process (https://github.com/rrnewton/hermit/issues/4034), so that change
// alters no recording.
// 0x138 -> 0x139: pidfd_send_signal with a PIDFD_SIGNAL_THREAD or
// PIDFD_SIGNAL_THREAD_GROUP flag, or through a /proc/<pid> directory, is
// forwarded, where Detcore answered EINVAL or EBADF without an event
// (https://github.com/rrnewton/hermit/issues/4108). An older recording holds no
// event for such a call, so this replayer would desynchronize at it.
// 0x13b -> 0x13c: pidfd_send_signal through the PIDFD_SELF_THREAD or
// PIDFD_SELF_THREAD_GROUP sentinel is forwarded, where Detcore answered EBADF
// without an event (https://github.com/rrnewton/hermit/issues/4147). An older
// recording holds no event for such a call, so this replayer would
// desynchronize at it.
// 0x13c -> 0x13d: timer_create with SIGEV_THREAD_ID answers EINVAL for a
// thread outside the caller's process (and for an invalid notify value, where
// it answered ENOSYS), and the timer's expirations go to that thread alone,
// dropped once it has exited, where they went to the whole process
// (https://github.com/rrnewton/hermit/issues/4164). Replay recomputes both, so
// an older recording replayed here could see different answers and deliveries.
// 0x13d -> 0x13e: a vfork child that Detcore backgrounded, and that the vfork
// gate waits for while every queued thread is excluded, is readmitted as soon
// as its call finishes, without a rejoin entry: before it, no turn can commit
// (https://github.com/rrnewton/hermit/issues/4163). An older recorder spun
// forever in that state, and replay recomputes the schedule, so an older
// recording replayed here could see the readmission where its recorder did
// not.
// 0x13e -> 0x13f: a guest in a nested PID namespace has its pids translated
// (https://github.com/rrnewton/hermit/issues/4155,
// https://github.com/rrnewton/hermit/issues/4090): kill and its relatives
// deliver where they answered ESRCH, fork returns the nested pid, waits reap
// where they answered ECHILD, the init's own default signals are dropped, and
// orphans go to the nested init. Replay recomputes these, so an older
// recording of such a guest could diverge.
// 0x13f -> 0x140: a record/replay pselect6 that may wait reads its temporary
// mask at the call's entry, where Linux copies it, and recording waits on a
// private copy; replay read it after the grant
// (https://github.com/rrnewton/hermit/issues/4095). An older recording whose
// sibling rewrote the mask between the two can replay differently here.
// 0x140 -> 0x141: sched_getaffinity accepts any length that is a nonzero
// multiple of sizeof(long), as Linux does for one CPU, and copies and returns
// min(len, 16), where it answered EINVAL below 16 bytes
// (https://github.com/rrnewton/hermit/issues/4160). Replay answers the call
// itself, so an older recording that saw EINVAL would see success here.
// 0x141 -> 0x142: a signal that another task sends ends a restartable call
// that Detcore polls internally (a blocking pipe read or write, readv, recv,
// send, accept4, connect, and the legacy wait4 polling) when the kernel would
// deliver it, where the poll ignored it and retried
// (https://github.com/rrnewton/hermit/issues/4187). The recorder logs each
// polled EAGAIN but not the scheduler's signaled answer, so recordings made
// before and after this change do not replay on each other's replayer.
// 0x142 -> 0x143: epoll_ctl is recorded, and replay answers it from the
// recording (https://github.com/rrnewton/hermit/issues/31). An older recording
// has no event for it.
// 0x143 -> 0x144 (https://github.com/rrnewton/hermit/pull/4131):
// a child-exit SIGCHLD the scheduler sends ends an emulated pause or nanosleep
// only when the thread's mask leaves it unblocked and it is caught or
// default-fatal, and a still-queued sleep is woken for it
// (https://github.com/rrnewton/hermit/issues/4126). Replay recomputes such a
// sleep's result, so an older recording of a blocked or default SIGCHLD during
// a nanosleep holds the EINTR it used to return, and this replayer would answer
// 0: a successful replay with different output (Codex review of
// https://github.com/rrnewton/hermit/pull/4131).
// 0x144 -> 0x145: an fcntl that writes its result through its argument
// (F_GETOWN_EX, which glibc uses for F_GETOWN; F_GETLK, F_GETLK64,
// F_OFD_GETLK; the write-hint queries) records the bytes it wrote, and replay
// writes them back (https://github.com/rrnewton/hermit/issues/2289). An older
// recording has no such event.
// 0x145 -> 0x146: a successful exec takes back the POSIX timer signals
// Detcore sent that are still queued, as Linux discards them
// (flush_itimer_signals; https://github.com/rrnewton/hermit/issues/4184),
// where they survived the exec and reached the new image. Replay recomputes
// this, so an older recording replayed here could see a delivery its recorder
// saw and this replayer drops.
// 0x146 -> 0x147: while a vfork barrier stands, the scheduler selects other
// runnable threads when no barrier child is queued to run a turn and none is in
// the background (each is blocked on another thread or polling a call that is
// not ready) (https://github.com/rrnewton/hermit/issues/4194). A polling child
// used to run alone; replay recomputes the schedule, so an older recording of a
// vfork child that polled for a host event, with other threads runnable, could
// replay differently. A backgrounded child is still waited for, as before.
// 0x147 -> 0x148: get_mempolicy answers as a one-node Linux machine
// (https://github.com/rrnewton/hermit/issues/4214): MPOL_F_MEMS_ALLOWED fills
// the node mask with node 0 and the default policy's mask is written empty,
// where Detcore left the guest's buffer unwritten, and flag combinations Linux
// refuses are EINVAL, where Detcore answered 0. Replay answers the call
// itself, so an older recording of such a call would replay with different
// answers.
// 0x148 -> 0x149: when the guest's CPUID comes from Detcore's table,
// /proc/cpuinfo describes that virtual CPU, generated from the table, where it
// showed the host's model, flags and caches
// (https://github.com/rrnewton/hermit/issues/4161). Record virtualizes CPUID,
// and Detcore renders procfs over the recorded bytes during recording and
// again during replay, so an older recording would replay with different bytes.
// 0x149 -> 0x14a: the procfs files that describe the kernel and its fixed
// limits (/proc/version, /proc/cmdline and 12 kernel and vm sysctls)
// are procfs files whose contents Detcore fixes from its machine description
// (https://github.com/rrnewton/hermit/issues/4266), where they were ordinary
// files whose host bytes were recorded as read. Replay serves a classified
// procfs file itself, so an older recording of a read of one would replay
// with different events.
// 0x14a -> 0x14b: a guest's thread-directed SIGCHLD (tgkill, pthread_kill,
// pthread_sigqueue) that the sender's turn recorded ends a precise futex wait, before or after the wait
// parks, when the waiter's mask leaves it unblocked and it is caught
// (https://github.com/rrnewton/hermit/issues/4132). The same copy has ended an
// emulated pause or nanosleep since
// https://github.com/rrnewton/hermit/pull/4177, which changed those results
// with no bump of its own; this one covers both. Replay recomputes both
// results, so an older recording of such a send would replay with the wait or
// the sleep ended early: a successful replay with different output.
// 0x14b -> 0x14c: under record, a vfork child whose call runs in the
// background no longer stops every other thread; it is readmitted as soon as
// the call finishes and the turn is written to the rejoin log, which replay
// follows (https://github.com/rrnewton/hermit/issues/4211). An older recording
// has no rejoin entry for such a readmission, so this replay could not follow
// it.
// 0x14c -> 0x14d: the kernel's tables (/proc/interrupts, softirqs, zoneinfo,
// meminfo, modules and 16 others) are fixed files that Detcore renders from
// its machine description with no kernel read
// (https://github.com/rrnewton/hermit/issues/4266), where Detcore sanitized
// the host's file and kept its shape: its interrupt names, zones and one
// column per host CPU. And every open whose spelling Detcore does not
// classify (nearly every open outside /proc and /sys) now records a readlink
// of the descriptor, which replay answers, so both runs classify it alike. An
// older recording would replay with different bytes and events.
// 0x14d -> 0x14e: a child-exit SIGCHLD the scheduler sends a parked precise
// futex waiter is placed at that commit point, so the waiter's woken turn is
// granted ahead of a runnable sibling rather than deferred behind it, and the
// waiter, not the sibling, takes the signal and returns EINTR
// (https://github.com/rrnewton/hermit/issues/4304). Replay recomputes the
// schedule and futex results, so an older recording of such an exit would
// replay with the handler in another thread and a different futex result.
// 0x14e -> 0x14f: a timer's process-directed signal (alarm, an itimer, a
// POSIX timer) is sent to a thread that does not block it, not to the thread
// that armed it or the one chaos drew when that thread blocks it, and that
// thread is the one woken (https://github.com/rrnewton/hermit/issues/4320).
// Replay recomputes the send, so an older recording of a timer whose arming
// thread blocked its signal would replay with another thread woken. And a
// background rt_sigsuspend that the host reports as ERESTARTSYS returns
// ERESTARTNOHAND, the only result Linux's rt_sigsuspend has, so an older
// recording of one would replay with a different result and restart.
// 0x14f -> 0x150: a robust futex word that one exit's walk finds owned by its
// dying owner reads as marked FUTEX_OWNER_DIED for the rest of that exit, as
// Linux's own store makes it (https://github.com/rrnewton/hermit/issues/4238).
// A word reached twice, through an alias or an entry that is its own
// successor, was woken again, waking a second waiter Linux leaves to time out.
// Replay recomputes the wakes, so an older recording of such an exit would
// replay with one wake fewer: a successful replay with different output.
// 0x150 -> 0x151 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a pause or
// nanosleep that a child-exit SIGCHLD from the scheduler wakes is resumed
// first, as a woken futex waiter is, so the sleeper, not a running sibling,
// takes the signal and returns EINTR
// (https://github.com/rrnewton/hermit/issues/4333). Replay recomputes the
// schedule, so an older recording of such an exit would replay with the
// handler in another thread and the sleep still asleep.
// 0x151 -> 0x152 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a call that looks
// up a /proc name outside the --proc level (kallsyms, sysvipc,
// sys/kernel/core_pattern, ...) is issued with that name replaced by one
// procfs cannot hold, so its recorded answer is Linux's for a missing name
// (https://github.com/rrnewton/hermit/issues/4266). An older recording holds
// the kernel's answer for the name itself.
// 0x152 -> 0x153 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): `minimal` is the
// default /proc level, and a recording stores its level (`record start
// --proc`), which its replay runs at
// (https://github.com/rrnewton/hermit/issues/4266). An older recording ran at
// `standard` without saying so, and would replay at `minimal`, where the
// standard level's names are missing.
// 0x153 -> 0x154 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a call outside the
// schedule that begins while a signal the scheduler sent, and every thread
// blocked, waits in the shared queue, and that the call's mask admits and the
// process catches, is armed for the release barrier at its grant and rejoins
// at a fixed point (https://github.com/rrnewton/hermit/issues/4234). Under
// record it was readmitted through the rejoin log instead, so an older
// recording would replay with that call rejoining at another turn.
// 0x154 -> 0x155 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a recording can carry the x86-64-v3 CPU profile and the
// host xstate facts its guest's CPUID leaf 0xD and XCOMP answers came from
// (https://github.com/rrnewton/hermit/issues/4243). An older replayer would
// ignore them and replay a v3 recording against the v2 table, so its guest
// would see a different CPU.
// 0x155 -> 0x156: a call that looks up a name in a process's or a thread's
// directory outside the --proc level (limits, wchan, mem, attr, ...) is
// issued with that name replaced by one procfs cannot hold, as for the
// system-wide names at 0x152 (https://github.com/rrnewton/hermit/issues/4266).
// An older recording holds the kernel's answer for the name itself.
// 0x156 -> 0x157 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a standard
// signal Linux discards when it is sent (ignored, default-ignored with no
// handler, or uncaught to a PID namespace's init) no longer ends a futex wait,
// a sleep, a waitid, or an rt_sigtimedwait or signalfd take once a handler is
// installed, though ptrace still queues its copy; the scheduler records each
// send's fate in its turn (https://github.com/rrnewton/hermit/issues/4293).
// Replay recomputes these results, so an older recording of such a send would
// replay with the wait ended early: a successful replay with different output.
// 0x157 -> 0x158 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): Detcore answers
// readlink of the /proc links it knows the targets of (self, thread-self,
// mounts, net, the caller's namespace links) without a system call, and reads
// the caller's descriptor link spelled through its thread as
// /proc/thread-self/fd/N (https://github.com/rrnewton/hermit/issues/4266). An
// older recording holds a kernel readlink of the guest's own spelling for each
// of those.
// 0x158 -> 0x159 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): /proc/<pid>/cgroup
// is a procfs file whose every hierarchy reads as for a process in the root
// cgroup (https://github.com/rrnewton/hermit/issues/4378), where it was an
// unclassified open that recorded a readlink of its descriptor and the host's
// bytes. Replay serves a classified procfs file itself, so an older recording
// of a read of one would replay with different events.
// 0x159 -> 0x15a (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1; reserved as
// 0x15a): /proc/loadavg is a fixed procfs file, whose bytes come from
// Detcore's table with no kernel read, also under record/replay
// (https://github.com/rrnewton/hermit/issues/4213). An older recording holds
// the captured kernel read, which a new replay never takes, so it would go
// out of step.
// 0x15a -> 0x15b (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): a pause or
// nanosleep that a child-exit SIGCHLD wakes, in a process whose send window is
// unusable, claims the signal in record and replay as it does in a plain run,
// unless another thread outside the schedule could take it
// (https://github.com/rrnewton/hermit/issues/4343). An older recording of such
// an exit holds the sleep returning 0 or never waking, and would replay
// against a schedule in which it returns EINTR.
// 0x15b -> 0x15c (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): under record and
// replay a background read whose descriptor number a later close,
// close_range, dup2 or dup3 replaces before the read enters the kernel, or
// which was not open at its grant, keeps a woken sleep from claiming its
// SIGCHLD (https://github.com/rrnewton/hermit/issues/4410). An older
// recording of such a sleep holds the claim, which a new replay would not make.
// 0x15c -> 0x15d (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1; reserved as
// 0x15d): /proc/uptime is a procfs file Detcore renders
// (https://github.com/rrnewton/hermit/issues/4213), and a /proc/uptime the
// kernel opened (under record and replay, after chroot) now takes Detcore's
// rendered-file path for readv, preadv, preadv2 and lseek. A vectored read of
// it records a capture where an older binary refused it with ENOSYS, so an
// older recording containing one would replay out of step.
// 0x15d -> 0x15e (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1; reserved as
// 0x15e): clone3 with set_tid asking for an id a task of the run had, other
// than the pid of the caller's own zombie child, is refused
// (https://github.com/rrnewton/hermit/issues/4370). An older recording in
// which such a clone3 succeeded would replay to the refusal.
// 0x15e -> 0x15f (taken at land time; https://github.com/rrnewton/hermit/issues/4273,
// R2b-1): pipe read/write/writev woken by a kept signal decided in the sender's
// turn. REVERTED by 0x160.
// 0x15f -> 0x160 (taken at land time, HIGHEST_SHIPPED + 1): the 0x15f change is
// reverted, because its writev turn rule ended an atomic writev to a full pipe
// with a short count with no signal involved (writev_determinism's atomic
// full-pipe case: 4096 of 4096+ written, Linux blocks and completes it). Pipe
// calls woken by signals decide as they did at 0x15e; a 0x15f recording of
// such a wake would replay to another result, so the version moves forward.
// 0x160 -> 0x161 (taken at land time, HIGHEST_SHIPPED + 1; reserved as
// 0x160): a pidfd from clone(CLONE_PIDFD) or clone3's pidfd field is tracked
// as pidfd_open's is (https://github.com/rrnewton/hermit/issues/4430), so
// pidfd_send_signal through it is forwarded and recorded where Detcore
// answered EBADF without recording anything. An older recording holding such
// a send would replay out of step.
// 0x161 -> 0x162 (taken at land time, HIGHEST_SHIPPED + 1; reserved as
// 0x163): Detcore sends a parent-death signal (PR_SET_PDEATHSIG with a
// standard signal other than SIGKILL, SIGSTOP and SIGALRM) itself when the
// parent thread leaves, and arms the kernel with 0
// (https://github.com/rrnewton/hermit/issues/4440). An older recording of
// such a prctl or of the kernel's signal would replay out of step.
// 0x162 -> 0x163 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1; reserved as
// 0x15f): /proc/stat is a procfs file Detcore renders
// (https://github.com/rrnewton/hermit/issues/4213), and a /proc/stat the
// kernel opened (under record and replay, after chroot) now takes Detcore's
// rendered-file path for readv, preadv, preadv2 and lseek, as /proc/uptime
// did at 0x15d. A vectored read of it records a capture where an older binary
// refused it with ENOSYS, so an older recording containing one would replay
// out of step; and every read of it shows Detcore's layout, not the host's
// lines, so an older recording's guest saw other bytes.
// 0x163 -> 0x164 (taken at land time, HIGHEST_SHIPPED + 1): R2b-1 relanded
// (https://github.com/rrnewton/hermit/issues/4273): a pipe read, a pipe write
// or a pipe writev woken by a kept signal ends as Linux's does, and a pipe
// writev makes its first attempt in the slice of its own entry, so the kernel
// imports the iovec array the call began with. (The 0x15f writev requested a
// turn first and then read the array again, so another thread's rewrite in
// that turn changed the bytes an atomic writev to a full pipe wrote:
// writev_determinism's atomic full-pipe case.) Replay recomputes these
// results, so an older recording of such a wake would replay to another
// result.
// 0x164 -> 0x165 (taken at land time, as the reservation file's land-time
// numbering rule gives a ready change HIGHEST_SHIPPED + 1): sendfile records
// the bytes it copied and where its input stopped, and replay applies them
// instead of copying again (https://github.com/rrnewton/hermit/issues/4416).
// An older recording holds sendfile's count alone, which a new replay cannot
// read as its event.
// 0x165 -> 0x166 (taken at land time, HIGHEST_SHIPPED + 1): a joiner woken by
// a thread's CLONE_CHILD_CLEARTID is admitted at the next run-queue drain, in
// DetTid order with the other admissions, where it was pushed at once
// (https://github.com/rrnewton/hermit/issues/4362, part 2 step 0b). That moves
// it behind a lower-DetTid admission already pending, which turn selection,
// unrecorded, recomputes: an older recording could replay another order
// (Codex review of https://github.com/rrnewton/hermit/pull/4453, P2).
// 0x166 -> 0x167 (HIGHEST_SHIPPED + 1): a descriptor opened through one of
// the guest's own descriptor links (/proc/self/fd/N, /dev/fd/N and their
// thread and numeric spellings) records the path of descriptor N, not the
// link's spelling, so a name relative to it is looked up below that file as
// Linux looks it up (https://github.com/rrnewton/hermit/issues/4213,
// https://github.com/rrnewton/hermit/issues/4485). Under record and replay
// such a name is now classified, and checked against the --proc level, as
// the procfs file it is (a name outside the level is answered as absent
// before any open is recorded, where the alias spelling let the open reach
// the kernel), so an older recording with such an open would replay out of
// step.
// 0x167 -> 0x168 (taken at land time, HIGHEST_SHIPPED + 1): under record and
// replay, a POSIX timer's expiry is sent as an SI_TIMER record carrying the
// timer's id and si_value (rt_sigqueueinfo, rt_tgsigqueueinfo), not a bare
// kill or tgkill, so exec's own flush_itimer_signals removes it, and the
// scheduler no longer takes timer copies back at exec
// (https://github.com/rrnewton/hermit/issues/4262). An older recording's
// guest saw si_code 0 and no si_value for such a signal, and a replay would
// show it SI_TIMER.
// 0x168 -> 0x169 (reserved as 0x169): /proc/cpuinfo is a procfs file Detcore
// renders where it answers the guest's CPUID instruction from its own table
// (CPUID virtualized and intercepted;
// https://github.com/rrnewton/hermit/issues/4213), and there a /proc/cpuinfo
// the kernel opened (under record and replay, after chroot) takes Detcore's
// rendered-file path for readv, preadv, preadv2 and lseek, as /proc/stat did
// at 0x163. A vectored read of it records a capture where an older binary
// refused it with ENOSYS, so an older recording containing one would replay
// out of step.
// 0x169 -> 0x16a (reserved as 0x16a): a relative name below a directory
// descriptor Detcore has no record of (one received over SCM_RIGHTS on
// ptrace or KVM) is the kernel's lookup where Detcore answered EBADF before
// the recorder (https://github.com/rrnewton/hermit/issues/4213, K1-0), so an
// openat, stat, access or readlinkat through such a descriptor is now
// recorded, and an older recording containing one would replay out of step.
// 0x16a -> 0x16b was K3a (the recorded container mount table), shipped in
// f8a824cf and reverted in ec169b91. 0x16b -> 0x16c (no format change from
// 0x16a): this tree has no mount table, so it takes a new label rather than
// reusing 0x16b. A K3a recording is then refused by the exact-match gate
// instead of being replayed without its table, and the shipped 0x16b label is
// not moved backward.
// 0x16c -> 0x16d: K3a again (https://github.com/rrnewton/hermit/issues/4213):
// a recording carries its container's mount table, and Detcore serves
// /proc/<pid>/mountinfo and /proc/<pid>/mounts (and /proc/mounts) from it in
// place of the kernel's bytes. A mounts file is now a procfs file Detcore
// serves from a snapshot, where it was read through to the kernel, and
// neither file captures the kernel's bytes, so an older recording that reads
// one would replay out of step.
// 0x16d -> 0x16e (number to be set at land): a restartable call on an
// internal pipe or socket (read, readv, recv*, accept4, the scalar pipe
// write) blocks every blockable signal before its attempt when a signal is
// pending (R2b-2 stage 2, https://github.com/rrnewton/hermit/issues/4273), so
// a pending signal no longer stops the recorded attempt: the attempt's result
// is recorded where an older recording has the stopped attempt's ERESTARTSYS,
// and an older recording containing one would replay out of step.
// 0x16e -> 0x16f: Detcore keeps only the guest's own rows of /proc/locks,
// matching OFD and flock rows against the locks the guest's own open file
// descriptions hold, which their fdinfo names. After every successful
// F_OFD_SETLK or F_OFD_SETLKW, every flock call, and every successful
// pidfd_getfd, Detcore reads the descriptor's /proc/self/fdinfo through the
// recorder (an openat, its reads and a close), and replay matches the
// recorded /proc/locks rows against those recorded bytes. An older recording
// has no such events there and would replay out of step.
// 0x16f -> 0x170 was R2b-2 stage 3 (a recorded private SIGCHLD ends gated
// waits and pipe calls), shipped in 4381d927 and reverted in a6fb8ad0.
// 0x170 -> 0x171 (no format change from 0x16f): this tree does not end those
// calls on a recorded private SIGCHLD, so it takes a new label rather than
// reusing 0x170. A stage 3 recording is then refused by the exact-match gate
// instead of being replayed to other results, and the shipped 0x170 label is
// not moved backward.
// 0x171 -> 0x172: replay recomputes which /proc/locks rows are the guest's
// from the recorded raw rows and fdinfo reads, and that decision changed
// after 0x16f's format shipped: the descriptor-table ledger (d8117115) stops
// a description counting when no guest table references it and hides what it
// cannot follow, an fdinfo that cannot be opened or read no longer stops the
// run at the lock call but at the next /proc/locks read while that
// description is still referenced, and the fdinfo is read through
// /proc/thread-self. A 0x16f or 0x171 recording would replay different guest
// bytes, or refuse where it recorded a run that went on, so it is refused
// here instead.
// 0x172 -> 0x173 (R2b-2 stage 3 re-landed; 0x170's format under a new
// label): a `SIGCHLD` a guest sent a thread, which the schedule recorded in
// its private queue, ends that thread's gated wait (poll, ppoll, epoll_wait,
// epoll_pwait, rt_sigtimedwait) and its restartable pipe read, readv or
// scalar write, as on Linux, where it was held until the call returned
// (https://github.com/rrnewton/hermit/issues/4273). Such a turn also blocks
// signals in place before its timerfd publication. The recorded decisions
// change, so an older recording containing one would replay out of step.
// 0x173 -> 0x176 (reserved as 0x176; 0x174 and 0x175 are reserved by other
// lanes, and whichever lands later takes HIGHEST_SHIPPED + 1): Detcore reads
// a description's fdinfo after a lock call only through an anchor proved to
// be procfs. The recorded sequence is now an openat of the guest's /proc
// (O_PATH), its fstatfs and fstat, an openat2 of thread-self/fdinfo/<fd>
// beneath it (RESOLVE_BENEATH | RESOLVE_NO_XDEV), that entry's fstat, the
// reads and two closes, where an older recording holds one openat of
// /proc/thread-self/fdinfo/<fd>, so it would replay out of step.
// 0x176 -> 0x177 (reserved as 0x175; taken at land time as HIGHEST_SHIPPED + 1
// after 0x176 shipped first): record/replay retains ptrace's actual
// timer-delivery capability, so its stored Config and preemption.timer_enabled
// summary report the supported mechanism. The shared timer gate preserves
// capable ptrace arming and logical expiry, while timerless configurations no
// longer issue physical arms/rearms or manufacture skid state. This
// observable-behavior correction takes a new label even though the event
// layout is unchanged.
// 0x177 -> 0x178 (reserved as 0x178; 0x177 is the dbt lane's, landed as
// 9231425b): the procfs anchor's fix-forward also runs fstatfs on the opened
// fdinfo entry (it must be procfs too, so a sibling's dup2 over the anchor's
// descriptor number cannot pass) and closes the entry on every path, so the
// recorded sequence after each lock call is now the anchor's openat, fstatfs
// and fstat, the openat2, the entry's fstatfs and fstat, the reads and two
// closes. A 0x176 or 0x177 recording has no entry fstatfs and would replay
// out of step.
// 0x178 -> 0x179 (reserved as queue hint 0x17b in minibeads dh-gxmvo; takes
// HIGHEST_SHIPPED + 1 at land): a vfork parent resumes at its child's
// successful exec or exit, as Linux resumes it (`vfork(2)`), instead of once
// nothing else can run, and record logs that readmission in the rejoin log at
// step 2a. Every recording of a vfork now holds a different schedule, so an
// older one would replay out of step at its first vfork.
// 0x179 -> 0x17a (taken at land time as HIGHEST_SHIPPED + 1; reserved as
// 0x17a): fresh-image DBT Tool start and postexec now run before the first
// application instruction with its actual entry PC and live SP. Earlier images
// admitted those hooks at their first syscall, after guest instructions had
// executed. The lifecycle timing/context is observable even with an unchanged
// wire layout, so recordings require a new behavior version.
// 0x17a -> 0x17b: the vfork rejoin (0x179, 7ff8743b) is reverted, because it
// broke record/replay of make -jN (minibeads dh-ojrf): a vfork parent is again
// held until nothing else can run, and the rejoin log entries are gone. A
// recording made with the rejoin is refused by the exact-match gate.
// 0x17b -> 0x17c (takes HIGHEST_SHIPPED + 1 at land): a thread readmitted from
// a blocking pool whose call a signal ended keeps that call's temporary mask
// until its continuation turn (`continuation_masks`), instead of the kernel's
// SigBlk, which replay does not share with record (minibeads dh-ojrf). Replay
// now chooses the SIGCHLD taker, a timer signal's fate and a discard as record
// did, so an older recording could replay a different order there.
// 0x17c -> 0x17d (takes HIGHEST_SHIPPED + 1 at land): the vfork rejoin
// re-lands on the continuation-mask fix: a vfork parent again resumes at its
// child's successful exec or exit (`vfork(2)`), with the readmission logged at
// step 2a, so every recording of a vfork holds a different schedule.
// 0x17d -> 0x17e (taken at land time, HIGHEST_SHIPPED + 1): a signal that stops
// a call's first injection (its mask or action probe) before the probe runs now
// makes the call restart after delivery (-ERESTARTNOINTR), as if the signal came
// first (https://github.com/rrnewton/hermit/issues/4526), where the call failed
// with the probe's -ERESTARTSYS. A recording made before this holds that EINTR
// and would replay out of step.
// 0x17e -> 0x17f (taken at land time, HIGHEST_SHIPPED + 1): the remaining
// first-probe sites restart too (masked epoll_pwait2, zero-timeout masked ppoll,
// record/replay pselect6's entry read), and masked ppoll, epoll_pwait2 and
// rt_sigtimedwait read the virtual clock after their first probe instead of
// before it (https://github.com/rrnewton/hermit/issues/4526). A recording made
// before this can hold the not-run probe's EINTR at those entries.
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x17f);

/// The highest RECORD_VERSION this project has ever shipped.
///
/// RECORD_VERSION must never move BACKWARD onto it. This is a COMPILE-TIME
/// assertion rather than a test on purpose: it is decidable from two constants,
/// so it should break the build rather than a test run, and it cannot be
/// skipped, filtered or left unrun.
///
/// WHY IT EXISTS AT ALL. The rejection set in `record_version_requires_an_exact_match`
/// used to be a list of specific versions, and `!compatible_with(0x10a)` was
/// catching a backward move BY ACCIDENT. Replacing that list with a window derived
/// from RECORD_VERSION fixes its silent narrowing but cannot catch a regression,
/// because a derived window re-derives from whatever the constant currently says.
/// Dropping the list without this floor would have traded a stale check for a
/// weaker one.
///
/// The failure it guards is near, not hypothetical: a long-stale branch that bumped
/// 0x109 -> 0x10a while main advanced to 0x10e regresses the constant the moment its
/// conflict is resolved by taking the branch side. The build would then stamp the
/// current schema with a label an older reader already claims, and the exact-match
/// gate would accept a stream whose shape it does not know -- the desynchronization
/// the version exists to prevent.
///
/// RAISE THIS IN THE SAME COMMIT THAT RAISES RECORD_VERSION.
const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x17f;

const _: () = assert!(
    RECORD_VERSION.0 >= HIGHEST_SHIPPED_RECORD_VERSION,
    "RECORD_VERSION is BELOW HIGHEST_SHIPPED_RECORD_VERSION. A recording made by this \
     build would carry a label an older reader already claims. If this fired during a \
     rebase, the version constant was resolved BACKWARD -- merge it forward."
);

/// Metadata associated with the recording. This is serialized as a JSON file.
#[derive(Debug, Serialize, Deserialize)]
pub struct Metadata {
    /// The real path to the program.
    pub exe: PathBuf,
    /// The name of the program.
    pub program: String,
    /// The first argument passed to the program.
    pub arg0: String,
    /// Program arguments (not including arg0).
    pub args: Vec<String>,
    /// The working directory of the program.
    pub current_dir: PathBuf,
    /// The hostname in the UTS namespace used by the program.
    pub hostname: Option<String>,
    /// The domainname in the UTS namespace used by the program.
    pub domainname: Option<String>,
    /// The environment variables used by the program.
    pub envs: BTreeMap<String, String>,
    /// Hermit record/replay version.
    pub version: RecordVersion,
    /// Why this recording cannot be replayed faithfully, when the recording
    /// run said so (`detcore::detlog::replay_refusal`): replay refuses before
    /// starting the guest (<https://github.com/rrnewton/hermit/issues/3994>).
    #[serde(default)]
    pub replay_refused: Option<String>,
    /// Actual guest proc mount mode after setup. Missing in older recordings or
    /// when the guest's proc mount could not be inspected; diagnostic only.
    #[serde(default)]
    pub proc_readonly: Option<bool>,
    /// Recording-namespace mount roots proven to be Hermit-owned.
    ///
    /// Replay consumes the recorder's raw syscall buffers, so it must apply the
    /// recording-time raw mount IDs rather than IDs from the fresh replay
    /// namespace.  Older recordings default to no private-root rewrites.
    #[serde(default)]
    pub mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,
    /// Recording-namespace raw mount IDs in canonical row order.
    #[serde(default)]
    pub mountinfo_mount_ids: Vec<u64>,
    /// Whether `mountinfo_mount_ids` is a producer-owned snapshot, including a
    /// valid empty snapshot.
    #[serde(default)]
    pub mountinfo_mount_ids_captured: bool,
    /// Recording-time raw mount IDs in the order Detcore numbered them, which
    /// is the order the guest first observed them through mountinfo or fdinfo.
    /// Replay numbers each raw ID by its position in this list.
    #[serde(default)]
    pub mount_id_assignment_order: Vec<u64>,
    /// The recording container's mount table, which Detcore renders as the
    /// guest's `/proc/<pid>/mountinfo` and `/proc/<pid>/mounts` in place of the
    /// kernel's bytes, at recording and again at replay
    /// (<https://github.com/rrnewton/hermit/issues/4213>). `None` in a
    /// recording made before 0x16d, whose replay keeps the kernel's bytes.
    #[serde(default)]
    pub container_mount_table: Option<detcore_model::container_mounts::ContainerMountTable>,
    /// Whether the recording ran in an isolated network namespace with only
    /// loopback (`record start --network local`). `None` means the recorder
    /// did not say, which is every recording made before this field existed;
    /// those recordings ran on the host network. An autopilot replay uses the
    /// same network, so syscalls the replayer re-executes see the same
    /// interfaces and ports they were recorded against.
    #[serde(default)]
    pub local_networking: Option<bool>,
    /// The `/proc` level the recording ran at (`record start --proc`, or the
    /// default where the caller names none). Record and replay both run at
    /// it, so a replay sees the names its recording saw. Every recorder
    /// writes it; `None` is only a hand-written metadata.json, which takes
    /// the default.
    #[serde(default)]
    pub proc_level: Option<detcore_model::config::ProcLevel>,
    /// The CPU the guest saw (<https://github.com/rrnewton/hermit/issues/4243>).
    /// Absent means x86-64-v2.
    #[serde(default)]
    pub cpu_profile: detcore_model::cpu_profile::CpuProfile,
    /// Under x86-64-v3, the host facts the guest's leaf 0xD and XCOMP answers
    /// came from. Replay refuses a host where any of them differs.
    #[serde(default)]
    pub cpu_profile_host: Option<detcore_model::cpu_profile::HostXstate>,
    /// Under x86-64-v3, the hex SHA-256 of the full CPUID table and the bound
    /// host facts (`detcore::virtual_cpuid_digest`). Replay recomputes it.
    #[serde(default)]
    pub cpu_table_digest: Option<String>,
    /// Under x86-64-v3, what the host qualification leaves uncovered
    /// (`detcore_model::cpu_profile::V3_RESIDUAL`), so that a reader of the
    /// recording sees it.
    #[serde(default)]
    pub cpu_profile_residual: Option<String>,
}

impl Metadata {
    /// Creates a new metadata object, populating it with information about a
    /// command.
    pub fn new(command: &Command) -> Result<Self, Error> {
        let exe = command.find_program()?;

        let program = command.get_program().to_string_lossy().into_owned();
        let arg0 = command.get_arg0().to_string_lossy().into_owned();

        let args = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        let envs = command
            .get_captured_envs()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();

        let current_dir = command
            .get_current_dir()
            .map_or_else(|| env::current_dir().unwrap(), ToOwned::to_owned);

        let hostname = command
            .get_hostname()
            .map(|s| s.to_string_lossy().into_owned());
        let domainname = command
            .get_domainname()
            .map(|s| s.to_string_lossy().into_owned());

        Ok(Self {
            exe,
            program,
            arg0,
            args,
            current_dir,
            hostname,
            domainname,
            envs,
            replay_refused: None,
            version: RECORD_VERSION,
            proc_readonly: None,
            mountinfo_root_rewrites: Vec::new(),
            mountinfo_mount_ids: Vec::new(),
            mountinfo_mount_ids_captured: false,
            mount_id_assignment_order: Vec::new(),
            container_mount_table: None,
            local_networking: None,
            proc_level: None,
            cpu_profile: Default::default(),
            cpu_profile_host: None,
            cpu_table_digest: None,
            cpu_profile_residual: None,
        })
    }

    /// Reads the metadata of the recording stored in `dir`.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let metadata_path = dir.join(METADATA_NAME);
        // Buffered: serde_json reads an unbuffered file one byte per read(2).
        serde_json::from_reader(std::io::BufReader::new(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        ))
        .with_context(|| format!("Failed to parse {:?}", metadata_path))
    }

    /// Constructs a command from the metadata.
    pub fn command(&self) -> Command {
        // NOTE: We bypass the normal $PATH search here by passing in the
        // absolute path to the program directly.
        let mut command = Command::new(&self.exe);
        command.arg0(&self.arg0);
        command.args(&self.args);
        command.env_clear();
        command.envs(&self.envs);
        command.current_dir(&self.current_dir);

        if let Some(hostname) = &self.hostname {
            command.hostname(hostname);
        }

        if let Some(domainname) = &self.domainname {
            command.domainname(domainname);
        }

        command
    }
}

pub fn record_or_replay_config(data: &Path) -> detcore::Config {
    record_or_replay_config_with_timer_support(data, record_timer_support())
}

fn record_or_replay_config_with_timer_support(
    data: &Path,
    timer_support: Result<(), String>,
) -> detcore::Config {
    // NOTE: Record and replay should use the exact same Detcore configuration.
    // Callers add the completed producer namespace's mountinfo order and
    // mount-ID assignment order after this common base is built, so replay
    // uses recording-time raw IDs rather than IDs from its fresh container.
    //
    // WHY THIS IS NOT `hermit run --strict`, WRITTEN HERE ON PURPOSE.
    //
    // `virtualize_time: false` below is DELIBERATE, and the rationale used to live only
    // in the separate dev-hermit workspace -- not in this repository, beside the code it
    // governs. The cost of that was measured: three agents in one night read this line,
    // searched this repo's source, git log, docs/ and issues, found nothing, and could
    // not tell a design decision from a determinism bug; two coordinators then spent
    // hours treating it as a candidate defect. A decision recorded in a different
    // repository from the code it governs is, in practice, undocumented. Hence this
    // comment. See rrnewton/hermit#2295.
    //
    // WHAT RECORD/REPLAY ACTUALLY GUARANTEES. Replay re-executes a recording against the
    // recorded syscall data, so what must be reproducible is THIS recording's replay --
    // not agreement between two independent recordings. Time is therefore left real: the
    // recording captures what the guest actually observed, and replay returns those
    // recorded values. `hermit record start --verify` records once, replays that
    // recording, and compares the two; its success message says exactly that ("replay
    // matched recording") and does not claim more.
    //
    // WHAT IT DOES NOT GUARANTEE, which is the part that misleads readers. Because time
    // is not virtualized, two INDEPENDENT recordings of the same program observe
    // different clock values. Demonstrated: `hermit run -- date` twice returns the
    // identical virtual epoch, while `hermit record start -- date` twice returns real
    // wall-clock times seconds apart. Replay fidelity says nothing about that, and
    // the focused `independent_mountinfo_recordings_are_canonical` test compares
    // two independent mountinfo recordings after replacing exactly field 3, the
    // major:minor device column. With `virtualize_metadata: false`, Linux may
    // legitimately allocate a different anonymous-block-device minor for the
    // private procfs mount in each container. That test still compares mount ID,
    // parent ID, root, mountpoint, options, optional fields, separator,
    // filesystem type, source, and superblock options byte-for-byte. It does not
    // establish general cross-recording determinism, and a green `--verify`
    // remains only a replay-fidelity result.
    //
    // This configuration differs from `hermit run --strict` on four of the five
    // properties that define it (see run.rs: only `sequentialize_threads` matches).
    // Any claim that recording is "strict" in that sense is wrong; the `--strict` flag
    // on `hermit record` is accepted for command-line compatibility, and its only effect
    // is to refuse a host whose retired-branch counter is inexact
    // (https://github.com/rrnewton/hermit/issues/3810).
    let default_config: detcore::Config = Default::default();
    let mut config = detcore::Config {
        // Record and replay are determinism claims, so an unsupported syscall
        // must invalidate the operation instead of being recorded from or
        // replayed against the live host.
        panic_on_unsupported_syscalls: true,
        // Return a typed error through Reverie rather than unwinding across the
        // backend callback. The tracer owns process-tree cleanup on that error.
        exit_on_unsupported_syscall: true,
        shutdown_on_unsupported_syscall: false,
        unsupported_syscall_report_fd: None,
        panic_on_rcb_overshoot: false,
        sequentialize_threads: true,
        runs_post_fork: default_config.runs_post_fork,
        // Record/replay keeps a partial Detcore subscription. Complete coverage
        // of the Determinized classification begins in v0x10a; madvise policy
        // semantics begin in v0x102.
        passthru_opt: true,
        deterministic_io: false,
        virtualize_time: crate::RECORD_REPLAY_VIRTUALIZES_TIME,
        virtualize_metadata: false,
        mountinfo_root_rewrites: Vec::new(),
        mountinfo_device_rewrites: Vec::new(),
        mountinfo_mount_ids: Vec::new(),
        mountinfo_mount_ids_captured: false,
        mountinfo_mount_namespace: None,
        mount_id_assignment_order: Vec::new(),
        // The recorder and the replayer set the table from the recording.
        container_mount_table: None,
        virtualize_cpuid: true,
        // The recorder and the replayer set the profile from the command line
        // and the recording; see `record_or_replay_config`'s callers.
        cpu_profile: default_config.cpu_profile,
        cpu_profile_host: None,
        // Record and replay run under the ptrace backend.
        backend: <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities(),
        // As `hermit run` sets it for ptrace (`prepare_backend_config` in
        // lib.rs). With it off, the scheduler did not model signal targets, so
        // a thread asleep in a backgrounded rt_sigsuspend that a child-exit
        // SIGCHLD woke was not held at the release barrier: the scheduler could
        // pop the next timer, or report a terminal deadlock, before the thread's
        // continuation posted (https://github.com/rrnewton/hermit/issues/3963).
        // A constant, unlike `hermit run`: replay rebuilds this configuration
        // and must decide which signals end a wait exactly as the recording did,
        // whatever terminal replay itself runs under. `false` keeps Linux's rule
        // for the terminal signals a guest sends itself, so a caught SIGINT
        // still ends a FUTEX_WAIT or rt_sigtimedwait in every recording; a
        // terminal's own signal during an interactive recording comes from
        // outside the container, and replay cannot reproduce where it lands.
        guest_may_inherit_a_terminal: false,
        in_guest_detlog_forward_policy: None,
        in_guest_site_patching_off: false,
        // The first image's link stays the kernel's in record and replay.
        launch_image: None,
        // Not measured for record and replay.
        inherited_seccomp: detcore::InheritedSeccomp::Unknown,
        has_uts_namespace: true,
        // The path to the directory where syscalls will be recorded.
        replay_data: Some(data.to_path_buf()),
        clock_multiplier: None,
        epoch: default_config.epoch,
        gdbserver: false,
        gdbserver_port: default_config.gdbserver_port,
        kill_daemons: default_config.kill_daemons,
        max_timeslice: default_config.max_timeslice,
        target_timeslice: default_config.target_timeslice,
        target_timeslice_syscalls_only: default_config.target_timeslice_syscalls_only,
        futex_wake_yields: default_config.futex_wake_yields,
        // Never in record mode, whatever the default: recordings made before
        // the option existed, and record mode's own scheduling, have no
        // hand-off (https://github.com/rrnewton/hermit/issues/4380).
        pipe_wake_latency: None,
        scheduler_turn_cost: default_config.scheduler_turn_cost,
        seed: default_config.seed,
        rng_seed: default_config.rng_seed,
        imprecise_timers: false,
        // Off in record and replay, a constant like the rest: replay rebuilds
        // this configuration and must decide every signal question as the
        // recording did, and recordings made before the copy was on by
        // default (A5 step 2d) read the kernel's `/proc` view throughout.
        signal_state_copy: false,
        // The copy is off, so no delivery is checked.
        signal_delivery_check_fatal: false,
        chaos: false,
        sigint_instakill: false,
        warn_non_zero_binds: false,
        sched_heuristic: Default::default(),
        sched_seed: default_config.sched_seed,
        network_trace: Default::default(),
        recordreplay_modes: true,
        replaying: false,
        record_preemptions: false,
        record_preemptions_to: None,
        replay_preemptions_from: None,
        replay_schedule_from: None,
        replay_exhausted_panic: false,
        die_on_desync: true,
        stacktrace_event: Vec::new(),
        stacktrace_signal: None,
        preemption_stacktrace: false,
        preemption_stacktrace_log_file: None,
        stop_after_turn: None,
        stop_after_iter: None,
        debug_externalize_sockets: false,
        debug_futex_mode: BlockingMode::Precise,
        sched_sticky_random_param: 0.0,
        no_rcb_time: false,
        detlog_heap: false,
        detlog_stack: false,
        detlog_regs: false,
        detlog_io_buffers: crate::RECORD_REPLAY_HASHES_IO_BUFFERS,
        detlog_regs_cadence: 1,
        sysinfo_uptime_offset: 120,
        memory: default_config.memory,
        proc_level: default_config.proc_level,
        interrupt_at: vec![],
        happens_before: None,
        record_host_inputs: false,
        host_input_log: None,
        untraced_code_range: None,
        fatal_core_capture: None,
        fuzz_futexes: false,
        chaos_target_races: false,
        chaos_per_thread_slowdown: false,
        chaos_slowdown_max_factor: 10.0,
        chaos_epoch_length_ns: 0,
        fuzz_seed: None,
    }
    .with_backend(|backend| {
        backend.supports_blocked_wait_signal_interruption = true;
        backend.runs_guest_code_between_injections = false;
    });
    if let Err(reason) = timer_support {
        disable_timeslice_without_a_timer(&mut config, &reason);
    }
    config
}

/// Whether this host can arm Reverie's ptrace timer for a record or replay:
/// working perf counters AND a PMU profile for its CPU. Without a profile,
/// Reverie gives no task a timer, so a timeslice would be configured and never
/// delivered.
fn record_timer_support() -> Result<(), String> {
    timer_support_from(
        reverie_ptrace::is_perf_supported(),
        reverie_ptrace::host_pmu_profile(),
    )
}

/// [`record_timer_support`] from what the host answered: whether perf works,
/// and whether Reverie has a PMU profile for its CPU.
fn timer_support_from(
    perf_supported: bool,
    profile: Result<(), reverie_ptrace::PmuValidationError>,
) -> Result<(), String> {
    if !perf_supported {
        return Err("hardware perf counters are not supported on this machine".to_owned());
    }
    profile.map_err(|error| error.to_string())
}

/// Turn off `config`'s timeslice, which this host cannot deliver (`reason`).
fn disable_timeslice_without_a_timer(config: &mut detcore::Config, reason: &str) {
    if config.max_timeslice.is_some() {
        tracing::warn!("{reason}; Records/Replays run without a timeslice and may randomly fail!");
        config.max_timeslice = None;
    }
}

#[cfg(test)]
mod tests {
    use reverie::Tool;
    use reverie::syscalls::Sysno;

    use super::*;

    // A host whose counters work but whose CPU has no PMU profile (Reverie's
    // UnsupportedCpu) records without a timeslice, as a host without counters
    // does, rather than keeping one no task can be given.
    #[test]
    fn a_record_without_a_timer_has_no_timeslice() {
        let unknown = || reverie_ptrace::PmuValidationError::UnsupportedCpu {
            family: 0x06,
            model: 0x01,
        };
        assert_eq!(timer_support_from(true, Ok(())), Ok(()));
        let no_profile = timer_support_from(true, Err(unknown())).unwrap_err();
        assert!(no_profile.contains("model 0x1"), "{no_profile}");
        let no_perf = timer_support_from(false, Err(unknown())).unwrap_err();
        assert!(
            no_perf.contains("perf counters are not supported"),
            "{no_perf}"
        );
        let supported = record_or_replay_config_with_timer_support(
            Path::new("replay-data"),
            timer_support_from(true, Ok(())),
        );
        assert_eq!(
            supported.backend.delivers_preemption_timer_events,
            <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities()
                .delivers_preemption_timer_events,
            "record and replay use ptrace's timer mechanism"
        );
        assert_eq!(
            supported.max_timeslice,
            detcore::Config::default().max_timeslice
        );
        assert!(supported.use_preemption_timer());
        assert!(!supported.chaos);
        for reason in [no_perf, no_profile] {
            let unavailable =
                record_or_replay_config_with_timer_support(Path::new("replay-data"), Err(reason));
            assert_eq!(unavailable.max_timeslice, None);
            assert!(!unavailable.use_preemption_timer());
            assert_eq!(unavailable.backend, supported.backend);
        }
        let mut config = detcore::Config::default();
        assert!(config.max_timeslice.is_some());
        disable_timeslice_without_a_timer(
            &mut config,
            "Unsupported CPU family 0x6, model 0x1: Reverie has no performance-counter profile for it",
        );
        assert_eq!(config.max_timeslice, None);
        // Already off: stays off.
        disable_timeslice_without_a_timer(&mut config, "again");
        assert_eq!(config.max_timeslice, None);
    }

    #[test]
    fn pre_yield_readmission_recordings_are_refused_by_the_version_gate() {
        // A 0x132 recording never readmitted a background call while a
        // yielder was queued, and its replayer cannot follow one that did.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x132)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x131)));
    }

    #[test]
    fn pre_held_sleep_signal_recordings_are_refused_by_the_version_gate() {
        // A recording from before this epoch of a blocked or default SIGCHLD
        // during a nanosleep holds the EINTR the call used to return, where
        // this replayer recomputes 0: 0x143 (the previous epoch), 0x142,
        // 0x13e, 0x13d, 0x13c, 0x139, 0x138 and 0x135.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x143)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x142)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13e)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13d)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13c)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x139)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x138)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x135)));
    }

    #[test]
    fn pre_tight_stack_ppoll_recordings_are_refused_by_the_version_gate() {
        // A recording from before this epoch of a ppoll on a nearly full stack
        // holds its EFAULT and no event, which this replayer does not
        // reproduce: 0x133 (the previous epoch) and 0x132.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x133)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x132)));
    }

    #[test]
    fn pre_epoll_pwait2_recordings_are_refused_by_the_version_gate() {
        // A recording from before this epoch ran epoll_pwait2 live and holds
        // no event for it: 0x136 (the previous epoch), 0x135 and 0x134.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x136)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x135)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x134)));
    }

    #[test]
    fn pre_epoll_pwait_recordings_are_refused_by_the_version_gate() {
        // A recording from before this epoch ran epoll_pwait live and holds
        // no event for it, so this replayer would desynchronize at the first
        // one: 0x12f (the previous epoch), 0x12e, 0x12d, 0x12c and 0x12b.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12e)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12d)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12c)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12b)));
    }

    #[test]
    fn record_version_requires_an_exact_match() {
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));

        // DERIVED FROM THE COMPATIBILITY RULE, NOT ENUMERATED.
        //
        // This assertion used to be a list of specific versions -- 0x10a, 0x10c,
        // 0x105, 0x110. A list SILENTLY NARROWS every time RECORD_VERSION
        // advances: the named values drift away from the boundary that matters,
        // and the test keeps passing while checking less and less. It cannot
        // fail for the reason its name gives once the value has moved past the
        // range someone happened to write down.
        //
        // `compatible_with` is exact equality, so the property to assert is
        // "every OTHER version is refused". Re-deriving the cases from
        // RECORD_VERSION itself means the window travels with the constant and
        // cannot go stale.
        let current = RECORD_VERSION.0;
        for delta in 1..=16u32 {
            let older = RecordVersion(current - delta);
            assert!(
                !RECORD_VERSION.compatible_with(&older),
                "recorder at {current:#x} must refuse a recording made at {:#x}",
                older.0
            );
            let newer = RecordVersion(current + delta);
            assert!(
                !RECORD_VERSION.compatible_with(&newer),
                "recorder at {current:#x} must refuse a recording made at {:#x}",
                newer.0
            );
        }
    }

    #[test]
    fn record_and_replay_preserve_partial_subscriptions_and_fail_closed() {
        let config = record_or_replay_config(Path::new("replay-data"));
        assert!(config.passthru_opt);
        assert!(config.panic_on_unsupported_syscalls);
        assert!(config.exit_on_unsupported_syscall);
        assert!(!config.shutdown_on_unsupported_syscall);
    }

    /// Record mode never hands the turn to a readied pipe poller, whatever
    /// the default (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn record_and_replay_never_apply_the_pipe_wake_latency() {
        assert_eq!(
            record_or_replay_config(Path::new("replay-data")).pipe_wake_latency,
            None
        );
    }

    #[test]
    fn record_and_replay_use_run_default_memory() {
        let run_default = detcore::Config::default();
        assert_eq!(run_default.memory, 1_000_000_000);
        assert_eq!(
            record_or_replay_config(Path::new("replay-data")).memory,
            run_default.memory
        );
    }

    /// RECORDING DOES NOT VIRTUALIZE TIME, AND THE VERDICT NOW SAYS SO.
    ///
    /// `virtualize_time: false` here is deliberate (see the rationale block on
    /// `record_or_replay_config`), and it is what makes a green
    /// `record start --verify` mean something weaker than a green
    /// `run --verify`: the replay reproduced *that recording*, not that the guest
    /// is deterministic across invocations. Ported from the residual of
    /// hermit#2269.
    ///
    /// Pinned against the shared constant rather than a literal, because the
    /// value is now read in two places — the config the run uses, and the
    /// `ComparisonOptions` that discloses it in the report. If those drifted, the
    /// report would describe a time policy the run did not use, which is the very
    /// defect the disclosure exists to close.
    #[test]
    fn recording_does_not_virtualize_time_as_documented() {
        let config = record_or_replay_config(Path::new("replay-data"));
        assert!(
            !config.virtualize_time,
            "record/replay must not virtualize time; a green replay verdict would \
             otherwise be mistaken for a determinism result"
        );
        assert_eq!(
            config.virtualize_time,
            crate::RECORD_REPLAY_VIRTUALIZES_TIME,
            "the config the run uses and the constant the report discloses must be \
             the same decision, not two that happen to agree"
        );
    }

    #[test]
    fn record_and_replay_subscribe_every_determinized_syscall() {
        let config = record_or_replay_config(Path::new("replay-data"));
        let record = <detcore::Detcore<hermit_record::Recorder> as Tool>::subscriptions(&config);
        let replay = <detcore::Detcore<hermit_replay::Replayer> as Tool>::subscriptions(&config);

        for (phase, subscriptions) in [("record", record), ("replay", replay)] {
            let delivered = subscriptions.iter_syscalls().collect::<Vec<_>>();
            let missing = detcore::all_pinned_syscalls()
                .filter(|sysno| detcore::is_determinized_syscall(*sysno))
                .filter(|sysno| !delivered.contains(sysno))
                .collect::<Vec<_>>();
            assert!(
                missing.is_empty(),
                "{phase} lets Determinized syscalls bypass Detcore: {}",
                missing
                    .iter()
                    .map(|sysno| sysno.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            assert!(
                delivered.contains(&Sysno::syslog),
                "{phase} must deliver syslog to its deterministic Detcore handler"
            );
            // chdir is PassThrough for Detcore but its result depends on the
            // replay chroot, so record/replay subscribes it (hermit#3598);
            // umask, from the same PassThrough group, stays the sentinel that
            // record/replay does not subscribe everything.
            assert!(
                delivered.contains(&Sysno::chdir) && delivered.contains(&Sysno::getcwd),
                "{phase} must deliver chdir and getcwd so replay uses recorded results"
            );
            // A replayed descriptor may be a placeholder, so these must
            // return recorded results rather than run
            // (https://github.com/rrnewton/hermit/issues/3591).
            for sysno in [
                Sysno::fsync,
                Sysno::fdatasync,
                Sysno::syncfs,
                Sysno::fchmod,
                Sysno::fchown,
                Sysno::fsetxattr,
                Sysno::fremovexattr,
                Sysno::setxattr,
                Sysno::lsetxattr,
                Sysno::removexattr,
                Sysno::lremovexattr,
                Sysno::getxattr,
                Sysno::lgetxattr,
                Sysno::fgetxattr,
                Sysno::listxattr,
                Sysno::llistxattr,
                Sysno::flistxattr,
            ] {
                assert!(
                    delivered.contains(&sysno),
                    "{phase} must deliver {sysno} so replay uses its recorded result"
                );
            }
            assert!(
                !delivered.contains(&Sysno::umask),
                "{phase} must leave unlisted PassThrough umask unsubscribed"
            );
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#2373)
    /// A 0x10b recording predates flock forwarding, so it carries no flock event
    /// while this replayer expects one per call. Replaying it would consume some
    /// other event and desynchronize; the version gate must refuse it instead.
    #[test]
    fn record_version_rejects_pre_flock_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x10b)));
    }

    /// A 0x122 recording's procfs inode numbers came from the old pool, which
    /// started at 1 and counted only new host inodes. Replay re-renders them
    /// from the recorded raw bytes with the current pool, so the version gate
    /// must refuse the recording rather than show its guest other numbers.
    #[test]
    fn record_version_rejects_pre_request_ordinal_inode_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x122)));
    }

    /// A 0x123 recording has no events for accept, accept4, bind, listen or
    /// shutdown, which this replayer serves from the log, so replaying it would
    /// consume other calls' events; the version gate must refuse it.
    #[test]
    fn record_version_rejects_pre_accept_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x123)));
    }

    /// A 0x124 recording ran a blocking accept in the background, with one
    /// `Accept` event, while this replayer serves one event per in-turn
    /// attempt; the version gate must refuse it.
    #[test]
    fn record_version_rejects_backgrounded_accept_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x124)));
    }

    /// A 0x125 recording was made while a futex wake took the most recent
    /// waiters; replay recomputes every wake, so under the oldest-first order
    /// it could run another schedule. The version gate must refuse it.
    #[test]
    fn record_version_rejects_newest_first_futex_wake_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x125)));
    }

    /// A 0x126 recording was made while a vfork child kept running alone
    /// after its exec until the scheduler happened to see its live parent's
    /// continuation; replay recomputes turn selection, so under the release
    /// edge it could run another schedule. The version gate must refuse it.
    #[test]
    fn record_version_rejects_pre_vfork_release_edge_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x126)));
    }

    /// A 0x127 recording saw ENOSYS for the subreaper prctls, which this
    /// replayer runs live; replaying it could take another branch. The
    /// version gate must refuse it.
    #[test]
    fn record_version_rejects_pre_child_subreaper_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x127)));
    }

    /// A 0x128 recording was made by a scheduler that did not model signal
    /// targets in record and replay; replay recomputes where a signalled background thread rejoins
    /// and which signals end a wait, so it could run another schedule. The
    /// version gate must refuse it.
    #[test]
    fn record_version_rejects_streams_without_signal_target_modelling() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x128)));
    }

    /// A 0x129 recording held a committed SIGCHLD in a precise FUTEX_WAIT
    /// until the call returned; replay recomputes that wait and now ends it
    /// with EINTR, so it would take the other branch with no desync. The
    /// version gate must refuse it (Codex review of
    /// https://github.com/rrnewton/hermit/pull/4039).
    #[test]
    fn record_version_rejects_streams_that_held_a_committed_sigchld_in_a_futex_wait() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x129)));
    }

    /// A 0x12a recording gave a raw clone child with a non-SIGCHLD exit
    /// signal its creator's process identity; replay recomputes calls that
    /// name the process, so it could take another branch. The version gate
    /// must refuse it.
    #[test]
    fn record_version_rejects_streams_from_before_raw_clone_processes() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12a)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x129)));
    }

    #[test]
    fn record_version_rejects_streams_that_answered_a_futex_rewait_as_woken() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12b)));
    }

    #[test]
    fn record_version_rejects_streams_that_read_a_futex_wait_word_through_the_tracer() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12c)));
    }

    #[test]
    fn record_version_rejects_streams_whose_pidfd_signals_woke_no_receiver() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12d)));
    }

    #[test]
    fn record_version_rejects_streams_that_answered_a_zombie_kill_with_esrch() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x12e)));
    }

    #[test]
    fn record_version_rejects_streams_that_answered_a_zombie_sigqueueinfo_with_esrch() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x130)));
    }

    #[test]
    fn record_version_rejects_streams_that_answered_a_thread_id_kill_with_esrch() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x131)));
    }

    #[test]
    fn record_version_rejects_streams_that_answered_a_gone_sigqueueinfo_before_its_siginfo() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x134)));
    }

    #[test]
    fn record_version_rejects_streams_whose_stop_signals_armed_no_orphaned_group_record() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x137)));
    }

    /// Record and replay run under ptrace with serialized threads, so their
    /// scheduler models signal targets (`models_signal_targets` in
    /// detcore/src/scheduler.rs), as `hermit run` does on ptrace. Without it,
    /// a child-exit SIGCHLD that wakes a thread asleep in a backgrounded
    /// rt_sigsuspend does not hold the next pass at the release barrier, and
    /// recording `timeout 1 true` reported a false deadlock in 5 to 7 of 12
    /// concurrent runs (https://github.com/rrnewton/hermit/issues/3963).
    #[test]
    fn record_and_replay_model_signal_targets_as_ptrace_run_does() {
        let data = tempfile::tempdir().unwrap();
        let record = record_or_replay_config(data.path());
        let run =
            crate::prepare_backend_config(crate::DetConfig::default(), crate::Backend::Ptrace);
        assert!(record.sequentialize_threads);
        assert!(record.backend.supports_blocked_wait_signal_interruption);
        assert_eq!(
            record.backend.supports_blocked_wait_signal_interruption,
            run.backend.supports_blocked_wait_signal_interruption
        );
        assert!(!record.guest_may_inherit_a_terminal);
    }

    /// A 0x135 recording retired a SIGKILLed victim at its host-timed
    /// deregistration; replay recomputes turn selection, so under retirement
    /// at the kill turn it could run another schedule. The version gate must
    /// refuse it.
    #[test]
    fn record_version_rejects_pre_sigkill_retirement_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x135)));
    }

    /// A 0x138 recording holds no event for a pidfd_send_signal with a scope
    /// flag or a /proc/<pid> descriptor, which this replayer forwards
    /// (https://github.com/rrnewton/hermit/issues/4108).
    #[test]
    fn record_version_rejects_streams_that_answered_pidfd_scope_flags_without_an_event() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x138)));
    }

    /// A 0x13b recording holds no event for a pidfd_send_signal through a
    /// PIDFD_SELF sentinel, which this replayer forwards
    /// (https://github.com/rrnewton/hermit/issues/4147).
    #[test]
    fn record_version_rejects_streams_that_answered_pidfd_self_sentinels_without_an_event() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13b)));
    }

    /// A 0x13c recording was made when a SIGEV_THREAD_ID timer notified its
    /// whole process, which this replayer no longer does
    /// (https://github.com/rrnewton/hermit/issues/4164).
    #[test]
    fn record_version_rejects_streams_whose_sigev_thread_id_timers_notified_the_process() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13c)));
    }

    /// A recording older than 0x13e was made by a recorder that never
    /// readmitted a vfork-gated backgrounded child
    /// (https://github.com/rrnewton/hermit/issues/4163).
    #[test]
    fn record_version_rejects_streams_from_before_the_vfork_gate_readmission() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13d)));
    }

    /// A 0x13e recording was made when Detcore used a nested PID namespace's
    /// pids as container pids, which this replayer translates
    /// (https://github.com/rrnewton/hermit/issues/4155).
    #[test]
    fn record_version_rejects_streams_made_before_nested_pid_translation() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13e)));
    }

    /// A 0x13f or older recording read a waiting pselect6's temporary mask
    /// after the scheduler's grant, where this replayer reads it at the call's
    /// entry (https://github.com/rrnewton/hermit/issues/4095).
    #[test]
    fn record_version_rejects_streams_that_read_the_pselect6_mask_after_the_grant() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13e)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13d)));
    }

    /// A recording older than 0x141 was made when sched_getaffinity refused a
    /// set shorter than 16 bytes, which this replayer accepts
    /// (https://github.com/rrnewton/hermit/issues/4160).
    #[test]
    fn record_version_rejects_streams_whose_short_affinity_sets_were_refused() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x140)));
    }

    /// A recording older than 0x142 was made by a recorder whose internally
    /// polled restartable calls ignored a cross-task signal
    /// (https://github.com/rrnewton/hermit/issues/4187).
    #[test]
    fn record_version_rejects_streams_from_before_signals_ended_polled_calls() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x141)));
    }

    /// A recording older than 0x145 has no bytes for an fcntl that writes its
    /// result through its argument, which this replayer writes back
    /// (https://github.com/rrnewton/hermit/issues/2289).
    #[test]
    fn record_version_rejects_streams_without_fcntl_output_events() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x144)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x143)));
    }

    /// A recording older than 0x143 has no event for epoll_ctl, which this
    /// replayer reads from the recording
    /// (https://github.com/rrnewton/hermit/issues/31).
    #[test]
    fn record_version_rejects_streams_without_epoll_ctl_events() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x142)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x141)));
    }

    /// A 0x145 or older recording was made when a POSIX timer's queued signal
    /// survived exec, which this replayer takes back
    /// (https://github.com/rrnewton/hermit/issues/4184).
    #[test]
    fn record_version_rejects_streams_whose_timer_signals_survived_exec() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x145)));
    }

    /// A recording older than 0x147 was made by a scheduler that ran a polling
    /// vfork child alone while other threads were runnable
    /// (https://github.com/rrnewton/hermit/issues/4194).
    #[test]
    fn record_version_rejects_streams_from_before_the_vfork_gate_fallback() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x13f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x146)));
    }

    /// A recording older than 0x148 was made when get_mempolicy left the node
    /// mask unwritten and answered 0 to flags Linux refuses
    /// (https://github.com/rrnewton/hermit/issues/4214).
    #[test]
    fn record_version_rejects_streams_from_before_one_node_get_mempolicy() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x147)));
    }

    /// A recording older than 0x149 was made when /proc/cpuinfo showed the
    /// host's CPU while CPUID showed the virtual one
    /// (https://github.com/rrnewton/hermit/issues/4161).
    #[test]
    fn record_version_rejects_streams_from_before_virtual_cpuinfo() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x148)));
    }

    /// A recording older than 0x14a recorded the host's bytes for the procfs
    /// files Detcore now fixes from its machine description
    /// (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_fixed_kernel_procfs_files() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x148)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x149)));
    }

    /// A recording older than 0x14b was made when a guest's recorded
    /// thread-directed SIGCHLD did not end a precise futex wait, or, before
    /// https://github.com/rrnewton/hermit/pull/4177 (at 0x148), an emulated
    /// pause or nanosleep (https://github.com/rrnewton/hermit/issues/4132).
    /// A recording older than 0x151 was made when a `pause` or `nanosleep` a
    /// child-exit `SIGCHLD` from the scheduler woke could follow a running
    /// sibling, which then took the signal
    /// (https://github.com/rrnewton/hermit/issues/4333).
    #[test]
    fn record_version_rejects_streams_from_before_a_woken_sleep_takes_its_sigchld() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x150)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14e)));
    }

    /// A recording older than 0x14e was made when a child-exit `SIGCHLD` the
    /// scheduler sent a parked futex waiter could be deferred behind a
    /// runnable sibling, which then took it
    /// (https://github.com/rrnewton/hermit/issues/4304).
    /// A recording older than 0x150 was made when one exit's robust-list walk
    /// could wake a word it reached twice
    /// (https://github.com/rrnewton/hermit/issues/4238).
    #[test]
    fn record_version_rejects_streams_from_before_one_exit_marks_a_robust_word_once() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14f)));
    }

    #[test]
    fn record_version_rejects_streams_from_before_a_woken_futex_waiter_takes_its_sigchld() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14d)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14b)));
    }

    /// A recording older than 0x157 was made when a copy Linux discarded at
    /// its send ended a wait once a handler was installed
    /// (https://github.com/rrnewton/hermit/issues/4293).
    #[test]
    fn record_version_rejects_streams_from_before_discarded_sends_are_recorded() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x154)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x155)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x156)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x15a holds the sanitized kernel read of
    /// /proc/loadavg, which Detcore now renders as a fixed file
    /// (https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_loadavg_is_rendered() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x159)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x15b was made when a sleep a child-exit SIGCHLD
    /// woke under record did not claim the signal, so it returned 0 or slept on
    /// (https://github.com/rrnewton/hermit/issues/4343).
    #[test]
    fn record_version_rejects_streams_from_before_a_recorded_sleep_claims_its_sigchld() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15a)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x15c may hold a claim of a woken sleep's
    /// SIGCHLD made while a background read's descriptor had been replaced
    /// before the read entered the kernel
    /// (https://github.com/rrnewton/hermit/issues/4410).
    #[test]
    fn record_version_rejects_streams_from_before_replaced_read_descriptors_block_the_claim() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15b)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x16e has a restartable call's attempt stopped by
    /// a pending signal (ERESTARTSYS) where the call now blocks signals first
    /// and records the attempt's result (R2b-2 stage 2,
    /// https://github.com/rrnewton/hermit/issues/4273).
    #[test]
    fn record_version_rejects_streams_from_before_restartable_calls_block_first() {
        for older in 0x163..=0x16d {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x170 held a `SIGCHLD` a guest sent a thread,
    /// recorded in its private queue, until the thread's gated wait or
    /// restartable pipe call returned, where it now ends the call (R2b-2
    /// stage 3, https://github.com/rrnewton/hermit/issues/4273). 0x171 and
    /// 0x172 held it too, and 0x170, stage 3's first label, is refused as well.
    #[test]
    fn record_version_rejects_streams_from_before_a_private_sigchld_ends_a_wait() {
        for older in 0x163..=0x172 {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x16a answered EBADF, unrecorded, for a relative
    /// name below a directory descriptor received over SCM_RIGHTS, where the
    /// kernel's lookup is now recorded (K1-0 of
    /// https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_received_directory_lookups() {
        for older in 0x163..=0x169 {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x16d read the kernel's /proc/<pid>/mountinfo
    /// and /proc/<pid>/mounts where Detcore now serves the recording
    /// container's mount table (K3a of
    /// https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_the_container_mount_table() {
        // 0x16b is the first K3a's label (shipped, reverted), 0x16c the
        // revert's: both are refused by the exact-match gate as well.
        for older in 0x163..=0x16c {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x169 refused a vectored read of a /proc/cpuinfo
    /// the kernel opened (ENOSYS) where Detcore now captures it, as it renders
    /// /proc/cpuinfo where CPUID is virtualized and intercepted
    /// (https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_cpuinfo_is_rendered() {
        for older in 0x163..=0x168 {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x168 sent a POSIX timer's expiry as a bare
    /// kill or tgkill; it is now an SI_TIMER record with the timer's id and
    /// si_value (https://github.com/rrnewton/hermit/issues/4262).
    #[test]
    fn record_version_rejects_streams_from_before_posix_timer_records_carry_si_timer() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x167)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x167 looked a name relative to a descriptor
    /// opened through a descriptor link up below the link's spelling, which
    /// names no procfs file, where Detcore now looks it up below the file the
    /// link reaches (https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_descriptor_links_record_their_target() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x160)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x161)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x162)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x163)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x164)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x165)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x166)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x163 refused a vectored read of a /proc/stat
    /// the kernel opened (ENOSYS) where Detcore now captures it, and showed
    /// the guest the host's lines, as Detcore now renders /proc/stat
    /// (https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_stat_is_rendered() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15d)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15e)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15f)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x160)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x161)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x162)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x15d refused a vectored read of a /proc/uptime
    /// the kernel opened (ENOSYS) where Detcore now captures it, as it
    /// renders /proc/uptime (https://github.com/rrnewton/hermit/issues/4213).
    #[test]
    fn record_version_rejects_streams_from_before_uptime_is_rendered() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15c)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x161 may hold a pidfd_send_signal through a
    /// clone pidfd that Detcore answered EBADF unrecorded; it is now forwarded
    /// and recorded (https://github.com/rrnewton/hermit/issues/4430).
    #[test]
    fn record_version_rejects_streams_from_before_clone_pidfds_are_known() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x160)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x15e may hold a clone3 that reused, through
    /// set_tid, an id an earlier task had; that call is now refused
    /// (https://github.com/rrnewton/hermit/issues/4370).
    #[test]
    fn record_version_rejects_streams_from_before_reused_set_tid_is_refused() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x15d)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x164 was made when a pipe call woken by a kept
    /// signal could miss the wake or end on a copy Linux discarded (0x15e and
    /// 0x160 to 0x163), or when a pipe writev could write an iovec array
    /// another thread rewrote after the call began (0x15f)
    /// (https://github.com/rrnewton/hermit/issues/4273).
    #[test]
    fn record_version_rejects_streams_from_before_kept_wakes_stand() {
        for older in 0x15e..=0x163 {
            assert!(
                !RECORD_VERSION.compatible_with(&RecordVersion(older)),
                "{older:#x}"
            );
        }
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x162 may hold a PR_SET_PDEATHSIG the kernel
    /// armed and a parent-death signal it sent; Detcore now sends it
    /// (https://github.com/rrnewton/hermit/issues/4440).
    #[test]
    fn record_version_rejects_streams_from_before_parent_death_signals_are_sent_by_detcore() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x161)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A recording older than 0x166 was made while a clear-TID joiner was
    /// pushed onto the run queue at once, ahead of pending admissions; it is
    /// now admitted in DetTid order with them
    /// (https://github.com/rrnewton/hermit/issues/4362, part 2 step 0b).
    #[test]
    fn record_version_rejects_streams_from_before_cleartid_wakes_are_admitted() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x165)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    #[test]
    fn record_version_rejects_streams_from_before_private_sigchld_ends_a_wait() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14a)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x149)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x148)));
    }

    /// A recording older than 0x14c stopped every thread while a vfork child's
    /// call ran in the background and logged no rejoin for its readmission
    /// (https://github.com/rrnewton/hermit/issues/4211).
    #[test]
    fn record_version_rejects_streams_from_before_logged_vfork_rejoins() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14b)));
    }

    /// A recording older than 0x14d recorded the kernel's tables in the
    /// host's shape, where Detcore now renders fixed ones
    /// (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_fixed_kernel_tables() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14c)));
    }

    /// A recording older than 0x14f sent a timer's signal to its arming or
    /// drawn thread even when that thread blocked it
    /// (https://github.com/rrnewton/hermit/issues/4320).
    #[test]
    fn record_version_rejects_streams_from_before_mask_aware_timer_targets() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x14e)));
    }

    /// A recording older than 0x152 holds the kernel's answers for /proc
    /// names that Detcore now has the kernel answer as missing
    /// (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_procfs_levels() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x151)));
    }

    /// A recording older than 0x153 ran at the `standard` /proc level without
    /// storing it, and would replay at the new default, `minimal`
    /// (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_the_minimal_default() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x152)));
    }

    /// A recording older than 0x154 readmitted a call that began with a
    /// scheduler signal waiting in the shared queue through the rejoin log
    /// (https://github.com/rrnewton/hermit/issues/4234).
    #[test]
    fn record_version_rejects_streams_from_before_armed_waiting_signal_calls() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x153)));
    }

    /// A recording older than 0x165 holds sendfile's count without its copy,
    /// which a new replay needs (https://github.com/rrnewton/hermit/issues/4416).
    #[test]
    fn record_version_rejects_streams_from_before_sendfile_records_its_copy() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x164)));
        assert!(RECORD_VERSION.compatible_with(&RECORD_VERSION));
    }

    /// A replayer older than 0x155 ignores a recording's CPU profile and would
    /// replay an x86-64-v3 guest against the v2 table
    /// (<https://github.com/rrnewton/hermit/issues/4243>).
    #[test]
    fn record_version_rejects_streams_from_before_cpu_profiles() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x154)));
    }

    /// A recording older than 0x156 holds the kernel's answers for the names
    /// in a process's directory that Detcore now has the kernel answer as
    /// missing (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_per_process_levels() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x155)));
    }

    /// A recording older than 0x158 holds a kernel readlink for each /proc
    /// link whose target Detcore now decides itself
    /// (https://github.com/rrnewton/hermit/issues/4266).
    #[test]
    fn record_version_rejects_streams_from_before_decided_procfs_links() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x157)));
    }

    /// A recording older than 0x159 read /proc/<pid>/cgroup as an unclassified
    /// file, recording a readlink of its descriptor and the host's bytes
    /// (https://github.com/rrnewton/hermit/issues/4378).
    #[test]
    fn record_version_rejects_streams_from_before_the_process_cgroup_root() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x158)));
    }

    #[test]
    fn record_version_rejects_previous_memory_configuration() {
        // Metadata does not persist the memory configuration, so a recording made with the
        // previous hardcoded value cannot be replayed compatibly with the corrected default.
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x10a)));
    }

    #[test]
    fn record_version_rejects_pre_complete_determinized_subscription_streams() {
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x109)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x104)));
        assert!(!RECORD_VERSION.compatible_with(&RecordVersion(0x102)));
    }

    /// A recording made before the network choice was stored ran on the host
    /// network, and must still load, reporting no choice.
    #[test]
    fn metadata_without_a_network_choice_loads_as_unknown() {
        let metadata: Metadata = serde_json::from_value(serde_json::json!({
            "exe": "/bin/true",
            "program": "true",
            "arg0": "true",
            "args": [],
            "current_dir": "/",
            "hostname": null,
            "domainname": null,
            "envs": {},
            "version": RECORD_VERSION,
        }))
        .unwrap();
        assert_eq!(metadata.local_networking, None);
    }
}
