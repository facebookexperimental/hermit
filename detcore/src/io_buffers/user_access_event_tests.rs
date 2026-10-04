/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod user_access_event_tests {
    use rand::RngExt;
    use rand::SeedableRng;
    use rand_pcg::Pcg64Mcg;

    use super::*;

    #[derive(Clone, Default)]
    struct AllInfo(Arc<Mutex<Vec<String>>>);
    impl Subscriber for AllInfo {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == Level::INFO
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn event(&self, event: &Event<'_>) {
            let mut visitor = MessageVisitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0 {
                self.0.lock().unwrap().push(message);
            }
        }
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
    }

    #[derive(Clone, Copy, Debug)]
    enum Call {
        Getrandom,
        Read,
        Readv,
        Preadv,
        Preadv2,
        Preadv2Current,
    }
    impl Call {
        fn is_vector(self) -> bool {
            matches!(
                self,
                Self::Readv | Self::Preadv | Self::Preadv2 | Self::Preadv2Current
            )
        }
        fn advances_cursor(self) -> bool {
            matches!(self, Self::Read | Self::Readv | Self::Preadv2Current)
        }
    }
    const VECTOR_CALLS: [Call; 4] = [
        Call::Readv,
        Call::Preadv,
        Call::Preadv2,
        Call::Preadv2Current,
    ];
    const ALL_CALLS: [Call; 6] = [
        Call::Getrandom,
        Call::Read,
        Call::Readv,
        Call::Preadv,
        Call::Preadv2,
        Call::Preadv2Current,
    ];
    fn configured(
        call: Call,
        actions: Vec<(usize, Result<usize, Errno>)>,
    ) -> (Detcore, EventGuest, Syscall, DetFd) {
        let (mut tool, mut guest) = event_guest(FdType::Rng, None);
        let config = &mut guest.config;
        config.detlog_regs = true;
        config.detlog_heap = true;
        config.detlog_stack = true;
        config.detlog_io_buffers = true;
        config.syscall_clobbers_virtualized_by_backend = false;
        config.max_timeslice = std::num::NonZeroU64::new(1_000_000_000);
        tool.cfg = config.clone();
        let end = guest.thread.thread_logical_time.as_nanos() + std::time::Duration::from_secs(1);
        guest.thread.end_of_timeslice = Some(end);
        guest.thread.max_timeslice_end = Some(end);
        let alias = guest
            .thread
            .with_detfd(FD, |fd| fd.clone().with_fd(4))
            .unwrap();
        alias.advance_random_device_offset(7);
        guest
            .thread
            .file_metadata
            .lock()
            .unwrap()
            .file_handles
            .insert(4, alias.clone());
        {
            let mut audit = guest.memory.1.lock().unwrap();
            audit.user_copy_audit = true;
            audit.copy_actions = actions.into();
        }
        guest.memory.put_iovec(0, FIRST_DEST, 3);
        guest.memory.put_iovec(1, RETRY_DEST, 5);
        let syscall = match call {
            Call::Getrandom => reverie::syscalls::Getrandom::new()
                .with_buf(AddrMut::from_raw(FIRST_DEST))
                .with_buflen(8)
                .into(),
            Call::Read => reverie::syscalls::Read::new()
                .with_fd(FD)
                .with_buf(AddrMut::from_raw(FIRST_DEST))
                .with_len(8)
                .into(),
            Call::Readv => readv(2),
            Call::Preadv => reverie::syscalls::Preadv::new()
                .with_fd(FD)
                .with_iov(Addr::from_raw(IOV))
                .with_iov_len(2)
                .with_pos_l(19)
                .with_pos_h(u64::MAX)
                .into(),
            Call::Preadv2 | Call::Preadv2Current => reverie::syscalls::Preadv2::new()
                .with_fd(FD)
                .with_iov(Addr::from_raw(IOV))
                .with_iov_len(2)
                .with_pos_l(if matches!(call, Call::Preadv2Current) {
                    u64::MAX
                } else {
                    19
                })
                .with_pos_h(u64::MAX)
                .with_flags(0)
                .into(),
        };
        (tool, guest, syscall, alias)
    }
    fn expected(call: Call) -> ([u8; 8], Pcg64Mcg) {
        let mut generator = Pcg64Mcg::seed_from_u64(0);
        let mut bytes = [0; 8];
        if matches!(call, Call::Getrandom) {
            generator.fill(&mut bytes);
        } else if matches!(call, Call::Preadv | Call::Preadv2) {
            // Explicit offset19 is distinct from the shared cursor7.
            bytes = [148, 221, 38, 111, 184, 1, 74, 147];
        } else {
            // Literal seed0 stream offsets7..15, not production byte helpers.
            bytes = [40, 113, 186, 3, 76, 149, 222, 39];
        }
        (bytes, generator)
    }
    fn bytes_and_state(guest: &EventGuest, call: Call, changed: usize) {
        let (expected, mut generator) = expected(call);
        let actual = if call.is_vector() {
            [
                guest.memory.bytes(FIRST_DEST, 3),
                guest.memory.bytes(RETRY_DEST, 5),
            ]
            .concat()
        } else {
            guest.memory.bytes(FIRST_DEST, 8)
        };
        assert_eq!(&actual[..changed], &expected[..changed]);
        assert!(actual[changed..].iter().all(|byte| *byte == CANARY));
        assert_eq!(guest.memory.bytes(FIRST_DEST + 8, 1), [CANARY]);
        assert_eq!(guest.memory.bytes(RETRY_DEST + 8, 1), [CANARY]);
        let mut actual = guest.thread.prng.clone();
        for _ in 0..8 {
            assert_eq!(actual.random::<u64>(), generator.random::<u64>());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn central_random_fatal_copy_stops_all_enabled_after_copy_observers() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for call in ALL_CALLS {
                for whole_second_copy in [false, true] {
                    let first = if call.is_vector() { 3 } else { 4 };
                    let second = if whole_second_copy { 8 - first } else { 2 };
                    let logs = AllInfo::default();
                    let _subscriber = tracing::subscriber::set_default(logs.clone());
                    assert!(tracing::enabled!(Level::INFO));
                    let (tool, mut guest, syscall, alias) =
                        configured(call, vec![(first, Ok(first)), (second, Err(Errno::EIO))]);
                    let result = tool.handle_syscall_event(&mut guest, syscall).await;
                    let Error::Tool(error) = result.unwrap_err() else {
                        panic!("copy failure became a guest errno");
                    };
                    assert_eq!(
                        error
                            .downcast_ref::<crate::random::RandomCopyFailure>()
                            .unwrap()
                            .errno(),
                        Errno::EIO
                    );
                    bytes_and_state(&guest, call, first + second);
                    assert_eq!(
                        alias.random_device_offset(),
                        7,
                        "fatal copy committed aliased cursor"
                    );
                    let audit = guest.memory.1.lock().unwrap();
                    assert_eq!(audit.copy_lengths, [first, 8 - first]);
                    let permitted = if matches!(call, Call::Getrandom) {
                        vec![]
                    } else {
                        vec!["release"]
                    };
                    assert_eq!(
                        audit.after_copy, permitted,
                        "after-copy operations for {call:?}"
                    );
                    assert_eq!(
                        *guest.releases.lock().unwrap(),
                        usize::from(!matches!(call, Call::Getrandom))
                    );
                    assert!(guest.injected_iovecs.is_empty());
                    assert!(logs.0.lock().unwrap().iter().all(|message| {
                        !message.contains("finish syscall")
                            && !message.contains("[registers]")
                            && !message.contains("[memory]")
                            && !message.contains("[iobuf]")
                    }));
                    assert_eq!(guest.thread.stats.regs_sample_index, 0);
                    assert_eq!(guest.thread.last_rcb_timer, None);
                }
            }
        })
        .await
        .expect("central fatal-copy cases exceeded original deadline");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn central_random_readv_import_failure_stops_continuation_and_observers() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for call in VECTOR_CALLS {
                for error in [Errno::EPERM, Errno::EIO, Errno::ENOMEM, Errno::ENOSYS] {
                    for attempt in [1, 2] {
                        let logs = AllInfo::default();
                        let _subscriber = tracing::subscriber::set_default(logs.clone());
                        assert!(tracing::enabled!(Level::INFO));
                        let (tool, mut guest, syscall, alias) = configured(call, vec![]);
                        guest.memory.1.lock().unwrap().import_error = Some((attempt, error));
                        let before = guest.memory.0.lock().unwrap().clone();
                        let result = tool.handle_syscall_event(&mut guest, syscall).await;
                        let Err(Error::Tool(failure)) = result else {
                            panic!("backend import failure became a guest result: {result:?}");
                        };
                        assert_eq!(
                            failure
                                .downcast_ref::<crate::random::RandomCopyFailure>()
                                .unwrap()
                                .errno(),
                            error
                        );
                        assert_eq!(*guest.memory.0.lock().unwrap(), before);
                        bytes_and_state(&guest, call, 0);
                        assert_eq!(alias.random_device_offset(), 7);
                        let audit = guest.memory.1.lock().unwrap();
                        assert_eq!(audit.imported_entries, attempt);
                        assert!(audit.import_failed);
                        assert!(audit.copy_lengths.is_empty());
                        assert!(audit.observer_reads.is_empty());
                        assert_eq!(audit.after_import_error, ["release"]);
                        assert_eq!(*guest.releases.lock().unwrap(), 1);
                        assert!(guest.polls.lock().unwrap().is_empty());
                        assert!(guest.injected_iovecs.is_empty());
                        assert_eq!(guest.injected_zero_reads, 0);
                        assert!(logs.0.lock().unwrap().iter().all(|message| {
                            !message.contains("finish syscall")
                                && !message.contains("[registers]")
                                && !message.contains("[memory]")
                                && !message.contains("[iobuf]")
                        }));
                        assert_eq!(guest.thread.stats.regs_sample_index, 0);
                        assert_eq!(guest.thread.last_rcb_timer, None);
                    }
                }
            }
        })
        .await
        .expect("central import-failure cases exceeded original deadline");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn central_random_readv_import_guest_fault_keeps_observers_and_timer() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for call in VECTOR_CALLS {
                for attempt in [1, 2] {
                    let logs = AllInfo::default();
                    let _subscriber = tracing::subscriber::set_default(logs.clone());
                    assert!(tracing::enabled!(Level::INFO));
                    let (tool, mut guest, syscall, alias) = configured(call, vec![]);
                    guest.memory.1.lock().unwrap().import_error = Some((attempt, Errno::EFAULT));
                    let before = guest.memory.0.lock().unwrap().clone();
                    let result = tool.handle_syscall_event(&mut guest, syscall).await;
                    assert!(matches!(result, Err(Error::Errno(Errno::EFAULT))));
                    assert_eq!(*guest.memory.0.lock().unwrap(), before);
                    bytes_and_state(&guest, call, 0);
                    assert_eq!(alias.random_device_offset(), 7);
                    let audit = guest.memory.1.lock().unwrap();
                    assert_eq!(audit.imported_entries, attempt);
                    assert!(audit.import_failed);
                    assert!(audit.copy_lengths.is_empty());
                    assert_eq!(audit.observer_reads, [(FIRST_DEST, 8), (RETRY_DEST, 8)]);
                    for operation in [
                        "release",
                        "regs",
                        "memory-regions",
                        "memory-read",
                        "timer",
                        "set-regs",
                    ] {
                        assert!(audit.after_import_error.contains(&operation));
                    }
                    assert!(!audit.after_import_error.contains(&"user-read"));
                    assert_eq!(*guest.releases.lock().unwrap(), 1);
                    assert!(guest.injected_iovecs.is_empty());
                    assert_eq!(guest.injected_zero_reads, 0);
                    assert_eq!(guest.thread.stats.regs_sample_index, 1);
                    assert!(guest.thread.last_rcb_timer.is_some());
                    let messages = logs.0.lock().unwrap();
                    assert_eq!(
                        messages
                            .iter()
                            .filter(|m| m.contains("finish syscall"))
                            .count(),
                        1
                    );
                    assert_eq!(
                        messages
                            .iter()
                            .filter(|m| m.contains("[registers]"))
                            .count(),
                        1
                    );
                    assert_eq!(
                        messages.iter().filter(|m| m.contains("[memory]")).count(),
                        2
                    );
                    assert!(!messages.iter().any(|m| m.contains("[iobuf]")));
                }
            }
        })
        .await
        .expect("central import guest faults exceeded original deadline");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn central_random_success_and_guest_fault_keep_observers_and_timer() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for call in ALL_CALLS {
                for mode in 0..3 {
                    let first = if call.is_vector() { 3 } else { 4 };
                    let (actions, changed) = match mode {
                        0 => (vec![], 8),
                        1 => (vec![(0, Err(Errno::EFAULT))], 0),
                        _ => (vec![(first, Ok(first)), (0, Err(Errno::EFAULT))], first),
                    };
                    let logs = AllInfo::default();
                    let _subscriber = tracing::subscriber::set_default(logs.clone());
                    assert!(tracing::enabled!(Level::INFO));
                    let (tool, mut guest, syscall, alias) = configured(call, actions);
                    let result = tool.handle_syscall_event(&mut guest, syscall).await;
                    if mode == 1 {
                        assert!(matches!(result, Err(Error::Errno(Errno::EFAULT))));
                    } else {
                        assert_eq!(result.unwrap(), changed as i64);
                    }
                    bytes_and_state(&guest, call, changed);
                    assert_eq!(
                        alias.random_device_offset(),
                        7 + if call.advances_cursor() {
                            changed as u64
                        } else {
                            0
                        }
                    );
                    let audit = guest.memory.1.lock().unwrap();
                    assert!(audit.after_copy.contains(&"regs"));
                    assert!(audit.after_copy.contains(&"memory-regions"));
                    assert!(audit.after_copy.contains(&"memory-read"));
                    assert!(audit.after_copy.contains(&"timer"));
                    assert!(audit.after_copy.contains(&"set-regs"));
                    assert_eq!(guest.thread.stats.regs_sample_index, 1);
                    assert!(guest.thread.last_rcb_timer.is_some());
                    let messages = logs.0.lock().unwrap();
                    assert_eq!(
                        messages
                            .iter()
                            .filter(|m| m.contains("finish syscall"))
                            .count(),
                        1
                    );
                    assert_eq!(
                        messages
                            .iter()
                            .filter(|m| m.contains("[registers]"))
                            .count(),
                        1
                    );
                    assert_eq!(
                        messages.iter().filter(|m| m.contains("[memory]")).count(),
                        2
                    );
                    assert_eq!(
                        messages.iter().filter(|m| m.contains("[iobuf]")).count(),
                        if mode == 1 {
                            0
                        } else if call.is_vector() && mode == 0 {
                            2
                        } else {
                            1
                        }
                    );
                }
            }
        })
        .await
        .expect("central guest companions exceeded original deadline");
    }
}
