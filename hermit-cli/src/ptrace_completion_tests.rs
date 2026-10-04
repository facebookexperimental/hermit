/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
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

// Irreversible thread-exit/panic retention must not poison the rest of libtest.
// Each outer case selects exactly one child test. Its three-second deadline
// starts BEFORE re-exec; the separate rescue cannot satisfy any assertion.
fn isolated(name: &str, body: impl FnOnce()) {
    isolated_with_env(name, &[], body)
}

fn isolated_with_env(name: &str, environment: &[(&str, &std::path::Path)], body: impl FnOnce()) {
    isolated_with_child_setup(name, environment, || Ok(()), body)
}

// `child_setup` runs after fork and before exec: it must use only async-signal-safe
// operations. Opt-in credential changes must never affect the outer libtest process.
fn isolated_with_child_setup(
    name: &str,
    environment: &[(&str, &std::path::Path)],
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
    let deadline = monotonic_ns() + 3_000_000_000;
    let test = format!("ptrace_completion::tests::{name}");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env("HERMIT_OWNER_TEST_ROLE", name)
        .env("HERMIT_OWNER_TEST_DEADLINE", deadline.to_string());
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
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(monotonic_ns() < deadline, "late child completion {status}");
            assert!(status.success(), "selected child test failed: {status}");
            return;
        }
        if monotonic_ns() >= deadline {
            eprintln!("OWNER_CASE {name} original deadline FAILED; separate rescue begins");
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let rescue = Instant::now() + Duration::from_secs(2);
            while child.try_wait().unwrap().is_none() && Instant::now() < rescue {
                std::thread::sleep(Duration::from_millis(2));
            }
            panic!("selected child test exceeded original three-second deadline");
        }
        std::thread::sleep(Duration::from_millis(2));
    }
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
    isolated(
        "original_runtime_guards_and_admission_survive_checkpoint",
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

#[test]
fn nested_runtime_refuses_before_factory() {
    isolated("nested_runtime_refuses_before_factory", || {
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
    });
}

#[test]
fn repeated_checkpoint_keeps_one_owner_and_reentry_refuses() {
    isolated(
        "repeated_checkpoint_keeps_one_owner_and_reentry_refuses",
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
    isolated(
        "initial_poll_panic_retains_future_and_poison_refuses_repoll",
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
    isolated(
        "origin_thread_exit_retains_resources_and_process_admission",
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
    isolated(
        "absolute_startup_deadline_and_failed_cleanup_checkpoint",
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
    isolated(
        "origin_pidfd_and_primary_serialization_refuse_false_proofs",
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
    isolated(
        "mid_startup_timeout_retains_original_waiter_and_resources",
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
