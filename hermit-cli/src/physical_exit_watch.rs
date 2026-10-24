/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Observes guest processes' physical exits through pidfds, for backends whose
//! processes leave the host some time after their scheduler-granted exit.
//!
//! Detcore holds only abstract lifetime state; how a process's exit is observed
//! on the host belongs to the backend (owner directive 082). A backend hands
//! this watcher one pidfd per guest process (from `pidfd_open`, or a socket's
//! `SO_PEERPIDFD`) together with the process id Detcore knows it by. The
//! watcher calls the backend's callback with that id exactly once, after the
//! exit is complete as the guest's parent can see it; a backend wires the
//! callback to `GlobalState::complete_physical_process_exit`.
//!
//! The pidfd alone is not enough. `exit_notify` runs under
//! `write_lock_irq(&tasklist_lock)`: it sets the zombie state, then
//! `do_notify_parent` wakes pidfd waiters and only then queues SIGCHLD to the
//! parent. So when the pidfd becomes readable, the watcher calls
//! `waitid(P_PIDFD, pidfd, WEXITED | WNOHANG | WNOWAIT)`. While the process is
//! unreaped, the kernel takes `read_lock(&tasklist_lock)` for that call
//! (`__do_wait`), which cannot be granted until `exit_notify`'s write section,
//! and with it the SIGCHLD queueing, has finished. A non-parent gets `ECHILD`
//! and nothing is consumed; `WNOWAIT` leaves a parent's zombie in place. If
//! the process was already reaped, `release_task` ran after that section
//! anyway. Either way, a parent probing its pending signals or children sees
//! the exit in every run once it is reported.
//!
//! The watcher never waits on or reaps a process: the guest's own parent, or
//! the backend's launcher for the root, stays the only reaper.
//!
//! [`PhysicalExitWatch::new`] watches on a thread of its own.
//! [`PhysicalExitWatch::on_runtime`] watches on an existing tokio runtime
//! instead, for a launcher that must not create a thread: in-guest LiteInst's
//! coordinator shares the guest's PID namespace, where every thread takes a
//! process id the guest would otherwise see.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Why [`PhysicalExitWatch::drain`] returned without every watched process gone.
#[derive(Debug)]
pub enum DrainError {
    /// Some watched processes had not exited when the timeout expired; their ids.
    TimedOut(Vec<i32>),
    /// The watcher failed, so exits are no longer observed.
    Watcher(io::Error),
}

struct Entry {
    /// Shared with the entry's task in runtime mode.
    pidfd: Arc<OwnedFd>,
    raw_pid: i32,
    /// The process's birth identity, if the kernel provides one; see
    /// [`birth_identity`].
    identity: Option<u64>,
}

/// What [`PhysicalExitWatch::watch`] did with a registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watched {
    /// The process is now watched.
    New,
    /// A process with the same birth identity is already watched; this
    /// registration joined it, and its exit is reported once.
    Joined,
    /// A process with the same birth identity was already seen to exit; its
    /// exit was (or is being) reported, and is not reported again.
    AlreadyReported,
}

/// The filesystem magic of pidfs (`PIDFS_MAGIC`), Linux 6.9 and later.
const PIDFS_MAGIC: i64 = 0x5049_4446;

/// A process's birth identity: the inode number of a pidfd for it, when that
/// is a pidfs inode (Linux 6.9 and later), which is one per process and never
/// reused while the kernel runs. Before pidfs every pidfd shares one
/// anonymous inode, which names no process, so there is none. Every pidfd for
/// the same process has the same inode, however it was obtained
/// (`pidfd_open`, `SO_PEERPIDFD`, `CLONE_PIDFD`).
pub fn birth_identity(pidfd: &OwnedFd) -> Option<u64> {
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(pidfd.as_raw_fd(), &mut filesystem) } != 0 {
        return None;
    }
    // `f_type`'s width differs between targets.
    #[allow(clippy::unnecessary_cast)]
    if filesystem.f_type as i64 != PIDFS_MAGIC {
        return None;
    }
    let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::fstat(pidfd.as_raw_fd(), &mut metadata) } == 0).then_some(metadata.st_ino)
}

/// Returns once `entry`'s exit has been published to its parent; see the
/// module documentation for why this `waitid` call is the synchronization.
fn await_publication(entry: &Entry) -> io::Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                entry.pidfd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // Not our child, or already reaped: both after publication.
            Some(libc::ECHILD) => return Ok(()),
            Some(libc::EINTR) => continue,
            _ => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "waitid on the pidfd of exited process {}: {error}",
                        entry.raw_pid
                    ),
                ));
            }
        }
    }
}

#[derive(Default)]
struct State {
    /// Processes whose exit has not been observed yet.
    live: Vec<Entry>,
    /// Processes whose exit was observed but whose callback has not returned.
    delivering: Vec<i32>,
    /// Set by `Drop`; the thread exits at its next wakeup.
    shutdown: bool,
    /// The watcher's terminal error, if it stopped observing.
    failure: Option<io::Error>,
    /// Set by `fail_and_kill_all`: the run has failed, so a process watched
    /// from now on is killed as soon as it is registered.
    killing: bool,
    /// Birth identities whose exit was observed, for the rest of the run, so
    /// that a late registration for an exited process is recognized.
    exited: std::collections::HashSet<u64>,
}

impl State {
    /// `Some` once a drain can return: every report delivered, or a failure.
    fn drain_outcome(&self) -> Option<Result<(), DrainError>> {
        if let Some(failure) = &self.failure {
            return Some(Err(DrainError::Watcher(io::Error::new(
                failure.kind(),
                failure.to_string(),
            ))));
        }
        (self.live.is_empty() && self.delivering.is_empty()).then_some(Ok(()))
    }

    fn pending(&self) -> Vec<i32> {
        let mut pending: Vec<i32> = self.live.iter().map(|entry| entry.raw_pid).collect();
        pending.extend(&self.delivering);
        pending
    }

    fn fail(&mut self, error: io::Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
}

struct Shared {
    state: Mutex<State>,
    /// Notified whenever a callback returns or the watcher fails.
    changed: Condvar,
    /// The same notification for [`PhysicalExitWatch::drain_async`].
    changed_async: tokio::sync::Notify,
    /// Wakes the watcher thread's poll after `live` grows or on shutdown.
    wake: OwnedFd,
}

impl Shared {
    fn notify_changed(&self) {
        self.changed.notify_all();
        self.changed_async.notify_waiters();
    }

    fn record_failure(&self, error: io::Error) {
        self.state.lock().unwrap().fail(error);
        self.notify_changed();
    }

    /// Ends `raw_pid`'s delivery after its callback returned.
    fn delivered(&self, raw_pid: i32) {
        let mut state = self.state.lock().unwrap();
        if let Some(index) = state.delivering.iter().position(|&pid| pid == raw_pid) {
            state.delivering.remove(index);
        }
        drop(state);
        self.notify_changed();
    }

    fn wake_thread(&self) {
        let one: u64 = 1;
        // An eventfd write only fails if the counter would overflow, which
        // still leaves the descriptor readable, so the wakeup is not lost.
        let _ = unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
    }
}

type ExitCallback = Arc<dyn Fn(i32) + Send + Sync>;

/// How the watched pidfds are polled.
enum Driver {
    /// One thread polls every pidfd.
    Thread(Option<JoinHandle<()>>),
    /// One task per pidfd on an existing tokio runtime.
    Runtime {
        handle: tokio::runtime::Handle,
        on_exit: ExitCallback,
        tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    },
}

/// A watcher over a set of pidfds. See the module documentation.
pub struct PhysicalExitWatch {
    shared: Arc<Shared>,
    driver: Driver,
}

impl PhysicalExitWatch {
    fn shared() -> io::Result<Arc<Shared>> {
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            changed_async: tokio::sync::Notify::new(),
            wake: unsafe { OwnedFd::from_raw_fd(wake) },
        }))
    }

    /// Watches on `handle`'s runtime and creates no thread. `on_exit` is called
    /// on that runtime, once per watched process, with the id given to
    /// [`PhysicalExitWatch::watch`]. Wait for the reports with
    /// [`PhysicalExitWatch::drain_async`]: on a current-thread runtime the
    /// blocking [`PhysicalExitWatch::drain`] would stop the very tasks it
    /// waits for.
    pub fn on_runtime(
        handle: tokio::runtime::Handle,
        on_exit: impl Fn(i32) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Ok(Self {
            shared: Self::shared()?,
            driver: Driver::Runtime {
                handle,
                on_exit: Arc::new(on_exit),
                tasks: Mutex::new(Vec::new()),
            },
        })
    }

    /// Starts the watcher thread. `on_exit` is called on that thread, once per
    /// watched process, with the id given to [`PhysicalExitWatch::watch`].
    pub fn new(on_exit: impl Fn(i32) + Send + 'static) -> io::Result<Self> {
        let shared = Self::shared()?;
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("physical-exit-watch".to_string())
            .spawn(move || {
                // A panicking callback must not lose reports silently: record
                // it as the watcher's terminal failure, which `drain` returns
                // and `watch` refuses on.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    watch_loop(&thread_shared, &on_exit)
                }));
                let failure = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some(io::Error::other("the physical-exit callback panicked")),
                };
                if let Some(error) = failure {
                    thread_shared.record_failure(error);
                }
            })?;
        Ok(Self {
            shared,
            driver: Driver::Thread(Some(thread)),
        })
    }

    /// Watches one process. `pidfd` must refer to it; `raw_pid` is the id the
    /// callback reports. A process that has already exited is reported at once.
    ///
    /// Registrations are keyed by birth identity where the kernel provides
    /// one: a second registration for the same process (for example its
    /// admission and its parent's creation report, in either order) joins the
    /// first and is reported once, and one for a process already seen to exit
    /// is not reported again. So a second registration cannot add a watch
    /// for a process already watched or already reported, even when its pid
    /// has been reused. (The report itself still carries only `raw_pid`;
    /// carrying the identity through to the completion is later work.)
    pub fn watch(&self, pidfd: OwnedFd, raw_pid: i32) -> io::Result<Watched> {
        let identity = birth_identity(&pidfd);
        let mut state = self.shared.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(io::Error::new(failure.kind(), failure.to_string()));
        }
        if let Some(identity) = identity {
            if state.exited.contains(&identity) {
                return Ok(Watched::AlreadyReported);
            }
            if state
                .live
                .iter()
                .any(|entry| entry.identity == Some(identity))
            {
                return Ok(Watched::Joined);
            }
        }
        let pidfd = Arc::new(pidfd);
        state.live.push(Entry {
            pidfd: Arc::clone(&pidfd),
            raw_pid,
            identity,
        });
        let killing = state.killing;
        drop(state);
        if killing {
            // Still watched, so its exit is reported and drained like any other.
            if let Err(error) = send_sigkill(&pidfd, raw_pid) {
                self.shared.record_failure(error);
            }
        }
        match &self.driver {
            Driver::Thread(_) => self.shared.wake_thread(),
            Driver::Runtime {
                handle,
                on_exit,
                tasks,
            } => {
                let shared = Arc::clone(&self.shared);
                let on_exit = Arc::clone(on_exit);
                let task = handle.spawn(async move {
                    if let Err(error) = watch_one(&shared, pidfd, raw_pid, &on_exit).await {
                        shared.record_failure(error);
                    }
                });
                let mut tasks = tasks.lock().unwrap();
                tasks.retain(|task| !task.is_finished());
                tasks.push(task);
            }
        }
        Ok(Watched::New)
    }

    /// The watcher's terminal failure, if it has stopped observing exits.
    pub fn failure(&self) -> Option<io::Error> {
        let state = self.shared.state.lock().unwrap();
        state
            .failure
            .as_ref()
            .map(|failure| io::Error::new(failure.kind(), failure.to_string()))
    }

    /// Sends `SIGKILL` to every watched process that has not exited yet, and
    /// to every process watched from now on. Used when a run has failed, so
    /// that no guest outlives it, including one admitted after the failure;
    /// the exits that follow are reported as usual. `SIGKILL` also ends a
    /// stopped process. Every process is attempted; the first error other
    /// than `ESRCH` (the process already exited, so its report is on the way)
    /// is returned.
    pub fn fail_and_kill_all(&self) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        state.killing = true;
        let mut first_error = None;
        for entry in &state.live {
            if let Err(error) = send_sigkill(&entry.pidfd, entry.raw_pid) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Waits until every watched process has exited and its callback has
    /// returned, or until `timeout` expires. Blocks the calling thread, so a
    /// runtime-mode watcher is drained with [`PhysicalExitWatch::drain_async`].
    pub fn drain(&self, timeout: Duration) -> Result<(), DrainError> {
        // An unrepresentable deadline (for example Duration::MAX) waits forever.
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(outcome) = state.drain_outcome() {
                return outcome;
            }
            let Some(deadline) = deadline else {
                state = self.shared.changed.wait(state).unwrap();
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                return Err(DrainError::TimedOut(state.pending()));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }

    /// [`PhysicalExitWatch::drain`] without blocking the thread, so the
    /// watcher's own tasks keep running when it shares a runtime with the
    /// caller.
    pub async fn drain_async(&self, timeout: Duration) -> Result<(), DrainError> {
        let deadline = tokio::time::Instant::now().checked_add(timeout);
        loop {
            // Registered before the state is read, so a change in between
            // still wakes this wait.
            let changed = self.shared.changed_async.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.shared.state.lock().unwrap();
                if let Some(outcome) = state.drain_outcome() {
                    return outcome;
                }
                if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                    return Err(DrainError::TimedOut(state.pending()));
                }
            }
            match deadline {
                Some(deadline) => {
                    let _ = tokio::time::timeout_at(deadline, changed).await;
                }
                None => changed.await,
            }
        }
    }
}

impl Drop for PhysicalExitWatch {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        match &mut self.driver {
            Driver::Thread(thread) => {
                self.shared.wake_thread();
                if let Some(thread) = thread.take() {
                    let _ = thread.join();
                }
            }
            Driver::Runtime { tasks, .. } => {
                for task in tasks.get_mut().unwrap().drain(..) {
                    task.abort();
                }
            }
        }
    }
}

/// Runtime mode: observes one process's exit and reports it.
/// `SIGKILL` through `pidfd`. `ESRCH` (the process already exited, so its
/// report is on the way) is not an error.
fn send_sigkill(pidfd: &OwnedFd, raw_pid: i32) -> io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(io::Error::new(
                error.kind(),
                format!("SIGKILL to watched process {raw_pid}: {error}"),
            ));
        }
    }
    Ok(())
}

async fn watch_one(
    shared: &Shared,
    pidfd: Arc<OwnedFd>,
    raw_pid: i32,
    on_exit: &ExitCallback,
) -> io::Result<()> {
    // A pidfd is readable once its process has exited; POLLHUP after it is
    // reaped is reported as readable too. The registration is dropped before
    // the entry is.
    {
        let registration = AsyncFd::with_interest(Arc::clone(&pidfd), Interest::READABLE)?;
        let _ready = registration.readable().await?;
    }
    let entry = {
        let mut state = shared.state.lock().unwrap();
        let Some(index) = state
            .live
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.pidfd, &pidfd))
        else {
            return Ok(());
        };
        let entry = state.live.remove(index);
        state.exited.extend(entry.identity);
        state.delivering.push(raw_pid);
        entry
    };
    await_publication(&entry)?;
    // As on the thread: a panicking callback becomes the watcher's failure.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| on_exit(raw_pid)))
        .map_err(|_| io::Error::other("the physical-exit callback panicked"))?;
    shared.delivered(raw_pid);
    Ok(())
}

fn watch_loop(shared: &Shared, on_exit: &impl Fn(i32)) -> io::Result<()> {
    loop {
        // Snapshot the descriptors to poll. Entries are only removed by this
        // thread, so each index stays valid until the removal below.
        let mut fds = vec![libc::pollfd {
            fd: shared.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        {
            let state = shared.state.lock().unwrap();
            if state.shutdown {
                return Ok(());
            }
            fds.extend(state.live.iter().map(|entry| libc::pollfd {
                fd: entry.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
        }
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[0].revents != 0 {
            let mut counter: u64 = 0;
            let _ = unsafe {
                libc::read(
                    shared.wake.as_raw_fd(),
                    &mut counter as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
        }
        let exited: Vec<libc::c_int> = fds[1..]
            .iter()
            .filter(|fd| fd.revents != 0)
            .map(|fd| fd.fd)
            .collect();
        if exited.is_empty() {
            continue;
        }
        // POLLIN is the exit; POLLHUP/POLLERR/POLLNVAL on a pidfd also mean
        // the process can no longer be observed alive, so all end the entry.
        let reported: Vec<Entry> = {
            let mut state = shared.state.lock().unwrap();
            let (reported, live): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut state.live)
                .into_iter()
                .partition(|entry| exited.contains(&entry.pidfd.as_raw_fd()));
            state.live = live;
            state
                .exited
                .extend(reported.iter().filter_map(|entry| entry.identity));
            state
                .delivering
                .extend(reported.iter().map(|entry| entry.raw_pid));
            reported
        };
        // Report outside the lock: the callback takes the scheduler's lock.
        // A process stays in `delivering` until its callback has returned, so
        // `drain` cannot succeed before the report has been delivered.
        for entry in reported {
            await_publication(&entry)?;
            on_exit(entry.raw_pid);
            shared.delivered(entry.raw_pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;
    use std::process::Command;
    use std::sync::mpsc;

    use super::*;

    fn pidfd_open(pid: u32) -> OwnedFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(fd as i32) }
    }

    /// Kills and reaps its child however the test ends. `Child::kill` does not
    /// signal a child that was already reaped, so a reused pid is never hit.
    struct Reaped(Child);

    impl Reaped {
        fn spawn(command: &mut Command) -> Self {
            Self(command.spawn().unwrap())
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The same guard for a raw `fork` child; `reap` disarms it.
    struct ForkedChild(Option<libc::pid_t>);

    impl ForkedChild {
        fn reap(&mut self) -> libc::c_int {
            let pid = self.0.take().unwrap();
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            status
        }
    }

    impl Drop for ForkedChild {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let mut status = 0;
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
            }
        }
    }

    #[test]
    fn exiting_child_is_reported_once_with_its_pid() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(Command::new("sh").args(["-c", "exit 3"]));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        watch.drain(Duration::from_secs(30)).unwrap();
        // The watcher never reaps: the exit status is still the parent's to collect.
        assert_eq!(child.0.wait().unwrap().code(), Some(3));
        drop(watch);
        assert!(
            receiver.try_recv().is_err(),
            "the exit was reported more than once"
        );
    }

    /// Whether this kernel's pidfds are pidfs inodes, decided from raw
    /// filesystem metadata rather than by [`birth_identity`], the function
    /// under test.
    fn pidfs_by_metadata() -> bool {
        let pidfd = pidfd_open(std::process::id());
        let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::fstatfs(pidfd.as_raw_fd(), &mut filesystem) },
            0
        );
        #[allow(clippy::unnecessary_cast)]
        let magic = filesystem.f_type as i64;
        magic == 0x5049_4446
    }

    /// Every pidfd for one process has the same birth identity, two processes
    /// have different ones, and a non-pidfd has none. Before pidfs (Linux
    /// 6.9) no pidfd has one.
    #[test]
    fn a_birth_identity_names_one_process() {
        let pidfs = pidfs_by_metadata();
        let first = Reaped::spawn(Command::new("sleep").arg("60"));
        let second = Reaped::spawn(Command::new("sleep").arg("60"));
        let (a, b) = (pidfd_open(first.pid()), pidfd_open(first.pid()));
        let other = pidfd_open(second.pid());
        let anonymous = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(anonymous >= 0);
        let anonymous = unsafe { OwnedFd::from_raw_fd(anonymous) };
        assert_eq!(birth_identity(&anonymous), None);
        if !pidfs {
            assert_eq!(birth_identity(&a), None);
            assert_eq!(birth_identity(&other), None);
            return;
        }
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(a.as_raw_fd(), &mut metadata) }, 0);
        let identity = metadata.st_ino;
        assert_eq!(birth_identity(&a), Some(identity));
        assert_eq!(birth_identity(&b), Some(identity));
        assert_ne!(birth_identity(&other), Some(identity));
        assert!(birth_identity(&other).is_some());
    }

    /// A second registration of the same process joins the first, and its
    /// exit is reported once.
    #[test]
    fn a_second_registration_of_a_process_joins_the_first() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(Command::new("sleep").arg("60"));
        let pid = child.pid();
        let keyed = pidfs_by_metadata();
        assert_eq!(
            watch.watch(pidfd_open(pid), pid as i32).unwrap(),
            Watched::New
        );
        let second = watch.watch(pidfd_open(pid), pid as i32).unwrap();
        assert_eq!(second, if keyed { Watched::Joined } else { Watched::New });
        child.0.kill().unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        let reports = receiver.try_iter().collect::<Vec<i32>>();
        let expected = if keyed {
            vec![pid as i32]
        } else {
            vec![pid as i32; 2]
        };
        assert_eq!(reports, expected);
    }

    /// A registration that arrives after the process's exit was observed is
    /// recognized and not reported again, even after the process was reaped.
    #[test]
    fn a_registration_after_the_exit_was_observed_is_not_reported_again() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(Command::new("sh").args(["-c", "exit 0"]));
        let pid = child.pid();
        let late = pidfd_open(pid);
        if !pidfs_by_metadata() {
            return;
        }
        assert_eq!(
            watch.watch(pidfd_open(pid), pid as i32).unwrap(),
            Watched::New
        );
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        watch.drain(Duration::from_secs(30)).unwrap();
        child.0.wait().unwrap();
        assert_eq!(
            watch.watch(late, pid as i32).unwrap(),
            Watched::AlreadyReported
        );
        watch.drain(Duration::from_secs(30)).unwrap();
        drop(watch);
        assert!(receiver.try_recv().is_err(), "the exit was reported again");
    }

    #[test]
    fn drain_honours_its_timeout() {
        let watch = PhysicalExitWatch::new(|_| {}).unwrap();
        let mut child = Reaped::spawn(Command::new("sleep").arg("60"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        let start = Instant::now();
        match watch.drain(Duration::from_millis(200)) {
            Err(DrainError::TimedOut(live)) => assert_eq!(live, [pid as i32]),
            other => panic!("expected a timeout, got {other:?}"),
        }
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(200),
            "returned early: {waited:?}"
        );
        assert!(waited < Duration::from_secs(20), "overran: {waited:?}");
        child.0.kill().unwrap();
        assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn drain_without_processes_accepts_an_unbounded_timeout() {
        let watch = PhysicalExitWatch::new(|_| {}).unwrap();
        watch.drain(Duration::MAX).unwrap();
    }

    #[test]
    fn drain_waits_for_the_callback_to_return() {
        // The process has exited, but the report has not been delivered until
        // the callback returns; drain must not succeed in between.
        let (started_sender, started) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel::<()>();
        let watch = PhysicalExitWatch::new(move |pid| {
            started_sender.send(pid).unwrap();
            let _ = release_receiver.recv();
        })
        .unwrap();
        // Rebound after `watch`, so it is dropped first if an assertion below
        // fails: the paused callback then returns and `watch` can join.
        let release = release;
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        match watch.drain(Duration::from_millis(200)) {
            Err(DrainError::TimedOut(pending)) => assert_eq!(pending, [pid as i32]),
            other => panic!("drain returned before the callback did: {other:?}"),
        }
        release.send(()).unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        child.0.wait().unwrap();
    }

    #[test]
    fn a_panicking_callback_fails_the_watcher() {
        let watch = PhysicalExitWatch::new(|_| panic!("callback failure under test")).unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        match watch.drain(Duration::from_secs(30)) {
            Err(DrainError::Watcher(_)) => {}
            other => panic!("expected the watcher's failure, got {other:?}"),
        }
        child.0.wait().unwrap();
        let other = Reaped::spawn(Command::new("sleep").arg("60"));
        assert!(
            watch
                .watch(pidfd_open(other.pid()), other.pid() as i32)
                .is_err(),
            "a failed watcher accepted a new process"
        );
    }

    #[test]
    fn kill_all_ends_every_watched_process() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut children: Vec<Reaped> = (0..3)
            .map(|_| Reaped::spawn(Command::new("sleep").arg("60")))
            .collect();
        // A stopped process is ended too: stop one and wait until it is stopped.
        let stopped = children[0].pid();
        assert_eq!(
            unsafe { libc::kill(stopped as libc::pid_t, libc::SIGSTOP) },
            0
        );
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                stopped,
                &mut info,
                libc::WSTOPPED | libc::WNOWAIT,
            )
        };
        assert_eq!(rc, 0, "waitid: {}", io::Error::last_os_error());
        assert_eq!(info.si_code, libc::CLD_STOPPED);
        for child in &children {
            watch
                .watch(pidfd_open(child.pid()), child.pid() as i32)
                .unwrap();
        }
        watch.fail_and_kill_all().unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        let mut reported: Vec<i32> = receiver.try_iter().collect();
        reported.sort_unstable();
        let mut expected: Vec<i32> = children.iter().map(|child| child.pid() as i32).collect();
        expected.sort_unstable();
        assert_eq!(reported, expected);
        for child in &mut children {
            assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        }
    }

    #[test]
    fn a_process_watched_after_kill_all_is_killed_and_reported() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        watch.fail_and_kill_all().unwrap();
        // Admitted after the run failed: it must not outlive the run.
        let mut late = Reaped::spawn(Command::new("sleep").arg("60"));
        watch
            .watch(pidfd_open(late.pid()), late.pid() as i32)
            .unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        assert_eq!(
            receiver.try_iter().collect::<Vec<i32>>(),
            [late.pid() as i32]
        );
        assert_eq!(late.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn already_exited_process_is_reported_at_once() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        let pidfd = pidfd_open(pid);
        // Wait for the exit without reaping, so the pidfd is readable before it is watched.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(rc, 0, "waitid: {}", io::Error::last_os_error());
        watch.watch(pidfd, pid as i32).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        child.0.wait().unwrap();
    }

    fn thread_count() -> usize {
        std::fs::read_dir("/proc/self/task").unwrap().count()
    }

    #[test]
    fn runtime_mode_reports_each_exit_once_and_creates_no_thread() {
        // Counting this process's threads needs a process no other test
        // shares, so the test re-runs itself alone in a fresh one.
        if std::env::var_os(INNER_RUN).is_none() {
            return run_in_fresh_process(
                "physical_exit_watch::tests::runtime_mode_reports_each_exit_once_and_creates_no_thread",
                false,
            );
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let threads = thread_count();
        runtime.block_on(async {
            let (sender, receiver) = mpsc::channel();
            let watch =
                PhysicalExitWatch::on_runtime(tokio::runtime::Handle::current(), move |pid| {
                    sender.send(pid).unwrap()
                })
                .unwrap();
            let mut children: Vec<Reaped> = (0..3)
                .map(|code| Reaped::spawn(Command::new("sh").args(["-c", &format!("exit {code}")])))
                .collect();
            for child in &children {
                watch
                    .watch(pidfd_open(child.pid()), child.pid() as i32)
                    .unwrap();
            }
            watch.drain_async(Duration::from_secs(30)).await.unwrap();
            assert_eq!(
                thread_count(),
                threads,
                "the runtime-mode watcher started a thread"
            );
            let mut reported: Vec<i32> = receiver.try_iter().collect();
            reported.sort_unstable();
            let mut expected: Vec<i32> = children.iter().map(|child| child.pid() as i32).collect();
            expected.sort_unstable();
            assert_eq!(reported, expected);
            // The watcher never reaps: each status is still the parent's to collect.
            for (code, child) in children.iter_mut().enumerate() {
                assert_eq!(child.0.wait().unwrap().code(), Some(code as i32));
            }
            drop(watch);
            assert!(
                receiver.try_recv().is_err(),
                "an exit was reported more than once"
            );
        });
    }

    #[tokio::test]
    async fn runtime_mode_drain_honours_its_timeout() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::on_runtime(tokio::runtime::Handle::current(), move |pid| {
            sender.send(pid).unwrap()
        })
        .unwrap();
        let mut sleeper = Reaped::spawn(Command::new("sleep").arg("60"));
        let pid = sleeper.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        let start = Instant::now();
        match watch.drain_async(Duration::from_millis(200)).await {
            Err(DrainError::TimedOut(live)) => assert_eq!(live, [pid as i32]),
            other => panic!("expected a timeout, got {other:?}"),
        }
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(200),
            "returned early: {waited:?}"
        );
        assert!(waited < Duration::from_secs(20), "overran: {waited:?}");
        assert!(receiver.try_recv().is_err(), "reported a live process");
        sleeper.0.kill().unwrap();
        watch.drain_async(Duration::from_secs(30)).await.unwrap();
        assert_eq!(receiver.try_recv().unwrap(), pid as i32);
        assert_eq!(sleeper.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[tokio::test]
    async fn runtime_mode_reports_an_already_exited_process() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::on_runtime(tokio::runtime::Handle::current(), move |pid| {
            sender.send(pid).unwrap()
        })
        .unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        let pidfd = pidfd_open(pid);
        // Readable before it is registered: the registration must still fire.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(rc, 0, "waitid: {}", io::Error::last_os_error());
        watch.watch(pidfd, pid as i32).unwrap();
        watch.drain_async(Duration::from_secs(30)).await.unwrap();
        assert_eq!(receiver.try_recv().unwrap(), pid as i32);
        child.0.wait().unwrap();
    }

    #[tokio::test]
    async fn runtime_mode_panicking_callback_fails_the_watcher() {
        let watch = PhysicalExitWatch::on_runtime(tokio::runtime::Handle::current(), |_| {
            panic!("callback failure under test")
        })
        .unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        match watch.drain_async(Duration::from_secs(30)).await {
            Err(DrainError::Watcher(_)) => {}
            other => panic!("expected the watcher's failure, got {other:?}"),
        }
        child.0.wait().unwrap();
        let other = Reaped::spawn(Command::new("sleep").arg("60"));
        assert!(
            watch
                .watch(pidfd_open(other.pid()), other.pid() as i32)
                .is_err(),
            "a failed watcher accepted a new process"
        );
    }

    /// Set in a test's own fresh process; see [`run_in_fresh_process`].
    const INNER_RUN: &str = "HERMIT_PHYSICAL_EXIT_WATCH_INNER_RUN";

    /// Re-runs the test `name` alone in a fresh process of this test binary and
    /// requires it to pass. Used where the property needs a process no other
    /// test shares: under a threaded harness another test can fork a child
    /// that inherits this test's descriptors, or a harness thread can take a
    /// signal. With `block_sigchld`, the fresh process starts with SIGCHLD
    /// blocked in every thread (a blocked mask survives exec, and `pre_exec`
    /// runs after std resets the child's mask).
    fn run_in_fresh_process(name: &str, block_sigchld: bool) {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(INNER_RUN, "1");
        if block_sigchld {
            unsafe {
                command.pre_exec(|| {
                    let set = sigchld_set();
                    if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "inner run failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "inner run did not execute the test:\n{stdout}\n{stderr}"
        );
    }

    fn sigchld_set() -> libc::sigset_t {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
        }
        set
    }

    #[test]
    fn sigchld_is_queued_before_the_exit_is_reported() {
        // The kernel wakes pidfd waiters before it queues SIGCHLD to the
        // parent. The callback must find SIGCHLD already pending, observed
        // without any call that takes tasklist_lock itself. That needs every
        // thread of the process to block SIGCHLD, so nothing can dequeue it,
        // which this test process cannot arrange for its harness threads. So
        // the test re-runs itself in a fresh process whose initial mask blocks
        // SIGCHLD (a blocked mask survives exec, and pre_exec runs after std
        // resets the child's mask).
        if std::env::var_os(INNER_RUN).is_some() {
            return sigchld_inner();
        }
        run_in_fresh_process(
            "physical_exit_watch::tests::sigchld_is_queued_before_the_exit_is_reported",
            true,
        );
    }

    fn sigchld_inner() {
        let chld = sigchld_set();
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) };
        assert_eq!(
            unsafe { libc::sigismember(&mask, libc::SIGCHLD) },
            1,
            "the inner process must start with SIGCHLD blocked"
        );
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| {
            let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe { libc::sigpending(&mut pending) };
            let queued = unsafe { libc::sigismember(&pending, libc::SIGCHLD) } == 1;
            sender.send((pid, queued)).unwrap();
        })
        .unwrap();
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        for _ in 0..50 {
            let mut child = Reaped::spawn(&mut Command::new("true"));
            let pid = child.pid();
            watch.watch(pidfd_open(pid), pid as i32).unwrap();
            let (reported, queued) = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
            assert_eq!(reported, pid as i32);
            assert!(queued, "exit of {pid} reported before SIGCHLD was queued");
            child.0.wait().unwrap();
            // Consume this child's SIGCHLD before the next child.
            let signal = unsafe { libc::sigtimedwait(&chld, std::ptr::null_mut(), &zero) };
            assert_eq!(signal, libc::SIGCHLD);
        }
    }

    #[test]
    fn descriptors_are_closed_before_the_pidfd_is_readable() {
        // The property the scheduler relies on: once the exit is reported, a
        // pipe whose only writer was the exited process reads EOF. Run in a
        // fresh process, so no other test's fork can inherit the writer.
        if std::env::var_os(INNER_RUN).is_none() {
            return run_in_fresh_process(
                "physical_exit_watch::tests::descriptors_are_closed_before_the_pidfd_is_readable",
                false,
            );
        }
        let mut pipe = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read_end = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Child: only async-signal-safe calls. Keep the write end, exit.
            unsafe {
                libc::close(read_end.as_raw_fd());
                libc::_exit(0)
            };
        }
        let mut child = ForkedChild(Some(pid));
        drop(write_end);
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        watch.watch(pidfd_open(pid as u32), pid).unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(30)).unwrap(), pid);
        let mut byte = 0u8;
        let flags = unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL) };
        unsafe {
            libc::fcntl(
                read_end.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        };
        let n = unsafe {
            libc::read(
                read_end.as_raw_fd(),
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
            )
        };
        assert_eq!(
            n,
            0,
            "expected EOF, got {n} ({})",
            io::Error::last_os_error()
        );
        child.reap();
    }

    /// A parent that auto-reaps (SIGCHLD set to SIG_IGN, or SA_NOCLDWAIT)
    /// never holds a zombie: the kernel reaps the child as it exits. The exit
    /// is still reported exactly once, through the reaped (ECHILD) path of the
    /// publication barrier. The disposition is process-wide, so each case runs
    /// alone in a fresh process.
    #[test]
    fn a_child_of_a_parent_ignoring_sigchld_is_reported_once() {
        if std::env::var_os(INNER_RUN).is_none() {
            return run_in_fresh_process(
                "physical_exit_watch::tests::a_child_of_a_parent_ignoring_sigchld_is_reported_once",
                false,
            );
        }
        auto_reaped_inner(libc::SIG_IGN, 0);
    }

    #[test]
    fn a_child_of_a_parent_with_sa_nocldwait_is_reported_once() {
        if std::env::var_os(INNER_RUN).is_none() {
            return run_in_fresh_process(
                "physical_exit_watch::tests::a_child_of_a_parent_with_sa_nocldwait_is_reported_once",
                false,
            );
        }
        auto_reaped_inner(libc::SIG_DFL, libc::SA_NOCLDWAIT);
    }

    fn auto_reaped_inner(handler: libc::sighandler_t, flags: libc::c_int) {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        for _ in 0..20 {
            // The child waits on a pipe until it is watched, so it cannot be
            // reaped before its pidfd exists.
            let mut pipe = [0; 2];
            assert_eq!(
                unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
                0
            );
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                let mut byte = 0u8;
                unsafe {
                    libc::close(pipe[1]);
                    libc::read(pipe[0], (&raw mut byte).cast(), 1);
                    libc::_exit(0)
                };
            }
            unsafe { libc::close(pipe[0]) };
            watch.watch(pidfd_open(pid as u32), pid).unwrap();
            unsafe { libc::close(pipe[1]) };
            assert_eq!(receiver.recv_timeout(Duration::from_secs(30)).unwrap(), pid);
            // Auto-reaped: there is no zombie left to collect.
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, -1);
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
        watch.drain(Duration::from_secs(30)).unwrap();
        drop(watch);
        assert!(
            receiver.try_recv().is_err(),
            "an exit was reported more than once"
        );
    }
}
