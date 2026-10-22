/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Watch an owned GDB in an exec-started supervisor outside the guest namespace.
//!
//! The parent remains single-threaded while constructing/running the raw-clone
//! container. Only final-scope Drop may start a helper-reaper thread: the two
//! callers in replay and record_start return directly from that final replay or
//! its setup error. This is a CLI lifetime contract, not a generic fork-safe
//! background-process abstraction. A future caller must preserve that ordering.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;

use hermit::Context;
use hermit::Error;

use super::gdb_watch_helper as helper;

pub(super) mod lifecycle;

/// Existing diagnostic; a successful probe is still unauthenticated and may
/// reach a stranger. This change does not fix that separate reporting limitation.
pub const CLIENT_EXITED_BEFORE_CONNECTING: &str = "the gdb client hermit spawned exited before it finished connecting to the \
     gdbserver, so the replay had no debugger to serve";

type ReapOwner = Arc<Mutex<Option<Child>>>;
// Only exceptional cleanup refusals enter this process-lifetime owner store.
// Keeping the Child is not a successful reap; the diagnostic says so explicitly.
static UNCONFIRMED_REAPS: Mutex<Vec<ReapOwner>> = Mutex::new(Vec::new());

struct OwnerIdentity {
    pid: libc::pid_t,
    namespace: File,
    pidfd: OwnedFd,
}

impl OwnerIdentity {
    fn capture() -> Result<Self, Error> {
        // SAFETY: getpid and pidfd_open have no pointer arguments.
        let pid = unsafe { libc::getpid() };
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error()).context("open GDB watch parent pidfd");
        }
        // SAFETY: pidfd_open returned a new owned descriptor.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        Ok(Self {
            pid,
            namespace: File::open("/proc/thread-self/ns/pid")
                .context("retain GDB watch parent PID namespace")?,
            pidfd,
        })
    }

    fn is_current(&self) -> Result<bool, Error> {
        // Raw clone copies this entire object. A foreign copy must not send
        // Done, wait the parent's child, or start its own reaper thread.
        if unsafe { libc::getpid() } != self.pid || helper::parent_exited(self.pidfd.as_raw_fd())? {
            return Ok(false);
        }
        let current = std::fs::metadata("/proc/thread-self/ns/pid")
            .context("identify GDB watch caller PID namespace")?;
        let original = self.namespace.metadata()?;
        Ok(current.dev() == original.dev() && current.ino() == original.ino())
    }
}

fn retain_unconfirmed(owner: ReapOwner, error: impl std::fmt::Display) {
    eprintln!(
        "GDB helper reap is unconfirmed: {error}; retaining its child owner until process exit"
    );
    UNCONFIRMED_REAPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(owner);
}

fn reap_after_final_scope(child: Child, force_spawn_refusal: bool) {
    let owner = Arc::new(Mutex::new(Some(child)));
    let reaper_owner = Arc::clone(&owner);
    let reap = move || {
        let mut child = reaper_owner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("one helper reap owner");
        // Never hold the owner lock across wait, and never use waitpid(-1).
        if let Err(error) = child.wait() {
            *reaper_owner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(child);
            retain_unconfirmed(reaper_owner, error);
        }
    };
    let started = if force_spawn_refusal {
        // Test seam models Builder refusing and destroying its consumed closure.
        drop(reap);
        Err(io::Error::from_raw_os_error(libc::EAGAIN))
    } else {
        std::thread::Builder::new()
            .name("gdb-helper-reaper".into())
            .spawn(reap)
    };
    if let Err(error) = started {
        retain_unconfirmed(owner, error);
    }
}

/// Owns the helper and its private IPC, not the GDB Child (the helper owns that).
pub struct GdbClientWatch {
    owner: OwnerIdentity,
    helper: Option<Child>,
    stream: Option<UnixStream>,
    statuses: helper::StatusReader,
    done_sent: bool,
    finished: bool,
    client_exited_early: bool,
    #[cfg(test)]
    client_pid: u32,
    #[cfg(test)]
    helper_control_fd: i32,
    #[cfg(test)]
    client_exited_before_container_finished: bool,
    #[cfg(test)]
    force_reaper_spawn_refusal: bool,
}

impl GdbClientWatch {
    /// Start the helper before any container clone, then require confirmation
    /// that it owns the actual GDB. Both CLI command builders use arguments and
    /// inherited stdio; see ClientCommand's internal configuration contract.
    pub fn spawn(command: Command, port: u16) -> Result<Self, Error> {
        let owner = OwnerIdentity::capture()?;
        let (stream, helper_stream) =
            UnixStream::pair().context("create GDB helper control socket")?;
        let control = helper_stream.as_raw_fd();
        let parent = owner.pidfd.as_raw_fd();
        let mut launcher = helper::helper_command(control, parent);
        // The only pre_exec work is async-signal-safe fcntl on two owned FDs.
        // Neither descriptor is made inheritable in the original parent.
        unsafe {
            launcher.pre_exec(move || {
                for fd in [control, parent] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = launcher.spawn().context("start exec-owned GDB helper")?;
        drop(helper_stream);
        let mut watch = Self {
            owner,
            helper: Some(child),
            stream: Some(stream),
            statuses: helper::StatusReader::default(),
            done_sent: false,
            finished: false,
            client_exited_early: false,
            #[cfg(test)]
            client_pid: 0,
            #[cfg(test)]
            helper_control_fd: control,
            #[cfg(test)]
            client_exited_before_container_finished: false,
            #[cfg(test)]
            force_reaper_spawn_refusal: false,
        };
        helper::ClientCommand::new(command, port).send(watch.stream.as_mut().unwrap())?;
        let status = watch
            .statuses
            .next(watch.stream.as_mut().unwrap())?
            .context("missing GDB spawn status")?;
        match status.kind {
            helper::READY if status.value > 0 => {
                #[cfg(test)]
                {
                    watch.client_pid = status.value;
                }
                Ok(watch)
            }
            helper::SPAWN_FAILED => Err(io::Error::from_raw_os_error(status.value as i32))
                .context("Failed to run gdb command. Please make sure it is in your $PATH."),
            _ => anyhow::bail!("unexpected GDB helper startup status: {status:?}"),
        }
    }

    fn signal_done(&mut self) -> Result<(), Error> {
        if self.done_sent {
            return Ok(());
        }
        let stream = self
            .stream
            .as_ref()
            .context("GDB helper control socket is closed")?;
        let byte = [helper::DONE];
        loop {
            // One byte after the acknowledged command cannot fill our send
            // queue. DONTWAIT also keeps every Drop error path nonblocking.
            let sent = unsafe {
                libc::send(
                    stream.as_raw_fd(),
                    byte.as_ptr().cast(),
                    1,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if sent == 1 {
                self.done_sent = true;
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("notify GDB helper of container completion");
        }
    }

    fn accept_status(&mut self, status: helper::Status) -> Result<bool, Error> {
        match status.kind {
            helper::EXITED_BEFORE_DONE if status.value == 0 => {
                #[cfg(test)]
                {
                    self.client_exited_before_container_finished = true;
                }
            }
            helper::RELEASE_CONNECTED if status.value == 0 => self.client_exited_early = true,
            helper::FINISHED if status.value <= 1 => {
                anyhow::ensure!(
                    self.client_exited_early == (status.value != 0),
                    "inconsistent final GDB helper status"
                );
                return Ok(true);
            }
            helper::FAILED => anyhow::bail!(
                "GDB helper failed: {}",
                io::Error::from_raw_os_error(status.value as i32)
            ),
            _ => anyhow::bail!("unexpected GDB helper status: {status:?}"),
        }
        Ok(false)
    }

    /// Publish completion, wait for GDB and reap its helper. Interactive GDB is
    /// deliberately allowed to outlive the container; this wait is unchanged.
    /// Protocol failures are errors, never a silent 'not early' report.
    pub fn finish(&mut self) -> Result<bool, Error> {
        anyhow::ensure!(
            self.owner.is_current()?,
            "GDB watch used outside its owning process"
        );
        if self.finished {
            return Ok(self.client_exited_early);
        }
        self.signal_done()?;
        let observed: Result<bool, Error> = (|| {
            self.stream.as_mut().unwrap().set_nonblocking(false)?;
            loop {
                let status = self
                    .statuses
                    .next(self.stream.as_mut().unwrap())?
                    .context("missing final GDB helper status")?;
                if self.accept_status(status)? {
                    return Ok(self.client_exited_early);
                }
            }
        })();
        // Reap even when the status stream was malformed or truncated.
        let reaped = self
            .helper
            .as_mut()
            .context("GDB helper already consumed")?
            .wait()
            .context("reap GDB helper");
        match reaped {
            Ok(status) => {
                self.helper.take();
                self.finished = status.success() && observed.is_ok();
                if !status.success() {
                    return Err(observed
                        .err()
                        .unwrap_or_else(|| anyhow::anyhow!("GDB helper exited with {status}")));
                }
                observed
            }
            Err(error) => match observed {
                Err(observation) => {
                    Err(observation.context(format!("reap GDB helper failed: {error:#}")))
                }
                Ok(_) => Err(error),
            },
        }
    }

    #[cfg(test)]
    fn observed_client_exit(&mut self) -> Result<bool, Error> {
        self.stream.as_mut().unwrap().set_nonblocking(true)?;
        while let Some(status) = self.statuses.next(self.stream.as_mut().unwrap())? {
            anyhow::ensure!(
                !self.accept_status(status)?,
                "helper finished before container completion"
            );
        }
        Ok(self.client_exited_before_container_finished)
    }
}

impl Drop for GdbClientWatch {
    /// Only the original process owns cleanup. Raw-cloned copies close their
    /// local aliases without sending Done or touching the parent's child.
    fn drop(&mut self) {
        match self.owner.is_current() {
            Ok(false) => return,
            Err(error) => {
                if let Some(child) = self.helper.take() {
                    retain_unconfirmed(Arc::new(Mutex::new(Some(child))), error);
                }
                return;
            }
            Ok(true) => {}
        }
        let Some(mut child) = self.helper.take() else {
            return;
        };
        if let Err(error) = self.signal_done() {
            eprintln!("GDB helper completion notification failed: {error:#}");
        }
        self.stream.take();
        match child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                #[cfg(test)]
                let force_refusal = self.force_reaper_spawn_refusal;
                #[cfg(not(test))]
                let force_refusal = false;
                reap_after_final_scope(child, force_refusal);
            }
            Err(error) => retain_unconfirmed(Arc::new(Mutex::new(Some(child))), error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use super::helper::CLIENT_POLL_INTERVAL;
    use super::*;

    #[test]
    fn a_refused_gdb_spawn_is_reported_before_a_watch_is_returned() {
        let error = GdbClientWatch::spawn(Command::new("/no/such/hermit-gdb-client"), 1234)
            .err()
            .expect("a missing GDB unexpectedly spawned");
        assert!(error.to_string().contains("Failed to run gdb command"));
        assert_eq!(
            error
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error),
            Some(libc::ENOENT)
        );
    }

    #[test]
    fn helper_owns_gdb_without_leaking_its_control_descriptors() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let mut watch = GdbClientWatch::spawn(command, 1234).unwrap();
        let helper_pid = watch.helper.as_ref().unwrap().id();
        let client_pid = watch.client_pid;
        struct EndClient(u32);
        impl Drop for EndClient {
            fn drop(&mut self) {
                // SAFETY: this is the live, owned stand-in announced by READY.
                unsafe {
                    libc::kill(self.0 as i32, libc::SIGTERM);
                }
            }
        }
        let rescue = EndClient(client_pid);
        // Pin the stand-in before checking its parent, so this pidfd names the
        // helper's only child even if the PID is reused later.
        // SAFETY: pidfd_open has no pointer arguments.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, client_pid as libc::pid_t, 0) };
        assert!(
            raw >= 0,
            "pin the GDB stand-in: {}",
            io::Error::last_os_error()
        );
        // SAFETY: pidfd_open returned a new owned descriptor.
        let client = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        let exited = || helper::parent_exited(client.as_raw_fd()).expect("poll the stand-in pidfd");
        let status = std::fs::read_to_string(format!("/proc/{client_pid}/status")).unwrap();
        let parent = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .unwrap();
        assert_eq!(parent.trim().parse::<u32>().unwrap(), helper_pid);

        let private = [watch.helper_control_fd, watch.owner.pidfd.as_raw_fd()].map(|fd| {
            let metadata = std::fs::metadata(format!("/proc/{helper_pid}/fd/{fd}")).unwrap();
            (metadata.dev(), metadata.ino())
        });
        for entry in std::fs::read_dir(format!("/proc/{client_pid}/fd")).unwrap() {
            // The stand-in's loader still opens and closes its own descriptors
            // after READY, so an entry may vanish before its stat. That alone
            // does not show it was never inherited; see the check below.
            let metadata = match std::fs::metadata(entry.unwrap().path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => panic!("stat GDB child descriptor: {error}"),
            };
            assert!(
                !private.contains(&(metadata.dev(), metadata.ino())),
                "the GDB child inherited a private watcher descriptor"
            );
        }
        // While it runs, the stand-in closes only descriptors it opened itself.
        // Its exit closes the rest: GNU sleep's atexit handler closes 1 and 2,
        // exit_files() then drops the whole table, and only after that does
        // the pidfd report the exit. So the skip is sound only if the same
        // stand-in still has descriptors 0 to 2 and has not exited.
        let stdio =
            (0..3).all(|fd| std::fs::metadata(format!("/proc/{client_pid}/fd/{fd}")).is_ok());
        assert!(
            stdio && !exited(),
            "the GDB stand-in exited during the descriptor scan, so a vanished entry may have been inherited"
        );
        drop(rescue);
        watch.finish().expect("GDB helper finish failed");
        // Control: finish() returns only after the helper reaps the stand-in,
        // so the same pidfd must now report the exit the check above rejects.
        assert!(exited(), "the stand-in pidfd did not report its exit");
    }

    #[test]
    fn refused_reaper_spawn_retains_the_exact_helper_without_blocking_drop() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let mut watch =
            GdbClientWatch::spawn(command, listener.local_addr().unwrap().port()).unwrap();
        let helper_pid = watch.helper.as_ref().unwrap().id();
        let client_pid = watch.client_pid;
        // This is a finite injected Builder refusal, not an OS-exhaustion claim.
        watch.force_reaper_spawn_refusal = true;
        let started = Instant::now();
        let original_error: Result<(), Error> = {
            let _watch = watch;
            Err(Error::msg("original container failure"))
        };
        let elapsed = started.elapsed();

        let owner = {
            let mut retained = UNCONFIRMED_REAPS.lock().unwrap();
            let index = retained
                .iter()
                .position(|owner| {
                    owner
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|child| child.id() == helper_pid)
                })
                .expect("reaper refusal lost the original helper Child");
            retained.remove(index)
        };
        let mut helper = owner
            .lock()
            .unwrap()
            .take()
            .expect("retained helper owner was empty");
        assert_eq!(helper.id(), helper_pid);

        // Rescue is separate from Drop: end the actual GDB stand-in, then reap
        // the retained original helper. The same five-second cleanup bound used
        // by the existing Drop/saturated-listener tests keeps a failure finite.
        let killed = unsafe { libc::kill(client_pid as i32, libc::SIGTERM) };
        assert_eq!(killed, 0, "failed to end the stand-in client");
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let status = loop {
            if let Some(status) = helper.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = helper.kill();
                let _ = helper.wait();
                break None;
            }
            thread::sleep(CLIENT_POLL_INTERVAL);
        };
        assert_eq!(
            original_error.unwrap_err().to_string(),
            "original container failure"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "Drop blocked {elapsed:?} after reaper refusal"
        );
        assert!(
            status
                .expect("retained helper did not finish within 5s")
                .success()
        );
    }

    /// `finish()` must not sit out a grace period it no longer needs.
    ///
    /// ⚠️ THE GRACE SLEEP IS ON HERMIT'S OWN RETURN PATH, because `finish()` sets
    /// `container_done` and then JOINS this thread. A flat `for _ in
    /// 0..RELEASE_GRACE_TICKS { sleep }` cannot notice the container finishing, so
    /// hermit waited out the remainder of a pause whose only purpose is to slow
    /// down contacting a stranger. MEASURED on this test, entering the sleep and
    /// then calling `finish()`:
    ///
    ///     without the `done` check .... 381.7ms
    ///     with it ..................... 0.52ms
    ///
    /// The bound is one `RELEASE_RETRY_INTERVAL` (20ms) plus scheduling, so 100ms
    /// is generous while still an order of magnitude below the regression.
    ///
    /// ⚠️ THIS IS NOT THE `done` CHECK THAT WAS REMOVED FROM THIS LOOP. That one
    /// read `done` to decide whether the early-exit report was EARNED, which it
    /// cannot do -- `done` is correlation, not authentication. This one decides
    /// only when to stop sleeping, and no report depends on it.
    #[test]
    fn finish_does_not_wait_out_a_grace_period_after_the_container_is_done() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => drop(stream),
                    Err(_) => break,
                }
            }
        });

        let client = std::process::Command::new("/bin/true");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");
        // Let the client exit, so the watcher reaches the release loop.
        thread::sleep(Duration::from_millis(200));
        // Long enough for one successful connect to land the watcher INSIDE the
        // grace sleep, short enough that the sleep is still running.
        thread::sleep(Duration::from_millis(120));

        let started = std::time::Instant::now();
        let _ = watch.finish().expect("GDB helper finish failed");
        let lag = started.elapsed();

        assert!(
            lag < Duration::from_millis(100),
            "finish() took {lag:?} to join after setting container_done; the grace \
             sleep is ignoring `done` and is charging hermit's return path for a \
             pause that only exists to rate-limit contacting a stranger"
        );
    }

    /// Connecting to a listener that is not ours must not end the watch.
    ///
    /// ⚠️ THIS IS THE HANG, RESTORED BY A SUCCESSFUL CONNECT, and it is live on
    /// `main` without the grace window: the release port is guessable
    /// (`16384 + tid % 1024`, or 1234 for `replay`), so an unrelated process can
    /// own it in exactly the window the release loop exists for. A watcher that
    /// concludes on the first successful connect then stops watching while our
    /// own gdbserver is still blocked in `accept()` with nobody coming.
    ///
    /// ⚠️ THE DISCRIMINATOR IS THE RETRY, NOT THE FLAG, and that is deliberate.
    /// An earlier version of this fix tried to tell "we released ours" from "a
    /// stranger accepted" by reading the socket for a prompt EOF. Measured, that
    /// probe cannot do it: our probe reads BEFORE it disconnects, so a gdbstub
    /// that has just accepted has no reason to close, and the genuine-release and
    /// already-served cases BOTH read as not-accepted. Asserting a retry count
    /// asks only what this loop actually controls.
    ///
    /// ⚠️ AND THE NAME NOW SAYS "STILL LATCHES THE REPORT", BECAUSE IT DOES. This
    /// was called `a_connect_to_a_stranger_is_not_treated_as_a_release` while
    /// discarding `watch.finish()` and never reading the flag — so the test named
    /// for the defect did not assert it, and `assert!(!watch.finish())` fails
    /// here. A stranger IS still treated as a release for reporting purposes.
    /// Naming the test for the property it has, and asserting the gap it does
    /// not close, is the difference between coverage and the appearance of it.
    ///
    /// Bounded, and asserts a count rather than blocking, so a regression is a
    /// named red and not a wedged runner.
    #[test]
    fn a_stranger_does_not_end_the_release_attempts_but_still_latches_the_report() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
        let port = listener.local_addr().expect("no local addr").port();

        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });

        let client = std::process::Command::new("/bin/true");

        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");
        // Let it exit first, so the watcher observes the exit rather than racing.
        thread::sleep(Duration::from_millis(200));
        // The container is DELIBERATELY never marked done: the stranger did not
        // release our accept, because it never had it.
        //
        // ⚠️ A FIXED WALL-CLOCK WINDOW, NOT ONE DERIVED FROM THE CONSTANT UNDER
        // TEST. This slept `RELEASE_RETRY_INTERVAL * (RELEASE_GRACE_TICKS + 10)`,
        // so the observation window SCALED WITH THE PACE IT WAS MEASURING and the
        // attempt count stayed ~2 for every setting. Measured: RELEASE_GRACE_TICKS
        // of 2, 5, 25, 50 and 100 ALL PASSED — a 40ms grace hammers a stranger
        // 12.5x harder than a 500ms one and the test could not see it. A test
        // whose window moves with its subject pins nothing.
        //
        // 1600ms admits three or four attempts at the current pace, depending
        // on the first probe phase. Subtract setup probes so the fixed window
        // remains exactly 1600ms after moving client spawn into the helper. The
        // bounds below are set to catch a 2x move in either direction while
        // leaving room for `thread::sleep` overshoot.
        const OBSERVATION_WINDOW: Duration = Duration::from_millis(1600);
        let before_window = accepted.load(Ordering::SeqCst);
        thread::sleep(OBSERVATION_WINDOW);
        let attempts = accepted.load(Ordering::SeqCst) - before_window;

        // Let the watcher finish before asserting, so a failure reports a count
        // rather than leaving a thread running under the harness.
        watch.signal_done().expect("failed to notify helper");
        let reported_early = watch.finish().expect("GDB helper finish failed");

        assert!(
            attempts >= 3,
            "the watcher made {attempts} connection(s) to a peer that never released \
             anything; a successful connect must be an ATTEMPT, not a conclusion, or a \
             stranger on a guessable port silently restores the hang"
        );

        // ⚠️ AND A CEILING, BECAUSE THE FLOOR ALONE LEFT THE PAUSE UNTESTED.
        // `agent(hermit-001)` measured that the grace loop's body could be gutted
        // with all seven cells still green: the floor pins "it retried" and says
        // nothing about the rate, which is the only property the pause actually
        // has. Without the pause the loop spins as fast as connect returns:
        // MEASURED 16714 attempts in a comparable window.
        assert!(
            attempts <= 6,
            "the watcher made {attempts} connections in {OBSERVATION_WINDOW:?}; the \
             pause is a RATE LIMIT on contacting a peer that may not be ours, and an \
             unpaced retry loop hammers a stranger every RELEASE_RETRY_INTERVAL"
        );

        // ⚠️ THE OPEN DEFECT, PINNED RATHER THAN LEFT UNSTATED. This test is named
        // for a release and until now discarded `finish()` entirely, so it never
        // touched the report at all: `assert!(!reported_early)` FAILS here, and
        // the test named for the defect did not assert it.
        //
        // A stranger's accept still latches `client_exited_early` at the connect
        // site, and nothing lowers it — `store(false)` appears nowhere in this
        // file. So hermit tells the operator "the client exited before
        // connecting" on a port collision. THIS HEAD DOES NOT FIX THAT; it fixes
        // the hang, and this assertion states the remaining gap in executable
        // form so it cannot change unnoticed in either direction.
        //
        // Closing it needs the listener IDENTIFIED rather than the port guessed —
        // `done` is correlation not authentication, `peer_addr` cannot separate
        // two loopback listeners, and an EOF probe was measured non-
        // discriminating. Tracked as `gdb_watcher_release_probe`.
        //
        // ⚠️ WHOEVER FIXES IT: this assertion is expected to fail, and the fix is
        // to INVERT it, not to delete it.
        assert!(
            reported_early,
            "a stranger no longer latches the false early-exit report -- if that is \
             deliberate, invert this assertion and retire `gdb_watcher_release_probe`; \
             if it is accidental, the report has become silent instead of correct"
        );
    }

    /// A client that exits without connecting must release an accept that is
    /// already waiting.
    ///
    /// This is the hang, in miniature: a listener with nobody coming. Without
    /// the watcher, `accept()` here never returns.
    #[test]
    fn a_client_that_exits_without_connecting_releases_a_waiting_accept() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let client = std::process::Command::new("/bin/true");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");

        // ⚠️ BOUNDED, BECAUSE A HANGING TEST IS WORSE THAN A RED ONE. The first
        // version called the blocking `accept()` and argued that hanging was "the
        // honest failure for this property, since the defect under test IS a
        // hang". That reasoning is wrong in a suite: a red test names itself in
        // one line, while a wedged one consumes the whole run's budget and is
        // reported as a timeout somewhere else entirely.
        listener
            .set_nonblocking(true)
            .expect("failed to set the test listener non-blocking");
        let deadline = Instant::now() + Duration::from_secs(30);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        assert!(
            accepted.is_some(),
            "the watcher did not release a waiting accept within 30s after the client exited; \\
             the release is broken and the gdbserver would block forever"
        );

        assert!(
            watch.finish().expect("GDB helper finish failed"),
            "the client exited while the container was still running, so that must be reported"
        );
    }

    /// The watcher must NOT report an early exit when the container finished
    /// first, or every healthy gdb session would be labelled a failure -- and
    /// `finish()` must WAIT for that client rather than walking away from it.
    ///
    /// ⚠️ THIS TEST ASSERTED THE OPPOSITE ONE COMMIT AGO, and the reversal is the
    /// substance of this change. It read:
    ///
    /// ```text
    /// assert!(elapsed < Duration::from_secs(2),
    ///     "finish() took {elapsed:?}; it must not wait on a client that outlives the container");
    /// ```
    ///
    /// That encoded "hermit must not wait for the gdb it spawned" as a
    /// requirement. It never was one: before this file existed, both call sites
    /// ended with `let _ = gdb_client.wait();` and hermit waited. The requirement
    /// the assertion was reaching for is real, but it belongs on `Drop` -- see
    /// `dropping_a_watch_never_waits_for_the_client` -- because the defect was a
    /// wait taken BEFORE the release decision, on paths that were merely
    /// returning an error. Telling those two waits apart is the whole change.
    #[test]
    fn finish_waits_for_a_client_that_outlives_the_container() {
        // Bound but never accepted: the container "finishes" without the client
        // having exited, which is the ordinary ordering.
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Long enough that "did it wait?" is unambiguous, short enough that a
        // stray cannot outlive the suite. An earlier version left `sleep 30`
        // alive after `cargo test` returned 0.
        let mut client = std::process::Command::new("/bin/sleep");
        client.arg("1");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");

        // ⚠️ THIS CALLS `finish()`, WHICH IS THE POINT. An earlier version poked
        // `container_done` and read `client_exited_early` by hand and then
        // detached the thread -- because calling `finish()` would have hit the
        // blocking join and hung. So the test AVOIDED the defect instead of
        // catching it. Going through the real entry point is what makes this a
        // regression test rather than a description.
        let started = Instant::now();
        let reported_early = watch.finish().expect("GDB helper finish failed");
        let elapsed = started.elapsed();

        assert!(
            !reported_early,
            "a client still running when the container finished must not be reported as an early exit"
        );
        assert!(
            elapsed >= Duration::from_millis(900),
            "finish() returned after {elapsed:?}, before a 1-second client could have exited; \
             hermit is not waiting for the gdb it spawned"
        );
        drop(listener);
    }

    /// ⚠️ THE ONLY NEW BEHAVIOUR WITH TEETH: `finish()` JOINS, SO THE RELEASE
    /// PROBE IS NOW ON HERMIT'S RETURN PATH. A bare `TcpStream::connect` has no
    /// timeout, and against a listener whose accept queue is full the SYN is
    /// dropped rather than refused, so the call stalls. While the watcher was
    /// detached that only delayed a background thread. `agent(hermit-dbgrev14)`
    /// measured the same schedule both ways: **741ms with `connect_timeout`,
    /// 135.5s with main's bare `connect`.**
    ///
    /// ⚠️ AND THIS TEST RETURNS EARLY RATHER THAN FAILING IF IT CANNOT SATURATE
    /// THE QUEUE, WHICH IS A SILENT PASS AND IS THE DELIBERATE CHOICE. Saturation
    /// depends on the host's backlog behaviour; a test that goes RED when the
    /// environment will not cooperate teaches people to ignore it, which costs
    /// more than the coverage is worth. The bound is therefore ALSO carried by an
    /// old-fails/new-passes mutation recorded on the pull request, not by this
    /// cell alone. Saturation is verified before anything is asserted, so the pass
    /// is never taken as evidence when the setup did not hold.
    #[test]
    fn finish_is_bounded_when_the_release_probe_meets_a_saturated_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();
        let peer = SocketAddr::from(([127, 0, 0, 1], port));

        // Shrink the accept queue to its minimum and never accept from it.
        // SAFETY: `listen` on an owned listening descriptor; writes no memory.
        unsafe {
            libc::listen(std::os::fd::AsRawFd::as_raw_fd(&listener), 0);
        }
        let mut held = Vec::new();
        for _ in 0..8 {
            match TcpStream::connect_timeout(&peer, Duration::from_millis(250)) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }

        // Verify the setup rather than assuming it: if a fresh connect still
        // completes, the queue is not full and this cell would prove nothing.
        if TcpStream::connect_timeout(&peer, Duration::from_millis(250)).is_ok() {
            return;
        }

        // A client that has already exited sends the watcher into the release
        // loop, where it meets the stalling peer.
        let client = std::process::Command::new("/bin/true");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");
        thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        watch.finish().expect("GDB helper finish failed");
        let elapsed = started.elapsed();

        drop(held);
        drop(listener);

        assert!(
            elapsed < Duration::from_secs(5),
            "finish() blocked {elapsed:?} against a listener that never answers; the release \
             probe must be bounded now that finish() joins the watcher"
        );
    }

    /// ⚠️ THE PROMPTNESS GUARANTEE, ON THE PATH THAT ACTUALLY NEEDS IT. An early
    /// `?` between the spawn and the run drops the watch without a container ever
    /// having existed, so a client still trying to connect to a port nobody will
    /// bind would never exit. The original defect was a `Drop` that inherited a
    /// blocking wait and hung paths that were merely returning an error --
    /// measured at 30.001s. Restoring the success-path wait must not bring that
    /// back with it, and this is what says so.
    #[test]
    fn dropping_a_watch_never_waits_for_the_client() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        let mut client = std::process::Command::new("/bin/sleep");
        client.arg("30");
        let watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");
        let pid = watch.client_pid;

        let started = Instant::now();
        drop(watch);
        let elapsed = started.elapsed();

        // The detached watcher owns the client and reaps it. Kill the stand-in so
        // the suite leaves nothing running: a reviewer measured a `/bin/sleep 30`
        // alive after `cargo test` returned 0, reparented to the user subreaper,
        // and a leak like that is invisible in a green run.
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();

        assert!(
            elapsed < Duration::from_secs(5),
            "dropping the watch blocked {elapsed:?} on a live client; the error path must never \
             wait for one"
        );
        drop(listener);
    }

    /// A client that exits AFTER the container finished is a healthy teardown and
    /// must not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS DEFECT 2, AND IT IS THE INVERSE OF A HANG: a correct outcome
    /// described as a failure. The first version read `done` only once, after a
    /// blocking `wait()`, so ANY client that exited before the container returned
    /// was flagged — and the flag adds a context line saying the client never
    /// connected. A false cause attached to a real failure sends the next reader
    /// hunting a bug that does not exist.
    #[test]
    fn a_client_that_exits_after_the_container_finished_is_not_reported_as_early() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // Still alive when the container finishes, gone shortly after.
        let mut client = std::process::Command::new("/bin/sleep");
        client.arg("1");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");

        // The container returns while the client is still running.
        let reported_early = watch.finish().expect("GDB helper finish failed");
        assert!(
            !reported_early,
            "reported early while the client was still alive"
        );

        // ⚠️ THIS TAIL USED TO SLEEP 2500ms AND RE-READ THE FLAG, AND THE JOIN MADE
        // IT DEAD CODE — recorded rather than deleted quietly, because it is the
        // one behavioural gain of joining that is easy to miss. It read "the
        // watcher is detached and still polling; it must observe the exit and stay
        // silent", which was true when `finish()` detached: the flag could still
        // move after `finish()` returned, so re-reading it later was a real second
        // observation. `finish()` now joins, so the watcher has already run to
        // completion and nothing can set the flag afterwards — the re-read could
        // not fail for ANY implementation, and the sleep was 2.5s of dead time.
        // `agent(hermit-dbgrev14)` caught it.
        //
        // The property is unchanged and is now decided entirely by the assertion
        // above: joining turned `finish()`'s answer from a snapshot of a running
        // thread into a settled fact.
        assert!(
            !watch.client_exited_early,
            "the flag moved after `finish()` returned, so joining did not settle it"
        );
        drop(listener);
    }

    /// The client exits FIRST, and the container then finishes on its own. There
    /// was no blocked accept to release, so this is a healthy teardown and must
    /// not be reported as "exited before connecting".
    ///
    /// ⚠️ THIS IS THE CASE RE-READING `done` CANNOT CATCH, and pinning it is why
    /// the flag moved. The client is gone before the container returns, so a
    /// check taken at client-exit time sees `done == false` — and so does a check
    /// taken an instant later. Only asking "did we have to release an accept?"
    /// distinguishes a stranded container from an ordinary shutdown.
    ///
    /// No listener is bound on `port`, so the release connect can never succeed:
    /// exactly the shape of a container that needed no rescuing.
    #[test]
    fn a_client_that_exits_first_but_strands_nobody_is_not_reported_as_early() {
        // Bind and immediately drop, so the port is plausible but nothing listens.
        let port = {
            let l = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind");
            l.local_addr().expect("no local addr").port()
        };

        let client = std::process::Command::new("/bin/true");
        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");

        // Let the watcher observe the exit and spin on the release a few times.
        thread::sleep(Duration::from_millis(200));

        // The container now finishes under its own power.
        assert!(
            !watch.finish().expect("GDB helper finish failed"),
            "a client that exited without stranding the container was reported as having \
             exited before connecting -- a healthy teardown described as a failed connect"
        );
        thread::sleep(Duration::from_millis(100));
        assert!(
            !watch.client_exited_early,
            "the flag was set after the fact for a container that was never blocked"
        );
    }
    /// A client that CONNECTED, completed its session and exited must not be
    /// reported as having exited before connecting.
    ///
    /// [`CLIENT_EXITED_BEFORE_CONNECTING`] says the client "exited before it
    /// finished connecting", so the negation -- connected, served, quit -- is what
    /// must never be flagged. The other cells pin clients that never connect at
    /// all; this is the path a real `gdb -batch ... quit` takes on every
    /// successful run, and it is named here so the record has it.
    ///
    /// ⚠️ THIS IS A SCENARIO CELL, NOT INDEPENDENT COVERAGE, AND SAYING SO IS THE
    /// POINT. Reverting the defect-2 correction fails this cell AND
    /// `a_client_that_exits_first_but_strands_nobody`, because both end with
    /// NOTHING LISTENING on the port -- that cell binds nothing, this one drops
    /// the listener after the accept -- so both exercise the same branch: connect
    /// fails, loop spins, `done` arrives, no flag. Fail-on-revert therefore does
    /// not establish that this cell catches anything the suite would otherwise
    /// miss. Raised by `agent(hermit-001)` on the review of this change.
    ///
    /// ⚠️ AND IT CANNOT DETECT A REVERIE CONTRACT CHANGE, WHICH AN EARLIER VERSION
    /// OF THIS COMMENT CLAIMED. The `drop(listener)` below is this test's OWN, not
    /// `reverie-ptrace`'s. If `wait_for_tcp_connection` ever kept its listener
    /// bound, this cell would go on passing, because it drops the listener itself
    /// either way. The reverie side is pinned where the fact lives, by
    /// `the_listener_is_closed_once_the_client_is_accepted` in that repository.
    ///
    /// ⚠️ THE DISCRIMINATING SHAPE IS NOT WRITABLE GREEN TODAY. It would be a
    /// listener that stays BOUND and answering after the accept -- the only case
    /// where the release connect succeeds without our accept having been released.
    /// MEASURED on hermit#2678's head a826d51a2116: in exactly that scenario the
    /// flag comes out TRUE (`accepts_after_session=1 total_accepts=3
    /// flag_reported=true`), so a cell asserting the correct answer would be RED.
    /// The store at the successful connect precedes the grace loop, and moving it
    /// after would suppress the flag in the TRUE-positive case too, because a
    /// released accept also lets the container finish inside the grace window.
    /// This is the conceded port-collision exposure, tracked as
    /// `gdb_watcher_release_probe`; it is not reachable in production only because
    /// reverie drops the listener.
    #[test]
    fn a_client_that_connected_and_finished_its_session_is_not_reported_as_early() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a test listener");
        let port = listener.local_addr().expect("no local addr").port();

        // A stand-in for `gdb -batch ... quit`: connect, wait for the server to
        // finish the session, then exit. Waiting on the accepted connection makes
        // the ordering a fact rather than a delay: the client cannot exit before
        // the accept and listener drop have happened.
        let mut client = std::process::Command::new("bash");
        client.arg("-c").arg(format!(
            "exec 3<>/dev/tcp/127.0.0.1/{port} || exit 1; IFS= read -r -n 1 <&3; exec 3>&-"
        ));

        let mut watch =
            GdbClientWatch::spawn(client, port).expect("failed to spawn the stand-in client");

        // The gdbserver accepts. Bounded, because a hanging test is worse than a
        // red one: it names itself in a line, a wedged one eats the whole run.
        listener
            .set_nonblocking(true)
            .expect("failed to set the test listener non-blocking");
        let deadline = Instant::now() + Duration::from_secs(30);
        let accepted = loop {
            match listener.accept() {
                Ok(pair) => break Some(pair),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("accept failed: {e}"),
            }
        };
        let (mut accepted, _) = accepted.expect("the stand-in client never connected within 30s");

        // ⚠️ THE REVERIE CONTRACT, REPRODUCED. `wait_for_tcp_connection` returns
        // the stream and drops the listener; from here on the port answers
        // nothing, while the session itself stays open.
        drop(listener);

        // End the accepted session, then wait for the peer to close it. EOF is
        // direct evidence that the client completed the session; no elapsed time
        // is used as a substitute for that fact.
        accepted
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("failed to bound the wait for the stand-in client to close");
        accepted
            .write_all(b"q")
            .expect("failed to finish the stand-in client's session");
        let mut byte = [0_u8; 1];
        assert_eq!(
            accepted
                .read(&mut byte)
                .expect("the stand-in client did not close its session within 30s"),
            0,
            "the stand-in client sent unexpected data instead of closing its session"
        );

        // `finish()` publishes container completion. Wait until the watcher has
        // reaped the client and confirmed that the container is still running,
        // so this cell cannot silently take the container-finished-first branch
        // and pass without exercising its case.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !watch
            .observed_client_exit()
            .expect("failed to observe client exit")
            && Instant::now() < deadline
        {
            thread::sleep(CLIENT_POLL_INTERVAL);
        }
        assert!(
            watch
                .observed_client_exit()
                .expect("failed to observe client exit"),
            "the watcher did not observe the stand-in client exit while the container was still running"
        );

        assert!(
            !watch.finish().expect("GDB helper finish failed"),
            "a client that connected, finished its session and exited was reported as having \
             exited before connecting -- the flag's own documented meaning, inverted"
        );
        drop(accepted);
    }
}
