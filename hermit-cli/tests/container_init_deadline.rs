/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An external deadline must be able to end a hung `hermit` run.
//!
//! `Container::run` puts the process that carries the guest at PID 1 of a fresh
//! PID namespace, and Linux discards `SIG_DFL`-disposition signals sent to a
//! namespace init even from an ancestor namespace. Before the fix in
//! `bin/hermit/container.rs` that made `timeout N` a no-op against a hung run:
//! `timeout` signalled the process group, the outer `hermit` process died, the
//! init process discarded the same signal, `timeout` exited 124 without
//! escalating, and the init process was reparented to host PID 1 and kept
//! running. Three runs survived that way for more than 45 hours and filled the
//! filesystem.
//!
//! Nothing else in the suite would catch a regression here: every observable a
//! caller normally checks -- the deadline exit code 124, the absence of output,
//! the supervisor returning promptly -- looks identical whether or not the run
//! was actually killed. So these tests assert on process liveness directly.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;
use std::time::Instant;

/// A guest that never finishes on its own, and that does not cooperate with
/// `SIGTERM`, so the only way the run can end is for something to kill it.
///
/// Ignoring `SIGTERM` in the guest is load-bearing, not incidental. `timeout`
/// signals the whole process group, and an ordinary ptrace-backend guest is an
/// ordinary host process in that group: it takes the `SIGTERM`, dies, and
/// `hermit` then exits normally because its guest finished. Measured on a
/// build without the fix, a plain spinning `/bin/sh` guest leaves nothing
/// behind for exactly that reason, so a test using one passes whether or not
/// the container init can be killed. With `trap "" TERM` the guest survives the
/// group signal, which is the shape the out-of-disk incident had -- there the
/// guest was inside a KVM backend and was not a host-visible process in the
/// group at all -- and the container init is then the only thing that can end
/// the run.
const SPINNER: &[&str] = &["/bin/sh", "-c", "trap '' TERM; while : ; do : ; done"];

/// Wall-clock budget for the supervised run in [`bare_timeout_kills_a_hung_hermit_run`].
/// It must comfortably exceed hermit's startup cost on a loaded host, because
/// the test refuses to pass unless it saw the container init alive first.
const DEADLINE_SECS: u64 = 15;

/// How long to wait for hermit to get as far as forking its container init.
const STARTUP_BUDGET: Duration = Duration::from_secs(12);

/// How long a correctly supervised run may take to disappear after the stimulus.
const TEARDOWN_BUDGET: Duration = Duration::from_secs(20);

/// These tests each start a real guest, so run them one at a time.
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Fields of `/proc/<pid>/stat` after the comm field, which is parenthesised and
/// may itself contain spaces and parentheses. Index 0 is `state`, so `session`
/// (the sixth field overall) is index 3.
const STAT_SESSION_INDEX: usize = 3;

fn session_of(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm
        .split_whitespace()
        .nth(STAT_SESSION_INDEX)?
        .parse()
        .ok()
}

/// Every live pid in `session`, which is how we see the whole run -- the
/// supervisor, the outer `hermit` process, the container init, and the guest.
fn pids_in_session(session: i32) -> Vec<i32> {
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if session_of(pid) == Some(session) {
            pids.push(pid);
        }
    }
    pids
}

/// Whether `pid` is PID 1 of a PID namespace below ours. `/proc/<pid>/status`
/// reports `NSpid:` as one entry per namespace from ours inward, so the
/// container init is the process whose innermost pid is 1.
fn is_namespace_init(pid: i32) -> bool {
    let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .is_some_and(|line| {
            let pids: Vec<&str> = line.split_whitespace().collect();
            pids.len() >= 2 && pids[pids.len() - 1] == "1"
        })
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn poll_until<T>(budget: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(50));
    }
}

/// Kills a whole session even when its members are namespace inits, which
/// ignore every default-disposition signal. Used by the cleanup guard so a
/// failing test can never leak the very processes it is about.
fn kill_session(session: i32) {
    for _ in 0..20 {
        let pids = pids_in_session(session);
        if pids.is_empty() {
            return;
        }
        for pid in pids {
            // SAFETY: `kill` takes a pid and a signal number and touches no
            // caller memory. A stale pid can only fail with ESRCH.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        sleep(Duration::from_millis(100));
    }
}

/// Ensures a test failure cannot leave a run behind.
struct SessionGuard(i32);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        kill_session(self.0);
    }
}

/// Spawn `argv` as the leader of its own session, so the test can account for
/// every process the run creates by scanning for that session id.
fn spawn_in_new_session(argv: &[&str]) -> io::Result<Child> {
    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]);
    hermit_test::configure_guest_execution(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and allocates nothing. It is called
    // in the forked child before exec, where this process is not already a
    // session leader, so it cannot fail.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

fn hermit_bin() -> &'static str {
    hermit_test::hermit_binary()
        .to_str()
        .expect("Hermit test binary path is not valid UTF-8")
}

/// Start a hung run and wait until its container init exists, so that any
/// later assertion is about a run that genuinely reached the dangerous state.
fn start_hung_run(argv: &[&str]) -> (Child, SessionGuard, i32) {
    let child = spawn_in_new_session(argv)
        .unwrap_or_else(|error| panic!("failed to spawn {argv:?}: {error}"));
    let session = child.id() as i32;
    let guard = SessionGuard(session);

    let init = poll_until(STARTUP_BUDGET, || {
        pids_in_session(session)
            .into_iter()
            .find(|pid| is_namespace_init(*pid))
    });

    let init = init.unwrap_or_else(|| {
        panic!(
            "hermit never forked a container init within {STARTUP_BUDGET:?} for {argv:?}; \
             the run under test never reached the state this test is about, so a pass \
             here would be vacuous. Live pids in session {session}: {:?}",
            pids_in_session(session)
        )
    });

    (child, guard, init)
}

fn assert_session_drains(session: i32, what: &str) {
    let survivors = poll_until(TEARDOWN_BUDGET, || {
        let pids = pids_in_session(session);
        pids.is_empty().then_some(())
    });
    assert!(
        survivors.is_some(),
        "{what}: processes from this run were still alive {TEARDOWN_BUDGET:?} later: {:?}. \
         This is the out-of-disk incident's failure mode: the run outlives its supervisor \
         and keeps burning CPU and writing logs with nothing watching it.",
        pids_in_session(session)
            .into_iter()
            .map(|pid| {
                let comm = fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
                format!("{pid} ({})", comm.trim())
            })
            .collect::<Vec<_>>()
    );
}

/// The acceptance criterion: a plain `timeout N hermit run ...`, with no
/// `--kill-after` and no `-s KILL`, actually ends the run.
#[test]
fn bare_timeout_kills_a_hung_hermit_run() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let deadline = DEADLINE_SECS.to_string();

    let mut argv = vec!["timeout", deadline.as_str(), hermit_bin(), "run", "--"];
    argv.extend_from_slice(SPINNER);

    let (mut supervisor, guard, init) = start_hung_run(&argv);
    assert!(
        alive(init),
        "container init {init} should still be running while the deadline is pending"
    );

    let status = supervisor.wait().expect("failed to wait for timeout(1)");
    assert_eq!(
        status.code(),
        Some(124),
        "expected timeout(1) to report its deadline (124); got {status:?}. \
         A different code means the guest finished on its own and the test proved nothing."
    );

    assert_session_drains(guard.0, "bare `timeout` against a hung hermit run");
}

/// Isolates the parent-death signal: kill only the outer `hermit` process, with
/// `SIGKILL` so no handler in the container init can be what cleans up, and the
/// container init must still go away.
#[test]
fn container_init_dies_with_the_hermit_process_that_forked_it() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut argv = vec![hermit_bin(), "run", "--"];
    argv.extend_from_slice(SPINNER);

    let (mut hermit, guard, init) = start_hung_run(&argv);
    let outer = hermit.id() as i32;
    assert_ne!(
        outer, init,
        "the container init must be a distinct process from the hermit CLI"
    );

    // SAFETY: `kill` takes a pid and a signal number and touches no caller
    // memory. `outer` is our own child and has not been reaped yet.
    assert_eq!(
        unsafe { libc::kill(outer, libc::SIGKILL) },
        0,
        "failed to kill the outer hermit process: {}",
        io::Error::last_os_error()
    );
    let status = hermit.wait().expect("failed to wait for hermit");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    assert_session_drains(
        guard.0,
        "container init after its hermit parent was killed outright",
    );
}

/// Isolates the stop-signal handlers: signal only the container init, leaving
/// its `hermit` parent alive so the parent-death signal cannot be what ends the
/// run. Each of these is discarded outright while the container init leaves the
/// disposition at `SIG_DFL`.
#[test]
fn container_init_honours_signals_aimed_at_it_directly() {
    let _lock = HERMIT_RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let mut argv = vec![hermit_bin(), "run", "--"];
        argv.extend_from_slice(SPINNER);

        let (mut hermit, guard, init) = start_hung_run(&argv);
        let outer = hermit.id() as i32;
        assert!(
            alive(outer),
            "signal {signal}: the hermit parent must still be alive, or this test \
             would be measuring the parent-death signal instead"
        );

        // SAFETY: `kill` takes a pid and a signal number and touches no caller
        // memory. `init` was observed alive immediately above.
        assert_eq!(
            unsafe { libc::kill(init, signal) },
            0,
            "signal {signal}: failed to signal container init {init}: {}",
            io::Error::last_os_error()
        );

        let died = poll_until(TEARDOWN_BUDGET, || (!alive(init)).then_some(()));
        assert!(
            died.is_some(),
            "signal {signal}: container init {init} ignored it and is still running \
             {TEARDOWN_BUDGET:?} later. The kernel discards default-disposition \
             signals sent to a namespace init, so without a handler this signal \
             never arrives at all."
        );

        let _ = hermit.wait();
        assert_session_drains(guard.0, &format!("run after signal {signal} to its init"));
    }
}
