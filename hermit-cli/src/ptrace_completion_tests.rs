/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in LICENSE.
 */

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use super::*;

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) },
        0
    );
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn process_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut ts) },
        0
    );
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

// The child tree's CPU: utime+stime+cutime+cstime summed over every live
// process in the tree, so a worker reaped by a still-live middle process
// counts immediately through that process's cutime/cstime. pgid_cpu in
// tests/external_signal_interrupt.rs walks the same children tree, but
// takes cutime+cstime at the root only and utime+stime at every process.
//
// Fail-closed contract: only the unreaped root must be readable. A
// descendant that exits between its parent's `children` listing and its own
// stat read has vanished; the read can fail with either ENOENT (NotFound)
// or ESRCH (proc_single_show if it was reaped between open and read). CPU
// it was reaped with is already in an ancestor's cutime/cstime term of this
// same sum, so it is skipped. Every other failure returns Err naming the
// cause; callers route it through the kill path before panicking, so a
// measurement error never leaks the child.
fn tree_cpu(root: u32) -> Result<Duration, String> {
    let clk_tck = {
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        if tck > 0 { tck as u64 } else { 100 }
    };
    let mut ticks: u64 = 0;
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if seen.len() > 4096 {
            return Err(format!(
                "process tree of {root} exceeds the 4096-process cap"
            ));
        }
        if !seen.insert(pid) {
            continue;
        }
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error)
                if pid != root
                    && (error.kind() == std::io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH)) =>
            {
                // Vanished descendant: skip it (see the contract above).
                continue;
            }
            Err(error) => {
                return Err(format!("cannot read /proc/{pid}/stat: {error}"));
            }
        };
        let Some(close) = stat.rfind(')') else {
            return Err(format!("malformed /proc/{pid}/stat"));
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        if fields.len() < 15 {
            return Err(format!("short /proc/{pid}/stat ({} fields)", fields.len()));
        }
        for i in [11, 12, 13, 14] {
            let tick = fields[i]
                .parse::<u64>()
                .map_err(|_| format!("unparsable tick field {i} in /proc/{pid}/stat"))?;
            ticks = ticks.saturating_add(tick);
        }
        if let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            for task in tasks.flatten() {
                if let Ok(children) = std::fs::read_to_string(task.path().join("children")) {
                    stack.extend(
                        children
                            .split_whitespace()
                            .filter_map(|text| text.parse::<u32>().ok()),
                    );
                }
            }
        }
    }
    Ok(Duration::from_secs_f64(ticks as f64 / clk_tck as f64))
}

// SIGKILL the child's whole process group and reap the child. Every failure
// path out of the sampling loop goes through here first, so no panic —
// budget trip, backstop trip or measurement error — leaves the group running.
fn kill_child_group(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let rescue = Instant::now() + Duration::from_secs(2);
    while Instant::now() < rescue {
        match child.try_wait() {
            // A wait error while rescuing must not mask the caller's named
            // budget or measurement failure; the group kill has been sent.
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

// One isolated case's child-CPU budget is the primary limit (CPU-primary
// testing rule); the wall backstop below only catches a child that hangs
// without burning CPU. Load inflates wall time far more than CPU time,
// but contention also inflates measured child CPU itself (the
// real_random loaded medians are 0.92-1.12 s against 0.41-0.49 s at
// light load), so budgets are sized from loaded measurements rather
// than light ones. Every isolated case declares its budget at its call
// site — there is no default. Each budget is 1.5x the worst recorded
// run, completed or censored, across every same-measurand sample of
// the same test body (the loaded sample and the earlier recorded
// light-load samples), rounded up to the next 0.1 s; a run killed at
// a budget counts at its kill reading, with the budget taking the
// first bucket strictly above 1.5x that reading. The
// loaded sample was measured 2026-11-23 (a throwaway instrument build
// with budgets lifted, the env-gated HERMIT_MEASURE_CHILD_CPU print in
// this wrapper emitting one MEASURE line per completed child from
// tree_cpu itself), run directly per case (libtest --exact) in
// dev-nosan, 8 workers confined to one full core (AllowedCPUs=0,158)
// on an AMD EPYC 9D85 host (CPUID faulting present): 245 passed runs
// over 24 cases, n=10 per case (a_tracer_thread_operation n=15). The
// light-load samples are the earlier n=20-per-mode runs of the same
// bodies, plus pooled re-measurements of original_runtime_guards and
// mid_startup; one original_runtime_guards run there was killed at the
// former 0.2 s budget, proving a completion above 0.2 s, so that case
// takes the first bucket strictly above 1.5x 0.2 s. A 2026-11-24
// loaded re-measurement of readv_live_import and the four other
// real_random cases on the current observer body (same arm and
// instrument, n=20 each) is also a sizing sample for those five.
// Budgets run 0.3-2.9 s. Each call site's comment names the governing
// maxima. Sources on minibeads dh-74hbms: loaded full-set table and
// raw-record hashes (comment dh-74hbms-ccdcd578607), earlier
// light-load per-run table (comment dh-74hbms-c16418f641d), the
// combined-rule check with the five light-governed cases (comment
// dh-74hbms-c3f8ae4dea3), and the two re-measurements (comments
// dh-74hbms-cb289d91ad5 and dh-74hbms-c6bbd55ac68). Intel hosts are
// unmeasured; no host-class factor is inferred.

// Wall backstop for one isolated case: a generous outer hang-catcher,
// never a load-sensitive gate, so it is sized from the LOADED wall
// measurement above like the CPU budgets. The largest in-process wall
// time under that load was 7.062 s; 57 s is the ceiling of 8x that
// maximum (56.5 s), anchored to that 2026-11-23 sample: against the
// worst wall time recorded since (8.997 s, in the re-measurements) its
// margin is 6.3x, and the later samples are not folded in. This
// relaxes the previous 21 s backstop, which was
// sized from light-load wall times, for every isolated case and with it
// the child's own pre-start deadline (HERMIT_OWNER_TEST_DEADLINE). Only
// a hang reaches it; the CPU budget remains the primary bound.
const CHILD_WALL_BACKSTOP_NS: u64 = 57_000_000_000;

// Run one isolated case under the measured child-CPU budget its caller declares.
fn isolated_budgeted(name: &str, cpu_budget: Duration, body: impl FnOnce()) {
    isolated_with_child_setup_budgeted(name, &[], cpu_budget, || Ok(()), body)
}

// `child_setup` runs after fork and before exec: it must use only async-signal-safe
// operations. Opt-in credential changes must never affect the outer libtest process.
fn isolated_with_child_setup_budgeted(
    name: &str,
    environment: &[(&str, &std::path::Path)],
    cpu_budget: Duration,
    child_setup: fn() -> std::io::Result<()>,
    body: impl FnOnce(),
) {
    if std::env::var("HERMIT_OWNER_TEST_ROLE").as_deref() == Ok(name) {
        let deadline: u64 = std::env::var("HERMIT_OWNER_TEST_DEADLINE")
            .unwrap()
            .parse()
            .unwrap();
        body();
        assert!(
            monotonic_ns() < deadline,
            "case exceeded original pre-start deadline"
        );
        println!("OWNER_CASE {name} passed before original deadline");
        return;
    }
    let started_ns = monotonic_ns();
    let deadline = started_ns + CHILD_WALL_BACKSTOP_NS;
    let test = format!("ptrace_completion::tests::{name}");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env("HERMIT_OWNER_TEST_ROLE", name)
        .env("HERMIT_OWNER_TEST_DEADLINE", deadline.to_string())
        // The child's CPU is the budget measurand, so its environment must
        // not let the runner's backtrace setting into the measurement: a
        // symbolized backtrace for a deliberate panic costs ~0.2 s of child
        // CPU under RUST_BACKTRACE=1 (measured), which is runner state,
        // not case behavior. Per-case environment below may still override.
        .env("RUST_BACKTRACE", "0")
        .env("RUST_LIB_BACKTRACE", "0");
    for (key, value) in environment {
        command.env(key, value);
    }
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            child_setup()
        });
    }
    let mut child = command.spawn().unwrap();
    // CPU is sampled at a coarse cadence; try_wait stays tight. Completion
    // is detected with waitid(WNOWAIT) so the final CPU reading is taken
    // while the zombie still holds its accounting, before reaping. Every
    // exit from the watch loop passes through the single wait() below, so
    // the child is reaped on all paths; failures are named after it.
    enum Watch {
        Completed { now_ns: u64, wall: Duration },
        Failed(String),
        MeasureAfterExit(String),
    }
    let mut child_cpu = Duration::ZERO;
    let mut last_sample = 0u64;
    let outcome = 'watch: loop {
        let now_ns = monotonic_ns();
        let wall = Duration::from_nanos(now_ns - started_ns);
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if waited != 0 {
            let error = std::io::Error::last_os_error();
            let pid = child.id();
            kill_child_group(&mut child);
            break 'watch Watch::Failed(format!(
                "child CPU measurement failed: waitid on {pid}: {error}"
            ));
        }
        if unsafe { info.si_pid() } != 0 {
            match tree_cpu(child.id()) {
                Ok(cpu) => {
                    child_cpu = cpu;
                    break 'watch Watch::Completed { now_ns, wall };
                }
                Err(reason) => {
                    // The root is an unreaped zombie here; kill any surviving
                    // members of its process group before failing so the
                    // measurement error leaks nothing.
                    kill_child_group(&mut child);
                    break 'watch Watch::MeasureAfterExit(reason);
                }
            }
        }
        if now_ns - last_sample >= 25_000_000 {
            last_sample = now_ns;
            match tree_cpu(child.id()) {
                Ok(cpu) => child_cpu = cpu,
                Err(reason) => {
                    eprintln!("OWNER_CASE {name} CPU measurement FAILED; separate rescue begins");
                    kill_child_group(&mut child);
                    break 'watch Watch::Failed(format!("child CPU measurement failed: {reason}"));
                }
            }
        }
        if child_cpu >= cpu_budget || now_ns >= deadline {
            let bound = if child_cpu >= cpu_budget {
                "CPU budget"
            } else {
                "wall backstop"
            };
            eprintln!("OWNER_CASE {name} {bound} FAILED; separate rescue begins");
            kill_child_group(&mut child);
            break 'watch Watch::Failed(format!(
                "selected child test exceeded its {bound}: {:.3} s CPU of {:.3} s CPU budget (wall limit {:.3} s) after {:.3} s wall",
                child_cpu.as_secs_f64(),
                cpu_budget.as_secs_f64(),
                Duration::from_nanos(CHILD_WALL_BACKSTOP_NS).as_secs_f64(),
                wall.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    // Only the arms whose pre-restructure code unwrapped the wait do so
    // here; a failed wait on a failure path must never replace that
    // path's named panic with an anonymous unwrap panic.
    let waited = child.wait();
    match outcome {
        Watch::Completed { now_ns, wall } => {
            let status = waited.unwrap();
            assert!(
                child_cpu < cpu_budget && now_ns < deadline,
                "late child completion {status}: {:.3} s CPU of {:.3} s CPU budget (wall limit {:.3} s) after {:.3} s wall",
                child_cpu.as_secs_f64(),
                cpu_budget.as_secs_f64(),
                Duration::from_nanos(CHILD_WALL_BACKSTOP_NS).as_secs_f64(),
                wall.as_secs_f64()
            );
            assert!(status.success(), "selected child test failed: {status}");
        }
        Watch::Failed(message) => panic!("{message}"),
        Watch::MeasureAfterExit(reason) => {
            let status = waited.unwrap();
            panic!(
                "child CPU measurement failed: {reason} \
                 (child already exited: {status})"
            );
        }
    }
}

// Permanent negative test: a child that only burns CPU must trip its CPU
// budget (800 ms, above this binary's startup CPU) while its wall time stays
// far below the backstop, proving the limit that fires is the CPU budget.
#[test]
#[should_panic(expected = "exceeded its CPU budget")]
fn a_cpu_burning_child_trips_its_cpu_budget() {
    isolated_budgeted(
        "a_cpu_burning_child_trips_its_cpu_budget",
        Duration::from_millis(800),
        || {
            let start_cpu = process_cpu_ns();
            let mut acc = 0u64;
            // Bound the spin by this child's own process CPU, not wall time;
            // 2 s is comfortably beyond the 800 ms budget that must kill it.
            while process_cpu_ns() - start_cpu < 2_000_000_000 {
                acc = acc.wrapping_add(1);
                std::hint::black_box(acc);
            }
        },
    );
}

// Red control at the smallest resized case budget: the same planted
// spinner must also trip a 300 ms budget by name, so the loaded resize
// cannot silently stop enforcing the budget class most cases now carry.
#[test]
#[should_panic(expected = "exceeded its CPU budget")]
fn a_cpu_burning_child_trips_the_smallest_case_budget() {
    isolated_budgeted(
        "a_cpu_burning_child_trips_the_smallest_case_budget",
        Duration::from_millis(300),
        || {
            let start_cpu = process_cpu_ns();
            let mut acc = 0u64;
            // Bound the spin by this child's own process CPU; 1 s is
            // comfortably beyond the 300 ms budget that must kill it.
            while process_cpu_ns() - start_cpu < 1_000_000_000 {
                acc = acc.wrapping_add(1);
                std::hint::black_box(acc);
            }
        },
    );
}

// The measurement contract: a live root reads fine, and a missing root
// fails closed by name (only a vanished *descendant* may be skipped).
#[test]
fn tree_cpu_reads_a_live_root_and_fails_closed_on_a_missing_one() {
    // Burn at least 30 ms of this process's own CPU before reading. A fresh
    // opt process can legitimately have accumulated less than one 10 ms
    // /proc tick, so positivity must be established rather than assumed.
    let start_cpu = process_cpu_ns();
    let mut acc = 0u64;
    while process_cpu_ns() - start_cpu < 30_000_000 {
        acc = acc.wrapping_add(1);
        std::hint::black_box(acc);
    }
    let own = tree_cpu(std::process::id()).expect("own process must be readable");
    assert!(own > Duration::ZERO, "own CPU should be positive: {own:?}");
    let missing = tree_cpu(u32::MAX).expect_err("a missing root must fail closed");
    assert!(
        missing.contains("cannot read /proc/4294967295/stat"),
        "missing-root error must name the stat it could not read: {missing}"
    );
}

fn pending<T: std::fmt::Debug>(result: Result<T, Error>) -> HermitCleanupUnconfirmed {
    result
        .unwrap_err()
        .downcast::<HermitCleanupUnconfirmed>()
        .unwrap()
}
fn refusal<T: std::fmt::Debug>(
    result: Result<T, Error>,
    expected: impl FnOnce(&RecoveryRefusal) -> bool,
) {
    let error = result.unwrap_err().downcast::<RecoveryRefusal>().unwrap();
    assert!(expected(&error), "unexpected refusal: {error}");
}

#[test]
fn original_runtime_guards_and_admission_survive_checkpoint() {
    isolated_budgeted(
        "original_runtime_guards_and_admission_survive_checkpoint",
        // Worst run: loaded n=10 max 0.190 s, light max 0.190 s plus one
        // run killed at the former 0.2 s budget -> 0.4 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(400),
        || {
            let polls = Rc::new(Cell::new(0));
            let path = Rc::new(RefCell::new(PathBuf::new()));
            let outer_path = path.clone();
            let outer_polls = polls.clone();
            let diagnostic = pending(run(None, move |control| async move {
                let directory = tempfile::tempdir()?;
                *path.borrow_mut() = directory.path().to_path_buf();
                let (send, receive) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    send.send(73u32).unwrap();
                });
                polls.set(polls.get() + 1);
                control.yield_checkpoint().await;
                let value = receive.await?;
                assert!(directory.path().exists());
                Ok(value)
            }));
            assert!(outer_path.borrow().exists());
            assert_eq!(outer_polls.get(), 1);
            refusal(diagnostic.resume::<u64>(), |e| {
                matches!(e, RecoveryRefusal::WrongResultType)
            });
            let other = diagnostic.clone();
            std::thread::spawn(move || {
                refusal(other.resume::<u32>(), |e| {
                    matches!(e, RecoveryRefusal::WrongIdentity)
                })
            })
            .join()
            .unwrap();
            let factory_called = Cell::new(false);
            refusal(
                run(None, |_| {
                    factory_called.set(true);
                    async { Ok(1u32) }
                }),
                |e| matches!(e, RecoveryRefusal::AdmissionClosed),
            );
            assert!(!factory_called.get());
            assert_eq!(diagnostic.resume::<u32>().unwrap(), 73);
            assert_eq!(outer_polls.get(), 1);
            assert!(!outer_path.borrow().exists());
            refusal(diagnostic.resume::<u32>(), |e| {
                matches!(e, RecoveryRefusal::UnknownKey)
            });
            assert_eq!(run(None, |_| async { Ok(89u32) }).unwrap(), 89);
        },
    );
}

/// An operation that a public launch function starts runs on the calling
/// thread's tracer thread (https://github.com/rrnewton/hermit/issues/4389).
/// Its checkpoint is resumed from the caller, on that same tracer thread, and
/// refused from any other thread.
#[test]
fn a_tracer_thread_operation_resumes_from_its_caller() {
    isolated_budgeted(
        "a_tracer_thread_operation_resumes_from_its_caller",
        // Worst run: loaded n=15 max 0.170 s, light max 0.190 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let caller = std::thread::current().id();
            let on_tracer = move || {
                std::thread::current().id() != caller
                    && std::thread::current().name() == Some("hermit-tracer")
            };
            let diagnostic = pending(run_on_tracer_thread(None, move |control| async move {
                let started = on_tracer();
                control.yield_checkpoint().await;
                Ok(started && on_tracer())
            }));
            let other = diagnostic.clone();
            std::thread::spawn(move || {
                refusal(other.resume::<bool>(), |e| {
                    matches!(e, RecoveryRefusal::WrongIdentity)
                })
            })
            .join()
            .unwrap();
            assert!(diagnostic.resume::<bool>().unwrap());
            assert!(run_on_tracer_thread(None, move |_| async move { Ok(on_tracer()) }).unwrap());
        },
    );
}

/// Runs `child` in a forked child and returns its exit status: 0 when it
/// returned true. `None` if it had not exited within two seconds, when it is
/// killed; a stale tracer handle shows as such a hang.
fn in_forked_child(child: impl FnOnce() -> bool) -> Option<i32> {
    match unsafe { libc::fork() } {
        0 => {
            let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(child)).unwrap_or(false);
            unsafe { libc::_exit(if ok { 0 } else { 1 }) }
        }
        pid => {
            assert!(pid > 0, "fork failed");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut status = 0;
            loop {
                if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid {
                    return libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
                }
                if Instant::now() >= deadline {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                        libc::waitpid(pid, &mut status, 0);
                    }
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A child forked after a launch inherits its parent's tracer handle but not
/// the thread. Its own launch gets a tracer thread of its own instead of
/// waiting on the parent's forever (Claude review of the tracer thread,
/// https://github.com/rrnewton/hermit/issues/4389).
#[test]
fn a_forked_child_launches_on_a_tracer_thread_of_its_own() {
    isolated_budgeted(
        "a_forked_child_launches_on_a_tracer_thread_of_its_own",
        // Worst run: loaded n=10 max 0.170 s, light max 0.230 s -> 0.4 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(400),
        || {
            assert_eq!(
                run_on_tracer_thread(None, |_| async { Ok(1u32) }).unwrap(),
                1
            );
            let child = in_forked_child(|| {
                run_on_tracer_thread(None, |_| async { Ok(2u32) }).is_ok_and(|value| value == 2)
            });
            assert_eq!(child, Some(0));
            assert_eq!(
                run_on_tracer_thread(None, |_| async { Ok(3u32) }).unwrap(),
                3
            );
        },
    );
}

/// A forked child cannot resume its parent's retained operation: its resume is
/// not sent to the parent's tracer thread, which it does not have, and the
/// identity check refuses it, as before the tracer thread.
#[test]
fn a_forked_child_cannot_resume_its_parents_operation() {
    isolated_budgeted(
        "a_forked_child_cannot_resume_its_parents_operation",
        // Worst run: loaded n=10 max 0.160 s, light max 0.150 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let diagnostic = pending(run_on_tracer_thread(None, |control| async move {
                control.yield_checkpoint().await;
                Ok(5u32)
            }));
            let other = diagnostic.clone();
            let child = in_forked_child(move || {
                other
                    .resume::<u32>()
                    .unwrap_err()
                    .downcast::<RecoveryRefusal>()
                    .is_ok_and(|e| matches!(e, RecoveryRefusal::WrongIdentity))
            });
            assert_eq!(child, Some(0));
            assert_eq!(diagnostic.resume::<u32>().unwrap(), 5);
        },
    );
}

/// Enters a user namespace, plus `extra` namespaces, mapping the caller to
/// root there as `unshare -Ur` does, so the test keeps capabilities across
/// exec. For `isolated_with_child_setup`: it runs between fork and exec, while
/// the child has one thread, which a new user namespace needs, and uses only
/// raw calls on stack buffers. A PID namespace has to wait for the test body:
/// once a process unshares one, it can no longer start threads, and libtest
/// starts the test's.
fn enter_root_mapped_user_namespace(extra: libc::c_int) -> std::io::Result<()> {
    fn write_proc(path: &std::ffi::CStr, data: &[u8]) -> std::io::Result<()> {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        unsafe { libc::close(fd) };
        if written == data.len() as isize {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    // "0 <id> 1": map `id` outside to root inside.
    fn map_line(buf: &mut [u8; 32], mut id: u32) -> usize {
        let mut digits = [0u8; 10];
        let mut count = 0;
        loop {
            digits[count] = b'0' + (id % 10) as u8;
            count += 1;
            id /= 10;
            if id == 0 {
                break;
            }
        }
        buf[..2].copy_from_slice(b"0 ");
        for i in 0..count {
            buf[2 + i] = digits[count - 1 - i];
        }
        buf[2 + count..4 + count].copy_from_slice(b" 1");
        4 + count
    }
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | extra) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    write_proc(c"/proc/self/setgroups", b"deny")?;
    let mut line = [0u8; 32];
    let len = map_line(&mut line, uid);
    write_proc(c"/proc/self/uid_map", &line[..len])?;
    let len = map_line(&mut line, gid);
    write_proc(c"/proc/self/gid_map", &line[..len])
}

fn enter_user_namespace() -> std::io::Result<()> {
    enter_root_mapped_user_namespace(0)
}

fn enter_user_and_mount_namespaces() -> std::io::Result<()> {
    enter_root_mapped_user_namespace(libc::CLONE_NEWNS)
}

/// A child in a new PID namespace can have its parent's PID: a container init
/// that unshares a PID namespace and forks makes another PID 1. Its stale
/// tracer handle is still recognised, by the namespace: the child's resume of
/// the parent's operation is refused, and its launch reaches admission on a
/// tracer thread of its own instead of waiting on the parent's (Claude
/// re-check of the tracer thread).
#[test]
fn a_child_with_its_parents_pid_in_a_new_pid_namespace_has_its_own_tracer() {
    isolated_with_child_setup_budgeted(
        "a_child_with_its_parents_pid_in_a_new_pid_namespace_has_its_own_tracer",
        &[],
        // Worst run: loaded n=10 max 0.170 s, light max 0.180 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        enter_user_namespace,
        || {
            // A PID namespace for this process's children, then a fork into it
            // to become its PID 1.
            assert_eq!(
                unsafe { libc::unshare(libc::CLONE_NEWPID) },
                0,
                "unshare: {}",
                std::io::Error::last_os_error()
            );
            let init = in_forked_child(|| {
                // PID 1, with a tracer thread and a retained operation.
                if std::process::id() != 1 {
                    return false;
                }
                let diagnostic = pending(run_on_tracer_thread(None, |control| async move {
                    control.yield_checkpoint().await;
                    Ok(7u32)
                }));
                // A process can pick its children's PID namespace once, so one
                // child, another PID 1, makes both probes.
                if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
                    return false;
                }
                let other = diagnostic.clone();
                let child = in_forked_child(move || {
                    std::process::id() == 1
                        // Its resume is refused by identity, not sent to the
                        // parent's tracer thread.
                        && other
                            .resume::<u32>()
                            .unwrap_err()
                            .downcast::<RecoveryRefusal>()
                            .is_ok_and(|e| matches!(e, RecoveryRefusal::WrongIdentity))
                        // Its launch reaches admission on a tracer thread of
                        // its own, which refuses it while the copied operation
                        // is retained, as before the tracer thread; on the
                        // parent's missing thread it would never return.
                        && run_on_tracer_thread(None, |_| async { Ok(8u32) })
                            .unwrap_err()
                            .downcast::<RecoveryRefusal>()
                            .is_ok_and(|e| matches!(e, RecoveryRefusal::AdmissionClosed))
                });
                child == Some(0) && diagnostic.resume::<u32>().is_ok_and(|value| value == 7)
            });
            assert_eq!(init, Some(0));
        },
    );
}

/// This host's kernel has PID namespaces: the `pid` entry is read and held.
#[test]
fn the_pid_namespace_is_read_from_its_entry() {
    let (namespace, file) = open_pid_namespace().unwrap();
    let file = file.expect("the namespace file is held");
    let meta = file.metadata().unwrap();
    assert_eq!(namespace, PidNamespace::Inode(meta.dev(), meta.ino()));
}

/// On a kernel with PID namespaces, a `pid` entry hidden from the caller's
/// `ns` directory is never read as a kernel without them: not under a tmpfs
/// mounted over that directory (not procfs), nor under another procfs
/// directory mounted there (which has no `mnt` entry either) (Codex review).
#[test]
fn a_masked_ns_directory_is_not_read_as_a_kernel_without_pid_namespaces() {
    isolated_with_child_setup_budgeted(
        "a_masked_ns_directory_is_not_read_as_a_kernel_without_pid_namespaces",
        &[],
        // Worst run: loaded n=10 max 0.180 s, light max 0.180 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        enter_user_and_mount_namespaces,
        || {
            let pid = std::process::id();
            let ns = std::ffi::CString::new(format!("/proc/{pid}/ns")).unwrap();
            let fdinfo = std::ffi::CString::new(format!("/proc/{pid}/fdinfo")).unwrap();
            let mount = |source: &std::ffi::CStr, kind: Option<&std::ffi::CStr>, flags| {
                let result = unsafe {
                    libc::mount(
                        source.as_ptr(),
                        ns.as_ptr(),
                        kind.map_or(std::ptr::null(), |kind| kind.as_ptr()),
                        flags,
                        std::ptr::null(),
                    )
                };
                if result == 0 {
                    return true;
                }
                let error = std::io::Error::last_os_error();
                // A host whose user namespaces may not mount over procfs
                // cannot mask the entry this way either.
                assert_eq!(
                    error.raw_os_error(),
                    Some(libc::EPERM),
                    "mount over {ns:?}: {error}"
                );
                false
            };
            let unmount = || {
                assert_eq!(unsafe { libc::umount2(ns.as_ptr(), libc::MNT_DETACH) }, 0);
            };
            if !mount(c"tmpfs", Some(c"tmpfs"), 0) {
                eprintln!("SKIPPED: this host refuses mounts over procfs in a user namespace");
                return;
            }
            assert!(
                open_pid_namespace().is_err(),
                "a tmpfs over the ns directory"
            );
            unmount();
            assert!(mount(&fdinfo, None, libc::MS_BIND));
            assert!(
                open_pid_namespace().is_err(),
                "a procfs directory with no mnt entry over the ns directory"
            );
            unmount();
            assert!(matches!(
                open_pid_namespace(),
                Ok((PidNamespace::Inode(..), Some(_)))
            ));
        },
    );
}

/// Without a readable PID namespace (here a tmpfs over `/proc`), a process
/// cannot tell its own tracer handle from an inherited one, so launches run on
/// the calling thread, as before the tracer thread, and refuse there
/// (`Identity`). A PID 1 forked into a new PID namespace by such a process,
/// with the same PID, refuses at once too instead of waiting on a tracer thread
/// it does not have (Claude review of
/// https://github.com/rrnewton/hermit/issues/4389 follow-up).
#[test]
fn without_a_readable_pid_namespace_launches_stay_on_the_caller() {
    isolated_with_child_setup_budgeted(
        "without_a_readable_pid_namespace_launches_stay_on_the_caller",
        &[],
        // Worst run: loaded n=10 max 0.190 s, light max 0.210 s -> 0.4 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(400),
        enter_user_and_mount_namespaces,
        || {
            assert_eq!(
                unsafe {
                    libc::mount(
                        c"tmpfs".as_ptr(),
                        c"/proc".as_ptr(),
                        c"tmpfs".as_ptr(),
                        0,
                        std::ptr::null(),
                    )
                },
                0,
                "mount tmpfs over /proc: {}",
                std::io::Error::last_os_error()
            );
            let refused_identity = || {
                run_on_tracer_thread(None, |_| async { Ok(1u32) })
                    .unwrap_err()
                    .downcast::<RecoveryRefusal>()
                    .is_ok_and(|e| matches!(e, RecoveryRefusal::Identity(_)))
            };
            assert_eq!(
                unsafe { libc::unshare(libc::CLONE_NEWPID) },
                0,
                "unshare: {}",
                std::io::Error::last_os_error()
            );
            let init = in_forked_child(move || {
                if std::process::id() != 1 || !refused_identity() {
                    return false;
                }
                if unsafe { libc::unshare(libc::CLONE_NEWPID) } != 0 {
                    return false;
                }
                in_forked_child(move || std::process::id() == 1 && refused_identity()) == Some(0)
            });
            assert_eq!(init, Some(0));
        },
    );
}

#[test]
fn nested_runtime_refuses_before_factory() {
    isolated_budgeted(
        "nested_runtime_refuses_before_factory",
        // Worst run: loaded n=10 max 0.160 s, light max 0.280 s -> 0.5 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(500),
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let called = Cell::new(false);
            runtime.block_on(async {
                refusal(
                    run(None, |_| {
                        called.set(true);
                        async { Ok(()) }
                    }),
                    |e| matches!(e, RecoveryRefusal::NestedRuntime),
                );
            });
            assert!(!called.get());
            assert!(run(None, |_| async { Ok(()) }).is_ok());
        },
    );
}

#[test]
fn repeated_checkpoint_keeps_one_owner_and_reentry_refuses() {
    isolated_budgeted(
        "repeated_checkpoint_keeps_one_owner_and_reentry_refuses",
        // Worst run: loaded n=10 max 0.150 s, light max 0.170 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let visits = Rc::new(Cell::new(0));
            let copy = visits.clone();
            let first = pending(run(None, move |control| async move {
                copy.set(1);
                control.yield_checkpoint().await;
                copy.set(2);
                control.yield_checkpoint().await;
                copy.set(3);
                Ok(101u32)
            }));
            let entry = OWNERS.with(|o| o.borrow().get(&first.key).unwrap().clone());
            // Private refusal seam, not a claim that public nested-runtime rejection
            // permits recursively entering block_on.
            entry.in_flight.set(true);
            refusal(first.resume::<u32>(), |e| {
                matches!(e, RecoveryRefusal::Reentrant)
            });
            entry.in_flight.set(false);
            assert_eq!(visits.get(), 1);
            let second = pending(first.resume::<u32>());
            assert_eq!(first.key, second.key);
            assert_eq!(visits.get(), 2);
            assert_eq!(second.resume::<u32>().unwrap(), 101);
            assert_eq!(visits.get(), 3);
            assert_eq!(*UNRESOLVED.lock().unwrap(), 0);
        },
    );
}

struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct PanicFuture {
    polls: Arc<AtomicUsize>,
    _guard: DropCount,
}
impl Future for PanicFuture {
    type Output = Result<(), Error>;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        panic!("owned poll panic discriminator");
    }
}
#[test]
fn initial_poll_panic_retains_future_and_poison_refuses_repoll() {
    isolated_budgeted(
        "initial_poll_panic_retains_future_and_poison_refuses_repoll",
        // Worst run: loaded n=10 max 0.170 s, light max 0.200 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let polls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let diagnostic = pending(run(None, |_| PanicFuture {
                polls: polls.clone(),
                _guard: DropCount(drops.clone()),
            }));
            assert_eq!(diagnostic.stage(), HermitCleanupStage::Poisoned);
            assert_eq!(diagnostic.primary_kind(), FailureKind::Panic);
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            refusal(diagnostic.resume::<()>(), |e| {
                matches!(e, RecoveryRefusal::Poisoned)
            });
            refusal(run(None, |_| async { Ok(()) }), |e| {
                matches!(e, RecoveryRefusal::AdmissionClosed)
            });
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        },
    );
}

#[test]
fn origin_thread_exit_retains_resources_and_process_admission() {
    isolated_budgeted(
        "origin_thread_exit_retains_resources_and_process_admission",
        // Worst run: loaded n=10 max 0.160 s, light max 0.210 s -> 0.4 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(400),
        || {
            let drops = Arc::new(AtomicUsize::new(0));
            let copy = drops.clone();
            let diagnostic = std::thread::spawn(move || {
                pending(run(None, move |control| async move {
                    let guard = DropCount(copy);
                    control.yield_checkpoint().await;
                    std::hint::black_box(&guard);
                    Ok(())
                }))
            })
            .join()
            .unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            refusal(diagnostic.resume::<()>(), |e| {
                matches!(e, RecoveryRefusal::WrongIdentity)
            });
            refusal(run(None, |_| async { Ok(()) }), |e| {
                matches!(e, RecoveryRefusal::AdmissionClosed)
            });
            drop(diagnostic);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(*UNRESOLVED.lock().unwrap(), 1);
        },
    );
}

#[test]
fn absolute_startup_deadline_and_failed_cleanup_checkpoint() {
    isolated_budgeted(
        "absolute_startup_deadline_and_failed_cleanup_checkpoint",
        // Worst run: loaded n=10 max 0.430 s, light max 0.380 s -> 0.7 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(700),
        || {
            let started = Rc::new(Cell::new(false));
            let copy = started.clone();
            let diagnostic = pending(run(Some(Duration::ZERO), |_| async move {
                copy.set(true);
                Ok(19u32)
            }));
            assert!(!started.get());
            assert_eq!(diagnostic.primary_kind(), FailureKind::RunTimeout);
            assert!(
                diagnostic
                    .resume::<u32>()
                    .unwrap_err()
                    .is::<GuestTimedOut>()
            );
            assert!(started.get());
            // Private control seam exercises the actual driver budget. It does not
            // substitute for a real Detcore failed-GlobalState join integration test.
            let began = Instant::now();
            let checkpoint = pending(run(None, |control| async move {
                control.stage.set(HermitCleanupStage::GlobalStateCleanup);
                control.received_failure.set(true);
                control
                    .global_cleanup_deadline
                    .set(Some(Instant::now() + ATTEMPT_BUDGET));
                std::future::pending::<()>().await;
                Ok(())
            }));
            assert!(began.elapsed() >= ATTEMPT_BUDGET);
            assert_eq!(checkpoint.stage(), HermitCleanupStage::GlobalStateCleanup);
            assert_eq!(*UNRESOLVED.lock().unwrap(), 1);
        },
    );
}

#[test]
fn origin_pidfd_and_primary_serialization_refuse_false_proofs() {
    isolated_budgeted(
        "origin_pidfd_and_primary_serialization_refuse_false_proofs",
        // Worst run: loaded n=10 max 0.160 s, light max 0.150 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let process = OriginalProcess::capture(std::process::id()).unwrap();
            process.check().unwrap();
            assert!(
                matches!(OriginalProcess::check_fd(-1), Err(RecoveryRefusal::Identity(e)) if e.raw_os_error() == Some(libc::EBADF))
            );
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(fd >= 0);
            unsafe {
                libc::close(fd);
            }
            assert!(
                matches!(OriginalProcess::check_fd(fd), Err(RecoveryRefusal::OriginalProcessUnavailable { events, .. }) if events & libc::POLLNVAL != 0)
            );
            let diagnostic = pending(run(None, |control| async move {
                control.yield_checkpoint().await;
                Ok(())
            }));
            let serialized = crate::SerializableError::from(
                Error::new(diagnostic.clone()).context(GuestTimedOut {
                    limit: Duration::from_secs(1),
                    survivors: None,
                }),
            );
            assert_eq!(serialized.kind(), FailureKind::Error);
            assert_eq!(
                serialized.cleanup_stage(),
                Some(HermitCleanupStage::Startup)
            );
            diagnostic.resume::<()>().unwrap();
            fn assert_send_sync<T: Send + Sync>() {}
            assert_send_sync::<HermitCleanupUnconfirmed>();
        },
    );
}

#[path = "ptrace_origin_tests.rs"]
mod origin_tests;

#[test]
fn mid_startup_timeout_retains_original_waiter_and_resources() {
    isolated_budgeted(
        "mid_startup_timeout_retains_original_waiter_and_resources",
        // Worst run: loaded n=10 max 0.420 s, light max 0.440 s -> 0.7 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(700),
        || {
            let polls = Rc::new(Cell::new(0));
            let resumed = Rc::new(Cell::new(false));
            let path = Rc::new(RefCell::new(PathBuf::new()));
            let drops = Arc::new(AtomicUsize::new(0));
            let (send, receive) = tokio::sync::oneshot::channel::<u32>();
            let copy_polls = polls.clone();
            let copy_resumed = resumed.clone();
            let copy_path = path.clone();
            let copy_drops = drops.clone();
            let diagnostic = pending(run(Some(Duration::from_millis(10)), move |_| async move {
                // An actual async startup dependency, not a synthetic completed
                // tracer or termination handle. The retained oneshot, directory and
                // DropCount all belong to this one original future/runtime.
                let directory = tempfile::tempdir()?;
                let _guard = DropCount(copy_drops);
                *copy_path.borrow_mut() = directory.path().to_path_buf();
                copy_polls.set(copy_polls.get() + 1);
                let value = receive.await?;
                assert_eq!(value, 113);
                assert!(directory.path().exists());
                copy_resumed.set(true);
                Ok(value)
            }));
            assert_eq!(diagnostic.stage(), HermitCleanupStage::Startup);
            assert_eq!(diagnostic.primary_kind(), FailureKind::RunTimeout);
            assert_eq!(polls.get(), 1);
            assert!(!resumed.get());
            assert!(path.borrow().exists());
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            refusal(run(None, |_| async { Ok(()) }), |error| {
                matches!(error, RecoveryRefusal::AdmissionClosed)
            });
            send.send(113).unwrap();
            assert!(
                diagnostic
                    .resume::<u32>()
                    .unwrap_err()
                    .is::<GuestTimedOut>()
            );
            assert_eq!(polls.get(), 1);
            assert!(resumed.get());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!path.borrow().exists());
            assert_eq!(*UNRESOLVED.lock().unwrap(), 0);
            assert_eq!(run(None, |_| async { Ok(127u32) }).unwrap(), 127);
        },
    );
}

include!("ptrace_random_failure_tests.rs");

/// The output-capturing API must identify a child adopted by its tracer as
/// orphaned, just as the ordinary CLI timeout does. The isolated process acts
/// as a subreaper, giving adoption the same parent identity as namespace init.
#[test]
fn captured_output_timeout_names_the_orphan_after_its_root_exits() {
    isolated_budgeted(
        "captured_output_timeout_names_the_orphan_after_its_root_exits",
        // Worst run: loaded n=10 max 0.550 s, light max 0.400 s, plus a
        // censored calibration run killed at 0.710 s of the former 0.7 s
        // budget (1-core head calibration run 4) -> 1.1 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(1100),
        || {
            use std::os::unix::ffi::OsStrExt;

            assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
            let directory = tempfile::tempdir().unwrap();
            let fifo = directory.path().join("never-written");
            let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
            // The freestanding guest where the build supplies it, which runs
            // under the x86-64-v2 profile on any host (crate::unit_guest);
            // otherwise the host's shell and cat. The guest's orphan execs
            // itself through a link named "cat", which names the process
            // "cat" as `exec /bin/cat` does, and blocks in openat as cat does.
            let command = match crate::unit_guest() {
                Some(guest) => {
                    let cat = directory.path().join("cat");
                    std::os::unix::fs::symlink(&guest, &cat).unwrap();
                    let mut command = reverie::process::Command::new(&guest);
                    command.arg("orphan").arg(&fifo).arg(&cat);
                    command
                }
                None => {
                    let mut command = reverie::process::Command::new("/bin/sh");
                    command
                        .args([
                            "-c",
                            "printf 'before-orphan\\n'; exec /bin/cat \"$1\" & exit 0",
                            "orphan-capture",
                        ])
                        .arg(&fifo);
                    command
                }
            };
            let mut config = detcore::Config {
                sequentialize_threads: true,
                deterministic_io: true,
                has_uts_namespace: false,
                notice_host_network_abstract_binds: false,
                max_timeslice: None,
                ..Default::default()
            };
            config.validate();
            let error = crate::run_with_output_backend_timeout(
                command,
                config,
                false,
                &None,
                crate::Backend::Ptrace,
                Some(Duration::from_secs(1)),
            )
            .unwrap_err();
            let failure = error
                .downcast_ref::<HermitPtraceFailure>()
                .unwrap_or_else(|| panic!("expected completed timeout cleanup: {error:#}"));
            assert_eq!(failure.kind(), FailureKind::RunTimeout);
            let captured = failure.failure().captured_prefix().unwrap();
            assert_eq!(captured.stdout(), b"before-orphan\n");
            let primary = failure.failure().primary();
            let reverie::Error::Tool(primary) = primary else {
                panic!("expected timeout tool failure: {primary:?}");
            };
            let timeout = primary.downcast_ref::<GuestTimedOut>().unwrap();
            assert_eq!(timeout.limit, Duration::from_secs(1));
            let survivors = timeout.survivors.as_deref().unwrap();
            assert!(
                survivors.contains("1 guest process was still running: pid ")
                    && survivors.contains("\"cat\"")
                    && survivors.contains(&format!(
                        "parent {}, orphaned: its parent exited",
                        std::process::id()
                    ))
                    && survivors.contains(" in openat"),
                "captured-output timeout lost the adopted child's identity: {survivors}"
            );
        },
    );
}

#[test]
fn terminal_result_releases_owned_future_and_keeps_concurrent_caller_alive() {
    isolated_budgeted(
        "terminal_result_releases_owned_future_and_keeps_concurrent_caller_alive",
        // Worst run: loaded n=10 max 0.170 s, light max 0.160 s -> 0.3 s.
        // Sources: loaded dh-74hbms-ccdcd578607, light dh-74hbms-c16418f641d, combined dh-74hbms-c3f8ae4dea3.
        Duration::from_millis(300),
        || {
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let other = std::thread::spawn(move || {
                run_on_tracer_thread(None, move |_| async move {
                    entered_tx.send(()).unwrap();
                    release_rx.await?;
                    Ok(73u32)
                })
            });
            entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let copied_drops = drops.clone();
            let path = Arc::new(Mutex::new(PathBuf::new()));
            let copied_path = path.clone();
            let error = run_on_tracer_thread::<(), _, _>(None, move |_| async move {
                let directory = tempfile::tempdir()?;
                *copied_path.lock().unwrap() = directory.path().to_owned();
                let _guard = DropCount(copied_drops);
                let error = crate::error::terminal_replay_refusal_for_test().await;
                assert!(
                    directory.path().is_dir(),
                    "backing must outlive finalization"
                );
                Err(error)
            })
            .unwrap_err();
            assert!(error.is::<detcore::TerminalReplayRefusal>());
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!path.lock().unwrap().exists());
            assert!(
                crate::tracer_thread::execute(|| OWNERS.with(|owners| owners.borrow().is_empty()))
                    .unwrap()
            );
            assert_eq!(*UNRESOLVED.lock().unwrap(), 0);
            release_tx.send(()).unwrap();
            assert_eq!(other.join().unwrap().unwrap(), 73);
            assert_eq!(
                run_on_tracer_thread(None, |_| async { Ok(89u32) }).unwrap(),
                89
            );
        },
    );
}

#[tokio::test]
async fn finalization_secondary_preserves_nested_cleanup_kind_and_stage() {
    let pending = HermitCleanupUnconfirmed {
        key: 0,
        origin: OriginNumbers::current().unwrap().0,
        stage: HermitCleanupStage::GlobalStateCleanup,
        snapshot: Snapshot {
            kind: FailureKind::RunTimeout,
            primary: "original timeout during cleanup".into(),
            ..Snapshot::startup()
        },
    };
    let error = detcore::combine_finalization::<()>(
        Err(Error::new(pending)),
        Err(crate::error::terminal_replay_refusal_for_test().await),
    )
    .unwrap_err();
    let error =
        detcore::combine_finalization::<()>(Err(error), Err(Error::msg("another output failure")))
            .unwrap_err();
    let serialized = crate::SerializableError::from(Error::new(reverie::Error::Tool(error)));
    assert_eq!(serialized.kind(), FailureKind::RunTimeout);
    assert_eq!(
        serialized.cleanup_stage(),
        Some(HermitCleanupStage::GlobalStateCleanup)
    );
    let diagnostic = format!("{serialized:?}");
    assert!(diagnostic.contains("Incomplete"));
    assert!(diagnostic.contains("another output failure"));
}

/// A run torn down for a terminal outcome (a guest deadlock, a virtual
/// timeout) completes with the outcome's status in place of a guest status it
/// never had, keeping the output the guest wrote. It does only when the
/// outcome is the run's primary cause and the teardown recorded no failure of
/// its own; an output run with no captured bytes has nothing honest to report
/// and stays a failure too.
#[test]
fn a_terminal_outcome_completes_with_its_status_and_the_captured_output() {
    use detcore::TerminalOutcome;
    for (outcome, status) in [
        (
            TerminalOutcome::GuestDeadlock,
            detcore_model::HERMIT_GUEST_DEADLOCK_EXIT,
        ),
        (
            TerminalOutcome::VirtualTimeout { bound_ns: 7 },
            crate::HERMIT_DEADLINE_EXIT,
        ),
    ] {
        let ended = || reverie::Error::Tool(Error::new(SchedulerEndedRun::new(outcome, None)));
        assert_eq!(clean_terminal_outcome(&ended(), 0), Some(outcome));
        assert_eq!(
            clean_terminal_outcome(&ended(), 1),
            None,
            "a teardown failure"
        );

        let status = reverie::ExitStatus::Exited(status);
        assert_eq!(
            <reverie::ExitStatus as TerminalOutcomeResult>::terminal_outcome(outcome, None),
            Some(status)
        );
        let output = <reverie::process::Output as TerminalOutcomeResult>::terminal_outcome(
            outcome,
            Some(&CapturedPrefix::default()),
        )
        .expect("captured bytes make an output");
        assert_eq!(output.status, status);
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
        assert!(
            <reverie::process::Output as TerminalOutcomeResult>::terminal_outcome(outcome, None)
                .is_none()
        );
    }
    assert_eq!(
        clean_terminal_outcome(&reverie::Error::Tool(Error::msg("an earlier failure")), 0),
        None
    );
}

/// A terminal outcome whose teardown recorded a failure of its own (a
/// secondary: a capture drain or cleanup error) does not complete with the
/// outcome's status, with or without captured output: the run stays a failed
/// ptrace run, and its primary classifies as Hermit's failure
/// (`FailureKind::Error`, reported as `HERMIT_INTERNAL_FAILURE` and 125), not
/// as the deadlock or the virtual timeout. Only a clean teardown completes with
/// the outcome's status (121, 124).
#[test]
fn a_deadlock_whose_teardown_failed_stays_hermits_failure() {
    use detcore::TerminalOutcome;
    let captured = CapturedPrefix::default();
    for outcome in [
        TerminalOutcome::GuestDeadlock,
        TerminalOutcome::VirtualTimeout { bound_ns: 7 },
    ] {
        let primary = reverie::Error::Tool(Error::new(SchedulerEndedRun::new(outcome, None)));
        for secondaries in [1, 2] {
            assert!(
                completed_terminal_outcome::<reverie::ExitStatus>(&primary, secondaries, None)
                    .is_none(),
                "{outcome:?}: {secondaries} teardown failure(s)"
            );
            assert!(
                completed_terminal_outcome::<reverie::process::Output>(
                    &primary,
                    secondaries,
                    Some(&captured)
                )
                .is_none(),
                "{outcome:?}: {secondaries} teardown failure(s), with output"
            );
        }
        assert_eq!(
            crate::error::classify_ptrace_primary(&primary),
            FailureKind::Error
        );
        let status = reverie::ExitStatus::Exited(outcome.exit_status());
        assert_eq!(
            completed_terminal_outcome::<reverie::ExitStatus>(&primary, 0, None),
            Some(status)
        );
        assert_eq!(
            completed_terminal_outcome::<reverie::process::Output>(&primary, 0, Some(&captured))
                .map(|output| output.status),
            Some(status)
        );
    }
}

/// Child half of the LeakSanitizer canary below: with
/// `HERMIT_LSAN_CANARY_CHILD` set it leaks one distinctive allocation and
/// returns, so the sanitizer reports that leak at process exit. Without
/// the variable it does nothing, so ordinary runs of this binary do not
/// leak.
#[test]
fn lsan_canary_leak_child() {
    if std::env::var_os("HERMIT_LSAN_CANARY_CHILD").is_some() {
        let leaked = std::hint::black_box(vec![0u8; 424_242]);
        std::mem::forget(leaked);
    }
}

/// Whether this process links the AddressSanitizer runtime, asked of the
/// dynamic symbol table rather than by scanning this binary's bytes.
fn asan_runtime_present() -> bool {
    let mut symbol = b"__asan".to_vec();
    symbol.extend_from_slice(b"_init\0");
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr().cast::<libc::c_char>()) };
    !found.is_null()
}

/// `remove_owner` frees the OWNERS map only once it is empty, so a map
/// holding a live or quarantined entry is never taken: the entry stays
/// in the map and the map keeps its allocation.
#[test]
fn remove_owner_never_takes_a_non_empty_map() {
    let mut owners = std::mem::ManuallyDrop::new(std::collections::BTreeMap::new());
    owners.insert(1u64, 10u64);
    owners.insert(2u64, 20u64);

    assert!(remove_owner(&mut owners, &1));
    assert_eq!(owners.len(), 1);
    assert_eq!(owners.get(&2), Some(&20), "the live entry must stay put");

    assert!(
        !remove_owner(&mut owners, &99),
        "a missing key removes nothing"
    );
    assert_eq!(owners.len(), 1);
}

/// Once the last entry is removed, `remove_owner` leaves an empty map
/// that can still be used. Whether its allocation was freed is not
/// observable here; `remove_owner_frees_the_retained_map_node` observes
/// that at process exit.
#[test]
fn remove_owner_leaves_an_empty_reusable_map() {
    let mut owners = std::mem::ManuallyDrop::new(std::collections::BTreeMap::new());
    owners.insert(1u64, 10u64);

    assert!(remove_owner(&mut owners, &1));
    assert!(owners.is_empty());
    owners.insert(3u64, 30u64);
    assert_eq!(owners.get(&3), Some(&30));
}

/// Child half of the process-exit free check below: leave a map in a
/// `ManuallyDrop` after `remove_owner` empties it. If the take is ever
/// lost, the retained BTreeMap node is still allocated when this process
/// exits and LeakSanitizer reports it.
#[test]
fn remove_owner_map_child() {
    if std::env::var_os("HERMIT_REMOVE_OWNER_CHILD").is_none() {
        return;
    }
    let mut owners = std::mem::ManuallyDrop::new(std::collections::BTreeMap::new());
    owners.insert(1u64, 10u64);
    assert!(remove_owner(&mut owners, &1));
    std::hint::black_box(&owners);
}

/// Process-level counterpart to the replacement check above: in an
/// AddressSanitizer build, run the child with leak detection on and
/// require a clean exit. A build without the runtime has no free to
/// observe at exit, so it returns without asserting.
#[test]
fn remove_owner_frees_the_retained_map_node() {
    if !asan_runtime_present() {
        return;
    }
    let exe = std::env::current_exe().expect("test binary path");
    let output = Command::new(&exe)
        .args([
            "--exact",
            "ptrace_completion::tests::remove_owner_map_child",
            "--nocapture",
        ])
        .env("HERMIT_REMOVE_OWNER_CHILD", "1")
        // This crate's fbcode unit tests run with detect_leaks=0; this
        // child turns detection back on for itself.
        .env("LSAN_OPTIONS", "detect_leaks=1")
        .output()
        .expect("failed to run the remove_owner child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("1 passed"),
        "the remove-owner child must have run: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the emptied OWNERS map must be freed before exit: {stderr}"
    );
    assert!(
        !stderr.contains("detected memory leaks"),
        "LeakSanitizer must not report the emptied OWNERS map: {stderr}"
    );
}

/// The OWNERS fix frees its map node rather than suppressing the leak
/// report, so LeakSanitizer must keep reporting every other leak. This
/// canary proves leak detection itself is still active: a deliberate
/// leak anywhere else is still reported and still fails the process. In
/// a build without AddressSanitizer there is no leak detection to prove,
/// so it returns without asserting.
#[test]
fn lsan_still_reports_a_deliberate_leak_outside_owners() {
    let exe = std::env::current_exe().expect("test binary path");
    if !asan_runtime_present() {
        return;
    }
    let output = Command::new(&exe)
        .args([
            "--exact",
            "ptrace_completion::tests::lsan_canary_leak_child",
            "--nocapture",
        ])
        .env("HERMIT_LSAN_CANARY_CHILD", "1")
        // This crate's fbcode unit tests run with detect_leaks=0; the
        // canary child turns detection back on for itself.
        .env("LSAN_OPTIONS", "detect_leaks=1")
        .output()
        .expect("failed to run the LSAN canary child");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "the deliberate canary leak must fail the child: {stderr}"
    );
    assert!(
        stderr.contains("detected memory leaks"),
        "LeakSanitizer must report the canary leak: {stderr}"
    );
    assert!(
        stderr.contains("424242 byte(s) leaked"),
        "the report must name the canary allocation, not the OWNERS node: {stderr}"
    );
}
