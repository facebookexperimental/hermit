/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! How long the SIGALRM ledger keeps a pipe description's published status
//! (dev-hermit dh-ru056 slice 2, finding 3 of the review of 29b3e6a8),
//! through the coordinator's real RPC handlers (`GlobalState::receive_rpc`,
//! `GlobalState::recv_create_child_thread`) and its thread retirement
//! (`Scheduler::logically_kill_thread`). A status lasts while some process
//! may hold the description: its creator from the `pipe2`, every process a
//! holder forks, and any later publisher. Each holder leaves at the release
//! Detcore sends for its last descriptor of the description, or at its
//! retirement. Each test names the mutation it catches.

use reverie::Tid;
use reverie::syscalls::CloneFlags;

use super::*;
use crate::tool_global::ChildRegistration;
use crate::tool_global::SigalrmControl;
use crate::types::OpenFileId;

const PARENT: DetTid = DetTid::from_raw(3);
const CHILD: DetTid = DetTid::from_raw(4);
const BLOCKING: i32 = libc::O_RDONLY;
const NONBLOCKING: i32 = libc::O_RDONLY | libc::O_NONBLOCK;

/// In-guest LiteInst, with registration as the coordinator's unit tests
/// drive it: no serial grant to wait for. No process handles SIGALRM, so
/// the ledger answers every question.
fn config() -> Config {
    let mut config = Config {
        sequentialize_threads: false,
        max_timeslice: None,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    config
}

/// `request` sent by `from`, answered by the real handler.
async fn rpc(
    state: &GlobalState,
    config: &Config,
    from: DetTid,
    request: GlobalRequest,
) -> GlobalResponse {
    state
        .receive_rpc(
            Tid::from_raw(from.as_raw()),
            (
                DetTime::new(config),
                MmId::initial(DetPid::from_raw(from.as_raw())),
                request,
            ),
        )
        .await
        .1
}

/// Registers `child` as a thread made by `PARENT`: the root with no flags, a
/// forked process with empty clone flags.
async fn register(state: &GlobalState, child: DetTid, flags: Option<CloneFlags>) {
    let registration = ChildRegistration {
        parent_dettid: PARENT,
        parent_detpid: PARENT,
        child_dettid: child,
        child_tid_addr: 0,
        flags,
        exit_signal: libc::SIGCHLD,
        physical_ids: None,
        maybe_priority: Some(DEFAULT_PRIORITY),
        parent_is_kernel_blocked: false,
    };
    assert!(matches!(
        state
            .recv_create_child_thread(
                Tid::from_raw(PARENT.as_raw()),
                MmId::initial(PARENT),
                registration,
            )
            .await,
        SchedulerRpcResult::Continue(())
    ));
}

async fn publish(state: &GlobalState, config: &Config, from: DetTid, id: OpenFileId, flags: i32) {
    assert_eq!(
        rpc(
            state,
            config,
            from,
            GlobalRequest::Sigalrm(SigalrmControl::PublishPipeStatus { id, flags }),
        )
        .await,
        GlobalResponse::Sigalrm(false)
    );
}

/// Whether the ledger says `from`'s copy, `flags`, is not the description's
/// latest status.
async fn differs(
    state: &GlobalState,
    config: &Config,
    from: DetTid,
    id: OpenFileId,
    flags: i32,
) -> bool {
    match rpc(
        state,
        config,
        from,
        GlobalRequest::Sigalrm(SigalrmControl::PipeStatusDiffers { id, flags }),
    )
    .await
    {
        GlobalResponse::Sigalrm(differs) => differs,
        other => panic!("unexpected answer {other:?}"),
    }
}

/// `from` closes its last descriptor of `id`: the release every such close
/// sends (`Detcore::release_port_for_open_file`).
async fn release(state: &GlobalState, config: &Config, from: DetTid, id: OpenFileId) {
    assert_eq!(
        rpc(state, config, from, GlobalRequest::ReleasePort(id)).await,
        GlobalResponse::ReleasePort(None)
    );
}

/// The reviewer's loop: one process makes a pipe, sets its read end
/// nonblocking and closes it, again and again, with one live description at
/// a time. Each status goes with its description's last close, so the ledger
/// never holds more than the one live description. Mutation "no release at
/// the last close" (`GlobalRequest::ReleasePort` leaves the ledger alone):
/// each closed description keeps its status, and a status published after
/// the close is still reported different.
#[tokio::test]
async fn a_closed_pipes_status_goes_with_its_last_close() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register(&state, PARENT, None).await;
    for sequence in 0..4 {
        let id = OpenFileId::new(DetPid::from_raw(PARENT.as_raw()), 900 + sequence);
        publish(&state, &config, PARENT, id, BLOCKING).await;
        publish(&state, &config, PARENT, id, NONBLOCKING).await;
        assert!(
            differs(&state, &config, PARENT, id, BLOCKING).await,
            "{sequence}: a stale copy differs while the description lives"
        );
        assert_eq!(state.sched.lock().unwrap().pipe_status_len(), 1);
        release(&state, &config, PARENT, id).await;
        assert!(
            !differs(&state, &config, PARENT, id, BLOCKING).await,
            "{sequence}: forgotten at its last close"
        );
        assert_eq!(state.sched.lock().unwrap().pipe_status_len(), 0);
    }
}

/// A fork's child holds a copy of each of its parent's descriptors, as
/// Linux's `dup_fd` copies the table and takes a reference on each
/// `struct file` (fs/file.c at v6.12, lines 303 and 372). The parent changes
/// the read end's status and closes its own descriptor of it: the child's
/// copy is still stale, so the status stays until the child's own release,
/// and with no change the child's copy is the description's. Mutation "the
/// fork does not add the child" (`Scheduler::sigalrm_process_created`): the
/// parent's release drops the status, and the child's stale copy reads as
/// current.
#[tokio::test]
async fn a_forked_alias_keeps_the_status_until_its_own_release() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register(&state, PARENT, None).await;
    let id = OpenFileId::new(DetPid::from_raw(PARENT.as_raw()), 910);
    publish(&state, &config, PARENT, id, BLOCKING).await;
    register(&state, CHILD, Some(CloneFlags::empty())).await;
    assert_eq!(
        state.sched.lock().unwrap().pipe_status_holders(id),
        Some(vec![PARENT, CHILD]),
        "the child holds what its creator held"
    );
    assert!(
        !differs(&state, &config, CHILD, id, BLOCKING).await,
        "the positive case: nobody changed the status"
    );
    publish(&state, &config, PARENT, id, NONBLOCKING).await;
    release(&state, &config, PARENT, id).await;
    assert!(
        differs(&state, &config, CHILD, id, BLOCKING).await,
        "the alias outlives its creator's close"
    );
    release(&state, &config, CHILD, id).await;
    assert_eq!(state.sched.lock().unwrap().pipe_status_holders(id), None);
}

/// The child that changed the status exits; the parent, which made the pipe
/// and still holds it, keeps seeing the change (the in-guest-trap
/// `setfl_exit` case of the CLI suite). Then the parent's own retirement,
/// with no close of its descriptor, drops the status. Mutation "retirement
/// leaves the ledger alone" (`Scheduler::retire_sigalrm_thread`): the status
/// of a description no live process holds stays.
#[tokio::test]
async fn a_retired_holder_leaves_and_the_last_takes_the_status() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register(&state, PARENT, None).await;
    let id = OpenFileId::new(DetPid::from_raw(PARENT.as_raw()), 920);
    publish(&state, &config, PARENT, id, BLOCKING).await;
    register(&state, CHILD, Some(CloneFlags::empty())).await;
    publish(&state, &config, CHILD, id, NONBLOCKING).await;
    state
        .sched
        .lock()
        .unwrap()
        .logically_kill_thread(&CHILD, &CHILD, MmId::initial(CHILD));
    assert!(
        differs(&state, &config, PARENT, id, BLOCKING).await,
        "the creator still holds the description its child changed"
    );
    assert_eq!(
        state.sched.lock().unwrap().pipe_status_holders(id),
        Some(vec![PARENT])
    );
    state
        .sched
        .lock()
        .unwrap()
        .logically_kill_thread(&PARENT, &PARENT, MmId::initial(PARENT));
    assert_eq!(state.sched.lock().unwrap().pipe_status_holders(id), None);
}
