/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! In-guest LiteInst's asynchronous exit completion, on the launcher's side.
//!
//! A guest process under in-guest LiteInst leaves the host some time after
//! Detcore grants its exit. Detcore holds turn selection until the process is
//! physically gone (`process_exits_complete_asynchronously`), and this module
//! is what tells it so. The coordinator admits each guest connection through
//! [`InGuestExitAdmission`], which receives the connecting process's pidfd
//! (`SO_PEERPIDFD`) before the connection is served and hands it to the shared
//! [`PhysicalExitWatch`]. When the process has exited and the kernel has
//! published that to its parent, the watcher reports it through the
//! [`ExitReporter`] the launcher attached, which reaches Detcore's
//! `GlobalTool::on_backend_process_exited`. Nothing here is in Detcore (owner
//! directive 082): Detcore only holds the abstract lifetime state.
//!
//! The watcher runs on the coordinator's own tokio runtime rather than on a
//! thread: the coordinator shares the guest's PID namespace, so a thread would
//! take a process id and shift every pid the guest sees.
//!
//! A watchdog on the same runtime reads the exits Detcore holds
//! (`GlobalTool::backend_pending_process_exits`) and fails the run, never
//! letting it match, when one does not complete:
//!
//! - after `EXIT_WATCHDOG` for any reason, for example a guest stopped
//!   between its exit grant and its native exit;
//! - after `KERNEL_WAIT` while the process sleeps (`D` or `S`) inside the
//!   kernel's exit path, which is how a guest serving a kernel
//!   network-filesystem client shows (an unsupported topology).
//!
//! Failing means a named diagnostic on stderr, Detcore's `backend_failed`, and
//! `SIGKILL` to every watched process. The watchdog also fails the run if the
//! watcher itself stops observing exits.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use reverie_rpc_transport::Admitted;
use reverie_rpc_transport::ConnectionAdmission;
use reverie_rpc_transport::ExitReporter;

use crate::physical_exit_watch::DrainError;
use crate::physical_exit_watch::PhysicalExitWatch;

/// How long a granted exit may stay incomplete before the run fails.
pub(crate) const EXIT_WATCHDOG: Duration = Duration::from_secs(60);

/// How long a granted exit may sleep inside the kernel's exit path before the
/// run fails (coord ruling C).
pub(crate) const KERNEL_WAIT: Duration = Duration::from_secs(1);

/// How often the watchdog reads the held exits.
const WATCHDOG_POLL: Duration = Duration::from_millis(100);

/// `PF_EXITING` in `/proc/<pid>/stat`'s flags: the task is in `do_exit`.
const PF_EXITING: u64 = 0x4;

/// The phase named in the `BackendFailure` the watchdog reports.
const WATCHDOG_PHASE: &str = "in-guest LiteInst exit watchdog";

/// The phase named in the `BackendFailure` a refused admission reports.
const ADMISSION_PHASE: &str = "in-guest LiteInst admission";
const COORDINATOR_PHASE: &str = "in-guest LiteInst coordinator";

#[derive(Clone, Copy)]
struct Limits {
    exit: Duration,
    kernel_wait: Duration,
    /// Reads whether a held process sleeps in its exit path:
    /// [`kernel_wait_state`], or a stand-in under test.
    exit_path_probe: fn(&ProcDirs, i32) -> Option<char>,
}

/// Each admitted process's `/proc/<pid>` directory, by guest-visible pid,
/// until its exit is reported.
type ProcDirs = Arc<Mutex<HashMap<i32, File>>>;

#[derive(Default)]
struct NativeWatchHistory {
    admitted_births: std::collections::BTreeSet<(i32, Option<u64>)>,
    reported: Vec<i32>,
}

struct Watching {
    /// Per-watch history cannot be reused after reporter replacement.
    native_history: Arc<Mutex<NativeWatchHistory>>,
    watch: PhysicalExitWatch,
    reporter: Arc<dyn ExitReporter>,
    proc_dirs: ProcDirs,
    outcome: Outcome,
}

/// What the launcher needs after the run, kept by the admission itself: the
/// backend's reference to the global state ends with the run, so neither
/// reports nor failures can rely on reaching it.
#[derive(Default)]
struct ExitOutcome {
    /// Every exit the watcher reported, in order.
    reported: Vec<i32>,
    /// The first failure: a fired watchdog, a stopped watcher, or a refused
    /// admission. The run must not report a result with it set.
    failure: Option<String>,
}

type Outcome = Arc<Mutex<ExitOutcome>>;

/// Records `failure` unless one is already recorded.
fn record_failure(outcome: &Outcome, failure: String) {
    outcome.lock().unwrap().failure.get_or_insert(failure);
}

/// The watcher and its watchdog task, once the reporter is attached.
struct Started {
    watching: Arc<Watching>,
    watchdog: tokio::task::JoinHandle<()>,
}

/// Admits in-guest LiteInst connections by pidfd and watches each admitted
/// process for its physical exit; see the module documentation.
pub struct InGuestExitAdmission {
    limits: Limits,
    /// `None` before the reporter is attached and after the drain.
    started: Mutex<Option<io::Result<Started>>>,
    outcome: Outcome,
    /// A native drain is a single consuming attempt, even if a caller later
    /// attaches another reporter to this ordinary admission object.
    native_completion_taken: std::sync::atomic::AtomicBool,
}

impl Default for InGuestExitAdmission {
    fn default() -> Self {
        Self::with_limits(Limits {
            exit: EXIT_WATCHDOG,
            kernel_wait: KERNEL_WAIT,
            exit_path_probe: kernel_wait_state,
        })
    }
}

impl ConnectionAdmission for InGuestExitAdmission {
    /// Called by the backend's launch on the coordinator's runtime.
    fn attach_exit_reporter(&self, reporter: Arc<dyn ExitReporter>) {
        let started = tokio::runtime::Handle::try_current()
            .map_err(io::Error::other)
            .and_then(|handle| {
                let proc_dirs = ProcDirs::default();
                let exits = Arc::clone(&reporter);
                let gone = Arc::clone(&proc_dirs);
                let reports = Arc::clone(&self.outcome);
                let native_history = Arc::new(Mutex::new(NativeWatchHistory::default()));
                let native_reports = Arc::clone(&native_history);
                let watch = PhysicalExitWatch::on_runtime(handle.clone(), move |pid| {
                    native_reports.lock().unwrap().reported.push(pid);
                    gone.lock().unwrap().remove(&pid);
                    reports.lock().unwrap().reported.push(pid);
                    exits.process_exited(pid);
                })?;
                let watching = Arc::new(Watching {
                    native_history,
                    watch,
                    reporter,
                    proc_dirs,
                    outcome: Arc::clone(&self.outcome),
                });
                let watchdog = handle.spawn(watchdog(Arc::clone(&watching), self.limits));
                Ok(Started { watching, watchdog })
            });
        *self.started.lock().unwrap() = Some(started);
    }

    /// A refused connection fails the run at once: its process may already be
    /// registered with Detcore, which would otherwise wait for a request it
    /// can never send.
    fn admit(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let admitted = self.admit_and_watch(peer);
        if let Err(error) = &admitted {
            self.fail_admission(error);
        }
        admitted
    }

    /// The server could not even offer the connection for admission (its
    /// peer pidfd could not be obtained). That is a refused connection too.
    fn admission_failed(&self, error: &io::Error) {
        self.fail_admission(error);
    }
}

impl InGuestExitAdmission {
    /// Fails the run for a refused connection.
    fn fail_admission(&self, error: &io::Error) {
        self.fail_now(
            format!("in-guest LiteInst refused a guest connection: {error}"),
            ADMISSION_PHASE,
        );
    }

    /// Fails the run when the coordinator itself stopped (for example, its
    /// RPC listener failed). The launch calls this before it returns the
    /// error, so that no guest outlives the run: the backend's blocking wait
    /// for the root guest would otherwise hold the runtime's shutdown open.
    pub fn fail_launch(&self, error: &dyn std::fmt::Display) {
        self.fail_now(
            format!("in-guest LiteInst coordinator failed: {error}"),
            COORDINATOR_PHASE,
        );
    }

    /// The named diagnostic, the persistent run failure, Detcore's
    /// `backend_failed`, and `SIGKILL` to every watched guest, including any
    /// admitted later.
    fn fail_now(&self, diagnostic: String, phase: &'static str) {
        eprintln!("hermit: {diagnostic}");
        record_failure(&self.outcome, diagnostic);
        // Native controls take scheduler -> registry -> started. Never call a
        // scheduler-facing reporter under started: that closes the reverse
        // started -> scheduler edge during concurrent provision/failure.
        let watching = match self.started.lock().unwrap().as_ref() {
            Some(Ok(started)) => Some(Arc::clone(&started.watching)),
            _ => None,
        };
        if let Some(watching) = watching {
            watching.reporter.backend_failed(reverie::BackendFailure {
                pid: reverie::Pid::from_raw(0),
                tid: reverie::Tid::from_raw(0),
                phase,
            });
            if let Err(error) = watching.watch.fail_and_kill_all() {
                eprintln!(
                    "hermit: in-guest LiteInst could not kill every guest after that failure: {error}"
                );
            }
        }
    }

    fn admit_and_watch(&self, peer: OwnedFd) -> io::Result<Admitted> {
        let process_id = guest_visible_pid(&peer)?;
        let birth = crate::physical_exit_watch::birth_identity(&peer);
        let proc_dir = open_proc_dir(&peer)?;
        let started = self.started.lock().unwrap();
        match started.as_ref() {
            Some(Ok(Started { watching, .. })) => {
                watching
                    .proc_dirs
                    .lock()
                    .unwrap()
                    .insert(process_id, proc_dir);
                watching
                    .native_history
                    .lock()
                    .unwrap()
                    .admitted_births
                    .insert((process_id, birth));
                watching.watch.watch(peer, process_id)?;
            }
            Some(Err(error)) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("in-guest LiteInst exit watcher did not start: {error}"),
                ));
            }
            None => {
                return Err(io::Error::other(
                    "in-guest LiteInst admitted a connection with no exit watcher (none attached yet, or already drained)",
                ));
            }
        }
        Ok(Admitted { process_id })
    }

    fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            started: Mutex::new(None),
            outcome: Outcome::default(),
            native_completion_taken: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Every exit the watcher reported, in the order reported.
    pub fn reported_exits(&self) -> Vec<i32> {
        self.outcome.lock().unwrap().reported.clone()
    }

    /// The first failure recorded during the run, if any.
    pub fn failure(&self) -> Option<String> {
        self.outcome.lock().unwrap().failure.clone()
    }

    /// Waits, bounded, until every admitted process's exit has been reported.
    /// Takes the watcher out and stops the watchdog, so watching ends here and
    /// nothing is admitted afterwards.
    pub async fn drain(&self, timeout: Duration) -> Result<(), DrainError> {
        let started = self.started.lock().unwrap().take();
        match started {
            Some(Ok(Started { watching, watchdog })) => {
                watchdog.abort();
                watching.watch.drain_async(timeout).await
            }
            Some(Err(error)) => Err(DrainError::Watcher(error)),
            None => Ok(()),
        }
    }
}

/// Actual retained registration of one living native root, before startup ACK.
/// Private fields join the pidfd to the same watch used for final completion.
pub struct NativeWatchedProcess {
    admission: Arc<InGuestExitAdmission>,
    watching: Arc<Watching>,
    pidfd: OwnedFd,
    pid: i32,
    identity: u64,
}

// SAFETY: only retain_native_process constructs this after joining the actual
// live pidfd to this admission's recorded watch registration. Fields never change.
unsafe impl detcore::detlog::NativePhysicalWatchIdentity for NativeWatchedProcess {
    fn retained_pidfd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.pidfd.as_fd()
    }
    fn process_id(&self) -> i32 {
        self.pid
    }
    fn birth_identity(&self) -> u64 {
        self.identity
    }
    fn watch_is_healthy(&self) -> bool {
        let started = self.admission.started.lock().unwrap();
        let history_matches = {
            let history = self.watching.native_history.lock().unwrap();
            history.admitted_births.len() == 1
                && history
                    .admitted_births
                    .contains(&(self.pid, Some(self.identity)))
        };
        matches!(started.as_ref(), Some(Ok(current))
            if Arc::ptr_eq(&current.watching, &self.watching))
            && history_matches
            && self.watching.watch.failure().is_none()
            && self.admission.outcome.lock().unwrap().failure.is_none()
    }
}

impl InGuestExitAdmission {
    /// Retain this actual already-admitted living root, never register a new
    /// process or infer its identity from a caller-supplied PID.
    pub fn retain_native_process(
        self: &Arc<Self>,
        pidfd: OwnedFd,
    ) -> io::Result<NativeWatchedProcess> {
        use detcore::detlog::NativePhysicalWatchIdentity;
        let pid = guest_visible_pid(&pidfd)?;
        let identity = crate::physical_exit_watch::birth_identity(&pidfd).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "native process requires an actual pidfs birth",
            )
        })?;
        let watching = match self.started.lock().unwrap().as_ref() {
            Some(Ok(started)) => Arc::clone(&started.watching),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "native process has no actual running watch",
                ));
            }
        };
        let result = NativeWatchedProcess {
            admission: self.clone(),
            watching,
            pidfd,
            pid,
            identity,
        };
        let mut poll = libc::pollfd {
            fd: result.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if !result.watch_is_healthy() || unsafe { libc::poll(&mut poll, 1, 0) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native process is not this admission's single living watched root",
            ));
        }
        Ok(result)
    }
}

/// Private affine evidence produced by the real watch drain below. No caller
/// can construct it from a PID, EOF, boolean, or a child wait alone.
pub struct NativePhysicalCompletion {
    pidfd: OwnedFd,
    pid: i32,
    identity: u64,
}
// SAFETY: fields are private and only drain_native_completion constructs them,
// after the actual watch report, complete drain, no failure and same dead birth.
unsafe impl detcore::detlog::NativePhysicalExitCompletion for NativePhysicalCompletion {
    fn retained_pidfd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.pidfd.as_fd()
    }
    fn process_id(&self) -> i32 {
        self.pid
    }
    fn birth_identity(&self) -> u64 {
        self.identity
    }
}
impl InGuestExitAdmission {
    /// Native-only final receipt, available from one consuming attempt.
    /// A failed attempt cannot be retried on this admission owner. Ordinary
    /// drain semantics remain unchanged.
    pub async fn drain_native_completion(
        &self,
        timeout: Duration,
        pidfd: OwnedFd,
        pid: i32,
        identity: u64,
    ) -> io::Result<NativePhysicalCompletion> {
        if self
            .native_completion_taken
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native completion was already consumed",
            ));
        }
        if pid <= 0
            || identity == 0
            || crate::physical_exit_watch::birth_identity(&pidfd) != Some(identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native completion retained birth differs",
            ));
        }
        // Consume the actual watcher under one lock. A concurrent ordinary or
        // native drain cannot turn a later None into completion evidence.
        let started = self.started.lock().unwrap().take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "native completion has no actual running watch",
            )
        })?;
        let Started { watching, watchdog } = started?;
        watchdog.abort();
        watching
            .watch
            .drain_async(timeout)
            .await
            .map_err(|error| match error {
                DrainError::Watcher(error) => error,
                DrainError::TimedOut(pids) => io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("native physical watch unfinished: {pids:?}"),
                ),
            })?;
        let outcome = self.outcome.lock().unwrap();
        let history = watching.native_history.lock().unwrap();
        if outcome.failure.is_some()
            || history.reported != [pid]
            || history.admitted_births.len() != 1
            || !history.admitted_births.contains(&(pid, Some(identity)))
            || crate::physical_exit_watch::birth_identity(&pidfd) != Some(identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native physical report missing/different/failed",
            ));
        }
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, 0) } != 1
            || poll.revents & libc::POLLIN == 0
            || poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native completion retained pidfd is not physically dead",
            ));
        }
        Ok(NativePhysicalCompletion {
            pidfd,
            pid,
            identity,
        })
    }
}

impl Drop for InGuestExitAdmission {
    fn drop(&mut self) {
        if let Some(Ok(Started { watchdog, .. })) = self.started.get_mut().unwrap().as_ref() {
            watchdog.abort();
        }
    }
}

/// Polls the exits Detcore holds until it fails the run or is aborted.
async fn watchdog(watching: Arc<Watching>, limits: Limits) {
    let mut held_since: HashMap<i32, Instant> = HashMap::new();
    // When each held process was first seen asleep in its exit path, without
    // a sample since then showing it awake: the kernel-wait check needs the
    // sleep itself to last, not just the hold.
    let mut asleep_since: HashMap<i32, Instant> = HashMap::new();
    loop {
        tokio::time::sleep(WATCHDOG_POLL).await;
        if let Some(error) = watching.watch.failure() {
            return fail_run(
                &watching,
                0,
                format!(
                    "in-guest LiteInst stopped observing guest exits, so a held exit could never complete: {error}"
                ),
            );
        }
        let pending = watching.reporter.pending_process_exits();
        held_since.retain(|pid, _| pending.contains(pid));
        asleep_since.retain(|pid, _| pending.contains(pid));
        let now = Instant::now();
        for pid in pending {
            let held = now.duration_since(*held_since.entry(pid).or_insert(now));
            if held >= limits.exit {
                return fail_run(
                    &watching,
                    pid,
                    format!(
                        "guest exit did not complete within {} s of its grant (process {pid}); a guest exit is waiting on another guest; possible guest-served filesystem (NFS/9p/CIFS) or other exit-time dependency",
                        limits.exit.as_secs()
                    ),
                );
            }
            let Some(state) = (limits.exit_path_probe)(&watching.proc_dirs, pid) else {
                asleep_since.remove(&pid);
                continue;
            };
            let asleep = now.duration_since(*asleep_since.entry(pid).or_insert(now));
            if asleep >= limits.kernel_wait {
                return fail_run(
                    &watching,
                    pid,
                    format!(
                        "guest exit waited on a kernel dependency for over {} s (process {pid} in state {state}); possible guest-served network filesystem (unsupported topology)",
                        limits.kernel_wait.as_secs()
                    ),
                );
            }
        }
    }
}

/// Fails the run: the named diagnostic, Detcore's `backend_failed`, and
/// `SIGKILL` to every watched process.
fn fail_run(watching: &Watching, pid: i32, diagnostic: String) {
    eprintln!("hermit: {diagnostic}");
    record_failure(&watching.outcome, diagnostic);
    watching.reporter.backend_failed(reverie::BackendFailure {
        pid: reverie::Pid::from_raw(pid),
        tid: reverie::Tid::from_raw(pid),
        phase: WATCHDOG_PHASE,
    });
    if let Err(error) = watching.watch.fail_and_kill_all() {
        eprintln!(
            "hermit: in-guest LiteInst could not kill every guest after that failure: {error}"
        );
    }
}

/// `Some(state)` when `pid` sleeps (`D` or `S`) inside the kernel's exit path.
fn kernel_wait_state(proc_dirs: &ProcDirs, pid: i32) -> Option<char> {
    let proc_dirs = proc_dirs.lock().unwrap();
    let dir = proc_dirs.get(&pid)?;
    // Through the directory opened at admission, never a fresh `/proc/<pid>`,
    // so a reused pid is never read. A reaped process reads nothing.
    let stat = std::fs::read_to_string(format!("/proc/self/fd/{}/stat", dir.as_raw_fd())).ok()?;
    exit_path_sleep(&stat)
}

/// Parses `/proc/<pid>/stat`: `Some(state)` for state `D` or `S` with
/// `PF_EXITING` set. `PF_EXITING` is what tells a kernel wait during the exit
/// from a guest that, after its exit grant, still waits in `S` for the
/// coordinator's reply before its native exit.
fn exit_path_sleep(stat: &str) -> Option<char> {
    // The command name may contain spaces and parentheses; it ends at the last ')'.
    let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let flags: u64 = fields.get(6)?.parse().ok()?;
    (matches!(state, 'D' | 'S') && flags & PF_EXITING != 0).then_some(state)
}

/// The value of the pidfd's fdinfo field `name` (for example `Pid:`).
fn fdinfo_field(pidfd: &OwnedFd, name: &str) -> io::Result<Option<String>> {
    let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))?;
    Ok(fdinfo
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .map(|value| value.trim().to_owned()))
}

fn parse_visible_pid(value: &str) -> io::Result<i32> {
    let pid: i32 = value
        .parse()
        .map_err(|error| io::Error::other(format!("pidfd fdinfo process id {value:?}: {error}")))?;
    if pid <= 0 {
        return Err(io::Error::other(format!(
            "the connecting process is not visible or already gone (pid {pid})"
        )));
    }
    Ok(pid)
}

/// The pid of `pidfd`'s process as the guest sees it: the last (innermost)
/// field of the pidfd's `NSpid:` fdinfo line, or its `Pid:` line on a kernel
/// that prints no `NSpid:`. A process that is not visible (`0`) or already
/// reaped (`-1`) is refused.
fn guest_visible_pid(pidfd: &OwnedFd) -> io::Result<i32> {
    let value = match fdinfo_field(pidfd, "NSpid:")? {
        Some(nspid) => nspid.split_whitespace().last().map(str::to_owned),
        None => fdinfo_field(pidfd, "Pid:")?,
    }
    .ok_or_else(|| io::Error::other("pidfd fdinfo names no process id"))?;
    parse_visible_pid(&value)
}

/// Opens `pidfd`'s process's `/proc` directory. Its `Pid:` is read again after
/// the open: unchanged, the process was not reaped before the open, so the
/// directory is that process's and not a reused pid's.
fn open_proc_dir(pidfd: &OwnedFd) -> io::Result<File> {
    let pid = |pidfd| {
        fdinfo_field(pidfd, "Pid:")?
            .ok_or_else(|| io::Error::other("pidfd fdinfo has no Pid: line"))
            .and_then(|value| parse_visible_pid(&value))
    };
    let before = pid(pidfd)?;
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(format!("/proc/{before}"))?;
    let after = pid(pidfd)?;
    if after != before {
        return Err(io::Error::other(format!(
            "the connecting process {before} was reaped while it was admitted"
        )));
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;
    use std::process::Command;

    use super::*;

    fn pidfd_open(pid: u32) -> OwnedFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(fd as i32) }
    }

    /// Kills and reaps its child however the test ends.
    struct Reaped(Child);

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Stands in for the coordinator's global tool.
    #[derive(Default)]
    struct FakeTool {
        pending: Mutex<Vec<i32>>,
        exited: Mutex<Vec<i32>>,
        failures: Mutex<Vec<(i32, &'static str)>>,
        panic_on_exit: bool,
    }

    impl ExitReporter for FakeTool {
        fn process_exited(&self, pid: i32) {
            assert!(!self.panic_on_exit, "exit report failure under test");
            self.exited.lock().unwrap().push(pid);
            self.pending.lock().unwrap().retain(|&held| held != pid);
        }

        fn pending_process_exits(&self) -> Vec<i32> {
            self.pending.lock().unwrap().clone()
        }

        fn backend_failed(&self, failure: reverie::BackendFailure) {
            self.failures
                .lock()
                .unwrap()
                .push((failure.pid.as_raw(), failure.phase));
        }
    }

    /// Native controls take scheduler -> registry -> watch identity. A failed
    /// admission must not invoke the scheduler callback while owning `started`,
    /// which would close the reverse edge and deadlock the live native run.
    #[tokio::test]
    async fn failure_callback_runs_without_the_admission_start_lock() {
        use std::sync::Weak;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        struct LockProbe {
            admission: Weak<InGuestExitAdmission>,
            calls: AtomicUsize,
            unlocked: AtomicBool,
        }
        impl ExitReporter for LockProbe {
            fn process_exited(&self, _: i32) {}
            fn pending_process_exits(&self) -> Vec<i32> {
                Vec::new()
            }
            fn backend_failed(&self, _: reverie::BackendFailure) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let owner = self.admission.upgrade().expect("live admission");
                self.unlocked
                    .store(owner.started.try_lock().is_ok(), Ordering::SeqCst);
            }
        }
        let admission = Arc::new(InGuestExitAdmission::with_limits(PRODUCTION));
        let probe = Arc::new(LockProbe {
            admission: Arc::downgrade(&admission),
            calls: AtomicUsize::new(0),
            unlocked: AtomicBool::new(false),
        });
        admission.attach_exit_reporter(probe.clone());
        admission.fail_launch(&"native lock-order regression control");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert!(
            probe.unlocked.load(Ordering::SeqCst),
            "backend_failed invoked with admission.started held"
        );
        assert!(admission.failure().is_some());
        admission.drain(Duration::from_secs(30)).await.unwrap();
    }

    fn admission(limits: Limits, tool: &Arc<FakeTool>) -> InGuestExitAdmission {
        let admission = InGuestExitAdmission::with_limits(limits);
        admission.attach_exit_reporter(Arc::clone(tool) as Arc<dyn ExitReporter>);
        admission
    }

    const PRODUCTION: Limits = Limits {
        exit: EXIT_WATCHDOG,
        kernel_wait: KERNEL_WAIT,
        exit_path_probe: kernel_wait_state,
    };

    /// Waits up to `limit` for the fake tool to record a backend failure.
    async fn failure_within(tool: &FakeTool, limit: Duration) -> Option<(i32, &'static str)> {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(&failure) = tool.failures.lock().unwrap().first() {
                return Some(failure);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    #[test]
    fn the_guest_visible_pid_is_read_from_the_pidfd() {
        let me = pidfd_open(std::process::id());
        assert_eq!(guest_visible_pid(&me).unwrap(), std::process::id() as i32);
        let dir = open_proc_dir(&me).unwrap();
        let stat =
            std::fs::read_to_string(format!("/proc/self/fd/{}/stat", dir.as_raw_fd())).unwrap();
        assert!(stat.starts_with(&format!("{} (", std::process::id())));
    }

    #[test]
    fn only_a_sleep_inside_the_exit_path_is_a_kernel_wait() {
        let stat = |state: &str, flags: u64| {
            format!("42 (a (tricky) name) {state} 1 42 42 0 -1 {flags} 0 0 0 0 0 0")
        };
        assert_eq!(exit_path_sleep(&stat("S", 0x404044)), Some('S'));
        assert_eq!(exit_path_sleep(&stat("D", 0x4)), Some('D'));
        // Asleep but not exiting: a guest waiting for a reply before its exit.
        assert_eq!(exit_path_sleep(&stat("S", 0x400040)), None);
        // Exiting but running, a zombie, or stopped.
        assert_eq!(exit_path_sleep(&stat("R", 0x4)), None);
        assert_eq!(exit_path_sleep(&stat("Z", 0x4)), None);
        assert_eq!(exit_path_sleep(&stat("t", 0x4)), None);
        assert_eq!(exit_path_sleep("garbage"), None);
    }

    #[tokio::test]
    async fn an_admitted_process_is_reported_once_after_it_exits() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut child = Reaped(Command::new("true").spawn().unwrap());
        let pid = child.0.id() as i32;
        let admitted = admission.admit(pidfd_open(child.0.id())).unwrap();
        assert_eq!(admitted.process_id, pid);
        admission.drain(Duration::from_secs(30)).await.unwrap();
        assert_eq!(*tool.exited.lock().unwrap(), [pid]);
        assert_eq!(admission.reported_exits(), [pid]);
        child.0.wait().unwrap();
        assert!(tool.failures.lock().unwrap().is_empty());
        assert_eq!(admission.failure(), None);
        assert!(
            admission.admit(pidfd_open(std::process::id())).is_err(),
            "a drained admission admitted a connection"
        );
        assert!(
            admission.failure().is_some(),
            "a refused admission must fail the run"
        );
    }

    #[tokio::test]
    async fn a_refused_admission_fails_the_run_at_once() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        // A connection whose process is already reaped cannot be admitted.
        let mut child = Command::new("true").spawn().unwrap();
        let pidfd = pidfd_open(child.id());
        child.wait().unwrap();
        assert!(admission.admit(pidfd).is_err());
        assert_eq!(
            *tool.failures.lock().unwrap(),
            [(0, ADMISSION_PHASE)],
            "the refusal must reach the scheduler as a backend failure"
        );
        let failure = admission
            .failure()
            .expect("a refused admission is a run failure");
        assert!(
            failure.starts_with("in-guest LiteInst refused a guest connection:"),
            "{failure}"
        );
        admission.drain(Duration::from_secs(30)).await.unwrap();
    }

    #[test]
    fn admission_before_the_reporter_is_refused() {
        let admission = InGuestExitAdmission::default();
        let me = pidfd_open(std::process::id());
        assert!(admission.admit(me).is_err());
    }

    /// A connection the server could not offer for admission (its peer pidfd
    /// could not be obtained) fails the run like a refused one (Codex
    /// verification of 823ae24c, HIGH).
    #[tokio::test]
    async fn a_connection_without_a_peer_pidfd_fails_the_run_at_once() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        admission.admission_failed(&io::Error::from_raw_os_error(libc::EMFILE));
        assert_eq!(*tool.failures.lock().unwrap(), [(0, ADMISSION_PHASE)]);
        let failure = admission
            .failure()
            .expect("an unadmittable connection is a run failure");
        assert!(
            failure.starts_with("in-guest LiteInst refused a guest connection:"),
            "{failure}"
        );
        admission.drain(Duration::from_secs(30)).await.unwrap();
    }

    /// A coordinator failure (for example, its RPC listener stopped) fails
    /// the run and kills every guest, including one admitted afterwards, so
    /// no guest outlives the run (Codex review of 2e52514d, both HIGH).
    #[tokio::test]
    async fn a_coordinator_failure_kills_every_guest_including_a_later_one() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut early = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        admission.admit(pidfd_open(early.0.id())).unwrap();
        admission.fail_launch(&"the RPC listener failed");
        let mut late = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        admission.admit(pidfd_open(late.0.id())).unwrap();
        admission.drain(Duration::from_secs(30)).await.unwrap();
        let mut reported = admission.reported_exits();
        reported.sort_unstable();
        let mut expected = vec![early.0.id() as i32, late.0.id() as i32];
        expected.sort_unstable();
        assert_eq!(reported, expected);
        assert_eq!(early.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(late.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert_eq!(*tool.failures.lock().unwrap(), [(0, COORDINATOR_PHASE)]);
        let failure = admission
            .failure()
            .expect("a coordinator failure is a run failure");
        assert!(
            failure.starts_with("in-guest LiteInst coordinator failed:"),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn the_watchdog_fails_the_run_for_an_exit_that_never_completes() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(
            Limits {
                exit: Duration::from_millis(300),
                ..PRODUCTION
            },
            &tool,
        );
        // A guest stopped between its exit grant and its native exit.
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        admission.admit(pidfd_open(child.0.id())).unwrap();
        tool.pending.lock().unwrap().push(pid);
        assert_eq!(
            failure_within(&tool, Duration::from_secs(20)).await,
            Some((pid, WATCHDOG_PHASE))
        );
        // The guests are killed, so the run cannot produce a matching result,
        // and the kill is an exit the watcher still reports.
        assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        admission.drain(Duration::from_secs(30)).await.unwrap();
        assert_eq!(*tool.exited.lock().unwrap(), [pid]);
        assert_eq!(tool.failures.lock().unwrap().len(), 1);
        // The failure persists for the launcher, which fails the run with it.
        let failure = admission
            .failure()
            .expect("a fired watchdog is a run failure");
        assert!(failure.contains("did not complete within"), "{failure}");
    }

    #[tokio::test]
    async fn a_sleeping_process_that_is_not_exiting_is_not_a_kernel_wait() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        // Asleep in S for longer than the kernel-wait limit, but not exiting:
        // the shape of a guest waiting for the coordinator's reply after its
        // exit grant.
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        admission.admit(pidfd_open(child.0.id())).unwrap();
        tool.pending.lock().unwrap().push(pid);
        assert_eq!(
            failure_within(&tool, KERNEL_WAIT + Duration::from_millis(800)).await,
            None
        );
        child.0.kill().unwrap();
        admission.drain(Duration::from_secs(30)).await.unwrap();
        assert_eq!(*tool.exited.lock().unwrap(), [pid]);
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn the_kernel_wait_check_fails_the_run_after_one_second_asleep_in_the_exit_path() {
        // No unprivileged test can put a process to sleep inside do_exit: the
        // real causes need a network or FUSE mount, and a lingering socket
        // close is skipped for an exiting task (inet_release checks
        // PF_EXITING). So the probe stands in for /proc here; reading and
        // parsing the real stat are tested above and below.
        fn asleep_in_exit(_: &ProcDirs, _: i32) -> Option<char> {
            Some('S')
        }
        let tool = Arc::new(FakeTool::default());
        let admission = admission(
            Limits {
                exit_path_probe: asleep_in_exit,
                ..PRODUCTION
            },
            &tool,
        );
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        admission.admit(pidfd_open(child.0.id())).unwrap();
        let held = Instant::now();
        tool.pending.lock().unwrap().push(pid);
        assert_eq!(
            failure_within(&tool, Duration::from_secs(20)).await,
            Some((pid, WATCHDOG_PHASE))
        );
        let waited = held.elapsed();
        assert!(waited >= KERNEL_WAIT, "failed the run early: {waited:?}");
        assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        admission.drain(Duration::from_secs(30)).await.unwrap();
        let failure = admission.failure().expect("a kernel wait is a run failure");
        assert!(
            failure.contains("waited on a kernel dependency"),
            "{failure}"
        );
    }

    /// Set by the brief-sleep test: whether its stand-in probe reports the
    /// held process asleep in its exit path.
    static ASLEEP_IN_EXIT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// A process held for over a second, but asleep in its exit path only
    /// briefly, is not a kernel wait: the sleep itself must last a second
    /// (Codex implementation review: a healthy delayed exit sampled once in a
    /// brief exit-path sleep must not fail the run).
    #[tokio::test]
    async fn a_long_hold_with_a_brief_exit_path_sleep_is_not_a_kernel_wait() {
        fn asleep_when_told(_: &ProcDirs, _: i32) -> Option<char> {
            ASLEEP_IN_EXIT
                .load(std::sync::atomic::Ordering::SeqCst)
                .then_some('D')
        }
        let tool = Arc::new(FakeTool::default());
        let admission = admission(
            Limits {
                exit_path_probe: asleep_when_told,
                ..PRODUCTION
            },
            &tool,
        );
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        admission.admit(pidfd_open(child.0.id())).unwrap();
        tool.pending.lock().unwrap().push(pid);
        // Held, and awake, for longer than the limit.
        tokio::time::sleep(KERNEL_WAIT + Duration::from_millis(300)).await;
        // Then asleep in the exit path: not yet a second of sleep.
        ASLEEP_IN_EXIT.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            failure_within(&tool, KERNEL_WAIT / 2).await,
            None,
            "a sleep shorter than the limit failed the run"
        );
        // Once the sleep itself lasts the limit, the check fires.
        assert_eq!(
            failure_within(&tool, Duration::from_secs(20)).await,
            Some((pid, WATCHDOG_PHASE))
        );
        ASLEEP_IN_EXIT.store(false, std::sync::atomic::Ordering::SeqCst);
        child.0.kill().unwrap();
        admission.drain(Duration::from_secs(30)).await.unwrap();
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn a_process_asleep_in_its_exit_path_for_under_a_second_is_not_failed() {
        fn asleep_in_exit(_: &ProcDirs, _: i32) -> Option<char> {
            Some('D')
        }
        let tool = Arc::new(FakeTool::default());
        let admission = admission(
            Limits {
                exit_path_probe: asleep_in_exit,
                ..PRODUCTION
            },
            &tool,
        );
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        admission.admit(pidfd_open(child.0.id())).unwrap();
        tool.pending.lock().unwrap().push(pid);
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The exit completes within the limit: the hold ends, nothing fails.
        tool.pending.lock().unwrap().clear();
        assert_eq!(
            failure_within(&tool, KERNEL_WAIT + Duration::from_millis(500)).await,
            None
        );
        child.0.kill().unwrap();
        admission.drain(Duration::from_secs(30)).await.unwrap();
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn a_failed_watcher_fails_the_run() {
        let tool = Arc::new(FakeTool {
            panic_on_exit: true,
            ..FakeTool::default()
        });
        let admission = admission(PRODUCTION, &tool);
        let mut child = Reaped(Command::new("true").spawn().unwrap());
        admission.admit(pidfd_open(child.0.id())).unwrap();
        assert_eq!(
            failure_within(&tool, Duration::from_secs(20)).await,
            Some((0, WATCHDOG_PHASE))
        );
        child.0.wait().unwrap();
        assert!(matches!(
            admission.drain(Duration::from_secs(30)).await,
            Err(DrainError::Watcher(_))
        ));
        let failure = admission
            .failure()
            .expect("a stopped watcher is a run failure");
        assert!(
            failure.contains("stopped observing guest exits"),
            "{failure}"
        );
    }
    #[tokio::test]
    async fn actual_native_completion_receipt_requires_one_watched_dead_birth() {
        use detcore::detlog::NativePhysicalExitCompletion;
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut child = Reaped(Command::new("true").spawn().unwrap());
        let pid = child.0.id() as i32;
        let held = pidfd_open(child.0.id());
        let identity = crate::physical_exit_watch::birth_identity(&held).unwrap();
        admission.admit(pidfd_open(child.0.id())).unwrap();
        let receipt = admission
            .drain_native_completion(Duration::from_secs(30), held, pid, identity)
            .await
            .unwrap();
        assert_eq!(receipt.process_id(), pid);
        assert_eq!(receipt.birth_identity(), identity);
        assert_eq!(admission.reported_exits(), [pid]);
        assert_eq!(*tool.exited.lock().unwrap(), [pid]);
        assert!(admission.failure().is_none());
        child.0.wait().unwrap();
        assert!(
            admission
                .drain_native_completion(
                    Duration::from_secs(1),
                    pidfd_open(std::process::id()),
                    std::process::id() as i32,
                    crate::physical_exit_watch::birth_identity(&pidfd_open(std::process::id()))
                        .unwrap()
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn actual_native_receipt_refuses_unwatched_or_wrong_birth() {
        let tool = Arc::new(FakeTool::default());
        let unstarted = InGuestExitAdmission::default();
        let held = pidfd_open(std::process::id());
        let identity = crate::physical_exit_watch::birth_identity(&held).unwrap();
        assert!(
            unstarted
                .drain_native_completion(
                    Duration::from_millis(1),
                    held,
                    std::process::id() as i32,
                    identity
                )
                .await
                .is_err()
        );
        let watch = admission(PRODUCTION, &tool);
        assert!(
            watch
                .drain_native_completion(
                    Duration::from_millis(1),
                    pidfd_open(std::process::id()),
                    std::process::id() as i32,
                    identity ^ 1
                )
                .await
                .is_err()
        );
        assert!(tool.exited.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn native_completion_refuses_another_dead_pidfd_with_a_watched_pid_number() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut watched = Reaped(Command::new("true").spawn().unwrap());
        let mut unrelated = Reaped(Command::new("true").spawn().unwrap());
        let held_other = pidfd_open(unrelated.0.id());
        let other_identity = crate::physical_exit_watch::birth_identity(&held_other).unwrap();
        let watched_fd = pidfd_open(watched.0.id());
        assert_ne!(
            crate::physical_exit_watch::birth_identity(&watched_fd),
            Some(other_identity)
        );
        admission.admit(watched_fd).unwrap();
        unrelated.0.wait().unwrap();
        // A dead pidfd and a real report with the supplied numeric PID still
        // name two different births. They must never produce a receipt.
        assert!(
            admission
                .drain_native_completion(
                    Duration::from_secs(30),
                    held_other,
                    watched.0.id() as i32,
                    other_identity
                )
                .await
                .is_err()
        );
        assert_eq!(admission.reported_exits(), [watched.0.id() as i32]);
        watched.0.wait().unwrap();
    }

    #[tokio::test]
    async fn native_completion_cannot_be_reissued_after_reporter_reattachment() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut child = Reaped(Command::new("true").spawn().unwrap());
        let pid = child.0.id() as i32;
        let held = pidfd_open(child.0.id());
        let repeated = pidfd_open(child.0.id());
        let identity = crate::physical_exit_watch::birth_identity(&held).unwrap();
        admission.admit(pidfd_open(child.0.id())).unwrap();
        let _receipt = admission
            .drain_native_completion(Duration::from_secs(30), held, pid, identity)
            .await
            .unwrap();
        admission.attach_exit_reporter(tool.clone());
        assert!(
            admission
                .drain_native_completion(Duration::from_secs(1), repeated, pid, identity)
                .await
                .is_err()
        );
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn native_source_identity_requires_the_actual_single_living_watch_registration() {
        use detcore::detlog::NativePhysicalWatchIdentity;
        let tool = Arc::new(FakeTool::default());
        let admission = Arc::new(admission(PRODUCTION, &tool));
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let other = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        assert!(
            admission
                .retain_native_process(pidfd_open(child.0.id()))
                .is_err()
        );
        admission.admit(pidfd_open(child.0.id())).unwrap();
        assert!(
            admission
                .retain_native_process(pidfd_open(other.0.id()))
                .is_err()
        );
        let retained = admission
            .retain_native_process(pidfd_open(child.0.id()))
            .unwrap();
        assert_eq!(retained.process_id(), pid);
        assert_eq!(
            retained.birth_identity(),
            crate::physical_exit_watch::birth_identity(&pidfd_open(child.0.id())).unwrap()
        );
        assert!(retained.watch_is_healthy());
        let final_fd = pidfd_open(child.0.id());
        child.0.kill().unwrap();
        let _completed = admission
            .drain_native_completion(
                Duration::from_secs(30),
                final_fd,
                pid,
                retained.birth_identity(),
            )
            .await
            .unwrap();
        assert!(!retained.watch_is_healthy());
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn native_watch_replacement_cannot_reuse_old_registration_or_reports() {
        use detcore::detlog::NativePhysicalWatchIdentity;
        let tool = Arc::new(FakeTool::default());
        let admission = Arc::new(admission(PRODUCTION, &tool));
        let mut child = Reaped(Command::new("sleep").arg("60").spawn().unwrap());
        let pid = child.0.id() as i32;
        admission.admit(pidfd_open(child.0.id())).unwrap();
        let original = admission
            .retain_native_process(pidfd_open(child.0.id()))
            .unwrap();
        let held = pidfd_open(child.0.id());
        let identity = original.birth_identity();
        admission.attach_exit_reporter(tool.clone());
        assert!(!original.watch_is_healthy());
        assert!(
            admission
                .retain_native_process(pidfd_open(child.0.id()))
                .is_err()
        );
        child.0.kill().unwrap();
        // The retained original watcher really reports this death. The new
        // empty watcher must not borrow that report for its own completion.
        original
            .watching
            .watch
            .drain_async(Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(admission.reported_exits(), [pid]);
        assert!(
            admission
                .drain_native_completion(Duration::from_secs(1), held, pid, identity)
                .await
                .is_err()
        );
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn native_completion_failed_birth_attempt_consumes_the_same_watch_attempt() {
        let tool = Arc::new(FakeTool::default());
        let admission = admission(PRODUCTION, &tool);
        let mut child = Reaped(Command::new("true").spawn().unwrap());
        let pid = child.0.id() as i32;
        let first = pidfd_open(child.0.id());
        let second = pidfd_open(child.0.id());
        let identity = crate::physical_exit_watch::birth_identity(&first).unwrap();
        admission.admit(pidfd_open(child.0.id())).unwrap();
        assert!(
            admission
                .drain_native_completion(Duration::from_secs(30), first, pid, identity ^ 1)
                .await
                .is_err()
        );
        assert!(
            admission
                .drain_native_completion(Duration::from_secs(30), second, pid, identity)
                .await
                .is_err()
        );
        admission.drain(Duration::from_secs(30)).await.unwrap();
        child.0.wait().unwrap();
    }
}
