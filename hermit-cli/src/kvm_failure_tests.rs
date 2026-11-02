/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::GlobalTool;

use super::*;

#[tokio::test]
async fn kvm_failed_completion_cleans_unstarted_scheduler_and_retains_typed_cause() {
    let config = DetConfig {
        sequentialize_threads: true,
        ..DetConfig::default()
    };
    let global_state = detcore::GlobalState::init_global_state(&config).await;
    global_state.report_backend_failure(reverie::BackendFailure {
        pid: reverie::Pid::from_raw(17),
        tid: reverie::Pid::from_raw(18),
        phase: "native setup failure control",
    });
    let completion = reverie_kvm::ToolRunCompletion {
        global_state,
        result: Err(reverie_kvm::Error::WithCleanup {
            primary: std::sync::Arc::new(reverie_kvm::Error::InvalidGuestPid(-17)),
            cleanup: vec![std::sync::Arc::new(reverie_kvm::Error::HostIo(
                std::io::Error::from_raw_os_error(libc::EIO),
            ))],
        }),
    };
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        finish_kvm_tool_completion(completion, false, &None),
    )
    .await
    .expect("failure completion must naturally finish the daemon");
    let error = result.expect_err("failure is not a guest status");
    assert!(
        format!("Error: {error}")
            .lines()
            .next()
            .unwrap()
            .contains("KVM guest execution failed: invalid KVM root guest PID -17"),
        "the manifest runner retains only the first Error line"
    );
    let backend = error
        .downcast_ref::<reverie_kvm::Error>()
        .expect("typed backend cause");
    assert!(matches!(
        backend.primary(),
        reverie_kvm::Error::InvalidGuestPid(-17)
    ));
    let reverie_kvm::Error::WithCleanup { cleanup, .. } = backend else {
        panic!("secondary cleanup cause was lost");
    };
    assert_eq!(cleanup.len(), 1);
    assert!(
        matches!(cleanup[0].as_ref(), reverie_kvm::Error::HostIo(error) if error.raw_os_error() == Some(libc::EIO))
    );
}

#[tokio::test]
async fn kvm_normal_completion_preserves_status_and_output() {
    let config = DetConfig {
        sequentialize_threads: false,
        ..DetConfig::default()
    };
    let global_state = detcore::GlobalState::init_global_state(&config).await;
    let completion = reverie_kvm::ToolRunCompletion {
        global_state,
        result: Ok((
            ExitStatus::Exited(37),
            b"stdout\n".to_vec(),
            b"stderr\n".to_vec(),
        )),
    };
    assert_eq!(
        finish_kvm_tool_completion(completion, false, &None)
            .await
            .unwrap(),
        (
            ExitStatus::Exited(37),
            b"stdout\n".to_vec(),
            b"stderr\n".to_vec()
        ),
    );

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("summary.json");
    let completion = reverie_kvm::ToolRunCompletion {
        global_state: detcore::GlobalState::init_global_state(&config).await,
        result: Ok((
            ExitStatus::Exited(37),
            b"stdout\n".to_vec(),
            b"stderr\n".to_vec(),
        )),
    };
    assert_eq!(
        finish_kvm_tool_completion(completion, false, &Some(path.clone()))
            .await
            .unwrap(),
        (
            ExitStatus::Exited(37),
            b"stdout\n".to_vec(),
            b"stderr\n".to_vec()
        ),
    );
    let bytes = std::fs::read(path).unwrap();
    let summary: detcore_model::summary::RunSummary = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        bytes,
        (serde_json::to_string_pretty(&summary).unwrap() + "\n").as_bytes()
    );

    // Death by a signal stays distinct from an ordinary exit with the same
    // shell code (128 + signal).
    for status in [
        ExitStatus::Signaled(reverie::process::Signal::SIGSEGV, false),
        ExitStatus::Signaled(reverie::process::Signal::SIGABRT, true),
        ExitStatus::Exited(139),
    ] {
        let completion = reverie_kvm::ToolRunCompletion {
            global_state: detcore::GlobalState::init_global_state(&config).await,
            result: Ok((status, Vec::new(), Vec::new())),
        };
        assert_eq!(
            finish_kvm_tool_completion(completion, false, &None)
                .await
                .unwrap()
                .0,
            status
        );
    }
}

#[tokio::test]
async fn kvm_failed_completion_never_publishes_a_success_summary() {
    let directory = tempfile::tempdir().unwrap();
    let absent = directory.path().join("absent.json");
    let existing = directory.path().join("existing.json");
    std::fs::write(&existing, b"existing summary\n").unwrap();
    let unwritable = directory.path().join("directory");
    std::fs::create_dir(&unwritable).unwrap();
    for path in [&absent, &existing, &unwritable] {
        let config = DetConfig {
            sequentialize_threads: true,
            ..DetConfig::default()
        };
        let global_state = detcore::GlobalState::init_global_state(&config).await;
        global_state.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(17),
            tid: reverie::Pid::from_raw(18),
            phase: "failed summary publication control",
        });
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Err(reverie_kvm::Error::InvalidGuestPid(-17)),
        };
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            finish_kvm_tool_completion(completion, true, &Some(path.clone())),
        )
        .await
        .expect("failure cleanup must finish naturally")
        .expect_err("an unwritable summary must not replace the backend failure");
        assert!(matches!(
            error.downcast_ref::<reverie_kvm::Error>(),
            Some(reverie_kvm::Error::InvalidGuestPid(-17))
        ));
        assert_eq!(
            error.to_string(),
            "KVM guest execution failed: invalid KVM root guest PID -17"
        );
    }
    assert!(!absent.exists());
    assert_eq!(std::fs::read(existing).unwrap(), b"existing summary\n");
    assert!(unwritable.is_dir());
    assert_eq!(std::fs::read_dir(unwritable).unwrap().count(), 0);
}

/// An exec the KVM backend could not finish past its point of no return,
/// because a host resource of Hermit's own process ran out, ends the run as a
/// policy refusal (exit 122) that names the resource. reverie-kvm's
/// `Error::HostResources`, alone or beneath the wrappers a run's failure is
/// aggregated in, keeps the policy class across the container boundary, and
/// its diagnostic leads the run's first Error line. Any other backend failure
/// keeps the backend-failure class.
#[tokio::test]
async fn kvm_exec_out_of_host_resources_past_its_point_of_no_return_is_a_policy_refusal() {
    use crate::error::FailureKind;
    const DIAGNOSTIC: &str = "KVM exec: host memory ran out after the old image was discarded: \
        replacing the image failed (allocating 67108864 bytes of host memory for a copy of the \
        new image failed) under this process's RLIMIT_AS (soft 4831838208 bytes, hard \
        unlimited); the old image cannot be restored, so the run ends here";
    let refusal = || reverie_kvm::Error::HostResources {
        resource: reverie_kvm::HostResource::Memory,
        diagnostic: DIAGNOSTIC.to_owned(),
    };
    for (backend, kind) in [
        (refusal(), FailureKind::PolicyRefusal),
        (
            reverie_kvm::Error::SharedFailure(std::sync::Arc::new(refusal())),
            FailureKind::PolicyRefusal,
        ),
        (
            reverie_kvm::Error::WithCleanup {
                primary: std::sync::Arc::new(refusal()),
                cleanup: vec![std::sync::Arc::new(reverie_kvm::Error::RunAborted)],
            },
            FailureKind::PolicyRefusal,
        ),
        (
            reverie_kvm::Error::InvalidGuestPid(-17),
            FailureKind::BackendRunFailure,
        ),
    ] {
        let config = DetConfig {
            sequentialize_threads: true,
            ..DetConfig::default()
        };
        let global_state = detcore::GlobalState::init_global_state(&config).await;
        global_state.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(17),
            tid: reverie::Pid::from_raw(18),
            phase: "exec host-resources refusal control",
        });
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Err(backend),
        };
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            finish_kvm_tool_completion(completion, false, &None),
        )
        .await
        .expect("failure cleanup must finish naturally")
        .expect_err("a backend failure is not a guest status");
        if kind == FailureKind::PolicyRefusal {
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("KVM guest execution failed: {DIAGNOSTIC}")),
                "{error}"
            );
        }
        assert_eq!(crate::error::SerializableError::from(error).kind(), kind);
    }
}

/// Each refusal by name of a KVM vfork child's operation that the backend's
/// copy of its parent's memory cannot carry back (reverie-kvm's
/// `Error::VforkChildCapability`), alone or beneath the wrappers a run's
/// failure is aggregated in, ends the run as a policy refusal (exit 122), not
/// as Hermit's internal failure (exit 125)
/// (https://github.com/rrnewton/hermit/issues/5025). A genuine backend failure
/// keeps the backend-failure class, and so does a run whose refusal is only a
/// cleanup error beside such a failure.
#[tokio::test]
async fn kvm_vfork_child_refusals_are_policy_refusals() {
    use crate::error::FailureKind;
    // The refusals of reverie-kvm-core's vfork write-back and of
    // reverie-linuxemu's dispatch, by operation and reason.
    const REFUSALS: [(&str, &str); 5] = [
        (
            "vfork write-back",
            "the vfork child left a mapping changed (by mmap, munmap, mprotect or mremap) when \
             its window ended, which on Linux would be its parent's too, and a copy carries \
             back contents, not mappings",
        ),
        (
            "vfork write-back",
            "the vfork child and another thread of its parent changed the same bytes during \
             the window, which a copy cannot order",
        ),
        (
            "vfork write-back",
            "another thread of the parent removed or replaced the mapping of a page the vfork \
             child changed during the window",
        ),
        (
            "brk in a vfork child",
            "a vfork child shares its parent's address space until it execs or exits, and runs \
             on a copy here whose changes to private memory go back to the parent, but whose \
             program break cannot: on Linux this call would move the parent's break too",
        ),
        (
            "thread creation in a vfork child",
            "a vfork child shares its parent's address space until it execs or exits, and runs \
             on a copy here whose changes go back to the parent when its window ends; Linux \
             ends that window when the creating task execs or exits, whatever threads of the \
             child still run, and a copy cannot go back while another thread still changes it",
        ),
    ];
    let failure = || reverie_kvm::Error::InvalidGuestPid(-17);
    let mut cases = Vec::new();
    for (operation, reason) in REFUSALS {
        let refusal = || reverie_kvm::Error::VforkChildCapability { operation, reason };
        cases.extend([
            (refusal(), FailureKind::PolicyRefusal),
            (
                reverie_kvm::Error::SharedFailure(std::sync::Arc::new(refusal())),
                FailureKind::PolicyRefusal,
            ),
            (
                reverie_kvm::Error::WithCleanup {
                    primary: std::sync::Arc::new(refusal()),
                    cleanup: vec![std::sync::Arc::new(reverie_kvm::Error::RunAborted)],
                },
                FailureKind::PolicyRefusal,
            ),
            (
                reverie_kvm::Error::WithCleanup {
                    primary: std::sync::Arc::new(failure()),
                    cleanup: vec![std::sync::Arc::new(refusal())],
                },
                FailureKind::BackendRunFailure,
            ),
        ]);
    }
    cases.push((failure(), FailureKind::BackendRunFailure));
    for (backend, kind) in cases {
        let description = backend.to_string();
        let config = DetConfig {
            sequentialize_threads: true,
            ..DetConfig::default()
        };
        let global_state = detcore::GlobalState::init_global_state(&config).await;
        global_state.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(17),
            tid: reverie::Pid::from_raw(18),
            phase: "vfork child refusal control",
        });
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Err(backend),
        };
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            finish_kvm_tool_completion(completion, false, &None),
        )
        .await
        .expect("failure cleanup must finish naturally")
        .expect_err("a backend failure is not a guest status");
        assert_eq!(
            crate::error::SerializableError::from(error).kind(),
            kind,
            "{description}"
        );
    }
}

#[tokio::test]
async fn kvm_failed_completion_preserves_requested_partial_preemption_recording() {
    let directory = tempfile::tempdir().unwrap();
    let writable = directory.path().join("preemptions.json");
    let unwritable = directory.path().join("directory");
    std::fs::create_dir(&unwritable).unwrap();
    for path in [None, Some(writable.clone()), Some(unwritable.clone())] {
        let config = DetConfig {
            sequentialize_threads: true,
            record_preemptions: true,
            record_preemptions_to: path.clone(),
            ..DetConfig::default()
        };
        let global_state = detcore::GlobalState::init_global_state(&config).await;
        global_state.report_backend_failure(reverie::BackendFailure {
            pid: reverie::Pid::from_raw(17),
            tid: reverie::Pid::from_raw(18),
            phase: "partial preemption recording control",
        });
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Err(reverie_kvm::Error::InvalidGuestPid(-17)),
        };
        let error = tokio::time::timeout(
            Duration::from_millis(100),
            finish_kvm_tool_completion(completion, false, &None),
        )
        .await
        .expect("failure cleanup must finish naturally")
        .expect_err("recording cleanup must retain the backend failure");
        assert!(matches!(
            error.downcast_ref::<reverie_kvm::Error>(),
            Some(reverie_kvm::Error::InvalidGuestPid(-17))
        ));
        assert_eq!(
            error.to_string(),
            "KVM guest execution failed: invalid KVM root guest PID -17"
        );
        assert_eq!(
            format!("{error:#}").contains("partial preemption recording failed:"),
            path.as_ref() == Some(&unwritable)
        );
    }
    let bytes = std::fs::read(writable).unwrap();
    let _: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(unwritable.is_dir());
}

#[tokio::test]
async fn kvm_failure_error_retains_scheduler_recording_and_primary_causes() {
    let scheduler = tokio::spawn(async { panic!("scheduler cleanup control") })
        .await
        .expect_err("the control must supply a real JoinError");
    let error = kvm_execution_error(
        reverie_kvm::Error::InvalidGuestPid(-17),
        Some(detcore::BackendFailureCleanup {
            scheduler: Err(scheduler),
            preemption_recording: Err("recording control".to_owned()),
        }),
    );
    assert_eq!(
        error.to_string(),
        "KVM guest execution failed: invalid KVM root guest PID -17"
    );
    assert!(matches!(
        error.downcast_ref::<reverie_kvm::Error>(),
        Some(reverie_kvm::Error::InvalidGuestPid(-17))
    ));
    assert!(
        error
            .downcast_ref::<tokio::task::JoinError>()
            .unwrap()
            .is_panic()
    );
    assert!(
        format!("{error:#}").contains("partial preemption recording failed: recording control")
    );
    let unrecovered = kvm_execution_error(reverie_kvm::Error::InvalidGuestPid(-19), None);
    assert_eq!(
        unrecovered.to_string(),
        "KVM guest execution failed: invalid KVM root guest PID -19"
    );
    assert!(matches!(
        unrecovered.downcast_ref::<reverie_kvm::Error>(),
        Some(reverie_kvm::Error::InvalidGuestPid(-19))
    ));
}

// The KVM executor's refusals by name leave the run's error as a policy
// refusal (exit 122 with its marker, once the container restores the class)
// through every wrapping a run's failure takes on its way to the serialization
// boundary: bare, as a guest thread's worker failure, and with a cleanup error
// and a failed recording in addition. Every other KVM error stays a backend
// failure in the same wrappings, and a refusal that is only a cleanup error
// does not make the run a refusal.
#[test]
fn kvm_named_refusals_are_policy_refusals_and_other_errors_stay_failures() {
    use std::sync::Arc;

    use crate::error::FailureKind;
    use crate::error::SerializableError;

    let in_worker = |cause| reverie_kvm::Error::WorkerFailure {
        tid: 5,
        error: Arc::new(cause),
    };
    let with_cleanup = |cause| reverie_kvm::Error::WithCleanup {
        primary: Arc::new(cause),
        cleanup: vec![Arc::new(reverie_kvm::Error::HostIo(
            std::io::Error::from_raw_os_error(libc::EIO),
        ))],
    };
    let recording_failed = || detcore::BackendFailureCleanup {
        scheduler: Ok(()),
        preemption_recording: Err("recording control".to_owned()),
    };
    let refusals: [fn() -> reverie_kvm::Error; 3] = [
        || reverie_kvm::Error::PotentiallyBlockingZeroRead { fd: 3 },
        || reverie_kvm::Error::BlockingInotifyRead { fd: 4 },
        // A shared-file operation the program asked for and the backend
        // does not implement.
        || reverie_kvm::Error::UnsupportedSharedFile {
            operation: "shared-file fork snapshot",
            reason: "another thread of the process is alive",
        },
    ];
    let failures: [fn() -> reverie_kvm::Error; 4] = [
        || reverie_kvm::Error::UnexpectedVcpuExit("hlt".to_owned()),
        || reverie_kvm::Error::InvalidGuestPid(-17),
        || reverie_kvm::Error::HostIo(std::io::Error::from_raw_os_error(libc::EIO)),
        // The shared-file path's own guards and its stop bound: the
        // backend's failure, never a refusal by name.
        || reverie_kvm::Error::SharedFileCapability {
            operation: "mapping publication",
            reason: "the address space's other vCPUs or host copies did not stop",
        },
    ];
    let cases = refusals
        .iter()
        .map(|make| (make, FailureKind::PolicyRefusal))
        .chain(
            failures
                .iter()
                .map(|make| (make, FailureKind::BackendRunFailure)),
        );
    for (make, kind) in cases {
        for error in [
            kvm_execution_error(make(), None),
            kvm_execution_error(in_worker(make()), None),
            kvm_execution_error(with_cleanup(in_worker(make())), Some(recording_failed())),
        ] {
            let message = format!("{error:#}");
            assert_eq!(SerializableError::from(error).kind(), kind, "{message}");
        }
    }
    let secondary = reverie_kvm::Error::WithCleanup {
        primary: Arc::new(reverie_kvm::Error::RunAborted),
        cleanup: vec![Arc::new(reverie_kvm::Error::PotentiallyBlockingZeroRead {
            fd: 3,
        })],
    };
    assert_eq!(
        SerializableError::from(kvm_execution_error(secondary, None)).kind(),
        FailureKind::BackendRunFailure
    );
}

#[cfg(feature = "kvm-native-test-support")]
mod combined {
    use std::future::Future;
    use std::future::poll_fn;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::task::Context;
    use std::task::Poll;
    use std::task::Wake;
    use std::task::Waker;
    use std::time::Instant;

    use reverie::Guest;
    use reverie::Pid;
    use reverie::Tool;
    use reverie::syscalls::CloneFlags;
    use reverie_kvm::native_test_support::NativeCallbackOutcome;
    use reverie_kvm::native_test_support::NativeChildCommand;
    use reverie_kvm::native_test_support::NativeToolCallback;
    use reverie_kvm::native_test_support::NativeToolOwner;

    use super::*;

    type CallbackFuture<'a> =
        Pin<Box<dyn Future<Output = Result<i64, reverie::Error>> + Send + 'a>>;

    struct StartThread(Arc<AtomicUsize>);

    impl NativeToolCallback<Detcore> for StartThread {
        fn run<'a, G: Guest<Detcore>>(
            &'a self,
            tool: &'a Detcore,
            guest: &'a mut G,
        ) -> CallbackFuture<'a> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                tool.handle_thread_start(guest).await?;
                Ok(37)
            })
        }
    }

    struct PublicationWake(Mutex<bool>, Condvar);

    impl Wake for PublicationWake {
        fn wake(self: Arc<Self>) {
            *self.0.lock().unwrap() = true;
            self.1.notify_all();
        }
    }

    // The callback pauses after the real registration RPC's first Pending.
    // Publication must precede the driver's second failure check, before it
    // can resolve any child Start gate. This observes the GlobalState hook,
    // rather than polling a worker cancellation flag.
    fn wait_for_publication(global: &detcore::GlobalState) {
        let wake = Arc::new(PublicationWake(Mutex::new(false), Condvar::new()));
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        let mut published = std::pin::pin!(global.wait_for_backend_failure());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if published.as_mut().poll(&mut context).is_ready() {
                return;
            }
            let mut ready = wake.0.lock().unwrap();
            while !*ready {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "terminal hook did not wake its subscriber"
                );
                let (next, _) = wake.1.wait_timeout(ready, remaining).unwrap();
                ready = next;
            }
            *ready = false;
        }
    }

    // Installed before any owner construction or startup precondition. A
    // rescue can release real scheduler waits but cannot satisfy a test.
    struct FailureRescue {
        stop: Option<mpsc::Sender<()>>,
        handle: Option<std::thread::JoinHandle<()>>,
        fired: Arc<AtomicBool>,
    }

    impl FailureRescue {
        fn new(global: Arc<detcore::GlobalState>) -> Self {
            let (stop, stopped) = mpsc::channel();
            let fired = Arc::new(AtomicBool::new(false));
            let worker_fired = fired.clone();
            let handle = std::thread::spawn(move || {
                if matches!(
                    stopped.recv_timeout(Duration::from_secs(2)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    worker_fired.store(true, Ordering::SeqCst);
                    global.report_backend_failure(reverie::BackendFailure {
                        pid: Pid::from_raw(17),
                        tid: Pid::from_raw(17),
                        phase: "native control watchdog rescue",
                    });
                }
            });
            Self {
                stop: Some(stop),
                handle: Some(handle),
                fired,
            }
        }

        fn finish(mut self) {
            self.reap();
            assert!(
                !self.fired.load(Ordering::SeqCst),
                "control needed watchdog rescue"
            );
        }

        fn reap(&mut self) {
            if let Some(stop) = self.stop.take() {
                let _ = stop.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.join().unwrap();
            }
        }
    }

    impl Drop for FailureRescue {
        fn drop(&mut self) {
            self.reap();
        }
    }

    struct RegisterChild {
        global: Arc<detcore::GlobalState>,
        release_failure: Option<mpsc::Sender<()>>,
        entered: Arc<AtomicUsize>,
    }

    impl NativeToolCallback<Detcore> for RegisterChild {
        fn run<'a, G: Guest<Detcore>>(
            &'a self,
            tool: &'a Detcore,
            guest: &'a mut G,
        ) -> CallbackFuture<'a> {
            Box::pin(async move {
                self.entered.fetch_add(1, Ordering::SeqCst);
                let mut registration = std::pin::pin!(tool.register_external_child(
                    guest,
                    Pid::from_raw(18),
                    0,
                    CloneFlags::empty(),
                    libc::SIGCHLD,
                    None,
                ));
                let mut waiting = false;
                poll_fn(|context| {
                    let result = registration.as_mut().poll(context);
                    if result.is_pending() && !waiting {
                        waiting = true;
                        if let Some(release) = &self.release_failure {
                            release
                                .send(())
                                .expect("owned worker exited before registration");
                            wait_for_publication(&self.global);
                        }
                    }
                    result
                })
                .await;
                Ok(37)
            })
        }
    }

    async fn prepare_parent(
        config: &DetConfig,
        global: Arc<detcore::GlobalState>,
    ) -> NativeToolOwner<Detcore> {
        let pid = Pid::from_raw(17);
        let tool = Arc::new(Detcore::new(pid, config));
        let thread = tool.init_thread_state(pid, None);
        let mut owner = NativeToolOwner::new(pid, tool, thread, global, config.clone()).unwrap();
        let start = StartThread(Arc::new(AtomicUsize::new(0)));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), owner.run_callback(&start))
                .await
                .unwrap()
                .unwrap(),
            NativeCallbackOutcome::Returned(Ok(37)),
        ));
        assert_eq!(start.0.load(Ordering::SeqCst), 1);
        owner
    }

    fn prepare_child(
        owner: &mut NativeToolOwner<Detcore>,
        config: &DetConfig,
    ) -> NativeToolOwner<Detcore> {
        let child_pid = Pid::from_raw(18);
        let child_tool = Arc::new(Detcore::new(child_pid, config));
        owner.thread_state_mut().clone_flags = Some(CloneFlags::empty());
        let child_thread = child_tool
            .init_thread_state(child_pid, Some((Pid::from_raw(17), owner.thread_state())));
        owner.thread_state_mut().clone_flags = None;
        owner
            .fork_child(child_pid, child_tool, child_thread)
            .unwrap()
    }

    fn config() -> DetConfig {
        prepare_backend_config(
            DetConfig {
                sequentialize_threads: true,
                max_timeslice: None,
                runs_post_fork: detcore::RunsPostFork::Child,
                ..DetConfig::default()
            },
            Backend::Kvm,
        )
    }

    async fn failed_child_control(register: bool) {
        let config = config();
        let global = Arc::new(detcore::GlobalState::init_global_state(&config).await);
        let rescue = FailureRescue::new(global.clone());
        let mut owner = prepare_parent(&config, global.clone()).await;
        let child = prepare_child(&mut owner, &config);
        let starts = Arc::new(AtomicUsize::new(0));
        let (gate, commands) = owner
            .spawn_child(child, StartThread(starts.clone()))
            .unwrap();
        assert!(gate.is_pending());
        let (release, released) = mpsc::channel();
        owner.spawn_host_worker(Pid::from_raw(19), move || {
            released
                .recv_timeout(Duration::from_secs(2))
                .expect("controller did not release owned worker");
            Err(reverie_kvm::Error::InvalidGuestPid(-19))
        });
        let entered = Arc::new(AtomicUsize::new(0));
        let callback = RegisterChild {
            global: global.clone(),
            release_failure: register.then(|| release.clone()),
            entered: entered.clone(),
        };
        if !register {
            release.send(()).unwrap();
            wait_for_publication(&global);
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), owner.run_callback(&callback))
                .await
                .unwrap()
                .unwrap(),
            NativeCallbackOutcome::RunFailed,
        ));
        assert_eq!(entered.load(Ordering::SeqCst), usize::from(register));
        assert!(
            gate.is_pending(),
            "failed callback must not start its constructed child"
        );
        drop(callback);
        let error = owner
            .finish(Err(reverie_kvm::Error::RunAborted))
            .await
            .unwrap_err();
        assert!(matches!(
            error.primary(),
            reverie_kvm::Error::InvalidGuestPid(-19)
        ));
        assert_eq!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            NativeChildCommand::Cancel
        );
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        rescue.finish();
        let global_state = Arc::try_unwrap(global)
            .unwrap_or_else(|_| panic!("finished owners retained GlobalState"));
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Err(error),
        };
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            finish_kvm_tool_completion(completion, false, &None),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(
            error
                .downcast_ref::<reverie_kvm::Error>()
                .unwrap()
                .primary(),
            reverie_kvm::Error::InvalidGuestPid(-19)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kvm_worker_failure_consumes_actual_registered_detcore_child_gate_before_join() {
        failed_child_control(true).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kvm_worker_failure_consumes_actual_unregistered_detcore_child_gate_before_join() {
        failed_child_control(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kvm_normal_detcore_child_registration_preserves_start_and_status_37() {
        let config = config();
        let global = Arc::new(detcore::GlobalState::init_global_state(&config).await);
        let rescue = FailureRescue::new(global.clone());
        let mut owner = prepare_parent(&config, global.clone()).await;
        let child = prepare_child(&mut owner, &config);
        let starts = Arc::new(AtomicUsize::new(0));
        let (_, commands) = owner
            .spawn_child(child, StartThread(starts.clone()))
            .unwrap();
        let callback = RegisterChild {
            global: global.clone(),
            release_failure: None,
            entered: Arc::new(AtomicUsize::new(0)),
        };
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), owner.run_callback(&callback))
                .await
                .unwrap()
                .unwrap(),
            NativeCallbackOutcome::Returned(Ok(37)),
        ));
        drop(callback);
        let (status, stdout, stderr) = owner.finish(Ok(ExitStatus::Exited(37))).await.unwrap();
        assert_eq!(status, ExitStatus::Exited(37));
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
        assert_eq!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            NativeChildCommand::Start
        );
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        {
            let mut failure = std::pin::pin!(global.wait_for_backend_failure());
            assert!(
                poll_fn(|context| Poll::Ready(failure.as_mut().poll(context).is_pending())).await
            );
        }
        rescue.finish();
        let global_state = Arc::try_unwrap(global)
            .unwrap_or_else(|_| panic!("finished owners retained GlobalState"));
        let completion = reverie_kvm::ToolRunCompletion {
            global_state,
            result: Ok((ExitStatus::Exited(37), stdout, stderr)),
        };
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(2),
                finish_kvm_tool_completion(completion, false, &None)
            )
            .await
            .unwrap()
            .unwrap()
            .0,
            ExitStatus::Exited(37)
        );
    }
}
