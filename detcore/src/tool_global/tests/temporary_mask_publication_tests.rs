/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A5 step 3c.1e A, through the coordinator's real RPC handlers
//! (`GlobalState::receive_rpc`, `GlobalState::recv_create_child_thread`):
//! fork-child seeding, the committed-send window record at a guest send's
//! commit point (`NotifySignalPending`), the temporary-mask publication and
//! its withdrawal (`PublishTemporaryMask`), and the `/proc` status query's
//! appended answer under the in-guest delivery gate. Each test names the
//! mutation it catches.

use reverie::Tid;
use reverie::syscalls::CloneFlags;

use super::*;
use crate::scheduler::guest_mask_publication::CommittedSendWindow;
use crate::scheduler::guest_mask_publication::PublishedGuestMask;
use crate::scheduler::guest_mask_publication::StatusGuestMask;
use crate::scheduler::guest_mask_publication::TemporaryMaskControl;
use crate::tool_global::ChildRegistration;
use crate::tool_global::SignalScope;

const SENDER: DetTid = DetTid::from_raw(61);
const TARGET: DetTid = DetTid::from_raw(62);

fn bit(signal: i32) -> u64 {
    crate::scheduler::kernel_signal_bit(signal)
}

/// An in-guest configuration with the delivery gate `gate`.
fn config(gate: bool) -> Config {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: gate,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    config
}

/// A coordinator with `SENDER` and `TARGET` registered, each its own process.
fn coordinator(config: &Config) -> GlobalState {
    let state = GlobalState::initialize(config, false);
    for thread in [SENDER, TARGET] {
        state
            .sched
            .lock()
            .unwrap()
            .thread_tree
            .add_child(thread, thread, true);
        install_test_registration(&state, thread, Ivar::new());
    }
    state
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

/// The committed-send window record at a guest send's commit point: the
/// `NotifySignalPending` handler, which the sender files only after its
/// injected send succeeded, records a thread-directed send in the target's
/// private queue and a process-directed one in its shared queue, while the
/// target holds a temporary-mask publication; the target's withdrawal
/// answers them. A send before the publication is not recorded. With the
/// gate off the publication is not taken and nothing is recorded. Mutation
/// "the guest send not recorded at its commit point": the window is empty.
#[tokio::test]
async fn a_guest_send_committed_during_a_publication_joins_the_targets_window() {
    let rt = libc::SIGRTMIN() + 2;
    for gate in [true, false] {
        let config = config(gate);
        let state = coordinator(&config);
        let notify = |signal, scope| {
            GlobalRequest::NotifySignalPending(
                TARGET,
                SigWrapper(signal),
                Some(DetPid::from_raw(TARGET.as_raw())),
                scope,
            )
        };
        // Before the publication: not recorded.
        assert_eq!(
            rpc(
                &state,
                &config,
                SENDER,
                notify(libc::SIGHUP, SignalScope::Thread)
            )
            .await,
            GlobalResponse::NotifySignalPending(())
        );
        let publish = TemporaryMaskControl::Publish {
            thread: TARGET,
            mask: 0,
            held_private: 0,
            held_shared: 0,
        };
        assert_eq!(
            rpc(
                &state,
                &config,
                TARGET,
                GlobalRequest::PublishTemporaryMask(publish)
            )
            .await,
            GlobalResponse::PublishTemporaryMask(None)
        );
        rpc(&state, &config, SENDER, notify(rt, SignalScope::Thread)).await;
        rpc(
            &state,
            &config,
            SENDER,
            notify(libc::SIGUSR1, SignalScope::Process),
        )
        .await;
        let withdrawn = rpc(
            &state,
            &config,
            TARGET,
            GlobalRequest::PublishTemporaryMask(TemporaryMaskControl::Withdraw {
                thread: TARGET,
                keep_published: false,
            }),
        )
        .await;
        let want = gate.then_some(CommittedSendWindow {
            sent_private: bit(rt),
            sent_shared: bit(libc::SIGUSR1),
        });
        assert_eq!(
            withdrawn,
            GlobalResponse::PublishTemporaryMask(want),
            "gate {gate}"
        );
    }
}

/// The `/proc` status query (`PublishedGuestSigmask`) under the gate is
/// answered with the appended `GuestSigmaskForStatus`: the tri-state and the
/// held masks of a publication in force; with the gate off, with the old
/// answer, byte for byte as before. Mutation "the old answer under the
/// gate": the held masks never reach the status reader.
#[tokio::test]
async fn the_status_query_answers_the_tri_state_and_held_masks_under_the_gate_only() {
    let temporary = bit(libc::SIGTERM);
    for gate in [true, false] {
        let config = config(gate);
        let state = coordinator(&config);
        let publish = TemporaryMaskControl::Publish {
            thread: TARGET,
            mask: temporary,
            held_private: bit(libc::SIGURG),
            held_shared: bit(libc::SIGWINCH),
        };
        rpc(
            &state,
            &config,
            TARGET,
            GlobalRequest::PublishTemporaryMask(publish),
        )
        .await;
        let answer = rpc(
            &state,
            &config,
            SENDER,
            GlobalRequest::PublishedGuestSigmask(TARGET),
        )
        .await;
        if gate {
            assert_eq!(
                answer,
                GlobalResponse::GuestSigmaskForStatus(StatusGuestMask {
                    mask: Some(PublishedGuestMask::Known(temporary)),
                    held_private: bit(libc::SIGURG),
                    held_shared: bit(libc::SIGWINCH),
                })
            );
        } else {
            assert_eq!(answer, GlobalResponse::PublishedGuestSigmask(None));
        }
    }
}

/// Fork-child seeding through `recv_create_child_thread`: a child registered
/// in its creator's turn starts with the mask its creator published (here
/// `Known`, and `Unknown`), as Linux's child starts with its creator's
/// `blocked` (kernel/fork.c:1086-1091 at v6.12). With the gate off, nothing.
/// Mutation "no seed at registration": the child holds nothing.
#[tokio::test]
async fn the_coordinator_seeds_a_new_child_with_its_creators_published_mask() {
    let parent = DetTid::from_raw(3);
    let child = DetTid::from_raw(4);
    let own = bit(libc::SIGUSR2);
    for gate in [true, false] {
        for published in [
            Some(PublishedGuestMask::Known(own)),
            Some(PublishedGuestMask::Unknown),
        ] {
            let case = format!("{published:?}, gate {gate}");
            // Registration as the coordinator's unit tests drive it, with no
            // serial grant to wait for.
            let config = Config {
                sequentialize_threads: false,
                ..config(gate)
            };
            let state = GlobalState::initialize(&config, false);
            let registration = |child_dettid, flags| ChildRegistration {
                parent_dettid: parent,
                parent_detpid: parent,
                child_dettid,
                child_tid_addr: 0,
                flags,
                exit_signal: libc::SIGCHLD,
                physical_ids: None,
                maybe_priority: Some(DEFAULT_PRIORITY),
                parent_is_kernel_blocked: false,
            };
            let parent_mm = MmId::initial(parent);
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(parent, None),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            state
                .sched
                .lock()
                .unwrap()
                .put_guest_mask_state_for_test(parent, published);
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(child, Some(CloneFlags::empty())),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            let held = state
                .sched
                .lock()
                .unwrap()
                .published_guest_mask_state(child);
            assert_eq!(held, gate.then_some(published).flatten(), "{case}");
        }
    }
}

/// A masked call that parked returns ready, then forks, and the child's
/// `/proc` status is read before its first turn: the call's end puts the
/// guest's own mask back (`SetSavedGuestSigmask`, as
/// `TemporaryMaskCall::finish` sends it), so the creator's status after the
/// ready return and the child's show the own mask, as Linux restores it
/// before the call returns (`restore_saved_sigmask_unless`, fs/select.c:301
/// at v6.12) and the child copies it (kernel/fork.c:1086-1091). Without the
/// put-back (an interrupted end, after which Linux keeps the temporary mask
/// in `blocked`, with the handler's `sa_mask` and the signal, until
/// `rt_sigreturn`: `signal_delivered`, kernel/signal.c:2951-2954) the creator
/// still publishes the temporary mask,
/// and the child is not seeded with it: `Unknown` where the backend keeps
/// the guest's mask apart, and otherwise nothing, so its readers take its
/// `SigBlk`. Both on a backend that keeps the guest's mask apart (the
/// creator published its own mask at an earlier request) and on one that
/// keeps none (every backend before step 3c.1e B: the put-back publishes
/// nothing). Mutation "seed copies a temporary entry": the rows without the
/// put-back seed the child with the temporary mask.
#[tokio::test]
async fn a_child_forked_after_a_parked_masked_call_starts_with_the_own_mask() {
    let parent = DetTid::from_raw(3);
    let child = DetTid::from_raw(4);
    let own = bit(libc::SIGUSR2);
    let temporary = bit(libc::SIGTERM);
    for keeps_mask_apart in [true, false] {
        for put_back in [true, false] {
            let case = format!("keeps the mask apart {keeps_mask_apart}, put back {put_back}");
            let config = Config {
                sequentialize_threads: false,
                ..config(true)
            };
            let state = GlobalState::initialize(&config, false);
            let registration = |child_dettid, flags| ChildRegistration {
                parent_dettid: parent,
                parent_detpid: parent,
                child_dettid,
                child_tid_addr: 0,
                flags,
                exit_signal: libc::SIGCHLD,
                physical_ids: None,
                maybe_priority: Some(DEFAULT_PRIORITY),
                parent_is_kernel_blocked: false,
            };
            let parent_mm = MmId::initial(parent);
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(parent, None),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            let own_published = keeps_mask_apart.then_some(own);
            {
                let mut sched = state.sched.lock().unwrap();
                let mut earlier = Resources::new(parent);
                earlier.guest_signal_mask = own_published;
                sched.record_request_guest_mask(parent, &earlier);
                // The park of a masked call: its request publishes the
                // temporary mask.
                let mut park = Resources::new(parent);
                park.blocked_signal_mask = Some(temporary);
                park.guest_signal_mask = Some(temporary);
                assert!(sched.record_request_guest_mask(parent, &park), "{case}");
            }
            if put_back {
                assert_eq!(
                    rpc(
                        &state,
                        &config,
                        parent,
                        GlobalRequest::SetSavedGuestSigmask(own_published),
                    )
                    .await,
                    GlobalResponse::SetSavedGuestSigmask(()),
                    "{case}"
                );
            }
            let status = |thread| {
                let state = &state;
                let config = &config;
                async move {
                    match rpc(
                        state,
                        config,
                        parent,
                        GlobalRequest::PublishedGuestSigmask(thread),
                    )
                    .await
                    {
                        GlobalResponse::GuestSigmaskForStatus(status) => status.mask,
                        other => panic!("unexpected status answer {other:?}"),
                    }
                }
            };
            let after_return = if put_back {
                own_published.map(PublishedGuestMask::Known)
            } else {
                Some(PublishedGuestMask::Known(temporary))
            };
            assert_eq!(status(parent).await, after_return, "{case}: creator");
            assert!(matches!(
                state
                    .recv_create_child_thread(
                        Tid::from_raw(parent.as_raw()),
                        parent_mm,
                        registration(child, Some(CloneFlags::empty())),
                    )
                    .await,
                SchedulerRpcResult::Continue(())
            ));
            let seeded = match (put_back, keeps_mask_apart) {
                (true, true) => Some(PublishedGuestMask::Known(own)),
                (true, false) | (false, false) => None,
                (false, true) => Some(PublishedGuestMask::Unknown),
            };
            assert_eq!(status(child).await, seeded, "{case}: child");
        }
    }
}

/// A creator for `tool_global::create_child_thread` through the
/// coordinator's real RPC handlers, whose backend keeps the guest's mask
/// apart (`Guest::resume_signal_mask` is `current`) or not (`None`).
struct ForkingGuest<'a> {
    global: &'a GlobalState,
    config: &'a Config,
    thread: crate::ThreadState<()>,
    current: Option<u64>,
    requests: std::sync::Mutex<Vec<GlobalRequest>>,
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ForkingGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.requests.lock().unwrap().push(message.2.clone());
        self.global
            .receive_rpc(Tid::from_raw(self.thread.dettid.as_raw()), message)
            .await
    }

    fn config(&self) -> &Config {
        self.config
    }
}

#[reverie::tool]
impl reverie::Guest<Detcore> for ForkingGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = ExternalRegistrationStack;

    fn tid(&self) -> reverie::Pid {
        reverie::Pid::from_raw(self.thread.dettid.as_raw())
    }
    fn pid(&self) -> reverie::Pid {
        reverie::Pid::from_raw(self.thread.detpid.unwrap().as_raw())
    }
    fn ppid(&self) -> Option<reverie::Pid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        panic!("a clone registration must not access guest memory")
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.thread
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.thread
    }
    fn resume_signal_mask(&self) -> Option<u64> {
        self.current
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("a clone registration must not read registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("a clone registration must not use a guest stack")
    }
    async fn daemonize(&mut self) {
        panic!("a clone registration must not daemonize")
    }
    async fn inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        _syscall: S,
    ) -> Result<i64, reverie::syscalls::Errno> {
        panic!("a clone registration must not inject a syscall")
    }
    async fn tail_inject<S: reverie::syscalls::SyscallInfo>(
        &mut self,
        _syscall: S,
    ) -> reverie::Never {
        panic!("a clone registration must not retire its creator")
    }
    fn set_timer(&mut self, _schedule: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("a clone registration must not set a timer")
    }
    fn set_timer_precise(
        &mut self,
        _schedule: reverie::TimerSchedule,
    ) -> Result<(), reverie::Error> {
        panic!("a clone registration must not set a timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("a clone registration must not read a host clock")
    }
}

/// An ordinary clone files no request, so the creator's last publication can
/// be older than its mask at the `fork` (after step 3c.1e B, an
/// `rt_sigprocmask` just before it). Under the gate the creator publishes
/// its current mask (the backend's record, `Guest::resume_signal_mask`) just
/// before `CreateChildThread` (`tool_global::create_child_thread`), so the
/// child is seeded with it, as Linux's child starts with its creator's
/// `blocked` at the clone (`arch_dup_task_struct`, kernel/fork.c:1086-1091 at
/// v6.12): the scheduler's view and the child's `/proc` status answer before
/// the child's first grant are the current mask. That holds after an earlier
/// own-mask request and after a masked call's temporary mask (which the
/// publication supersedes, so the child is not left `Unknown`). On a backend
/// that keeps no guest mask apart (every backend before B) the publication
/// is nothing, and the child holds nothing, so its readers take the kernel's
/// `SigBlk`. With the gate off nothing is sent. Mutation "no publication at
/// the clone": the child is seeded with the older mask, or `Unknown`.
#[tokio::test]
async fn a_clone_publishes_the_creators_current_mask_before_its_child_is_seeded() {
    let parent = DetTid::from_raw(3);
    let child = DetTid::from_raw(4);
    let older = bit(libc::SIGUSR2);
    let current = bit(libc::SIGUSR1) | bit(libc::SIGWINCH);
    let temporary = bit(libc::SIGTERM);
    for gate in [true, false] {
        for keeps_mask_apart in [true, false] {
            for last in ["own request", "masked call"] {
                let case =
                    format!("gate {gate}, keeps the mask apart {keeps_mask_apart}, last {last}");
                let config = Config {
                    sequentialize_threads: false,
                    ..config(gate)
                };
                let state = GlobalState::initialize(&config, false);
                let parent_mm = MmId::initial(parent);
                assert!(matches!(
                    state
                        .recv_create_child_thread(
                            Tid::from_raw(parent.as_raw()),
                            parent_mm,
                            ChildRegistration {
                                parent_dettid: parent,
                                parent_detpid: parent,
                                child_dettid: parent,
                                child_tid_addr: 0,
                                flags: None,
                                exit_signal: libc::SIGCHLD,
                                physical_ids: None,
                                maybe_priority: Some(DEFAULT_PRIORITY),
                                parent_is_kernel_blocked: false,
                            },
                        )
                        .await,
                    SchedulerRpcResult::Continue(())
                ));
                {
                    let mut sched = state.sched.lock().unwrap();
                    let mut earlier = Resources::new(parent);
                    earlier.guest_signal_mask = keeps_mask_apart.then_some(older);
                    sched.record_request_guest_mask(parent, &earlier);
                    if last == "masked call" {
                        let mut park = Resources::new(parent);
                        park.blocked_signal_mask = Some(temporary);
                        park.guest_signal_mask = Some(temporary);
                        sched.record_request_guest_mask(parent, &park);
                    }
                }
                let mut thread = crate::ThreadState::new(parent, &config, ());
                thread.detpid = Some(parent);
                let mut guest = ForkingGuest {
                    global: &state,
                    config: &config,
                    thread,
                    current: keeps_mask_apart.then_some(current),
                    requests: Default::default(),
                };
                crate::tool_global::create_child_thread(
                    &mut guest,
                    child,
                    0,
                    Some(CloneFlags::empty()),
                    libc::SIGCHLD,
                    None,
                )
                .await;
                let requests = guest.requests.lock().unwrap().clone();
                let registration = requests
                    .iter()
                    .position(|rpc| matches!(rpc, GlobalRequest::CreateChildThread(..)))
                    .unwrap_or_else(|| panic!("{case}: registered in {requests:?}"));
                let published: Vec<&GlobalRequest> = requests[..registration]
                    .iter()
                    .filter(|rpc| matches!(rpc, GlobalRequest::SetSavedGuestSigmask(_)))
                    .collect();
                if gate {
                    assert_eq!(
                        published,
                        vec![&GlobalRequest::SetSavedGuestSigmask(
                            keeps_mask_apart.then_some(current)
                        )],
                        "{case}: published just before the registration"
                    );
                } else {
                    assert!(published.is_empty(), "{case}: nothing sent");
                    continue;
                }
                let want = keeps_mask_apart.then_some(PublishedGuestMask::Known(current));
                assert_eq!(
                    state
                        .sched
                        .lock()
                        .unwrap()
                        .published_guest_mask_state(child),
                    want,
                    "{case}: the scheduler's view"
                );
                assert_eq!(
                    rpc(
                        &state,
                        &config,
                        parent,
                        GlobalRequest::PublishedGuestSigmask(child)
                    )
                    .await,
                    GlobalResponse::GuestSigmaskForStatus(StatusGuestMask {
                        mask: want,
                        held_private: 0,
                        held_shared: 0,
                    }),
                    "{case}: the child's status before its first grant"
                );
            }
        }
    }
}
