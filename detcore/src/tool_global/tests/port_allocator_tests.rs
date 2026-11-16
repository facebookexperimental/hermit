/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The deterministic ephemeral-port allocator behind `RequestPort`
//! (https://github.com/facebookexperimental/hermit/issues/135), through the
//! coordinator's real RPC handler. Each test names the mutation it catches.

use std::sync::atomic::Ordering::SeqCst;

use reverie::Tid;

use super::*;
use crate::tool_global::ChildRegistration;
use crate::types::OpenFileId;

const PARENT: DetTid = DetTid::from_raw(3);

fn config() -> Config {
    let mut config = Config {
        sequentialize_threads: false,
        max_timeslice: None,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    config
}

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

async fn register_root(state: &GlobalState) {
    let registration = ChildRegistration {
        parent_dettid: PARENT,
        parent_detpid: PARENT,
        child_dettid: PARENT,
        child_tid_addr: 0,
        flags: None,
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

async fn request_port(state: &GlobalState, config: &Config, id: OpenFileId) -> GlobalResponse {
    rpc(state, config, PARENT, GlobalRequest::RequestPort(id)).await
}

fn file(sequence: u64) -> OpenFileId {
    OpenFileId::new(DetPid::from_raw(PARENT.as_raw()), 700 + sequence)
}

/// Assignments are consecutive, and a released port is not re-issued to
/// the next request: the cursor has advanced past it, as Linux's
/// ephemeral search moves on rather than re-offering the last candidate.
/// Mutation "do not advance past an assignment": the third request below
/// gets the first port again, which is exactly the re-issue that binds
/// into a just-closed connection's host TIME_WAIT (issue 135).
#[tokio::test]
async fn a_released_port_is_not_the_next_candidate() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    let GlobalResponse::RequestPort(first) = request_port(&state, &config, file(0)).await else {
        panic!("first request refused");
    };
    let GlobalResponse::RequestPort(second) = request_port(&state, &config, file(1)).await else {
        panic!("second request refused");
    };
    assert_eq!(second, first + 1, "assignments are consecutive");
    // By state, not only by the answers: the cursor sits past the second
    // assignment, and both ports are held in each ledger map. Mutation
    // "ReleasePort leaves the ledger alone" is caught by these reads of
    // the maps around the release below, whatever the answers say.
    assert_eq!(state.next_port.load(SeqCst), first + 2);
    {
        let used = state.used_ports.lock().unwrap();
        assert_eq!(used.len(), 2);
        assert!(used.contains(&first) && used.contains(&second));
    }
    assert_eq!(
        state.open_file_to_port.lock().unwrap().get(&file(0)),
        Some(&first)
    );
    assert_eq!(
        rpc(&state, &config, PARENT, GlobalRequest::ReleasePort(file(0))).await,
        GlobalResponse::ReleasePort(Some(first)),
    );
    assert!(!state.used_ports.lock().unwrap().contains(&first));
    assert!(
        !state
            .open_file_to_port
            .lock()
            .unwrap()
            .contains_key(&file(0))
    );
    assert_eq!(
        state.next_port.load(SeqCst),
        first + 2,
        "release does not move the cursor back"
    );
    let GlobalResponse::RequestPort(third) = request_port(&state, &config, file(2)).await else {
        panic!("third request refused");
    };
    assert_eq!(
        third,
        first + 2,
        "the released port waits for the cursor to come back around"
    );
}

/// Release takes the port out of both ledger maps. Mutation "ReleasePort
/// leaves the ledger alone": the port stays in `used_ports` and the file
/// stays mapped, so range space leaks with every closed socket.
#[tokio::test]
async fn release_forgets_the_port() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    let GlobalResponse::RequestPort(port) = request_port(&state, &config, file(0)).await else {
        panic!("request refused");
    };
    assert!(state.used_ports.lock().unwrap().contains(&port));
    assert_eq!(
        rpc(&state, &config, PARENT, GlobalRequest::ReleasePort(file(0))).await,
        GlobalResponse::ReleasePort(Some(port)),
    );
    assert!(!state.used_ports.lock().unwrap().contains(&port));
    assert!(
        !state
            .open_file_to_port
            .lock()
            .unwrap()
            .contains_key(&file(0))
    );
}

/// With every port in the ephemeral range held, the allocator answers
/// PortFull, which `handle_bind` maps to EADDRINUSE: Linux's answer when
/// bind to port 0 finds the ephemeral range exhausted
/// ("net/ipv4/inet_connection_sock.c" `inet_csk_bind`, whose `get_port`
/// failure path returns -EADDRINUSE; the EADDRNOTAVAIL case is connect's
/// autobind instead). Mutation "wrap and hand out a held port": the
/// request below succeeds with a port some holder still owns.
#[tokio::test]
async fn a_full_range_answers_port_full() {
    let config = config();
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    let start = state.port_start_range.load(SeqCst);
    let end = state.port_end_range.load(SeqCst);
    {
        let mut used = state.used_ports.lock().unwrap();
        for port in start..=end {
            used.insert(port);
        }
    }
    assert_eq!(
        request_port(&state, &config, file(0)).await,
        GlobalResponse::PortFull,
    );
}

/// Both wraparounds at the top of the port space, on an override range
/// of 65533..=65535: assignments walk to the end, the cursor wraps to
/// the start, exhaustion answers PortFull, and after the middle port is
/// released a scan starting on the held end wraps around and finds it.
/// Mutation "the old scan" (a `fetch_add` past-the-end check plus an
/// end-minus-start bound): at a 65535 end the atomic addition wraps to
/// 0 itself and hands out port 0, and the inclusive range needs one
/// more scan step than the old bound allowed, so the released middle
/// port is never reached.
#[tokio::test]
async fn assignment_at_the_range_end_wraps_to_its_start() {
    let mut config = config();
    config.ephemeral_port_range_override = Some((65533, 65535));
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    for (sequence, expected) in [(0, 65533), (1, 65534), (2, 65535)] {
        let GlobalResponse::RequestPort(assigned) =
            request_port(&state, &config, file(sequence)).await
        else {
            panic!("request {sequence} refused");
        };
        assert_eq!(assigned, expected);
    }
    assert_eq!(
        state.next_port.load(SeqCst),
        65533,
        "the cursor wraps past the end-of-range assignment"
    );
    assert_eq!(
        request_port(&state, &config, file(3)).await,
        GlobalResponse::PortFull,
        "the three-port range is exhausted"
    );
    assert_eq!(
        rpc(&state, &config, PARENT, GlobalRequest::ReleasePort(file(1))).await,
        GlobalResponse::ReleasePort(Some(65534)),
    );
    // The failed scan left the cursor on a held port; wherever it sits,
    // the scan wraps around the top and finds the one free port.
    let GlobalResponse::RequestPort(wrapped) = request_port(&state, &config, file(4)).await else {
        panic!("wrapping request refused");
    };
    assert_eq!(wrapped, 65534, "the scan wraps to the released port");
}

/// An override whose first port is past its last is refused by name
/// instead of assigning from an empty range.
#[test]
fn an_inverted_ephemeral_range_override_is_refused_by_name() {
    let mut config = config();
    config.ephemeral_port_range_override = Some((40102, 40100));
    let error = GlobalState::validate_ephemeral_range(&config).unwrap_err();
    assert!(
        error.contains("ephemeral_port_range_override"),
        "the refusal names the field: {error}"
    );
}

/// With the cursor on a held 65535, the next allocation wraps to the
/// range start. On the parent loop (a `fetch_add` past-the-end check)
/// the atomic addition wraps to 0 itself and the request hands out
/// port 0.
#[tokio::test]
async fn a_scan_from_a_held_range_end_returns_the_start() {
    let mut config = config();
    config.ephemeral_port_range_override = Some((65533, 65535));
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    state.used_ports.lock().unwrap().insert(65535);
    state.next_port.store(65535, SeqCst);
    let GlobalResponse::RequestPort(assigned) = request_port(&state, &config, file(0)).await else {
        panic!("request from a held range end refused");
    };
    assert_eq!(assigned, 65533, "the scan wraps to the range start");
}

/// With the first two ports of a three-port range held, the next
/// allocation returns the third. On the parent's end-minus-start scan
/// bound it stops one candidate early and answers PortFull instead.
#[tokio::test]
async fn the_last_free_port_of_a_tiny_range_is_reachable() {
    let mut config = config();
    config.ephemeral_port_range_override = Some((65533, 65535));
    let state = GlobalState::initialize(&config, false);
    register_root(&state).await;
    for sequence in 0..2 {
        let GlobalResponse::RequestPort(_) = request_port(&state, &config, file(sequence)).await
        else {
            panic!("request {sequence} refused");
        };
    }
    // Pin the cursor back at the start, so the scan must pass both held
    // ports to reach the free one.
    state.next_port.store(65533, SeqCst);
    let GlobalResponse::RequestPort(assigned) = request_port(&state, &config, file(2)).await else {
        panic!("the last free port was not reached");
    };
    assert_eq!(assigned, 65535);
}
