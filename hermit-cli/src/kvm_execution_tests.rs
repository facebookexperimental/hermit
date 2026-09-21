/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use reverie::BackendStatsRequest;
use reverie::BackendStatsSource;
use reverie::GlobalRPC;
use reverie::Guest;
use reverie::Pid;
use reverie::Tool;

use super::*;

static INITIALIZED: AtomicUsize = AtomicUsize::new(0);
static STARTED: AtomicUsize = AtomicUsize::new(0);
static THREAD_EXITED: AtomicUsize = AtomicUsize::new(0);
static PROCESS_EXITED: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct SetupFailureTool(Detcore);

#[reverie::tool]
impl Tool for SetupFailureTool {
    type GlobalState = detcore::GlobalState;
    type ThreadState = <Detcore as Tool>::ThreadState;

    fn new(pid: Pid, config: &DetConfig) -> Self {
        Self(Detcore::new(pid, config))
    }

    fn init_thread_state(
        &self,
        tid: Pid,
        parent: Option<(Pid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        INITIALIZED.fetch_add(1, Ordering::SeqCst);
        self.0.init_thread_state(tid, parent)
    }

    async fn handle_thread_start<G: Guest<Self>>(
        &self,
        _guest: &mut G,
    ) -> Result<(), reverie::Error> {
        STARTED.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::other("initialized VM setup failure").into())
    }

    async fn on_exit_thread<G: GlobalRPC<Self::GlobalState>>(
        &self,
        tid: Pid,
        global: &G,
        thread: Self::ThreadState,
        status: ExitStatus,
    ) -> Result<(), reverie::Error> {
        THREAD_EXITED.fetch_add(1, Ordering::SeqCst);
        self.0.on_exit_thread(tid, global, thread, status).await?;
        Err(std::io::Error::other("separate initialized VM cleanup failure").into())
    }

    async fn on_exit_process<G: GlobalRPC<Self::GlobalState>>(
        self,
        pid: Pid,
        global: &G,
        status: ExitStatus,
    ) -> Result<(), reverie::Error> {
        PROCESS_EXITED.fetch_add(1, Ordering::SeqCst);
        self.0.on_exit_process(pid, global, status).await
    }
}

fn setup_failure_elf() -> Vec<u8> {
    // Existing static_elf_at layout: one RX LOAD at file offset 0x1000,
    // virtual 0x400000. UD2 must not execute during Tool setup. The untracked
    // pre-run intentionally reaches the exception HLT before Tool state exists.
    let mut image = vec![0; 0x1002];
    image[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    for (offset, value) in [(16, 2u16), (18, 62), (52, 64), (54, 56), (56, 1)] {
        image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [(20, 1u32), (64, 1), (68, 5)] {
        image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    for (offset, value) in [
        (24, 0x400000u64),
        (32, 64),
        (72, 0x1000),
        (80, 0x400000),
        (88, 0x400000),
        (96, 2),
        (104, 0x2000),
        (112, 0x1000),
    ] {
        image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    image[0x1000..].copy_from_slice(&[0x0f, 0x0b]);
    image
}

async fn check_initialized_vm_setup_failure(preamble: bool) {
    for count in [&INITIALIZED, &STARTED, &THREAD_EXITED, &PROCESS_EXITED] {
        count.store(0, Ordering::SeqCst);
    }
    let config = prepare_backend_config(
        DetConfig {
            sequentialize_threads: true,
            max_timeslice: None,
            ..DetConfig::default()
        },
        Backend::Kvm,
    );
    // KVM/PMU refusal is a failure to measure, never an early success.
    let mut backend = reverie_kvm::KvmBackend::new(16 * 1024 * 1024).unwrap();
    backend.set_root_pid(detcore::ROOT_DETPID.as_raw()).unwrap();
    backend.set_backend_stats_request(BackendStatsRequest::new(true));
    backend
        .install_static_elf(&setup_failure_elf(), "/setup-failure")
        .unwrap();
    if preamble {
        // Exercise the existing public API's untracked-to-tracked refusal.
        // UD2 reaches the exception HLT; no Tool state exists during this run.
        backend
            .run(|_, _| panic!("UD2 fixture cannot issue a syscall"))
            .unwrap();
        assert_eq!(backend.backend_stats().total_exits(), 1);
    }
    let exits_before_setup = backend.backend_stats().total_exits();
    let completion = backend
        .run_static_elf_with_tool_completion::<SetupFailureTool>(config, true)
        .await
        .expect("constructed GlobalState must be recovered");
    assert_eq!(backend.backend_stats().total_exits(), exits_before_setup);
    assert_eq!(STARTED.load(Ordering::SeqCst), usize::from(!preamble));
    for count in [&INITIALIZED, &THREAD_EXITED, &PROCESS_EXITED] {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    let error = completion.result.as_ref().unwrap_err();
    assert_setup_primary(error, preamble);
    let reverie_kvm::Error::WithCleanup { cleanup, .. } = error else {
        panic!("separate cleanup error was not retained");
    };
    assert!(cleanup.iter().any(|error| matches!(error.primary(),
        reverie_kvm::Error::Reverie(reverie::Error::Io(error))
        if error.to_string() == "separate initialized VM cleanup failure")));
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        finish_kvm_tool_completion(completion, false, &None),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_setup_primary(
        error.downcast_ref::<reverie_kvm::Error>().unwrap(),
        preamble,
    );
}

fn assert_setup_primary(error: &reverie_kvm::Error, preamble: bool) {
    if preamble {
        assert!(
            matches!(error.primary(), reverie_kvm::Error::GuestClock(message)
            if message.contains("cannot start a clock after untracked guest execution"))
        );
    } else {
        assert!(
            matches!(error.primary(), reverie_kvm::Error::Reverie(reverie::Error::Io(error))
            if error.to_string() == "initialized VM setup failure")
        );
    }
}

#[tokio::test]
async fn initialized_vm_setup_failures_consume_detcore_state_without_further_guest_execution() {
    for preamble in [false, true] {
        check_initialized_vm_setup_failure(preamble).await;
    }
}
