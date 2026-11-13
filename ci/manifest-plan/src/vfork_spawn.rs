//! Starting a program inside an invocation cgroup without copying this
//! process (dh-7n47nt).
//!
//! `std::process::Command` with a `pre_exec` closure, which the cgroup join
//! needed, starts every child with fork(). fork() copies this process's
//! address space while it holds the memory-map lock, and every other thread
//! that page-faults meanwhile waits. In the library's test process (about 330
//! MB, 80-140 threads) each fork took a median 114-135 ms and up to 3 s, and
//! CPU readers and census walks in the same process stalled behind them.
//!
//! Here the child is created with clone3(CLONE_VM | CLONE_VFORK |
//! CLONE_INTO_CGROUP), as glibc's posix_spawn creates its children
//! (`CLONE_VM | CLONE_VFORK` on a stack of their own), with the kernel placing
//! it in the invocation cgroup as it is created. The child shares this
//! process's memory until it execs, and this thread waits until then. So the
//! child runs only [`vfork_child`]: raw system calls on storage this thread
//! prepared beforehand, no allocation, no locks and no libc call, on a stack
//! of its own, never on this thread's.
//!
//! clone3 needs Linux 5.3 and CLONE_INTO_CGROUP 5.7. Where clone3 is refused
//! as a call (ENOSYS, E2BIG or EINVAL, as on older kernels or under a seccomp
//! filter, or EPERM from a filter, which a probe tells apart from a cgroup
//! that refused the child), [`Started::Unusable`] tells the caller to start
//! the program the old way, and the refusal is remembered for the rest of the
//! process, with one line on stderr. A refusal that is about the cgroup itself
//! is a failed join.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::OsString;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

/// What to start, and where.
pub(crate) struct Program<'a> {
    /// The program, then its arguments. A program named without a `/` is
    /// looked up in the `PATH` of its environment, as execvp looks it up.
    pub(crate) argv: &'a [String],
    /// This process's environment with these variables set over it.
    pub(crate) env_overrides: &'a BTreeMap<String, String>,
    pub(crate) cwd: &'a Path,
    /// Descriptors that become the program's stdin, stdout and stderr.
    pub(crate) stdio: [RawFd; 3],
    /// The invocation cgroup's directory, which the program starts in.
    pub(crate) cgroup: RawFd,
}

/// How [`start`] ended.
#[derive(Debug)]
pub(crate) enum Started {
    /// The program runs as process `pid`, in the cgroup.
    Running { pid: u32 },
    /// clone3 cannot start programs in a cgroup here, for `reason`; start the
    /// program another way.
    Unusable { reason: String },
    /// The kernel would not create the child in the cgroup.
    JoinFailed(io::Error),
    /// No child was created, or the child could not set itself up or execute
    /// the program, which then never ran.
    Failed(io::Error),
}

/// The probe child's owned lifecycle state. Held under
/// [`Clone3Support`]'s lock for every signal, reap marking
/// and fd close.
/// An owned file descriptor: closes when dropped, and is
/// neither Copy nor Clone, so it has exactly one owner.
struct OwnedFd(RawFd);

impl OwnedFd {
    fn raw(&self) -> RawFd {
        self.0
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

/// The probe child's owned wait handle: its pidfd when the
/// clone returned one, which path is in use, and its pid.
/// Moved as a whole when the background reaper takes over.
struct OwnedProbeChild {
    /// The pidfd, owned; closed when this handle drops,
    /// after its reap.
    pidfd: Option<OwnedFd>,
    /// Whether waits and signals use the pidfd (true) or
    /// the numeric pid (false, only when CLONE_PIDFD was
    /// refused at clone time).
    via_pidfd: bool,
    /// The child's numeric pid (for the fallback path and
    /// for test inspection).
    pid: libc::pid_t,
}

/// Who holds the probe child's wait handle. Exactly one
/// owner at a time: the slot holds it while the caller owns
/// it; taking it out for the background reaper is a move
/// (the slot then says HandedOff and the caller can neither
/// wait nor signal); after the reap it says Reaped.
enum ProbeChildSlot {
    /// No child has been created.
    NoChild,
    /// The caller owns the handle.
    Owned(OwnedProbeChild),
    /// The background reaper owns the handle.
    HandedOff,
    /// Reaped (or shown to have no waitable child left).
    Reaped,
}

/// Locks the probe child's lifecycle handle.
fn probe_child_slot(support: &Clone3Support) -> std::sync::MutexGuard<'_, ProbeChildSlot> {
    support
        .probe_child
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
/// Whether clone3 has been refused as a call in this process.
pub(crate) struct Clone3Support {
    unusable: OnceLock<String>,
    /// The startup probe's record, once it has run in this process
    /// (see [`Clone3Support::probe_cgroup_clone_once`]).
    cgroup_probe: OnceLock<CgroupProbeRecord>,
    /// For a test: the startup probe finds its child killed by SIGKILL,
    /// without making a call.
    #[cfg(test)]
    inject_cgroup_probe_kill: bool,
    /// For a test: the probe child pauses before writing its entry
    /// byte, held before it can run.
    #[cfg(test)]
    inject_probe_child_hangs_before_entry: bool,
    /// For a test: the probe child pauses after writing its entry
    /// byte, held after it ran.
    #[cfg(test)]
    inject_probe_child_hangs_after_entry: bool,
    /// For a test: the probe child kills itself after writing its
    /// entry byte, as a post-entry kill would.
    #[cfg(test)]
    inject_probe_child_kills_after_entry: bool,
    /// For a test: the parent does not read the entry pipe until
    /// after it has reaped the child, so the child has written
    /// and died before any read.
    #[cfg(test)]
    inject_probe_parent_read_delay: bool,
    /// For a test: the timeout kill fails with this errno, as a
    /// policy that refuses kill would make it.
    #[cfg(test)]
    inject_probe_timeout_kill_errno: Option<i32>,
    /// A probe child this process still owes a reap for, when
    /// its cleanup could not complete (a wait that failed, or a
    /// reaper that could not be started). The record is the
    /// owner: the next start retries the reap from it, and the
    /// child stays this process's own until then, so its pid
    /// cannot be reused first.
    unreaped_probe_child: std::sync::Mutex<Option<libc::pid_t>>,
    /// For a test: the first probe waits fail with this errno,
    /// as a wait policy that refuses wait4 would make them.
    #[cfg(test)]
    inject_probe_wait_errno: Option<i32>,
    /// For a test: how many probe waits are left to fail.
    #[cfg(test)]
    inject_probe_wait_remaining: std::sync::atomic::AtomicUsize,
    /// For a test: every reap-grace wait fails with this errno.
    #[cfg(test)]
    inject_probe_grace_wait_errno: Option<i32>,
    /// For a test: starting the background reaper fails.
    #[cfg(test)]
    inject_probe_reaper_spawn_fails: bool,
    /// For a test: an owned duplicate of the probe child's
    /// pidfd, held so a fixture whose injected wait answered
    /// for the real one can still clean its own child up
    /// through that fd alone. Taken and closed on every path
    /// that does not hand it to that cleanup.
    #[cfg(test)]
    probe_child_cleanup_pidfd: std::sync::Mutex<Option<OwnedFd>>,
    /// For a test: the caller fallback kill after a failed
    /// reaper spawn fails with this errno.
    #[cfg(test)]
    inject_probe_fallback_kill_errno: Option<i32>,
    /// For a test: an injected wait error fires only on a wait
    /// made after the entry byte was witnessed, so the fixture
    /// cannot fabricate it for a child still to enter.
    #[cfg(test)]
    inject_probe_wait_after_entry: bool,
    /// For a test: pause inside the reaper's debt return, so a
    /// regression can sample the debt and the slot together
    /// during the transition.
    #[cfg(test)]
    inject_debt_return_pause: std::sync::atomic::AtomicBool,
    /// For a test: the debt return has reached its pause.
    #[cfg(test)]
    debt_return_entered: std::sync::atomic::AtomicBool,
    /// For a test: the sampler has observed the held return
    /// and releases it. No timing is involved either way.
    #[cfg(test)]
    debt_return_release: std::sync::atomic::AtomicBool,
    /// For a test: the inode numbers of this probe child's
    /// pidfd and its duplicate, recorded at creation, so the
    /// fd census can name exactly this child's fds.
    #[cfg(test)]
    probe_child_fd_inodes: std::sync::Mutex<Vec<(u64, u64)>>,
    /// For a test: this kernel gives every pidfd the same
    /// inode (pre-pidfs kernels), so an inode census cannot
    /// attribute an fd to this child and must refuse by name.
    #[cfg(test)]
    probe_child_fd_census_shared: std::sync::atomic::AtomicBool,
    /// The probe child's owned wait handle, under one lock.
    /// Exactly one owner at a time; see [`ProbeChildSlot`].
    probe_child: std::sync::Mutex<ProbeChildSlot>,
    /// For the record only: whether the probe child is
    /// waited for through its pidfd. Never used to decide a
    /// signal or a wait.
    probe_child_waited_via_pidfd: AtomicBool,
    /// The probe child's pid, once its clone returned (-1 until then).
    pub(crate) probe_child_pid: AtomicI32,
    /// How long the probe may run before its child is killed and it
    /// records a timeout. Five seconds in production: starting the
    /// trivial program takes single-digit milliseconds even cold, so
    /// this is three orders of magnitude of headroom, and it leaves
    /// the first invocation its own deadline in realistic configs.
    probe_bound: std::time::Duration,
    /// For a test: how many startup probes were made, or answered for.
    #[cfg(test)]
    pub(crate) cgroup_probe_attempts: std::sync::atomic::AtomicUsize,
    /// For a test: answer the first `inject_limit` clone3 calls with this
    /// errno instead of making them.
    #[cfg(test)]
    pub(crate) inject: Option<i32>,
    #[cfg(test)]
    inject_limit: usize,
    /// For a test: answer the EPERM probe with this errno instead of making
    /// it.
    #[cfg(test)]
    inject_probe: Option<i32>,
    /// For a test: the child kills itself before exec.
    #[cfg(test)]
    kill_child_before_exec: bool,
    /// For a test: how many times clone3 was called, or answered for.
    #[cfg(test)]
    pub(crate) attempts: std::sync::atomic::AtomicUsize,
    /// For a test: how many EPERM probes were made, or answered for.
    #[cfg(test)]
    pub(crate) probes: std::sync::atomic::AtomicUsize,
    /// For a test: how many refusal notices went to stderr.
    #[cfg(test)]
    pub(crate) notices: std::sync::atomic::AtomicUsize,
}

impl Clone3Support {
    pub(crate) const fn new() -> Self {
        Self {
            unusable: OnceLock::new(),
            cgroup_probe: OnceLock::new(),
            #[cfg(test)]
            inject_cgroup_probe_kill: false,
            #[cfg(test)]
            inject_probe_child_hangs_before_entry: false,
            #[cfg(test)]
            inject_probe_child_hangs_after_entry: false,
            #[cfg(test)]
            inject_probe_child_kills_after_entry: false,
            #[cfg(test)]
            inject_probe_parent_read_delay: false,
            #[cfg(test)]
            inject_probe_timeout_kill_errno: None,
            unreaped_probe_child: std::sync::Mutex::new(None),
            #[cfg(test)]
            inject_probe_wait_errno: None,
            #[cfg(test)]
            inject_probe_wait_remaining: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            inject_probe_grace_wait_errno: None,
            #[cfg(test)]
            inject_probe_reaper_spawn_fails: false,
            #[cfg(test)]
            probe_child_cleanup_pidfd: std::sync::Mutex::new(None),
            #[cfg(test)]
            inject_probe_fallback_kill_errno: None,
            #[cfg(test)]
            inject_probe_wait_after_entry: false,
            #[cfg(test)]
            inject_debt_return_pause: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            debt_return_entered: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            debt_return_release: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            probe_child_fd_inodes: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            probe_child_fd_census_shared: std::sync::atomic::AtomicBool::new(false),
            probe_child: std::sync::Mutex::new(ProbeChildSlot::NoChild),
            probe_child_waited_via_pidfd: AtomicBool::new(false),
            probe_child_pid: AtomicI32::new(-1),
            probe_bound: std::time::Duration::from_secs(5),
            #[cfg(test)]
            cgroup_probe_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            inject: None,
            #[cfg(test)]
            inject_limit: 0,
            #[cfg(test)]
            inject_probe: None,
            #[cfg(test)]
            kill_child_before_exec: false,
            #[cfg(test)]
            attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            probes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            notices: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// For a test: a [`Clone3Support`] with these injections.
    #[cfg(test)]
    const fn injecting(
        inject: Option<i32>,
        inject_limit: usize,
        inject_probe: Option<i32>,
        kill_child_before_exec: bool,
    ) -> Self {
        Self {
            unusable: OnceLock::new(),
            cgroup_probe: OnceLock::new(),
            inject_cgroup_probe_kill: false,
            inject_probe_child_hangs_before_entry: false,
            inject_probe_child_hangs_after_entry: false,
            #[cfg(test)]
            inject_probe_child_kills_after_entry: false,
            #[cfg(test)]
            inject_probe_parent_read_delay: false,
            #[cfg(test)]
            inject_probe_timeout_kill_errno: None,
            unreaped_probe_child: std::sync::Mutex::new(None),
            #[cfg(test)]
            inject_probe_wait_errno: None,
            #[cfg(test)]
            inject_probe_wait_remaining: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            inject_probe_grace_wait_errno: None,
            #[cfg(test)]
            inject_probe_reaper_spawn_fails: false,
            #[cfg(test)]
            probe_child_cleanup_pidfd: std::sync::Mutex::new(None),
            #[cfg(test)]
            inject_probe_fallback_kill_errno: None,
            #[cfg(test)]
            inject_probe_wait_after_entry: false,
            #[cfg(test)]
            inject_debt_return_pause: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            debt_return_entered: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            debt_return_release: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            probe_child_fd_inodes: std::sync::Mutex::new(Vec::new()),
            #[cfg(test)]
            probe_child_fd_census_shared: std::sync::atomic::AtomicBool::new(false),
            probe_child: std::sync::Mutex::new(ProbeChildSlot::NoChild),
            probe_child_waited_via_pidfd: AtomicBool::new(false),
            probe_child_pid: AtomicI32::new(-1),
            probe_bound: std::time::Duration::from_secs(5),
            cgroup_probe_attempts: std::sync::atomic::AtomicUsize::new(0),
            inject,
            inject_limit,
            inject_probe,
            kill_child_before_exec,
            attempts: std::sync::atomic::AtomicUsize::new(0),
            probes: std::sync::atomic::AtomicUsize::new(0),
            notices: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// For a test: a [`Clone3Support`] whose startup probe finds its
    /// child killed by SIGKILL, without making a call.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_killed() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_cgroup_probe_kill = true;
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child pauses
    /// before its entry byte, with a short probe bound.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_hangs_before_entry() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_hangs_before_entry = true;
        support.probe_bound = std::time::Duration::from_millis(200);
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child pauses
    /// after its entry byte, with a short probe bound.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_hangs_after_entry() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_hangs_after_entry = true;
        support.probe_bound = std::time::Duration::from_millis(200);
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child kills
    /// itself after its entry byte.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_kills_after_entry() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_kills_after_entry = true;
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child kills
    /// itself after its entry byte while the parent's first read
    /// is delayed, so the byte is only in the pipe when the child
    /// is reaped.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_kills_after_entry_with_delayed_read() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_kills_after_entry = true;
        support.inject_probe_parent_read_delay = true;
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child pauses
    /// before its entry byte and whose timeout kill fails, with a
    /// short probe bound.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_hangs_with_failing_kill() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_hangs_before_entry = true;
        support.inject_probe_timeout_kill_errno = Some(libc::EPERM);
        support.probe_bound = std::time::Duration::from_millis(200);
        support
    }

    /// For a test: a [`Clone3Support`] whose first probe wait
    /// fails with `errno`, as a wait policy refusing wait4
    /// would make it. The child itself is an ordinary one.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_wait_fails_with(errno: i32) -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_wait_errno = Some(errno);
        support.inject_probe_wait_remaining = std::sync::atomic::AtomicUsize::new(1);
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child pauses
    /// before its entry byte and whose first probe wait answers
    /// ECHILD, as a SIGCHLD disposition that reaps children
    /// itself (SIG_IGN) makes waitpid answer.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_hangs_with_echild_wait() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_hangs_before_entry = true;
        support.inject_probe_wait_errno = Some(libc::ECHILD);
        support.inject_probe_wait_remaining = std::sync::atomic::AtomicUsize::new(1);
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child kills
    /// itself after its entry byte and whose first probe wait
    /// answers ECHILD.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_kills_after_entry_with_echild_wait() -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_kills_after_entry = true;
        support.inject_probe_wait_errno = Some(libc::ECHILD);
        support.inject_probe_wait_after_entry = true;
        support.inject_probe_wait_remaining = std::sync::atomic::AtomicUsize::new(1);
        support
    }

    /// For a test: a [`Clone3Support`] whose probe child pauses
    /// before its entry byte, whose timeout kill and every
    /// reap-grace wait fail, and (when `reaper_fails`) whose
    /// background reaper cannot be started. The short bound
    /// reaches the timeout path quickly.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_unreapable(reaper_fails: bool) -> Self {
        let mut support = Self::injecting(None, 0, None, false);
        support.inject_probe_child_hangs_before_entry = true;
        support.inject_probe_timeout_kill_errno = Some(libc::EPERM);
        support.inject_probe_grace_wait_errno = Some(libc::EPERM);
        support.inject_probe_reaper_spawn_fails = reaper_fails;
        support.probe_bound = std::time::Duration::from_millis(200);
        support
    }

    /// For a test: a [`Clone3Support`] like
    /// [`Clone3Support::cgroup_probe_child_unreapable`] with a
    /// failing reaper, whose caller fallback kill also fails:
    /// the bounded fallback poll then leaves the child owed.
    #[cfg(test)]
    pub(crate) const fn cgroup_probe_child_unreapable_with_failed_fallback_kill() -> Self {
        let mut support = Self::cgroup_probe_child_unreapable(true);
        support.inject_probe_fallback_kill_errno = Some(libc::EPERM);
        support
    }

    /// For a test: a [`Clone3Support`] whose clone3 always fails with
    /// `errno`, without being called, as under a filter that refuses every
    /// clone3: the EPERM probe gets `errno` too.
    #[cfg(test)]
    pub(crate) const fn refusing(errno: i32) -> Self {
        Self::injecting(Some(errno), usize::MAX, Some(errno), false)
    }

    /// For a test: a [`Clone3Support`] whose first clone3 fails with `errno`,
    /// without being called, as a cgroup that refuses one child would make it
    /// fail. The EPERM probe and every later clone3 are real.
    #[cfg(test)]
    pub(crate) const fn refusing_once(errno: i32) -> Self {
        Self::injecting(Some(errno), 1, None, false)
    }

    /// For a test: [`Clone3Support::refusing_once`], with the EPERM probe
    /// answered `probe` instead of made.
    #[cfg(test)]
    pub(crate) const fn refusing_once_with_probe(errno: i32, probe: i32) -> Self {
        Self::injecting(Some(errno), 1, Some(probe), false)
    }

    /// For a test: a [`Clone3Support`] whose children kill themselves with
    /// SIGKILL before they exec.
    #[cfg(test)]
    pub(crate) const fn killing_children_before_exec() -> Self {
        Self::injecting(None, 0, None, true)
    }

    /// Record that this process still owes a reap for the probe
    /// child `pid`, keeping ownership of it until a retry reaps
    /// it. Called when a wait failed or a reaper could not be
    /// started; never called when a wait showed no waitable
    /// child remains (ECHILD), which completes the obligation.
    fn keep_unreaped_probe_child(&self, pid: libc::pid_t) {
        let mut record = self
            .unreaped_probe_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *record != Some(pid) {
            eprintln!(
                "manifest-plan: probe child {pid} is still unreaped; \
                 this process keeps owing its reap and retries it at the next start"
            );
        }
        *record = Some(pid);
    }

    /// Retry the reap owed for a probe child, without blocking:
    /// one WNOHANG wait. Returns true when no waitable child
    /// remains (it was reaped, or there is none), clearing the
    /// record; false while the child is still there or its wait
    /// still fails, keeping the record for the next retry.
    pub(crate) fn reap_unreaped_probe_child(&self) -> bool {
        // The whole snapshot-wait-clear transaction holds the
        // record lock: two concurrent starters cannot snapshot
        // the same pid, have one reap it, and then wait on a
        // pid that may since have been reused.
        let mut record = self
            .unreaped_probe_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(pid) = *record else {
            return true;
        };
        if matches!(*probe_child_slot(self), ProbeChildSlot::HandedOff) {
            // The reaper owns the wait right now; a retry by
            // this caller must neither clear the debt nor mark
            // anything reaped.
            return false;
        }
        match wait_probe_child(self, pid, false, false) {
            ChildWait::Reaped(_) | ChildWait::NoWaitable => {
                *record = None;
                drop(record);
                mark_probe_child_reaped(self);
                true
            }
            ChildWait::Failed(errno) => {
                if errno != Some(libc::EINTR) {
                    eprintln!(
                        "manifest-plan: retrying the reap of probe child {pid} failed \
                         (errno {errno:?}); keeping the obligation"
                    );
                }
                false
            }
            ChildWait::StillRunning => false,
        }
    }

    /// For a test: how many of this process's open fds are
    /// this probe child's own pidfd or its duplicate, by the
    /// inode numbers recorded when they were created. Other
    /// tests' pidfds have other inodes and never count, and
    /// one leaked past its reap keeps its inode and still
    /// counts.
    #[cfg(test)]
    pub(crate) fn probe_child_fds_still_open(&self) -> Option<usize> {
        if self.probe_child_fd_census_shared.load(Ordering::Relaxed) {
            return None;
        }
        use std::os::unix::fs::MetadataExt as _;
        let inodes = self
            .probe_child_fd_inodes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if inodes.is_empty() {
            return Some(0);
        }
        let entries = match std::fs::read_dir("/proc/self/fd") {
            Ok(entries) => entries,
            Err(error) => {
                panic!("cannot read /proc/self/fd for the probe child's fd census: {error}")
            }
        };
        let mut count = 0;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    panic!("cannot read an entry of /proc/self/fd for the fd census: {error}")
                }
            };
            match std::fs::metadata(entry.path()) {
                Ok(meta) => {
                    if inodes.contains(&(meta.dev(), meta.ino())) {
                        count += 1;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // An fd of another test that closed between
                    // the listing and the stat: it was never one
                    // of this child's held fds, which stay open
                    // for the whole census.
                    continue;
                }
                Err(error) => {
                    panic!("cannot stat an fd for the probe child's fd census: {error}")
                }
            }
        }
        Some(count)
    }

    /// For a test: whether the injected wait failure was
    /// consumed, which is how a fixture knows its real wait
    /// never ran and the child remains its own corpse to
    /// clean up.
    #[cfg(test)]
    pub(crate) fn injected_wait_consumed(&self) -> bool {
        self.inject_probe_wait_errno.is_some()
            && self.inject_probe_wait_remaining.load(Ordering::Relaxed) == 0
    }

    /// For a test: whether this process has reaped the probe
    /// child. Once true, its pid must never be signalled again.
    #[cfg(test)]
    pub(crate) fn probe_child_is_reaped(&self) -> bool {
        matches!(*probe_child_slot(self), ProbeChildSlot::Reaped)
    }

    /// For a test: signal the probe child for cleanup, but only
    /// while this process still owns it and has not reaped it.
    /// The signal goes through the pidfd held from its clone,
    /// which names this exact child and cannot reach a process
    /// that later reuses the pid; the numeric pid is used only
    /// if no pidfd exists and the child is still unreaped.
    #[cfg(test)]
    pub(crate) fn kill_probe_child_for_cleanup(&self) {
        let pid = self.probe_child_pid.load(Ordering::Relaxed);
        if pid > 0 {
            let _ = kill_probe_child(self, pid);
        }
    }

    /// For a test: clean up a child whose injected wait
    /// answered for the real one. Proves through the child's
    /// own pidfd (waitid with WNOWAIT) that it has not
    /// exited before signalling it, signals only through
    /// that fd, and reaps only through that fd. Never a
    /// numeric pid.
    #[cfg(test)]
    pub(crate) fn cleanup_injected_wait_leftover(&self) {
        let fd = {
            let mut guard = self
                .probe_child_cleanup_pidfd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take()
        };
        let Some(fd) = fd else {
            panic!("injected-wait cleanup has no pidfd duplicate to clean through");
        };
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let waited = libc::waitid(
                libc::P_PIDFD,
                fd.raw() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            if waited == 0 && info.si_pid() == 0 {
                let _ = syscall6(
                    libc::SYS_pidfd_send_signal,
                    [fd.raw() as usize, libc::SIGKILL as usize, 0, 0, 0, 0],
                );
            }
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let _ = libc::waitid(
                libc::P_PIDFD,
                fd.raw() as libc::id_t,
                &mut info,
                libc::WEXITED,
            );
        }
        // `fd` (the OwnedFd duplicate) drops and closes here,
        // on this and every other path out of this function.
    }

    /// For a test: the probe child this process still owes a
    /// reap for, if any.
    #[cfg(test)]
    pub(crate) fn unreaped_probe_child(&self) -> Option<libc::pid_t> {
        *self
            .unreaped_probe_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns an owned probe child whose wait failed to this
    /// process as a still-owed debt: the handle going back
    /// into the slot and the debt record appearing are ONE
    /// transition under both locks, so no starter can ever
    /// observe the debt without its handle (or the handle
    /// without the debt).
    fn return_owned_as_debt(&self, owned: OwnedProbeChild) {
        let pid = owned.pid;
        let mut record = self
            .unreaped_probe_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut slot = probe_child_slot(self);
        *slot = ProbeChildSlot::Owned(owned);
        *record = Some(pid);
        #[cfg(test)]
        if self.inject_debt_return_pause.load(Ordering::Relaxed) {
            self.debt_return_entered.store(true, Ordering::Relaxed);
            // Held (inside both locks) until the test's
            // sampler has looked and releases it: a handshake,
            // not a timed window.
            while !self.debt_return_release.load(Ordering::Relaxed) {
                std::thread::yield_now();
            }
        }
        drop(slot);
        drop(record);
        eprintln!(
            "manifest-plan: probe child {pid} is still unreaped; \
             this process keeps owing its reap and retries it at the next start"
        );
    }

    /// For a test: release a debt return held at its pause.
    #[cfg(test)]
    pub(crate) fn release_debt_return(&self) {
        self.debt_return_release.store(true, Ordering::Relaxed);
    }

    /// For a test: a snapshot of (debt recorded, slot handed
    /// off) only if both locks are free right now; None while
    /// a transition holds them, when no observation of a
    /// half-return is possible at all.
    #[cfg(test)]
    pub(crate) fn debt_snapshot_try(&self) -> Option<(bool, bool)> {
        let record = self.unreaped_probe_child.try_lock().ok()?;
        let slot = self.probe_child.try_lock().ok()?;
        Some((record.is_some(), matches!(*slot, ProbeChildSlot::HandedOff)))
    }

    /// For a test: whether the debt return has reached its
    /// injected pause.
    #[cfg(test)]
    pub(crate) fn debt_return_entered(&self) -> bool {
        self.debt_return_entered.load(Ordering::Relaxed)
    }

    /// For a test: a consistent snapshot of (debt recorded,
    /// slot handed off), taken under both locks in the same
    /// order the debt return takes them.
    #[cfg(test)]
    pub(crate) fn debt_snapshot(&self) -> (bool, bool) {
        let record = self
            .unreaped_probe_child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = probe_child_slot(self);
        (record.is_some(), matches!(*slot, ProbeChildSlot::HandedOff))
    }

    /// The startup probe, run once per process: create one
    /// dedicated probe cgroup (through `create_probe_cgroup`, the
    /// caller's one chance to make it), start one trivial program
    /// in it, and watch how it dies. Creation, the probe, the
    /// unusable decision, cleanup and publication all happen
    /// inside this one initialization: concurrent callers wait
    /// for the same completed decision and observe the same
    /// record. Some kernels kill a CLONE_INTO_CGROUP child with
    /// SIGKILL before it can run when this process's cgroup has
    /// been killed before (its kill sequence differs from the
    /// fresh cgroup's); on such a kernel every program started
    /// here would die the same way, so clone3 is remembered
    /// unusable for the whole process and every start takes the
    /// fork path instead. Any other outcome (the child ran, or
    /// clone3 itself was refused, which `start` classifies on
    /// the real call) leaves clone3 in use.
    ///
    /// The initialization runs inside this one `get_or_init`,
    /// on the calling thread, and no caller proceeds past it
    /// without the published decision. It forks nothing: the
    /// probe child is a direct clone3, so no fork lands in any
    /// thread's interval (an invocation's start is measured to
    /// fork this process zero times).
    pub(crate) fn probe_cgroup_clone_once(
        self: &std::sync::Arc<Self>,
        create_probe_cgroup: impl FnOnce() -> Option<(RawFd, Box<dyn std::any::Any + Send>)>,
    ) {
        self.initialize_cgroup_probe(create_probe_cgroup);
    }

    /// The one initialization [`Clone3Support::probe_cgroup_clone_once`]
    /// runs, on whichever thread first reaches it.
    fn initialize_cgroup_probe(
        self: &std::sync::Arc<Self>,
        create_probe_cgroup: impl FnOnce() -> Option<(RawFd, Box<dyn std::any::Any + Send>)>,
    ) {
        self.cgroup_probe.get_or_init(|| {
            let Some((cgroup, guard)) = create_probe_cgroup() else {
                return CgroupProbeRecord {
                    outcome: "unavailable".to_string(),
                    kernel: kernel_release(),
                    reason: None,
                    child_wait: "none".to_string(),
                };
            };
            let outcome = cgroup_probe_outcome(std::sync::Arc::clone(self), cgroup);
            drop(guard);
            let record = CgroupProbeRecord {
                outcome: outcome.name().to_string(),
                kernel: kernel_release(),
                child_wait: if self.probe_child_pid.load(Ordering::Relaxed) <= 0 {
                    "none".to_string()
                } else if self.probe_child_waited_via_pidfd.load(Ordering::Relaxed) {
                    "pidfd".to_string()
                } else {
                    "pid".to_string()
                },
                reason: match outcome {
                    CgroupProbeOutcome::ChildKilledBySigkill => Some(format!(
                        "clone3-into-cgroup refused by kernel: the probe child was killed \
                         by SIGKILL before it could run (kernel {})",
                        kernel_release()
                    )),
                    CgroupProbeOutcome::TimedOut => Some(format!(
                        "clone3-into-cgroup probe timed out after {:?} (kernel {}); \
                         starting with fork() instead",
                        self.probe_bound,
                        kernel_release()
                    )),
                    _ => None,
                },
            };
            if let Some(reason) = &record.reason {
                remember_unusable(self, reason);
            }
            record
        });
    }

    /// The startup probe's record, once it has run.
    pub(crate) fn cgroup_probe_record(&self) -> Option<CgroupProbeRecord> {
        self.cgroup_probe.get().cloned()
    }
}

/// The startup probe's outcome, recorded once per process in its
/// own run-level output.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct CgroupProbeRecord {
    /// `usable`, `killed_by_sigkill`, `no_verdict`, `timed_out`,
    /// `wait_failed`, or `unavailable` (no probe cgroup could be
    /// created).
    pub outcome: String,
    /// The kernel release the probe ran on (`uname`).
    pub kernel: String,
    /// Why clone3 was refused, when it was.
    pub reason: Option<String>,
    /// How the probe child was waited for and signalled:
    /// `pidfd` when its clone returned a pidfd, `pid` on the
    /// fallback path, or `none` when no child was created.
    pub child_wait: String,
}

/// How the startup probe's child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CgroupProbeOutcome {
    /// The child started in the cgroup and ran.
    ChildRan,
    /// The kernel killed the child with SIGKILL before it could run.
    ChildKilledBySigkill,
    /// No verdict: clone3 was refused, or the child could not start
    /// for another reason. Not a kernel verdict either way.
    NoVerdict,
    /// The probe did not finish inside its bound; its child was
    /// killed to reap it. Not the kernel bug: the bound, not a
    /// wait status, decided.
    TimedOut,
    /// Waiting for the probe child failed with an error other
    /// than ECHILD: nothing about the child is proven, and its
    /// reap stays owed. A failure of the wait itself, given its
    /// own label rather than folded into a verdict.
    WaitFailed,
}

impl CgroupProbeOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::ChildRan => "usable",
            Self::ChildKilledBySigkill => "killed_by_sigkill",
            Self::NoVerdict => "no_verdict",
            Self::TimedOut => "timed_out",
            Self::WaitFailed => "wait_failed",
        }
    }
}

/// This kernel's release, for the probe's refusal message.
fn kernel_release() -> String {
    // SAFETY: `utsname` is plain old data and zero is a valid value.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut name) } != 0 {
        return "unknown".to_string();
    }
    let release = name
        .release
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect::<Vec<u8>>();
    String::from_utf8_lossy(&release).into_owned()
}

/// Start a trivial program in `cgroup` as a direct
/// clone3(CLONE_INTO_CGROUP | CLONE_PIDFD) child and reap it.
/// The deciding measurement showed this plain form is killed
/// by SIGKILL in exactly the same runs as the real
/// CLONE_VM | CLONE_VFORK | CLONE_INTO_CGROUP invocation
/// form: the kill this probes for happens while the kernel
/// creates the child in the cgroup, before any child code
/// runs, whichever flags carry the cgroup.
/// Mark the probe child reaped by this process (or shown to
/// have no waitable child remaining), and close its pidfd.
/// Once marked, nothing may signal its pid again.
fn mark_probe_child_reaped(support: &Clone3Support) {
    let mut slot = probe_child_slot(support);
    let old = std::mem::replace(&mut *slot, ProbeChildSlot::Reaped);
    drop(slot);
    // The owned handle drops (and closes its pidfd) here,
    // after the reap that freed it.
    drop(old);
}

/// Takes the probe child's handle out for the background
/// reaper: a move. The slot then says handed off, and the
/// caller can neither wait nor signal the child any more.
fn take_probe_child(support: &Clone3Support) -> Option<OwnedProbeChild> {
    let mut slot = probe_child_slot(support);
    if !matches!(*slot, ProbeChildSlot::Owned(_)) {
        return None;
    }
    match std::mem::replace(&mut *slot, ProbeChildSlot::HandedOff) {
        ProbeChildSlot::Owned(owned) => Some(owned),
        _ => None,
    }
}

/// Returns an owned handle whose wait failed to the slot,
/// as a still-owed debt for the next start to retry.
fn return_probe_child(support: &Clone3Support, owned: OwnedProbeChild) {
    *probe_child_slot(support) = ProbeChildSlot::Owned(owned);
}

/// Marks the slot reaped after the background reaper
/// finished the child it owns.
fn mark_probe_child_reaped_from_reaper(support: &Clone3Support) {
    *probe_child_slot(support) = ProbeChildSlot::Reaped;
}

/// How one wait for the probe child ended. Only `Reaped`
/// and `NoWaitable` complete the cleanup obligation; the
/// pidfd this process holds from the child's clone names
/// the exact child, so no wait or signal here can reach a
/// different process that later reuses the numeric pid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildWait {
    /// The child was reaped, with this wait status.
    Reaped(i32),
    /// The child is still running.
    StillRunning,
    /// No waitable child remains (ECHILD); no status exists.
    NoWaitable,
    /// The wait failed with this errno, if it had one.
    Failed(Option<i32>),
}

/// The wait status a completed `waitid` reports, synthesized
/// in waitpid's encoding, or none when no child changed
/// state. Reading the siginfo fields is unsafe: the union
/// member is only meaningful for a completed wait.
fn reaped_status(info: &libc::siginfo_t) -> Option<i32> {
    unsafe {
        if info.si_pid() == 0 {
            return None;
        }
        Some(match info.si_code {
            libc::CLD_EXITED => info.si_status() << 8,
            libc::CLD_DUMPED => info.si_status() | 0x80,
            _ => info.si_status(),
        })
    }
}

/// Signal the probe child, through its pidfd when this
/// process holds one (the usual case), and by its numeric
/// pid only on the fallback path where no pidfd exists.
fn kill_probe_child(support: &Clone3Support, pid: libc::pid_t) -> i32 {
    // The whole check-and-signal holds the slot lock, and
    // only the caller's owned handle may be signalled: after
    // the move to the reaper (or after the reap) there is
    // nothing here to signal.
    let slot = probe_child_slot(support);
    let ProbeChildSlot::Owned(owned) = &*slot else {
        return -libc::ESRCH;
    };
    if let (true, Some(pidfd)) = (owned.via_pidfd, &owned.pidfd) {
        // SAFETY: signalling this exact child by its pidfd,
        // held open under the lock.
        return unsafe {
            syscall6(
                libc::SYS_pidfd_send_signal,
                [pidfd.raw() as usize, libc::SIGKILL as usize, 0, 0, 0, 0],
            )
        } as i32;
    }
    let _ = pid;
    // SAFETY: the numeric pid path, used only when the clone
    // refused CLONE_PIDFD (no pidfd was ever obtained); the
    // child is still owned and unreaped, under the same lock
    // as every wait and reap marking.
    unsafe { libc::kill(owned.pid, libc::SIGKILL) }
}

/// The injected wait failure a test seam returns, if any:
/// factored out so the entry gate can be tested directly,
/// without creating or signalling any child. `after_entry`
/// fixtures fire only once `witnessed_entry` is set, and do
/// not consume their failure before then.
#[cfg(test)]
fn injected_wait_failure(
    inject: Option<i32>,
    after_entry: bool,
    witnessed_entry: bool,
    remaining: &std::sync::atomic::AtomicUsize,
) -> Option<i32> {
    let errno = inject?;
    if after_entry && !witnessed_entry {
        return None;
    }
    remaining
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
            left.checked_sub(1)
        })
        .ok()
        .map(|_| errno)
}

/// How an injected wait failure classifies: an injected
/// ECHILD is no-waitable, exactly as a real one; any other
/// errno is a failed wait.
#[cfg(test)]
fn injected_wait_result(errno: i32) -> ChildWait {
    if errno == libc::ECHILD {
        ChildWait::NoWaitable
    } else {
        ChildWait::Failed(Some(errno))
    }
}

/// One wait for the probe child, by pidfd when held and by
/// pid otherwise. In tests the main loop's first waits, and
/// the reap grace's waits, can be made to fail with injected
/// errnos; an injected main-loop error fires only when
/// `witnessed_entry` is set if the fixture asked for that
/// handshake, so it cannot be fabricated for a child that
/// has not entered yet.
fn wait_probe_child(
    support: &Clone3Support,
    pid: libc::pid_t,
    grace: bool,
    witnessed_entry: bool,
) -> ChildWait {
    #[cfg(test)]
    {
        let injected = if grace {
            support.inject_probe_grace_wait_errno
        } else {
            injected_wait_failure(
                support.inject_probe_wait_errno,
                support.inject_probe_wait_after_entry,
                witnessed_entry,
                &support.inject_probe_wait_remaining,
            )
        };
        if let Some(errno) = injected {
            return injected_wait_result(errno);
        }
    }
    let _ = (grace, witnessed_entry);
    let _ = pid;
    let mut slot = probe_child_slot(support);
    let ProbeChildSlot::Owned(owned) = &*slot else {
        return ChildWait::NoWaitable;
    };
    let waited = wait_child_once(owned, false);
    if matches!(waited, ChildWait::Reaped(_) | ChildWait::NoWaitable) {
        let old = std::mem::replace(&mut *slot, ProbeChildSlot::Reaped);
        drop(slot);
        drop(old);
    }
    waited
}

/// One wait on an owned probe child handle: through its
/// pidfd when it has one, and by its numeric pid only on
/// the clone path that refused CLONE_PIDFD. The caller (or
/// the reaper, after the move) is the handle's only owner.
fn wait_child_once(owned: &OwnedProbeChild, blocking: bool) -> ChildWait {
    if let (true, Some(pidfd)) = (owned.via_pidfd, &owned.pidfd) {
        // SAFETY: `info` is plain old data and zero is valid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = if blocking {
            libc::WEXITED
        } else {
            libc::WEXITED | libc::WNOHANG
        };
        let waited =
            unsafe { libc::waitid(libc::P_PIDFD, pidfd.raw() as libc::id_t, &mut info, flags) };
        if waited == 0 {
            return match reaped_status(&info) {
                Some(status) => ChildWait::Reaped(status),
                None => {
                    if blocking {
                        ChildWait::NoWaitable
                    } else {
                        ChildWait::StillRunning
                    }
                }
            };
        }
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::ECHILD) => ChildWait::NoWaitable,
            errno => ChildWait::Failed(errno),
        };
    }
    let mut status = 0;
    let waited = unsafe {
        libc::waitpid(
            owned.pid,
            &mut status,
            if blocking { 0 } else { libc::WNOHANG },
        )
    };
    if waited == owned.pid {
        return ChildWait::Reaped(status);
    }
    if waited == 0 {
        return ChildWait::StillRunning;
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ECHILD) => ChildWait::NoWaitable,
        errno => ChildWait::Failed(errno),
    }
}

/// Read the entry byte if it is already in the pipe, without
/// blocking. Used after the child is known to have exited:
/// the child may have written and died before the parent's
/// last poll, and the byte, not the timing, decides.
fn drain_entry_pipe(pipe: RawFd, entered: bool) -> bool {
    if entered {
        return true;
    }
    let mut poll = libc::pollfd {
        fd: pipe,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, 0) } > 0 {
        let mut byte = [0u8; 1];
        if unsafe { libc::read(pipe, byte.as_mut_ptr().cast(), 1) } == 1 {
            return true;
        }
    }
    false
}

fn cgroup_probe_outcome(
    support: std::sync::Arc<Clone3Support>,
    cgroup: RawFd,
) -> CgroupProbeOutcome {
    #[cfg(test)]
    {
        support
            .cgroup_probe_attempts
            .fetch_add(1, Ordering::Relaxed);
        if support.inject_cgroup_probe_kill {
            return CgroupProbeOutcome::ChildKilledBySigkill;
        }
    }
    // Everything the child needs is prepared before the clone:
    // the entry pipe, and /bin/true's path and argv/envp arrays.
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return CgroupProbeOutcome::NoVerdict;
    }
    let path = match std::ffi::CString::new("/bin/true") {
        Ok(path) => path,
        Err(_) => return CgroupProbeOutcome::NoVerdict,
    };
    let argv = [path.as_ptr(), std::ptr::null::<libc::c_char>()];
    let envp = [std::ptr::null::<libc::c_char>()];
    // Ask for a pidfd atomically at creation (CLONE_PIDFD):
    // only this parent uses the fd after clone3 returns, so
    // the kernel's put_user/fd_install ordering inside clone
    // cannot race its use here. On a kernel that refuses
    // CLONE_PIDFD, the clone below is retried without it and
    // the numeric pid path is used instead.
    let mut pidfd_slot: i32 = -1;
    let mut args = CloneArgs {
        flags: CLONE_INTO_CGROUP | CLONE_PIDFD,
        pidfd: &mut pidfd_slot as *mut i32 as usize as u64,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: cgroup as u64,
    };
    let mut result = unsafe {
        syscall6(
            libc::SYS_clone3,
            [
                &args as *const CloneArgs as usize,
                core::mem::size_of::<CloneArgs>(),
                0,
                0,
                0,
                0,
            ],
        )
    };
    if result < 0 && pidfd_slot < 0 {
        // Possibly a kernel without CLONE_PIDFD: retry once
        // without it. A failed clone created no child, so the
        // retry cannot create a second one.
        args.flags = CLONE_INTO_CGROUP;
        args.pidfd = 0;
        result = unsafe {
            syscall6(
                libc::SYS_clone3,
                [
                    &args as *const CloneArgs as usize,
                    core::mem::size_of::<CloneArgs>(),
                    0,
                    0,
                    0,
                    0,
                ],
            )
        };
    }
    if result == 0 {
        // The probe child. Raw system calls only, until execve:
        // one entry byte on the pipe, then execve. Everything it
        // touches was prepared before the clone.
        unsafe {
            syscall6(libc::SYS_close, [pipe_fds[0] as usize, 0, 0, 0, 0, 0]);
            #[cfg(test)]
            if support.inject_probe_child_hangs_before_entry {
                loop {
                    syscall6(libc::SYS_pause, [0; 6]);
                }
            }
            let byte = *b"E";
            syscall6(
                libc::SYS_write,
                [
                    pipe_fds[1] as usize,
                    &byte as *const u8 as usize,
                    1,
                    0,
                    0,
                    0,
                ],
            );
            #[cfg(test)]
            if support.inject_probe_child_kills_after_entry {
                // Outside the pidfd rule by the design's own
                // ruling: a process signalling itself (tgkill
                // on its own thread) cannot hit a reused pid.
                let pid = syscall6(libc::SYS_getpid, [0; 6]);
                syscall6(
                    libc::SYS_tgkill,
                    [pid as usize, pid as usize, libc::SIGKILL as usize, 0, 0, 0],
                );
            }
            #[cfg(test)]
            if support.inject_probe_child_hangs_after_entry {
                loop {
                    syscall6(libc::SYS_pause, [0; 6]);
                }
            }
            syscall6(
                libc::SYS_execve,
                [
                    path.as_ptr() as usize,
                    argv.as_ptr() as usize,
                    envp.as_ptr() as usize,
                    0,
                    0,
                    0,
                ],
            );
            loop {
                syscall6(libc::SYS_exit_group, [127, 0, 0, 0, 0, 0]);
            }
        }
    }
    unsafe {
        libc::close(pipe_fds[1]);
    }
    if result < 0 {
        unsafe {
            libc::close(pipe_fds[0]);
        }
        return CgroupProbeOutcome::NoVerdict;
    }
    let pid = result as libc::pid_t;
    support.probe_child_pid.store(pid, Ordering::Relaxed);
    {
        *probe_child_slot(&support) = ProbeChildSlot::Owned(OwnedProbeChild {
            pidfd: if pidfd_slot >= 0 {
                Some(OwnedFd(pidfd_slot))
            } else {
                None
            },
            via_pidfd: pidfd_slot >= 0,
            pid,
        });
        support
            .probe_child_waited_via_pidfd
            .store(pidfd_slot >= 0, Ordering::Relaxed);
        #[cfg(test)]
        if pidfd_slot >= 0 {
            fn fd_ino(fd: i32) -> (u64, u64) {
                let mut stat: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(fd, &mut stat) } == 0 {
                    (stat.st_dev, stat.st_ino)
                } else {
                    panic!(
                        "cannot stat the probe child's pidfd for the fd census: {}",
                        io::Error::last_os_error()
                    );
                }
            }
            let dup = unsafe { libc::fcntl(pidfd_slot, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                panic!(
                    "cannot duplicate the probe child's pidfd for test cleanup: {}",
                    io::Error::last_os_error()
                );
            }
            *support
                .probe_child_cleanup_pidfd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(OwnedFd(dup));
            *support
                .probe_child_fd_inodes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                vec![fd_ino(pidfd_slot), fd_ino(dup)];
            // Attribution check: a pidfd for a different
            // process (ourselves) must have a different
            // (dev, inode). On pre-pidfs kernels every pidfd
            // shares one anon inode, and the census in the
            // tests refuses by name instead of miscounting.
            let self_pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0usize) };
            if self_pidfd < 0 {
                // The attribution check itself could not run.
                // That is a fixture failure of its own, NOT
                // evidence of a shared inode: never let it
                // stand in for pidfs absence.
                panic!(
                    "cannot open a pidfd for this process to check the fd census attribution: {}",
                    io::Error::last_os_error()
                );
            }
            let shared = fd_ino(self_pidfd as i32) == fd_ino(pidfd_slot);
            support
                .probe_child_fd_census_shared
                .store(shared, Ordering::Relaxed);
            unsafe {
                libc::close(self_pidfd as i32);
            }
        }
    }
    // Poll the entry pipe and the child to the bound. The child is
    // this process's own: nobody else can reap it, so its pid
    // stays valid until this loop does.
    let started = std::time::Instant::now();
    let mut entered = false;
    let outcome = loop {
        if !entered {
            let remaining = support.probe_bound.saturating_sub(started.elapsed());
            let mut poll = libc::pollfd {
                fd: pipe_fds[0],
                events: libc::POLLIN,
                revents: 0,
            };
            // A test seam holds the read back until after the
            // reap below, so the drain there is the only path by
            // which the byte can be seen.
            #[cfg(test)]
            let ready = if support.inject_probe_parent_read_delay {
                std::thread::sleep(std::time::Duration::from_millis(2));
                0
            } else {
                unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(5) as i32) }
            };
            #[cfg(not(test))]
            let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(5) as i32) };
            if ready > 0 {
                let mut byte = [0u8; 1];
                let read = unsafe { libc::read(pipe_fds[0], byte.as_mut_ptr().cast(), 1) };
                if read == 1 {
                    entered = true;
                }
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        match wait_probe_child(&support, pid, false, entered) {
            ChildWait::Reaped(status) => {
                // The child may have written its byte and died
                // before this loop read the pipe (a poll that
                // timed out or was interrupted): drain what is
                // already there before classifying, or a child
                // that entered reads as one the kernel killed
                // before it could run.
                entered = drain_entry_pipe(pipe_fds[0], entered);
                mark_probe_child_reaped(&support);
                break probe_verdict(entered, status);
            }
            ChildWait::NoWaitable => {
                // No waitable child remains (a SIGCHLD
                // disposition of SIG_IGN reaps children itself):
                // the cleanup this loop owns is complete, but
                // ECHILD carries no wait status, so no successful
                // run can be claimed from it. The witness,
                // drained as after any exit, is the only
                // evidence the child ran.
                entered = drain_entry_pipe(pipe_fds[0], entered);
                mark_probe_child_reaped(&support);
                break if entered {
                    CgroupProbeOutcome::ChildRan
                } else {
                    CgroupProbeOutcome::NoVerdict
                };
            }
            ChildWait::Failed(errno) => {
                if errno != Some(libc::EINTR) {
                    // A wait that failed another way proves
                    // nothing about the child: it is still this
                    // process's own, so keep owing its reap for
                    // the next start to retry, and take no kernel
                    // verdict from it.
                    eprintln!(
                        "manifest-plan: waiting for the clone3 probe child {pid} failed \
                         (errno {errno:?}); no verdict is taken from it"
                    );
                    support.keep_unreaped_probe_child(pid);
                    break CgroupProbeOutcome::WaitFailed;
                }
            }
            ChildWait::StillRunning => {}
        }
        if started.elapsed() >= support.probe_bound {
            // Kill and reap, both bounded: the kill's result is
            // checked (the child may already be dead), the reap
            // polls for a one-second grace, and a child still not
            // reaped then (stuck in the kernel, say) is handed to
            // a background reaper while the timeout is published.
            // It stays this process's child throughout, so nobody
            // else can reap it and its pid cannot be reused first.
            #[cfg(test)]
            let killed = if let Some(errno) = support.inject_probe_timeout_kill_errno {
                -errno as isize
            } else {
                kill_probe_child(&support, pid) as isize
            };
            #[cfg(not(test))]
            let killed = kill_probe_child(&support, pid) as isize;
            // Reaped by this loop, or no waitable child remains:
            // either completes the cleanup. Any other wait error
            // completes nothing and hands the child on, below.
            let mut reaped = false;
            let mut no_waitable = false;
            let grace = std::time::Instant::now();
            while grace.elapsed() < std::time::Duration::from_secs(1) {
                match wait_probe_child(&support, pid, true, entered) {
                    ChildWait::Reaped(_) => {
                        reaped = true;
                        break;
                    }
                    ChildWait::NoWaitable => {
                        no_waitable = true;
                        break;
                    }
                    ChildWait::Failed(Some(libc::EINTR)) => continue,
                    ChildWait::Failed(_) => break,
                    ChildWait::StillRunning => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if reaped || no_waitable {
                mark_probe_child_reaped(&support);
            }
            if killed != 0 && !reaped && !no_waitable {
                // The kill failed (the child was already dead, or
                // a policy refuses kill): ownership passes to the
                // reaper or the retry record below. The timeout
                // stands either way.
                eprintln!(
                    "manifest-plan: clone3 probe kill of child {pid} failed; \
                     its cleanup is handed on"
                );
            }
            if !reaped && !no_waitable {
                // Hand the owned handle to the reaper over a
                // channel: a move. On a successful spawn the
                // caller sends it and keeps no copy; on a
                // failed spawn (or a failed send) the caller
                // still owns it and runs the fallback below.
                #[cfg(test)]
                let spawn_fails = support.inject_probe_reaper_spawn_fails;
                #[cfg(not(test))]
                let spawn_fails = false;
                let mut spawn_error: Option<io::Error> = if spawn_fails {
                    Some(io::Error::from_raw_os_error(libc::EAGAIN))
                } else {
                    None
                };
                let mut handed_off = false;
                if !spawn_fails {
                    match take_probe_child(&support) {
                        Some(owned) => {
                            let (sender, receiver) = std::sync::mpsc::channel::<OwnedProbeChild>();
                            let rs = std::sync::Arc::clone(&support);
                            match std::thread::Builder::new()
                                .name("clone3-probe-reaper".to_string())
                                .spawn(move || {
                                    if let Ok(owned) = receiver.recv() {
                                        reap_owned(owned, &rs);
                                    }
                                }) {
                                Ok(_handle) => match sender.send(owned) {
                                    Ok(()) => handed_off = true,
                                    Err(error) => {
                                        return_probe_child(&support, error.0);
                                        spawn_error =
                                            Some(io::Error::from_raw_os_error(libc::EAGAIN));
                                    }
                                },
                                Err(error) => {
                                    // The spawn failed before the
                                    // send: the caller still owns
                                    // the handle. Put it back and
                                    // run the fallback below.
                                    return_probe_child(&support, owned);
                                    spawn_error = Some(error);
                                }
                            }
                        }
                        None => {
                            spawn_error = Some(io::Error::from_raw_os_error(libc::EAGAIN));
                        }
                    }
                }
                let spawned: Result<(), io::Error> = if handed_off {
                    Ok(())
                } else {
                    Err(spawn_error.unwrap_or_else(|| io::Error::from_raw_os_error(libc::EAGAIN)))
                };
                if let Err(error) = spawned {
                    // No reaper could be started: this caller
                    // stays the owner, but it may not wait
                    // unboundedly for the child. It kills the
                    // child (by pidfd where held) and polls for
                    // one grace; a child still unreaped then is
                    // recorded loudly as owed, and no verdict is
                    // taken from its cleanup.
                    eprintln!(
                        "manifest-plan: cannot spawn the clone3 probe reaper ({error}); \
                         killing the probe child and polling its reap here instead"
                    );
                    #[cfg(test)]
                    let fallback_killed =
                        if let Some(errno) = support.inject_probe_fallback_kill_errno {
                            -errno as isize
                        } else {
                            kill_probe_child(&support, pid) as isize
                        };
                    #[cfg(not(test))]
                    let fallback_killed = kill_probe_child(&support, pid) as isize;
                    if fallback_killed != 0 {
                        eprintln!(
                            "manifest-plan: killing probe child {pid} after the failed \
                             reaper spawn also failed (errno {fallback_killed})"
                        );
                    }
                    let fallback_grace = std::time::Instant::now();
                    let mut fallback_done = false;
                    while fallback_grace.elapsed() < std::time::Duration::from_secs(1) {
                        match wait_probe_child(&support, pid, false, entered) {
                            ChildWait::Reaped(_) | ChildWait::NoWaitable => {
                                fallback_done = true;
                                break;
                            }
                            ChildWait::Failed(Some(libc::EINTR)) => continue,
                            ChildWait::Failed(_) => break,
                            ChildWait::StillRunning => {}
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    if fallback_done {
                        mark_probe_child_reaped(&support);
                    } else {
                        support.keep_unreaped_probe_child(pid);
                        break CgroupProbeOutcome::NoVerdict;
                    }
                }
            }
            break CgroupProbeOutcome::TimedOut;
        }
    };
    unsafe {
        libc::close(pipe_fds[0]);
    }
    #[cfg(test)]
    if !support.injected_wait_consumed() {
        // No injected wait answered, so no fixture cleanup
        // needs the duplicate: take and drop it here, so a
        // real probe leaves no extra fd behind.
        let _dup = support
            .probe_child_cleanup_pidfd
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
    outcome
}

/// Reap `pid`, retrying a wait that a signal interrupts. A
/// wait that fails another way does not complete the cleanup:
/// the child is recorded as still owed, for the next start to
/// retry, rather than silently dropped.
/// Reap the probe child this reaper owns, retrying a wait
/// that a signal interrupts. It is the handle's only owner
/// after the move. A wait that fails another way returns
/// the owned handle to the slot as a still-owed debt, for
/// the next start to retry, rather than silently dropping
/// it.
fn reap_owned(owned: OwnedProbeChild, support: &Clone3Support) {
    loop {
        match wait_child_once(&owned, true) {
            ChildWait::Reaped(_) | ChildWait::NoWaitable => {
                mark_probe_child_reaped_from_reaper(support);
                return;
            }
            ChildWait::Failed(Some(libc::EINTR)) => continue,
            ChildWait::Failed(_) => {
                support.return_owned_as_debt(owned);
                return;
            }
            ChildWait::StillRunning => continue,
        }
    }
}

/// The probe's verdict from the child's entry witness and its wait
/// status. The kernel bug kills the child while the kernel creates
/// it, before it enters the child code, so only a SIGKILL with no
/// entry is that refusal; a child that entered and was then killed
/// (for example by its own program, after exec) is a child that
/// ran, and clone3 stays usable.
fn probe_verdict(entered: bool, status: i32) -> CgroupProbeOutcome {
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        CgroupProbeOutcome::ChildRan
    } else if !entered && libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL {
        CgroupProbeOutcome::ChildKilledBySigkill
    } else if entered && libc::WIFSIGNALED(status) {
        // Entered, then killed (for example by its own program):
        // the clone path works, so this is a child that ran.
        CgroupProbeOutcome::ChildRan
    } else {
        CgroupProbeOutcome::NoVerdict
    }
}

/// This process's [`Clone3Support`].
pub(crate) static CLONE3: std::sync::LazyLock<std::sync::Arc<Clone3Support>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(Clone3Support::new()));

/// What an errno from clone3 means. The cgroup errnos are those of Linux
/// v6.12's `cgroup_css_set_fork` (kernel/cgroup/cgroup.c) and what it calls;
/// the others are clone3's own argument checks (kernel/fork.c).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// clone3, or one of its flags, is not available here: an older kernel
    /// (ENOSYS before 5.3; E2BIG or EINVAL for arguments or a flag it does not
    /// know, before 5.7) or a seccomp filter (ENOSYS).
    Unusable,
    /// Either a seccomp filter refused clone3 or the cgroup's write
    /// permission check did (`inode_permission`, for an immutable file or a
    /// security module); [`probe_refuses_clone3`] tells which.
    FilterOrJoin,
    /// The cgroup refused the child: not a cgroup directory (EBADF), not
    /// allowed (EACCES, ENOENT), on a read-only mount (EROFS), removed
    /// (ENODEV), a cgroup that may not hold processes (EBUSY, EOPNOTSUPP).
    Join,
    /// Anything else, such as EAGAIN or ENOMEM: no child was created.
    Failed,
}

fn refusal(errno: i32) -> Refusal {
    match errno {
        libc::ENOSYS | libc::E2BIG | libc::EINVAL => Refusal::Unusable,
        libc::EPERM => Refusal::FilterOrJoin,
        libc::EBADF
        | libc::EACCES
        | libc::ENOENT
        | libc::EROFS
        | libc::ENODEV
        | libc::EBUSY
        | libc::EOPNOTSUPP => Refusal::Join,
        _ => Refusal::Failed,
    }
}

/// Whether clone3 itself is refused here, asked by a call that cannot create
/// a child: clone3 with no arguments, which Linux refuses with EINVAL before
/// it reads anything (`copy_clone_args_from_user`: a size below
/// `CLONE_ARGS_SIZE_VER0`), unless a seccomp filter refuses the call first
/// (EPERM or ENOSYS).
fn probe_refuses_clone3(support: &Clone3Support) -> bool {
    matches!(probe_clone3(support), libc::EPERM | libc::ENOSYS)
}

/// The errno of clone3 with no arguments: EINVAL where clone3 is allowed.
fn probe_clone3(support: &Clone3Support) -> i32 {
    #[cfg(test)]
    {
        support.probes.fetch_add(1, Ordering::Relaxed);
        if let Some(errno) = support.inject_probe {
            return errno;
        }
    }
    #[cfg(not(test))]
    let _ = support;
    let result = unsafe { syscall6(libc::SYS_clone3, [0, 0, 0, 0, 0, 0]) };
    -result as i32
}

/// Remembers that clone3 is unusable here for `reason`, and says so on
/// stderr the first time: every later start in this process forks.
fn remember_unusable(support: &Clone3Support, reason: &str) {
    if support.unusable.set(reason.to_string()).is_ok() {
        #[cfg(test)]
        support.notices.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "manifest-plan: {reason}; invocations in a cgroup start with fork() from now on (dh-7n47nt)"
        );
    }
}

/// Start `program` in its cgroup, unless clone3 is unusable here.
pub(crate) fn start(support: &Clone3Support, program: &Program) -> Started {
    // A probe child whose cleanup could not complete is still
    // this process's own; retry its reap here, where every real
    // start passes, without blocking.
    support.reap_unreaped_probe_child();
    if let Some(reason) = support.unusable.get() {
        return Started::Unusable {
            reason: reason.clone(),
        };
    }
    let prepared = match Prepared::new(support, program) {
        Ok(prepared) => prepared,
        Err(error) => return Started::Failed(error),
    };
    let result = CHILD_STACK.with_borrow_mut(|stack| {
        if stack.is_none() {
            *stack = Some(ChildStack::new()?);
        }
        let stack = stack.as_ref().expect("set above");
        Ok::<i64, io::Error>(clone_into_cgroup(support, &prepared, stack, program.cgroup))
    });
    let result = match result {
        Ok(result) => result,
        Err(error) => return Started::Failed(error),
    };
    if result < 0 {
        let errno = (-result) as i32;
        let error = io::Error::from_raw_os_error(errno);
        let unusable = |error: io::Error| {
            let reason = format!("clone3 with CLONE_INTO_CGROUP was refused: {error}");
            remember_unusable(support, &reason);
            Started::Unusable { reason }
        };
        return match refusal(errno) {
            Refusal::Unusable => unusable(error),
            Refusal::FilterOrJoin if probe_refuses_clone3(support) => unusable(error),
            Refusal::FilterOrJoin | Refusal::Join => Started::JoinFailed(error),
            Refusal::Failed => Started::Failed(error),
        };
    }
    let pid = result as u32;
    // CLONE_VFORK returned only after the child execed or exited, and the
    // kernel's completion of that wait orders the child's store before this
    // load. A child killed by a signal before it execed stored nothing, so it
    // reads as a started program that a signal killed, which its wait
    // reports.
    let failure = prepared.state.failure.load(Ordering::Relaxed);
    if failure != 0 {
        // The child stored why it could not exec and exited; reap it, retrying
        // a wait that a signal interrupts so no zombie is left.
        let mut status = 0;
        while unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } == -1
            && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        return Started::Failed(io::Error::from_raw_os_error(failure));
    }
    Started::Running { pid }
}

/// `clone_args` as Linux 5.7 defines it (`CLONE_ARGS_SIZE_VER2`).
#[repr(C)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// Calls clone3 for `prepared` on `stack`, with every signal blocked on this
/// thread so that no handler of this process can run in the child before it
/// resets them. Returns the child's PID or the negative errno.
fn clone_into_cgroup(
    support: &Clone3Support,
    prepared: &Prepared,
    stack: &ChildStack,
    cgroup: RawFd,
) -> i64 {
    #[cfg(test)]
    {
        let attempt = support.attempts.fetch_add(1, Ordering::Relaxed);
        if let Some(errno) = support.inject {
            if attempt < support.inject_limit {
                return -i64::from(errno);
            }
        }
    }
    #[cfg(not(test))]
    let _ = support;
    let args = CloneArgs {
        flags: (libc::CLONE_VM | libc::CLONE_VFORK) as u64 | CLONE_INTO_CGROUP,
        pidfd: 0,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: stack.low as u64,
        stack_size: CHILD_STACK_BYTES as u64,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: cgroup as u64,
    };
    // Block every signal, glibc's internal ones too, which pthread_sigmask
    // leaves unblocked; glibc's posix_spawn blocks them the same way.
    let all: u64 = !0;
    let mut previous: u64 = 0;
    unsafe {
        syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &all as *const u64 as usize,
                &mut previous as *mut u64 as usize,
                8,
                0,
                0,
            ],
        );
    }
    let result = unsafe { arch::clone3_then_child(&args, &prepared.state) };
    unsafe {
        syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &previous as *const u64 as usize,
                0,
                8,
                0,
                0,
            ],
        );
    }
    result
}

/// `CLONE_INTO_CGROUP` (Linux 5.7), not in every libc crate release.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;
/// `CLONE_PIDFD` (Linux 5.2): the clone writes a pidfd for
/// the new child into `clone_args.pidfd`.
const CLONE_PIDFD: u64 = 0x1000;

/// The child's stack: no more than raw system calls run on it. Its frames
/// stay far under 1 KiB even unoptimized, and execve runs on the kernel's
/// stack, so 64 KiB leaves more than 64 times what it needs. A guard page
/// below it turns an overflow into the child's own SIGSEGV, never a write
/// into this process's memory; [`start`] then reports a started program that
/// SIGSEGV killed, as for any child a signal kills before it execs.
const CHILD_STACK_BYTES: usize = 64 * 1024;

struct ChildStack {
    /// The mapping: the guard page, then the stack.
    base: *mut libc::c_void,
    len: usize,
    /// The stack's lowest address; it grows down from `low + CHILD_STACK_BYTES`.
    low: *mut u8,
}

impl ChildStack {
    fn new() -> io::Result<Self> {
        let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
            .map_err(|_| io::Error::other("cannot read the page size"))?;
        let len = page + CHILD_STACK_BYTES;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::mprotect(base, page, libc::PROT_NONE) } != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::munmap(base, len) };
            return Err(error);
        }
        Ok(Self {
            base,
            len,
            low: unsafe { base.cast::<u8>().add(page) },
        })
    }
}

impl Drop for ChildStack {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base, self.len) };
    }
}

thread_local! {
    /// One child stack per thread that starts programs: CLONE_VFORK keeps
    /// this thread waiting until its child has execed or exited, so a thread
    /// never has two children on it.
    static CHILD_STACK: RefCell<Option<ChildStack>> = const { RefCell::new(None) };
}

/// Everything the child reads, prepared before clone3, and what it writes.
struct Prepared {
    state: ChildState,
    // Owners of the storage `state` points into.
    _strings: Vec<CString>,
    _pointers: Vec<Vec<*const libc::c_char>>,
}

/// What [`vfork_child`] reads through one pointer. Every pointer names
/// storage in [`Prepared`], which outlives the child's use of it: this thread
/// waits in clone3 until the child has execed or exited.
#[repr(C)]
pub(crate) struct ChildState {
    /// Paths to try, in order, to execute the program.
    candidates: *const *const libc::c_char,
    candidate_count: usize,
    /// NULL-terminated argv and envp.
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
    /// `/bin/sh`, a slot for the path that failed with ENOEXEC, then argv
    /// from its second entry, NULL-terminated: how execvp runs a file without
    /// a `#!` line. The child writes the slot.
    shell_argv: *mut *const libc::c_char,
    cwd: *const libc::c_char,
    stdio: [RawFd; 3],
    /// The errno that stopped the child, or 0 if it reached the program.
    failure: AtomicI32,
    /// For a test: the child kills itself first.
    #[cfg(test)]
    kill_before_exec: bool,
}

impl Prepared {
    fn new(support: &Clone3Support, program: &Program) -> io::Result<Self> {
        #[cfg(not(test))]
        let _ = support;
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
        let c_string =
            |bytes: Vec<u8>| CString::new(bytes).map_err(|_| invalid("a NUL byte in an argument"));
        let mut strings = Vec::new();
        let mut pointers = Vec::new();
        let name = program
            .argv
            .first()
            .ok_or_else(|| invalid("an empty argv"))?;
        // The environment the program gets: this process's, with the
        // overrides set over it.
        let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for (key, value) in program.env_overrides {
            environment.insert(key.into(), value.into());
        }
        let candidates: Vec<Vec<u8>> = if name.contains('/') {
            vec![name.as_bytes().to_vec()]
        } else {
            // execvp's search: each element of PATH, an empty one meaning the
            // working directory, and glibc's default when PATH is unset.
            let path = environment
                .get(std::ffi::OsStr::new("PATH"))
                .map(|path| path.as_bytes().to_vec())
                .unwrap_or_else(|| b"/bin:/usr/bin".to_vec());
            path.split(|byte| *byte == b':')
                .map(|dir| {
                    if dir.is_empty() {
                        name.as_bytes().to_vec()
                    } else {
                        let mut full = dir.to_vec();
                        full.push(b'/');
                        full.extend_from_slice(name.as_bytes());
                        full
                    }
                })
                .collect()
        };
        let pointer_list = |items: Vec<Vec<u8>>, strings: &mut Vec<CString>| {
            let mut list: Vec<*const libc::c_char> = Vec::with_capacity(items.len() + 1);
            for item in items {
                let string = c_string(item)?;
                list.push(string.as_ptr());
                strings.push(string);
            }
            Ok::<_, io::Error>(list)
        };
        let candidate_list = pointer_list(candidates, &mut strings)?;
        let candidate_count = candidate_list.len();
        let mut argv = pointer_list(
            program
                .argv
                .iter()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            &mut strings,
        )?;
        argv.push(std::ptr::null());
        let mut envp = pointer_list(
            environment
                .into_iter()
                .map(|(key, value)| {
                    let mut pair = key.into_vec();
                    pair.push(b'=');
                    pair.extend_from_slice(value.as_bytes());
                    pair
                })
                .collect(),
            &mut strings,
        )?;
        envp.push(std::ptr::null());
        let mut shell_argv = pointer_list(vec![b"/bin/sh".to_vec()], &mut strings)?;
        shell_argv.push(std::ptr::null());
        shell_argv.extend_from_slice(&argv[1..]);
        let cwd = c_string(program.cwd.as_os_str().as_bytes().to_vec())?;
        let cwd_pointer = cwd.as_ptr();
        strings.push(cwd);
        let state = ChildState {
            candidates: candidate_list.as_ptr(),
            candidate_count,
            argv: argv.as_ptr(),
            envp: envp.as_ptr(),
            shell_argv: shell_argv.as_mut_ptr(),
            cwd: cwd_pointer,
            stdio: program.stdio,
            failure: AtomicI32::new(0),
            #[cfg(test)]
            kill_before_exec: support.kill_child_before_exec,
        };
        pointers.push(candidate_list);
        pointers.push(argv);
        pointers.push(envp);
        pointers.push(shell_argv);
        Ok(Self {
            state,
            _strings: strings,
            _pointers: pointers,
        })
    }
}

/// A raw system call. The child cannot call libc: it shares this process's
/// memory and its thread's TLS, where libc keeps errno and locks.
#[inline(always)]
unsafe fn syscall6(number: libc::c_long, args: [usize; 6]) -> isize {
    unsafe { arch::syscall6(number as usize, args) }
}

/// `struct sigaction` as the kernel takes it on x86_64 and aarch64.
#[repr(C)]
struct KernelSigaction {
    handler: usize,
    flags: u64,
    restorer: usize,
    mask: u64,
}

/// The child: set up the program's process state as `std::process::Command`
/// would, then execute it, or store why not and exit.
///
/// # Safety
/// Entered only from [`arch::clone3_then_child`], on the child's own stack,
/// with `state` prepared by [`Prepared::new`]. It never returns, allocates,
/// takes a lock, or calls libc.
unsafe extern "C" fn vfork_child(state: *const ChildState) -> ! {
    unsafe {
        let state = &*state;
        let fail = |errno: isize| -> ! {
            state.failure.store(errno as i32, Ordering::Relaxed);
            // exit_group does not return; the loop only gives the closure
            // its `!` type.
            loop {
                syscall6(libc::SYS_exit_group, [127, 0, 0, 0, 0, 0]);
            }
        };
        let check = |result: isize| {
            if result < 0 {
                fail(-result);
            }
        };
        #[cfg(test)]
        if state.kill_before_exec {
            // Outside the pidfd rule by the design's own
            // ruling: a process signalling itself (tgkill
            // on its own thread) cannot hit a reused pid.
            let pid = syscall6(libc::SYS_getpid, [0; 6]);
            syscall6(
                libc::SYS_tgkill,
                [pid as usize, pid as usize, libc::SIGKILL as usize, 0, 0, 0],
            );
        }
        // The program leads a process group of its own.
        check(syscall6(libc::SYS_setpgid, [0, 0, 0, 0, 0, 0]));
        // Its stdin, stdout and stderr. The caller passes descriptors above 2,
        // so no target is also a later source; dup3 leaves the copies open
        // across exec.
        let mut target = 0;
        while target < 3 {
            check(syscall6(
                libc::SYS_dup3,
                [state.stdio[target] as usize, target, 0, 0, 0, 0],
            ));
            target += 1;
        }
        check(syscall6(
            libc::SYS_chdir,
            [state.cwd as usize, 0, 0, 0, 0, 0],
        ));
        // Every signal this process handles goes back to its default action
        // before any is unblocked, so no handler of this process can run in
        // the child; SIGPIPE, which Rust programs ignore, too, as
        // `std::process::Command` resets it. exec would reset handled ones
        // anyway; ignored ones stay ignored, as with Command. clone3's
        // CLONE_CLEAR_SIGHAND would reset the handled ones but leaves SIG_IGN
        // alone (`flush_signal_handlers(tsk, 0)`), so it would not reset
        // SIGPIPE as Command does; hence this loop. The child has its own copy
        // of the handlers (no CLONE_SIGHAND), so none of this touches this
        // process's.
        let mut signal = 1;
        while signal < 65 {
            if signal != libc::SIGKILL as usize && signal != libc::SIGSTOP as usize {
                let mut action = KernelSigaction {
                    handler: 0,
                    flags: 0,
                    restorer: 0,
                    mask: 0,
                };
                let read = syscall6(
                    libc::SYS_rt_sigaction,
                    [
                        signal,
                        0,
                        &mut action as *mut KernelSigaction as usize,
                        8,
                        0,
                        0,
                    ],
                );
                let handled = action.handler != libc::SIG_DFL && action.handler != libc::SIG_IGN;
                if read == 0 && (handled || signal == libc::SIGPIPE as usize) {
                    let default = KernelSigaction {
                        handler: libc::SIG_DFL,
                        flags: 0,
                        restorer: 0,
                        mask: 0,
                    };
                    check(syscall6(
                        libc::SYS_rt_sigaction,
                        [
                            signal,
                            &default as *const KernelSigaction as usize,
                            0,
                            8,
                            0,
                            0,
                        ],
                    ));
                }
            }
            signal += 1;
        }
        let empty: u64 = 0;
        check(syscall6(
            libc::SYS_rt_sigprocmask,
            [
                libc::SIG_SETMASK as usize,
                &empty as *const u64 as usize,
                0,
                8,
                0,
                0,
            ],
        ));
        // execvp's search: stop at the first error other than one that means
        // "not here"; report EACCES if any candidate gave it.
        let mut denied = false;
        let mut last = libc::ENOENT as isize;
        let mut index = 0;
        while index < state.candidate_count {
            let path = *state.candidates.add(index);
            let mut errno = -syscall6(
                libc::SYS_execve,
                [
                    path as usize,
                    state.argv as usize,
                    state.envp as usize,
                    0,
                    0,
                    0,
                ],
            );
            if errno as i32 == libc::ENOEXEC {
                // As glibc's execvp (`maybe_script_execute`): run the file
                // under the shell, and judge the shell's error like any other
                // candidate's, so a missing shell continues the search.
                *state.shell_argv.add(1) = path;
                let shell = *state.shell_argv;
                errno = -syscall6(
                    libc::SYS_execve,
                    [
                        shell as usize,
                        state.shell_argv as usize,
                        state.envp as usize,
                        0,
                        0,
                        0,
                    ],
                );
            }
            match errno as i32 {
                libc::EACCES => denied = true,
                libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ENODEV | libc::ETIMEDOUT => {}
                _ => fail(errno),
            }
            last = errno;
            index += 1;
        }
        fail(if denied { libc::EACCES as isize } else { last })
    }
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    /// A raw system call (the register convention of Reverie's
    /// `reverie-inguest` exec-stop bridge, `arch::syscall6`): number and
    /// result in rax, arguments in rdi, rsi, rdx, r10, r8, r9; the
    /// instruction clobbers rcx and r11.
    #[inline(always)]
    pub(super) unsafe fn syscall6(number: usize, args: [usize; 6]) -> isize {
        let result: isize;
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") number => result,
                in("rdi") args[0], in("rsi") args[1], in("rdx") args[2],
                in("r10") args[3], in("r8") args[4], in("r9") args[5],
                lateout("rcx") _, lateout("r11") _,
                options(nostack),
            );
        }
        result
    }

    /// clone3, then, in the child, [`super::vfork_child`] on the child's own
    /// stack. Returns, in this thread only, the child's PID or the negative
    /// errno.
    ///
    /// Why the child never touches this thread's stack:
    /// - `args.stack`/`stack_size` name the child's stack, and the kernel
    ///   starts the child with rsp at its top (`stack + stack_size`, 16-byte
    ///   aligned). Every register but rax (0) and rsp is this thread's.
    /// - In the child, the instructions after `syscall` up to `call` read
    ///   only registers (rax, r12) and write only rbp and rdi. `call` pushes
    ///   its return address at the new rsp, on the child's stack, and
    ///   `vfork_child` and everything it calls run there and never return.
    /// - This block is `nostack`, so nothing of this thread lives below its
    ///   rsp that the block could disturb, and the child's rsp never points
    ///   into this thread's stack. This thread waits in the kernel until the
    ///   child execs or exits (CLONE_VFORK), then resumes at the `test` with
    ///   its own rsp, rax = PID or -errno.
    pub(super) unsafe fn clone3_then_child(
        args: *const CloneArgs,
        state: *const ChildState,
    ) -> i64 {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "syscall",
                "test rax, rax",
                "jnz 2f",
                "xor ebp, ebp",
                "mov rdi, r12",
                "call {child}",
                "ud2",
                "2:",
                child = sym super::vfork_child,
                inlateout("rax") libc::SYS_clone3 => result,
                inlateout("rdi") args => _,
                in("rsi") core::mem::size_of::<CloneArgs>(),
                in("r12") state,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        result
    }
}

#[cfg(target_arch = "aarch64")]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    /// A raw system call (the register convention of Reverie's
    /// `reverie-inguest` exec-stop bridge, `arch::syscall6`): number in x8,
    /// arguments in x0-x5, result in x0, never x8.
    #[inline(always)]
    pub(super) unsafe fn syscall6(number: usize, args: [usize; 6]) -> isize {
        let result: isize;
        unsafe {
            core::arch::asm!(
                "svc #0",
                in("x8") number,
                inlateout("x0") args[0] => result,
                in("x1") args[1], in("x2") args[2], in("x3") args[3],
                in("x4") args[4], in("x5") args[5],
                options(nostack),
            );
        }
        result
    }

    /// clone3, then, in the child, [`super::vfork_child`] on the child's own
    /// stack. Returns, in this thread only, the child's PID or the negative
    /// errno.
    ///
    /// Why the child never touches this thread's stack:
    /// - `args.stack`/`stack_size` name the child's stack, and the kernel
    ///   starts the child with sp at its top (`stack + stack_size`, 16-byte
    ///   aligned). Every register but x0 (0) and sp is this thread's.
    /// - In the child, the instructions after `svc` up to `bl` read only
    ///   registers (x0, x20) and write only x29, x30 and x0. `bl` stores its
    ///   return address in x30, not in memory, and `vfork_child` and
    ///   everything it calls run on the child's stack and never return.
    /// - This block is `nostack`, so nothing of this thread lives below its
    ///   sp that the block could disturb, and the child's sp never points
    ///   into this thread's stack. This thread waits in the kernel until the
    ///   child execs or exits (CLONE_VFORK), then resumes at the `cbnz` with
    ///   its own sp, x0 = PID or -errno.
    pub(super) unsafe fn clone3_then_child(
        args: *const CloneArgs,
        state: *const ChildState,
    ) -> i64 {
        let result: i64;
        unsafe {
            core::arch::asm!(
                "svc #0",
                "cbnz x0, 2f",
                "mov x29, xzr",
                "mov x30, xzr",
                "mov x0, x20",
                "bl {child}",
                "brk #1",
                "2:",
                child = sym super::vfork_child,
                in("x8") libc::SYS_clone3,
                inlateout("x0") args => result,
                in("x1") core::mem::size_of::<CloneArgs>(),
                in("x20") state,
                options(nostack),
            );
        }
        result
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod arch {
    use super::ChildState;
    use super::CloneArgs;

    pub(super) unsafe fn syscall6(_number: usize, _args: [usize; 6]) -> isize {
        -(libc::ENOSYS as isize)
    }

    /// No child code for this architecture: clone3 reads as unavailable, and
    /// the caller starts programs the old way.
    pub(super) unsafe fn clone3_then_child(
        _args: *const CloneArgs,
        _state: *const ChildState,
    ) -> i64 {
        -i64::from(libc::ENOSYS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_clone3_errno_maps_to_its_refusal() {
        for errno in [libc::ENOSYS, libc::E2BIG, libc::EINVAL] {
            assert_eq!(refusal(errno), Refusal::Unusable, "errno {errno}");
        }
        assert_eq!(refusal(libc::EPERM), Refusal::FilterOrJoin);
        for errno in [
            libc::EBADF,
            libc::EACCES,
            libc::ENOENT,
            libc::EROFS,
            libc::ENODEV,
            libc::EBUSY,
            libc::EOPNOTSUPP,
        ] {
            assert_eq!(refusal(errno), Refusal::Join, "errno {errno}");
        }
        for errno in [libc::EAGAIN, libc::ENOMEM] {
            assert_eq!(refusal(errno), Refusal::Failed, "errno {errno}");
        }
    }

    /// The EPERM probe makes a real clone3 call, which creates no child; on
    /// this host, with no filter on clone3, it does not read as refused.
    #[test]
    fn the_eperm_probe_finds_clone3_allowed_here() {
        let support = Clone3Support::new();
        assert_eq!(probe_clone3(&support), libc::EINVAL);
        assert!(!probe_refuses_clone3(&support));
        assert!(probe_refuses_clone3(&Clone3Support::refusing(libc::EPERM)));
        assert!(probe_refuses_clone3(&Clone3Support::refusing(libc::ENOSYS)));
    }

    /// The probe double that finds its child killed by SIGKILL makes
    /// clone3 unusable for the process, records the verdict, and runs
    /// only once.
    #[test]
    fn a_killed_probe_child_makes_clone3_unusable_once() {
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_killed());
        support
            .probe_cgroup_clone_once(|| Some((-1, Box::new(()) as Box<dyn std::any::Any + Send>)));
        let record = support.cgroup_probe_record().expect("probe ran");
        assert_eq!(record.outcome, "killed_by_sigkill");
        assert!(
            record
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("clone3-into-cgroup refused by kernel"))
        );
        assert!(support.unusable.get().is_some());
        assert_eq!(support.cgroup_probe_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(support.notices.load(Ordering::Relaxed), 1);
        support
            .probe_cgroup_clone_once(|| Some((-1, Box::new(()) as Box<dyn std::any::Any + Send>)));
        assert_eq!(support.cgroup_probe_attempts.load(Ordering::Relaxed), 1);
    }

    /// Concurrent callers share the one initialization: exactly one
    /// probe and one probe-cgroup creation, and every caller observes
    /// the same record.
    #[test]
    fn concurrent_probe_callers_share_one_initialization() {
        use std::sync::atomic::AtomicUsize;
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_killed());
        let creations = std::sync::Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let support = std::sync::Arc::clone(&support);
                let creations = std::sync::Arc::clone(&creations);
                std::thread::spawn(move || {
                    support.probe_cgroup_clone_once(move || {
                        creations.fetch_add(1, Ordering::Relaxed);
                        Some((-1, Box::new(()) as Box<dyn std::any::Any + Send>))
                    });
                    support.cgroup_probe_record()
                })
            })
            .collect();
        let records: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(support.cgroup_probe_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(creations.load(Ordering::Relaxed), 1);
        assert!(records.iter().all(|record| *record == records[0]));
        assert_eq!(support.notices.load(Ordering::Relaxed), 1);
    }

    /// A probe clone3 refused as a call (here: no such cgroup) is no
    /// verdict: clone3 stays usable, and the outcome is recorded.
    #[test]
    fn a_refused_probe_clone_is_no_verdict() {
        let support = std::sync::Arc::new(Clone3Support::new());
        support
            .probe_cgroup_clone_once(|| Some((-1, Box::new(()) as Box<dyn std::any::Any + Send>)));
        let record = support.cgroup_probe_record().expect("probe ran");
        assert_eq!(record.outcome, "no_verdict");
        assert_eq!(record.reason, None);
        assert!(support.unusable.get().is_none());
        assert_eq!(support.cgroup_probe_attempts.load(Ordering::Relaxed), 1);
    }

    /// The verdict from the entry byte and the wait status: only a
    /// SIGKILL with no entry byte is the kernel refusal.
    #[test]
    fn probe_verdict_needs_a_child_that_never_entered() {
        assert_eq!(
            probe_verdict(false, libc::SIGKILL),
            CgroupProbeOutcome::ChildKilledBySigkill
        );
        assert_eq!(
            probe_verdict(true, libc::SIGKILL),
            CgroupProbeOutcome::ChildRan
        );
        assert_eq!(probe_verdict(true, 0), CgroupProbeOutcome::ChildRan);
        assert_eq!(probe_verdict(true, 127 << 8), CgroupProbeOutcome::NoVerdict);
    }

    /// The entry gate, tested directly with no child: an
    /// after-entry injection is not consumed before the byte
    /// is witnessed, is consumed once it is, and an injected
    /// ECHILD classifies as no-waitable. Removing the gate
    /// (firing before the witness) fails the first assertion
    /// on any host, whatever its scheduling.
    #[test]
    fn the_entry_gate_waits_for_a_witness_and_then_consumes() {
        let remaining = std::sync::atomic::AtomicUsize::new(1);
        assert_eq!(
            injected_wait_failure(Some(libc::ECHILD), true, false, &remaining),
            None
        );
        assert_eq!(remaining.load(Ordering::Relaxed), 1);
        assert_eq!(
            injected_wait_failure(Some(libc::ECHILD), true, true, &remaining),
            Some(libc::ECHILD)
        );
        assert_eq!(remaining.load(Ordering::Relaxed), 0);
        assert_eq!(injected_wait_result(libc::ECHILD), ChildWait::NoWaitable);
        assert_eq!(
            injected_wait_result(libc::EPERM),
            ChildWait::Failed(Some(libc::EPERM))
        );
    }

    /// Without the gate, an injected wait failure fires on
    /// the first wait, witnessed or not.
    #[test]
    fn an_ungated_injected_wait_fires_at_once() {
        let remaining = std::sync::atomic::AtomicUsize::new(1);
        assert_eq!(
            injected_wait_failure(Some(libc::EPERM), false, false, &remaining),
            Some(libc::EPERM)
        );
        assert_eq!(remaining.load(Ordering::Relaxed), 0);
    }

    /// A probe that created no child records that it waited
    /// by no path at all.
    #[test]
    fn a_probe_without_a_child_records_no_wait_path() {
        let support = std::sync::Arc::new(Clone3Support::new());
        support.probe_cgroup_clone_once(|| None);
        let record = support.cgroup_probe_record().expect("probe ran");
        assert_eq!(record.outcome, "unavailable", "{record:?}");
        assert_eq!(record.child_wait, "none", "{record:?}");
    }

    /// The real probe against a real invocation cgroup. On a healthy
    /// kernel the child runs and clone3 stays usable; on a kernel
    /// with the kill-sequence bug, in a runner cgroup that was
    /// killed before, the child is killed before it can run and the
    /// fallback is taken (clone3 remembered unusable, with the
    /// reason). Both are the supported outcomes; the bug case is
    /// asserted, not skipped.
    #[test]
    fn the_probe_child_runs_or_is_killed_before_it_can_run() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "the_probe_child_runs_or_is_killed_before_it_can_run",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::new());
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        match record.outcome.as_str() {
            "usable" => assert!(support.unusable.get().is_none(), "{record:?}"),
            "killed_by_sigkill" => {
                assert!(support.unusable.get().is_some(), "{record:?}");
                assert!(
                    record.reason.as_deref().is_some_and(
                        |reason| reason.contains("clone3-into-cgroup refused by kernel")
                    )
                );
            }
            other => panic!("unexpected probe outcome {other}: {record:?}"),
        }
    }

    /// A child that kills itself after writing its entry byte is a
    /// child that ran: its death is not the kernel refusal, and
    /// clone3 stays usable. Where the kernel kills the child before
    /// it can write, the refusal-plus-fallback outcome is asserted
    /// instead.
    #[test]
    fn a_probe_child_killed_after_entry_is_not_a_kernel_refusal() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_probe_child_killed_after_entry_is_not_a_kernel_refusal",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_child_kills_after_entry());
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        match record.outcome.as_str() {
            "usable" => assert!(support.unusable.get().is_none(), "{record:?}"),
            "killed_by_sigkill" => {
                assert!(support.unusable.get().is_some(), "{record:?}");
            }
            other => panic!("unexpected probe outcome {other}: {record:?}"),
        }
    }

    /// The interleaving where the child writes its entry byte and
    /// dies before the parent's (delayed) first read: the byte is
    /// drained after the reap and the verdict is a child that ran,
    /// never the kernel refusal. On a kernel that kills the child
    /// before it can write at all, the refusal is the correct
    /// verdict instead; a plain probe in the same test proves
    /// that is the environment, so the strict usable assertion
    /// still bites wherever the child can enter.
    #[test]
    fn a_child_that_wrote_and_died_before_the_read_still_ran() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_child_that_wrote_and_died_before_the_read_still_ran",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(
            Clone3Support::cgroup_probe_child_kills_after_entry_with_delayed_read(),
        );
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "usable" {
            assert!(support.unusable.get().is_none(), "{record:?}");
            return;
        }
        // The delayed child never entered: only a kernel that
        // kills every probe child before entry may produce this.
        // A plain probe must agree, or the written byte was lost.
        assert_eq!(record.outcome, "killed_by_sigkill", "{record:?}");
        let Some(plain_cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_child_that_wrote_and_died_before_the_read_still_ran_plain",
        ) else {
            return;
        };
        let plain = std::sync::Arc::new(Clone3Support::new());
        plain.probe_cgroup_clone_once(move || {
            Some((
                plain_cgroup.directory_fd(),
                Box::new(plain_cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let plain_record = plain.cgroup_probe_record().expect("plain probe ran");
        assert_eq!(
            plain_record.outcome, "killed_by_sigkill",
            "delayed child lost its written byte: {record:?}; plain probe: {plain_record:?}"
        );
    }

    /// A probe child held before its entry byte runs into the
    /// bound: the parent kills and reaps its own child, the verdict
    /// is a timeout inside the bound, and no child is left behind.
    /// Where the kernel kills the child at creation, that verdict
    /// replaces the timeout; both take the fallback.
    #[test]
    fn a_probe_child_held_before_entry_is_bounded_and_reaped() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_probe_child_held_before_entry_is_bounded_and_reaped",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_child_hangs_before_entry());
        let started = std::time::Instant::now();
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "probe took {:?}",
            started.elapsed()
        );
        let record = support.cgroup_probe_record().expect("probe ran");
        assert!(
            matches!(record.outcome.as_str(), "timed_out" | "killed_by_sigkill"),
            "{record:?}"
        );
        assert!(support.unusable.get().is_some());
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        assert!(
            !std::path::Path::new(&format!("/proc/{child}")).exists(),
            "probe child {child} is still in /proc (zombie or alive)"
        );
    }

    /// A timeout whose kill fails (a policy refusing kill): the
    /// initialization still publishes its timeout inside the bound
    /// plus the reap grace, and the fallback is taken. The child
    /// stays this process's own, so no pid can be reused first.
    #[test]
    fn a_timeout_with_a_failing_kill_still_publishes_its_timeout() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_timeout_with_a_failing_kill_still_publishes_its_timeout",
        ) else {
            return;
        };
        let support =
            std::sync::Arc::new(Clone3Support::cgroup_probe_child_hangs_with_failing_kill());
        let started = std::time::Instant::now();
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "probe took {:?}",
            started.elapsed()
        );
        let record = support.cgroup_probe_record().expect("probe ran");
        assert!(
            matches!(record.outcome.as_str(), "timed_out" | "killed_by_sigkill"),
            "{record:?}"
        );
        assert!(support.unusable.get().is_some());
    }

    /// A wait that fails with a non-ECHILD error proves nothing
    /// about the child: no verdict is taken, and the child is
    /// kept as still owed until a retry reaps it.
    #[test]
    fn a_failed_wait_keeps_the_child_owed_until_a_retry_reaps_it() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_failed_wait_keeps_the_child_owed_until_a_retry_reaps_it",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_wait_fails_with(libc::EPERM));
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        assert_eq!(record.outcome, "wait_failed", "{record:?}");
        assert!(support.unusable.get().is_none(), "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        assert_eq!(support.unreaped_probe_child(), Some(child));
        // The injection is spent: the retry does the real wait
        // and completes the obligation.
        let deadline = std::time::Instant::now();
        while !support.reap_unreaped_probe_child() {
            assert!(
                deadline.elapsed() < std::time::Duration::from_secs(5),
                "retry never reaped probe child {child}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(support.unreaped_probe_child(), None);
        assert!(
            !std::path::Path::new(&format!("/proc/{child}")).exists(),
            "probe child {child} is still in /proc"
        );
    }

    /// ECHILD with no entry byte: no wait status exists, so the
    /// child cannot be claimed to have run. No verdict, and the
    /// cleanup is complete (nothing is kept owed).
    #[test]
    fn an_echild_wait_without_a_byte_is_no_verdict() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "an_echild_wait_without_a_byte_is_no_verdict",
        ) else {
            return;
        };
        let support =
            std::sync::Arc::new(Clone3Support::cgroup_probe_child_hangs_with_echild_wait());
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        assert_eq!(record.outcome, "no_verdict", "{record:?}");
        assert!(support.unusable.get().is_none(), "{record:?}");
        assert_eq!(support.unreaped_probe_child(), None);
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        // The injected wait is what answered, so the real wait
        // never reaped this child: it is still this test's own
        // corpse (live or zombie), and only then may the test
        // signal it for cleanup.
        assert!(support.injected_wait_consumed());
        support.cleanup_injected_wait_leftover();
        match support.probe_child_fds_still_open() {
            Some(0) => {}
            Some(open) => panic!("{open} fds of child {child} remain"),
            None => {
                println!("pidfd leak census not checkable: no per-pid pidfd inodes (pidfs absent)");
            }
        }
    }

    /// ECHILD after the entry byte: the byte, not a synthesized
    /// status, shows the child ran, and clone3 stays usable.
    /// Where the kernel kills the child before it can write,
    /// no verdict is the correct answer instead, proven by a
    /// plain probe in the same test.
    #[test]
    fn an_echild_wait_after_the_byte_is_a_child_that_ran() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "an_echild_wait_after_the_byte_is_a_child_that_ran",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(
            Clone3Support::cgroup_probe_child_kills_after_entry_with_echild_wait(),
        );
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        // Cleanup is decided by whether the injected wait was
        // consumed, never by the verdict text: if it was, the
        // real wait never ran and the child is still this
        // test's own corpse to signal; if it was not, a real
        // wait reaped the child (the drain route, or the
        // kernel's kill), and its pid must never be signalled
        // again.
        let consumed = support.injected_wait_consumed();
        if consumed {
            support.cleanup_injected_wait_leftover();
            match support.probe_child_fds_still_open() {
                Some(0) => {}
                Some(open) => panic!("{open} fds of child {child} remain"),
                None => {
                    println!(
                        "pidfd leak census not checkable: no per-pid pidfd inodes (pidfs absent)"
                    );
                }
            }
        }
        if consumed {
            // The injected ECHILD is what the probe classified:
            // the byte had been witnessed, so this is a child
            // that ran.
            assert_eq!(record.outcome, "usable", "{record:?}");
            assert!(support.unusable.get().is_none(), "{record:?}");
            assert_eq!(support.unreaped_probe_child(), None);
            return;
        }
        if record.outcome == "usable" {
            // The real wait and the post-reap drain answered
            // first: also a child that ran, by the real path.
            assert!(support.unusable.get().is_none(), "{record:?}");
            assert_eq!(support.unreaped_probe_child(), None);
            return;
        }
        assert_eq!(record.outcome, "killed_by_sigkill", "{record:?}");
        let Some(plain_cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "an_echild_wait_after_the_byte_is_a_child_that_ran_plain",
        ) else {
            return;
        };
        let plain = std::sync::Arc::new(Clone3Support::new());
        plain.probe_cgroup_clone_once(move || {
            Some((
                plain_cgroup.directory_fd(),
                Box::new(plain_cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let plain_record = plain.cgroup_probe_record().expect("plain probe ran");
        assert_eq!(
            plain_record.outcome, "killed_by_sigkill",
            "ECHILD misclassified a child that ran: {record:?}; plain probe: {plain_record:?}"
        );
    }

    /// A reap-grace wait that fails (with the kill also
    /// refused): the background reaper, not the caller, owns
    /// the child from there, and reaps it when it dies.
    #[test]
    fn a_failed_grace_wait_hands_the_child_to_the_reaper() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_failed_grace_wait_hands_the_child_to_the_reaper",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_child_unreapable(false));
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "killed_by_sigkill" {
            // The kernel killed the child before it could hang:
            // the timeout path this test drives cannot occur.
            return;
        }
        assert_eq!(record.outcome, "timed_out", "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        assert_eq!(support.unreaped_probe_child(), None);
        // The child was handed to the reaper as a move, so
        // this test may not signal it at all: the dropped
        // probe cgroup's teardown has already initiated its
        // cleanup, and the reaper owns its wait. Just wait
        // for the reaper to finish it below.
        let deadline = std::time::Instant::now();
        while std::path::Path::new(&format!("/proc/{child}")).exists() {
            assert!(
                deadline.elapsed() < std::time::Duration::from_secs(5),
                "reaper never reaped probe child {child}"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Cleans up a test's probe child when dropped, so even a
    /// failing run of a reaper test cannot leave its paused
    /// child behind. It signals the child only while this
    /// process still owns it and has not reaped it, through
    /// its pidfd, so it can never signal a different process
    /// that later reuses the numeric pid; once the child is
    /// marked reaped it sends nothing at all.
    struct ProbeChildGuard {
        support: std::sync::Arc<Clone3Support>,
        #[allow(dead_code)]
        pid: i32,
    }

    impl ProbeChildGuard {
        fn new(support: std::sync::Arc<Clone3Support>, pid: i32) -> Self {
            Self { support, pid }
        }
    }

    impl Drop for ProbeChildGuard {
        fn drop(&mut self) {
            if self.support.probe_child_is_reaped() {
                return;
            }
            self.support.kill_probe_child_for_cleanup();
            let _dup = self
                .support
                .probe_child_cleanup_pidfd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            for _ in 0..200 {
                if self.support.reap_unreaped_probe_child() || self.support.probe_child_is_reaped()
                {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
    }

    /// A reaper that cannot be started (with the kill and the
    /// grace waits also refused): the caller itself stays the
    /// owner and kills the child, then polls its reap to a
    /// bounded grace, so the child is gone when the probe
    /// returns and nothing is left owed. No production caller
    /// wait blocks without a bound.
    #[test]
    fn a_failed_reaper_spawn_is_killed_and_reaped_by_the_caller() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_failed_reaper_spawn_keeps_the_child_owed_until_retried",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_child_unreapable(true));
        let started = std::time::Instant::now();
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "probe took {:?}",
            started.elapsed()
        );
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "killed_by_sigkill" {
            return;
        }
        assert_eq!(record.outcome, "timed_out", "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        let _guard = ProbeChildGuard::new(std::sync::Arc::clone(&support), child);
        assert_eq!(support.unreaped_probe_child(), None);
        assert!(
            !std::path::Path::new(&format!("/proc/{child}")).exists(),
            "probe child {child} is still in /proc"
        );
    }

    /// A guard dropped for a child this process already
    /// reaped sends nothing: the observed state (reaped, its
    /// pidfd closed, nothing owed) is the assertion, not any
    /// timing. Dropping it must not change that state.
    #[test]
    fn a_guard_for_a_reaped_child_sends_nothing() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_guard_for_a_reaped_child_sends_nothing",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::new());
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        assert!(support.probe_child_is_reaped());
        assert_eq!(support.unreaped_probe_child(), None);
        let guard = ProbeChildGuard::new(std::sync::Arc::clone(&support), child);
        drop(guard);
        assert!(support.probe_child_is_reaped());
        assert_eq!(support.unreaped_probe_child(), None);
    }

    /// The reaper's debt return and a concurrent snapshot:
    /// the snapshot, taken under the same locks, must never
    /// observe the debt recorded while the handle is still
    /// handed off. With the return's pause injected, the old
    /// two-step return (record first, handle second) is
    /// observed in exactly that state; the atomic return
    /// cannot be.
    #[test]
    fn a_returned_debt_is_never_visible_without_its_handle() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_returned_debt_is_never_visible_without_its_handle",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(
            Clone3Support::cgroup_probe_child_unreapable_with_failed_fallback_kill(),
        );
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "killed_by_sigkill" {
            return;
        }
        assert_eq!(record.outcome, "no_verdict", "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        let _guard = ProbeChildGuard::new(std::sync::Arc::clone(&support), child);
        // Simulate the reaper holding the handle, with no
        // debt recorded: take it out and clear the record.
        let owned = take_probe_child(&support).expect("caller owned the child");
        {
            let mut debt = support
                .unreaped_probe_child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *debt = None;
        }
        assert_eq!(support.debt_snapshot(), (false, true));
        support
            .inject_debt_return_pause
            .store(true, Ordering::Relaxed);
        let return_thread = {
            let returning = std::sync::Arc::clone(&support);
            std::thread::spawn(move || returning.return_owned_as_debt(owned))
        };
        while !support.debt_return_entered() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // While the return is held mid-transition, look with
        // try_lock: the atomic return holds both locks, so no
        // observation of a half-return is possible (None), and
        // in no case may debt-without-handle be seen. The old
        // two-step return, held at the same point, is observed
        // in exactly that state here.
        let during = support.debt_snapshot_try();
        assert_ne!(during, Some((true, true)), "during the held return");
        support.release_debt_return();
        return_thread.join().unwrap();
        assert_eq!(support.debt_snapshot(), (true, false));
    }

    /// A real probe (which clones with a pidfd and, in tests,
    /// a duplicate of it) leaves no fd naming its child open:
    /// every path out of the probe closes what it opened, and
    /// a duplicate leaked past the reap would still be seen.
    #[test]
    fn a_real_probe_leaves_no_extra_fds() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_real_probe_leaves_no_extra_fds",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::new());
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        match support.probe_child_fds_still_open() {
            None => {
                println!("pidfd leak census not checkable: no per-pid pidfd inodes (pidfs absent)");
            }
            Some(_) => {
                let deadline = std::time::Instant::now();
                while support.probe_child_fds_still_open() != Some(0) {
                    assert!(
                        deadline.elapsed() < std::time::Duration::from_secs(5),
                        "probe left {:?} of child {child}'s fds open",
                        support.probe_child_fds_still_open()
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            }
        }
    }

    /// The skip branch itself, forced: where the kernel is
    /// recorded as giving every pidfd one shared inode, the
    /// census reports itself not checkable (None) instead of
    /// counting, whatever fds are open.
    #[test]
    fn the_fd_census_names_its_skip_where_pidfs_is_absent() {
        let support = Clone3Support::new();
        support
            .probe_child_fd_census_shared
            .store(true, Ordering::Relaxed);
        assert_eq!(support.probe_child_fds_still_open(), None);
    }

    /// A reaper that cannot be started and a fallback kill
    /// that also fails: the caller polls only to its bounded
    /// grace, then leaves the child recorded as owed and takes
    /// no verdict from the failed cleanup. The guard cleans
    /// the still-paused child up afterwards.
    #[test]
    fn a_failed_fallback_kill_leaves_the_child_owed_without_blocking() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_failed_fallback_kill_leaves_the_child_owed_without_blocking",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(
            Clone3Support::cgroup_probe_child_unreapable_with_failed_fallback_kill(),
        );
        let started = std::time::Instant::now();
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "probe took {:?}",
            started.elapsed()
        );
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "killed_by_sigkill" {
            return;
        }
        assert_eq!(record.outcome, "no_verdict", "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        let _guard = ProbeChildGuard::new(std::sync::Arc::clone(&support), child);
        assert_eq!(support.unreaped_probe_child(), Some(child));
        // The obligation survives until a retry completes it:
        // the dropped probe cgroup has killed the child, so
        // the retry reaps the zombie and clears the record.
        assert!(support.reap_unreaped_probe_child());
        assert_eq!(support.unreaped_probe_child(), None);
        assert!(support.probe_child_is_reaped());
    }

    /// Concurrent retries of one owed child: while it lives,
    /// every starter's retry reaps nothing and the obligation
    /// stays recorded; once it dies, the retries converge on
    /// one reap and clear it. The wait-and-clear transaction
    /// is serialized, so no starter can wait on a pid another
    /// starter already reaped.
    #[test]
    fn concurrent_retries_share_one_owed_child() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "concurrent_retries_share_one_owed_child",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(
            Clone3Support::cgroup_probe_child_unreapable_with_failed_fallback_kill(),
        );
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        let record = support.cgroup_probe_record().expect("probe ran");
        if record.outcome == "killed_by_sigkill" {
            return;
        }
        assert_eq!(record.outcome, "no_verdict", "{record:?}");
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        let _guard = ProbeChildGuard::new(std::sync::Arc::clone(&support), child);
        assert_eq!(support.unreaped_probe_child(), Some(child));
        // Eight starters retry at once. The transaction is
        // serialized, so only one of them can issue the wait
        // that reaps the (teardown-killed) child; the others
        // observe the cleared record. All converge below.
        let retries: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| support.reap_unreaped_probe_child()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert!(retries.iter().all(|done| *done), "{retries:?}");
        assert!(support.probe_child_is_reaped());
        assert_eq!(support.unreaped_probe_child(), None);
        assert!(
            !std::path::Path::new(&format!("/proc/{child}")).exists(),
            "probe child {child} is still in /proc"
        );
    }

    /// A probe child held after its entry byte also runs into the
    /// bound and is reaped; the verdict is a timeout, not the
    /// kernel refusal.
    #[test]
    fn a_probe_child_held_after_entry_is_bounded_and_reaped() {
        let Some(cgroup) = crate::invocation_cgroup::tests::real_cgroup_or_declared_absent(
            "a_probe_child_held_after_entry_is_bounded_and_reaped",
        ) else {
            return;
        };
        let support = std::sync::Arc::new(Clone3Support::cgroup_probe_child_hangs_after_entry());
        let started = std::time::Instant::now();
        support.probe_cgroup_clone_once(move || {
            Some((
                cgroup.directory_fd(),
                Box::new(cgroup) as Box<dyn std::any::Any + Send>,
            ))
        });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "probe took {:?}",
            started.elapsed()
        );
        let record = support.cgroup_probe_record().expect("probe ran");
        assert!(
            matches!(record.outcome.as_str(), "timed_out" | "killed_by_sigkill"),
            "{record:?}"
        );
        assert!(support.unusable.get().is_some());
        let child = support.probe_child_pid.load(Ordering::Relaxed);
        assert!(child > 0);
        assert!(
            !std::path::Path::new(&format!("/proc/{child}")).exists(),
            "probe child {child} is still in /proc (zombie or alive)"
        );
    }
}
