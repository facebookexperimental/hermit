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
        result: Ok((37, b"stdout\n".to_vec(), b"stderr\n".to_vec())),
    };
    assert_eq!(
        finish_kvm_tool_completion(completion, false, &None)
            .await
            .unwrap(),
        (37, b"stdout\n".to_vec(), b"stderr\n".to_vec()),
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
            result: Ok((37, stdout, stderr)),
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
            37
        );
    }
}
