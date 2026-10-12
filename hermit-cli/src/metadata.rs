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
// `backend_supports_blocked_wait_signal_interruption`, so the scheduler models
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
pub(crate) const RECORD_VERSION: RecordVersion = RecordVersion(0x13c);

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
const HIGHEST_SHIPPED_RECORD_VERSION: u32 = 0x13c;

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
    /// Whether the recording ran in an isolated network namespace with only
    /// loopback (`record start --network local`). `None` means the recorder
    /// did not say, which is every recording made before this field existed;
    /// those recordings ran on the host network. An autopilot replay uses the
    /// same network, so syscalls the replayer re-executes see the same
    /// interfaces and ports they were recorded against.
    #[serde(default)]
    pub local_networking: Option<bool>,
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
            local_networking: None,
        })
    }

    /// Reads the metadata of the recording stored in `dir`.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        let metadata_path = dir.join(METADATA_NAME);
        serde_json::from_reader(
            fs::File::open(&metadata_path)
                .with_context(|| format!("Failed to open {:?}", metadata_path))?,
        )
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
        mount_id_assignment_order: Vec::new(),
        virtualize_cpuid: true,
        // Record and replay run under the ptrace backend.
        backend: <reverie_ptrace::PtraceBackend as reverie::Backend>::capabilities(),
        // As `hermit run` sets it for ptrace (`prepare_backend_config` in
        // lib.rs). With it off, the scheduler did not model signal targets, so
        // a thread asleep in a backgrounded rt_sigsuspend that a child-exit
        // SIGCHLD woke was not held at the release barrier: the scheduler could
        // pop the next timer, or report a terminal deadlock, before the thread's
        // continuation posted (https://github.com/rrnewton/hermit/issues/3963).
        backend_supports_blocked_wait_signal_interruption: true,
        backend_delivers_timer_events: false,
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
        scheduler_turn_cost: default_config.scheduler_turn_cost,
        seed: default_config.seed,
        rng_seed: default_config.rng_seed,
        imprecise_timers: false,
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
    };
    if let Err(reason) = record_timer_support() {
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
        let record = <detcore::Detcore<crate::recorder::Recorder> as Tool>::subscriptions(&config);
        let replay = <detcore::Detcore<crate::replayer::Replayer> as Tool>::subscriptions(&config);

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
        assert!(record.backend_supports_blocked_wait_signal_interruption);
        assert_eq!(
            record.backend_supports_blocked_wait_signal_interruption,
            run.backend_supports_blocked_wait_signal_interruption
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
