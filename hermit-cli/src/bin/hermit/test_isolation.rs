/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Running a unit test in a process of its own.

use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// Set in the copy of the test binary that [`rerun_alone`] starts.
const COPY: &str = "HERMIT_TEST_RERUN_ALONE";

/// Whether this process is a copy that [`rerun_alone`] started.
fn in_copy() -> bool {
    std::env::var_os(COPY).is_some()
}

/// Runs the calling test again, alone, in a fresh copy of this test binary.
///
/// Returns true in the calling process once the copy has run the test and
/// passed; the test then has nothing more to do. Returns false in the copy,
/// which goes on to run the test's body.
///
/// Every test that clones a container through `reverie::process::Container`
/// (directly or through Hermit's launch path) calls this first. That clone is
/// a raw `clone(2)`, which runs no `pthread_atfork` handler, so the child
/// starts with every lock that any other thread held at that instant, glibc's
/// malloc arena locks among them, and with no thread left to release them:
/// its next `malloc` or `fork` waits forever. `Container::run` therefore
/// requires a process with no other threads. Plain `cargo test` runs tests as
/// threads of one process, where such a test hung whenever a test beside it
/// held a malloc lock at the wrong moment, and its stuck child kept other
/// tests' pipes open, which hung them too. The copy holds only this test and
/// libtest's main thread, which waits for it: the arrangement nextest gives
/// every test. Every container clone in this binary checks, in a test build,
/// that it runs in such a copy ([`assert_alone_before_container_clone`]), so
/// a test that forgets this fails at once.
///
/// A test that sets per-process state that can be set only once, such as
/// the skid margin of Reverie's PMU configuration, calls it too. Beside other
/// tests, whichever of them set the state first decided what the rest saw.
/// Recording `--skid-margin` checks it as well
/// ([`assert_alone_before_setting_the_skid_margin`]).
///
/// The test is found by its thread's name, which libtest sets to the test's
/// path, so this must be called on the test's own thread.
pub(crate) fn rerun_alone() -> bool {
    rerun_marked(COPY)
}

/// Runs the calling test again, alone, in a fresh copy of this test binary
/// whose environment sets `marker`, and not [`COPY`] unless `marker` is it.
/// Returns true in the calling process once the copy has run the test and
/// passed, and false in the copy. [`rerun_alone`] is this with [`COPY`].
fn rerun_marked(marker: &str) -> bool {
    if std::env::var_os(marker).is_some() {
        return false;
    }
    let current = std::thread::current();
    let name = current
        .name()
        .expect("libtest names a test's thread after the test");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env_remove(COPY)
        .env(marker, "1")
        .stdin(Stdio::null())
        .output()
        .expect("start a copy of the test binary");
    assert!(
        one_test_passed(&output),
        "{name} failed, or did not run, in a process of its own ({}):\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

/// Whether `output`, from a copy of this test binary run with `--exact`, ran
/// exactly one test and that test passed: libtest's summary line, not just a
/// count that `11 passed` would also contain.
fn one_test_passed(output: &Output) -> bool {
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).contains("test result: ok. 1 passed")
}

/// Why a container clone needs a process of its own, for the panics below.
const CLONE_HAZARD: &str = "a container clone is a raw clone(2) that runs no pthread_atfork \
    handler, so its child starts holding every lock another thread of this process held at that \
    instant (glibc's malloc locks among them) and every descriptor the process had open (another \
    test's pipe among them): the child can hang, and the tests reading that pipe hang with it";

/// How long [`assert_alone_before_container_clone`] waits for another thread
/// of the process to leave the task list before it counts as running.
const JOINED_THREAD_GRACE: Duration = Duration::from_millis(500);

/// Panics unless this process may clone a container: it must be a copy that
/// [`rerun_alone`] started, and run no thread but the cloning one and the main
/// thread, where libtest waits for the test. Every container clone in this
/// binary makes this check first in a test build (`container_clone`), so a
/// test that clones without [`rerun_alone`] fails here at once, under nextest
/// as under plain `cargo test`, instead of hanging plain `cargo test` once in
/// a while; so does a test that starts a thread of its own before it clones.
///
/// The production binary does not contain it: `hermit` clones from its only
/// thread. A container child is a process of one thread, and inherits the
/// copy's environment, so a container cloned inside another passes it.
pub(crate) fn assert_alone_before_container_clone() {
    assert!(
        in_copy(),
        "this test clones a container in a process it shares with other tests; call \
         crate::app::test_isolation::rerun_alone() at its start, so it runs alone in a copy of the \
         test binary: {CLONE_HAZARD}"
    );
    // A thread just joined can stay listed for a moment after `join` returns:
    // its exit wakes the joiner before the kernel releases it. A thread still
    // listed after the grace period is running.
    let deadline = Instant::now() + JOINED_THREAD_GRACE;
    let mut others = other_threads();
    while !others.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        others = other_threads();
    }
    assert!(
        others.is_empty(),
        "this test clones a container while other threads of its process run (IDs {others:?}), \
         beside the main thread where libtest waits; crate::app::test_isolation::rerun_alone() gave \
         it a process of its own, so clone before starting threads: {CLONE_HAZARD}"
    );
}

/// The threads of this process other than the calling one and the main thread,
/// by their IDs in the PID namespace of the procfs at `/proc`.
fn other_threads() -> Vec<u32> {
    // `<pid>/task/<tid>`, in the same procfs as the task list below.
    let link = std::fs::read_link("/proc/thread-self").expect("read /proc/thread-self");
    let ids = |path: &str| -> Option<(u32, u32)> {
        let (pid, tid) = path.split_once("/task/")?;
        Some((pid.parse().ok()?, tid.parse().ok()?))
    };
    let (pid, tid) = link
        .to_str()
        .and_then(ids)
        .unwrap_or_else(|| panic!("/proc/thread-self is {link:?}, not <pid>/task/<tid>"));
    std::fs::read_dir("/proc/self/task")
        .expect("list /proc/self/task")
        .map(|entry| {
            let name = entry.expect("read /proc/self/task").file_name();
            name.to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .unwrap_or_else(|| panic!("/proc/self/task holds {name:?}"))
        })
        .filter(|&task| task != tid && task != pid)
        .collect()
}

/// Panics unless this process is a copy that [`rerun_alone`] started.
/// `RunOpts::install_pmu_config` checks this in a test build before it records
/// `--skid-margin` in Reverie's PMU configuration, which a process can set only
/// once: beside other tests, whichever set it first would decide what the rest
/// saw.
pub(crate) fn assert_alone_before_setting_the_skid_margin() {
    assert!(
        in_copy(),
        "this test records --skid-margin in Reverie's PMU configuration, which a process sets only \
         once, in a process it shares with other tests; call \
         crate::app::test_isolation::rerun_alone() at its start, so it runs alone in a copy of the test \
         binary and no other test sets the configuration first"
    );
}

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::fs::File;
    use std::io;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::mpsc;

    use clap::Parser;
    use reverie::process::Container;

    use super::*;
    use crate::app::container_clone::CheckedClone;

    /// How long a forking container child may take before it counts as stuck.
    /// One that can fork finishes within milliseconds.
    const FORK_BUDGET: Duration = Duration::from_secs(10);

    /// Set in the process that the tests of the checks outside a
    /// [`rerun_alone`] copy run in: a process of its own, where a check that
    /// failed to refuse could do no harm, but not a copy `rerun_alone` started.
    const OUTSIDE_A_COPY: &str = "HERMIT_TEST_OUTSIDE_A_RERUN_ALONE_COPY";

    /// A thread blocked inside glibc's `malloc_stats` while it holds the main
    /// malloc arena lock: `malloc_stats` writes to standard error under that
    /// lock, and standard error is a full pipe that nobody reads. Dropping it
    /// drains the pipe, which lets the thread finish and release the lock,
    /// and restores standard error.
    struct MallocLockHolder {
        reader: File,
        saved_stderr: OwnedFd,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MallocLockHolder {
        fn hold() -> Self {
            let (reader, writer) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
            let writer = File::from(writer);
            set_nonblocking(&writer, true);
            while nix::unistd::write(&writer, &[0; 4096]).is_ok() {}
            set_nonblocking(&writer, false);
            let saved_stderr = io::stderr().as_fd().try_clone_to_owned().unwrap();
            // SAFETY: both descriptors are open; standard error is replaced,
            // not closed, and restored on drop.
            assert_ne!(unsafe { libc::dup2(writer.as_raw_fd(), 2) }, -1);
            drop(writer);
            let (tid_sender, tid) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                tid_sender.send(nix::unistd::gettid()).unwrap();
                // SAFETY: malloc_stats takes no arguments.
                unsafe { libc::malloc_stats() };
            });
            let tid = tid.recv().unwrap();
            // `write(2, ...)` from inside malloc_stats, with the lock held.
            let deadline = Instant::now() + FORK_BUDGET;
            while !std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall"))
                .is_ok_and(|syscall| syscall.starts_with("1 0x2 "))
            {
                assert!(Instant::now() < deadline, "malloc_stats never blocked");
                std::thread::sleep(Duration::from_millis(1));
            }
            Self {
                reader: File::from(reader),
                saved_stderr,
                thread: Some(thread),
            }
        }
    }

    impl Drop for MallocLockHolder {
        fn drop(&mut self) {
            set_nonblocking(&self.reader, true);
            let thread = self.thread.take().unwrap();
            let mut buffer = [0; 4096];
            while !thread.is_finished() {
                if let Err(error) = self.reader.read(&mut buffer) {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            thread.join().unwrap();
            // SAFETY: both descriptors are open.
            unsafe { libc::dup2(self.saved_stderr.as_raw_fd(), 2) };
        }
    }

    fn set_nonblocking(file: &File, nonblocking: bool) {
        use nix::fcntl::FcntlArg;
        use nix::fcntl::OFlag;
        let mut flags =
            OFlag::from_bits_truncate(nix::fcntl::fcntl(file, FcntlArg::F_GETFL).unwrap());
        flags.set(OFlag::O_NONBLOCK, nonblocking);
        nix::fcntl::fcntl(file, FcntlArg::F_SETFL(flags)).unwrap();
    }

    /// Clones a container whose child forks, and fails if the child cannot
    /// fork within the budget: the child arms an alarm before it forks, whose
    /// default action ends it then.
    fn assert_a_container_child_can_fork() {
        let budget = libc::c_uint::try_from(FORK_BUDGET.as_secs()).unwrap();
        let forked = Container::new().checked_run(|| {
            // SAFETY: these change only this child's own signal state, and the
            // forked child only exits.
            unsafe {
                let mut alarm = std::mem::zeroed::<libc::sigset_t>();
                libc::sigemptyset(&mut alarm);
                libc::sigaddset(&mut alarm, libc::SIGALRM);
                libc::sigprocmask(libc::SIG_UNBLOCK, &alarm, std::ptr::null_mut());
                libc::signal(libc::SIGALRM, libc::SIG_DFL);
                libc::alarm(budget);
                let forked = match libc::fork() {
                    0 => libc::_exit(0),
                    -1 => false,
                    child => {
                        nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(child), None).is_ok()
                    }
                };
                libc::alarm(0);
                forked
            }
        });
        assert!(
            matches!(forked, Ok(true)),
            "the container child could not fork within {FORK_BUDGET:?} ({forked:?}): a lock \
             another thread held when the container was cloned reached it"
        );
    }

    /// A container cloned by a test that `rerun_alone` moved into a process
    /// of its own forks even while another test, in the process the test was
    /// started in, holds glibc's main malloc arena lock: the lock cannot
    /// reach that process. A container cloned in the holder's own process
    /// instead starts with the lock held and no owner to release it, and its
    /// `fork` waits forever. That is how `cargo test` hung, and why
    /// serializing only the container tests would not help: any test that
    /// allocates can be the one holding the lock.
    #[test]
    fn a_test_rerun_alone_is_out_of_reach_of_a_malloc_lock_another_test_holds() {
        const HOLDER: &str = "HERMIT_TEST_MALLOC_LOCK_HOLDER";
        if std::env::var_os(HOLDER).is_none() {
            // Standard error is about to be taken over, which only a process
            // running nothing else can afford.
            let name = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--test-threads=1"])
                .env(HOLDER, "1")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                one_test_passed(&output),
                "the lock-holding process failed or ran no test ({}):\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _held = (!in_copy()).then(MallocLockHolder::hold);
        if rerun_alone() {
            return;
        }
        assert_a_container_child_can_fork();
    }

    /// The message of a panic caught by `catch_unwind`.
    fn panic_message(payload: Box<dyn Any + Send>) -> String {
        match payload.downcast::<String>() {
            Ok(message) => *message,
            Err(payload) => payload.downcast_ref::<&str>().map_or_else(
                || "(a panic without a message)".to_owned(),
                |m| (*m).to_owned(),
            ),
        }
    }

    /// A test that clones a container without [`rerun_alone`] fails at the
    /// clone site, before the clone, with a message that names
    /// `rerun_alone()` and the hazard. It runs in a process of its own that
    /// `rerun_alone` did not start, so a check that let the clone through
    /// would clone where no other test runs, and fail here, not hang.
    #[test]
    fn a_container_clone_outside_a_rerun_alone_copy_fails_before_it_clones() {
        if rerun_marked(OUTSIDE_A_COPY) {
            return;
        }
        let refused = std::panic::catch_unwind(|| Container::new().checked_run(|| {}));
        let message = panic_message(refused.expect_err("cloned a container outside a copy"));
        assert!(
            message.contains("call crate::app::test_isolation::rerun_alone() at its start")
                && message.contains("runs no pthread_atfork"),
            "{message}"
        );
    }

    /// In a [`rerun_alone`] copy, a container clone made while another thread
    /// of the test runs fails at the clone site, before the clone, and names
    /// that thread; once the thread has ended, the same clone goes ahead.
    #[test]
    fn a_container_clone_beside_another_thread_of_the_test_fails_before_it_clones() {
        if rerun_alone() {
            return;
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            let _ = stopped.recv();
            nix::unistd::gettid().as_raw()
        });
        let refused = std::panic::catch_unwind(|| Container::new().checked_run(|| {}));
        drop(stop);
        let other = other.join().unwrap();
        let message = panic_message(refused.expect_err("cloned a container beside a thread"));
        assert!(
            message.contains(&format!(
                "while other threads of its process run (IDs [{other}])"
            )) && message.contains("runs no pthread_atfork"),
            "{message}"
        );
        let cloned = Container::new().checked_run(|| 7);
        assert!(matches!(cloned, Ok(7)), "{cloned:?}");
    }

    /// A test that records `--skid-margin` without [`rerun_alone`] fails
    /// before Reverie's PMU configuration takes it, with a message that names
    /// `rerun_alone()`, in a process of its own for the same reason as the
    /// clone test above.
    #[test]
    fn a_skid_margin_recorded_outside_a_rerun_alone_copy_fails_before_it_is_set() {
        if rerun_marked(OUTSIDE_A_COPY) {
            return;
        }
        let opts =
            crate::app::run::RunOpts::parse_from(["fakehermit", "--skid-margin=7777", "fakeprog"]);
        let refused =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| opts.install_pmu_config()));
        let message = panic_message(refused.expect_err("recorded --skid-margin outside a copy"));
        assert!(
            message.contains("call crate::app::test_isolation::rerun_alone() at its start"),
            "{message}"
        );
        assert!(
            reverie_ptrace::set_skid_margin_override(1).is_ok(),
            "the refused --skid-margin reached Reverie's PMU configuration"
        );
    }
}
