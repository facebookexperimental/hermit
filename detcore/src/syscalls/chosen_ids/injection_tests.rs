/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Error attribution through the real chosen-ID injection path. The scripted
//! kernel distinguishes rewritten arguments from the original call, whose
//! outcome is independent. The scripted reserved-page transport exercises the
//! real policy and registry protocol, but does not certify native force-write,
//! ptrace cancellation, or newborn lifecycle handling.

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

use futures::FutureExt;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Stack;
use reverie::Tool;
use reverie::reserved_input::InputAbi;
use reverie::reserved_input::InputGrantKind;
use reverie::reserved_input::InputOperationScope;
use reverie::reserved_input::InputTaskId;
use reverie::reserved_input::PhysicalInjectionReceipt;
use reverie::reserved_input::ReservedInputControl;
use reverie::reserved_input::ReservedInputLayout;
use reverie::syscalls::LocalMemory;

use super::*;
use crate::Config;
use crate::GlobalState;
use crate::ThreadState;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::types::DetPid;

#[derive(Default)]
struct Scratch(Vec<Box<[u8]>>);

impl Drop for Scratch {
    fn drop(&mut self) {}
}

impl Stack for Scratch {
    type StackGuard = Self;

    fn size(&self) -> usize {
        self.0.iter().map(|bytes| bytes.len()).sum()
    }

    fn capacity(&self) -> usize {
        1024
    }

    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        // SAFETY: the production path stages initialized byte/pid_t arrays.
        // The Box remains live through the corresponding injection.
        let bytes =
            unsafe { std::slice::from_raw_parts(&value as *const T as *const u8, size_of::<T>()) };
        let storage = bytes.to_vec().into_boxed_slice();
        let address = Addr::from_raw(storage.as_ptr() as usize).unwrap();
        self.0.push(storage);
        address
    }

    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("the chosen-ID path does not reserve an output buffer")
    }

    fn commit(self) -> Result<Self::StackGuard, Errno> {
        Ok(self)
    }
}

#[derive(Debug)]
struct PolicyRefused(String);

struct CloneGuest {
    config: Config,
    thread: ThreadState<()>,
    original_args: Box<[u8; CLONE_ARGS_SIZE_VER2]>,
    original_expected: [u8; CLONE_ARGS_SIZE_VER2],
    original_set_tid: Option<Box<[i32; 1]>>,
    original_result: Result<i64, Errno>,
    rewritten_errno: Errno,
    attempts: Vec<bool>,
    host_id_reports: AtomicUsize,
    proof_context: bool,
    real_identity: bool,
    retain_original_birth: bool,
    registry_queries: Mutex<Vec<i32>>,
    refused_registry_query: Option<usize>,
    input_control: ReservedInputControl,
    input_task: InputTaskId,
}

impl CloneGuest {
    fn new(refuse_host_ids: bool, original_result: Result<i64, Errno>) -> Self {
        let config = Config {
            guest_ids: detcore_model::pid::GuestIds::Chosen {
                base: 5000,
                refuse_host_ids,
            },
            sequentialize_threads: true,
            ..Config::default()
        };
        let mut original_args = Box::new([0; CLONE_ARGS_SIZE_VER2]);
        original_args[32..40].copy_from_slice(&(libc::SIGCHLD as u64).to_ne_bytes());
        let (input_control, input_task) = Self::input_owner(5000);
        Self {
            thread: ThreadState::new(DetPid::from_raw(5000), &config, ()),
            config,
            original_expected: *original_args,
            original_args,
            original_set_tid: None,
            original_result,
            rewritten_errno: Errno::EINVAL,
            attempts: Vec::new(),
            host_id_reports: AtomicUsize::new(0),
            proof_context: false,
            real_identity: false,
            retain_original_birth: false,
            registry_queries: Mutex::new(Vec::new()),
            refused_registry_query: None,
            input_control,
            input_task,
        }
    }

    fn input_owner(tid: i32) -> (ReservedInputControl, InputTaskId) {
        let control = ReservedInputControl::new_backend().unwrap();
        let task = control.backend_register_root(tid, tid).unwrap().task;
        control
            .backend_set_layout(
                task,
                ReservedInputLayout {
                    base: reverie::reserved_input::RESERVED_INPUT_BASE,
                    page_bytes: 4096,
                    #[cfg(target_arch = "x86_64")]
                    abi: InputAbi::X86_64,
                    #[cfg(target_arch = "aarch64")]
                    abi: InputAbi::Aarch64,
                },
            )
            .unwrap();
        // A scripted real-grant boundary, not a production bypass. Direct
        // missing-authority and lifecycle tests live in the native suite.
        control.grant(task, InputGrantKind::Ordinary).unwrap();
        (control, task)
    }

    fn original_call(&self) -> Syscall {
        Syscall::Clone3(
            syscalls::Clone3::new()
                .with_args(AddrMut::from_raw(self.original_args.as_ptr() as usize))
                .with_size(CLONE_ARGS_SIZE_VER2),
        )
    }

    fn chosen(&self, id: i32) -> ClonePlan {
        ClonePlan::Chosen(ChosenId {
            set_tid: vec![id],
            id: Some(id),
            fresh_allocation: false,
            restart: None,
            args: *self.original_args,
            size: CLONE_ARGS_SIZE_VER2,
            untraced: false,
            guest_entries: false,
        })
    }

    fn enable_fresh_attempt(&mut self) {
        self.config.sequentialize_threads = true;
        self.config.inherited_seccomp = detcore_model::config::InheritedSeccomp::Disabled;
        self.proof_context = true;
        self.thread = ThreadState::new(
            DetPid::from_raw(std::process::id() as i32),
            &self.config,
            (),
        );
        (self.input_control, self.input_task) = Self::input_owner(std::process::id() as i32);
        let ids = Box::new([1]);
        self.original_args[0..8]
            .copy_from_slice(&(libc::CLONE_NEWPID as u64 | (1 << 40)).to_ne_bytes());
        *self.original_args =
            ChosenIdCall::with_set_tid(*self.original_args, ids.as_ptr() as u64, 1);
        self.original_expected = *self.original_args;
        self.original_set_tid = Some(ids);
    }

    fn set_guest_choice(&mut self, id: i32) {
        if !self.real_identity {
            assert!(self.attempts.is_empty());
            self.real_identity = true;
            self.thread = ThreadState::new(
                DetPid::from_raw(std::process::id() as i32),
                &self.config,
                (),
            );
            (self.input_control, self.input_task) = Self::input_owner(std::process::id() as i32);
        }
        let ids = Box::new([id]);
        *self.original_args =
            ChosenIdCall::with_set_tid(*self.original_args, ids.as_ptr() as u64, 1);
        self.original_expected = *self.original_args;
        self.original_set_tid = Some(ids);
    }

    fn fresh_plan(&self, id: i32) -> ClonePlan {
        let ClonePlan::Chosen(mut chosen) = self.chosen(id) else {
            unreachable!()
        };
        chosen.fresh_allocation = true;
        chosen.set_tid = vec![1, id];
        chosen.guest_entries = true;
        ClonePlan::Chosen(chosen)
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for CloneGuest {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let mut request = message.2;
        while let GlobalRequest::WithForwardedCount(_, inner) = request {
            request = *inner;
        }
        match request {
            GlobalRequest::GaveHostId => {
                self.host_id_reports.fetch_add(1, SeqCst);
                (None, GlobalResponse::GaveHostId(()))
            }
            GlobalRequest::ThreadIdRefused(id) if self.proof_context => {
                let mut queries = self.registry_queries.lock().unwrap();
                queries.push(id.as_raw());
                assert!(queries.len() <= 2, "only the before/after registry checks");
                (
                    None,
                    GlobalResponse::ThreadIdRefused(
                        self.refused_registry_query == Some(queries.len()),
                    ),
                )
            }
            // Stop at the real named-refusal boundary, before its process
            // exit; the test inspects the diagnostic and injected calls.
            GlobalRequest::PolicyRefusal(reason) => std::panic::panic_any(PolicyRefused(reason)),
            request => panic!("unexpected clone-attribution RPC: {request:?}"),
        }
    }

    fn config(&self) -> &Config {
        &self.config
    }
}

#[reverie::tool]
impl Guest<Detcore> for CloneGuest {
    type Memory = LocalMemory;
    type Stack = Scratch;

    fn tid(&self) -> Pid {
        if self.proof_context || self.real_identity {
            // Real readable maps/namespace links for the heap-backed fixture;
            // the scripted RPC still names the exact candidate ID separately.
            Pid::from_raw(std::process::id() as i32)
        } else {
            Pid::from_raw(5000)
        }
    }
    fn pid(&self) -> Pid {
        self.tid()
    }
    fn ppid(&self) -> Option<Pid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        LocalMemory::new()
    }
    fn thread_state(&self) -> &ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("the chosen-ID path does not need guest registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("chosen-ID staging must not borrow the application stack")
    }
    async fn daemonize(&mut self) {
        panic!("the chosen-ID path must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        self.scripted_call(call, None)
    }
    fn reserved_input_control(&self) -> Option<ReservedInputControl> {
        Some(self.input_control.clone())
    }
    fn reserved_input_task(&self) -> Option<InputTaskId> {
        Some(self.input_task)
    }
    async fn inject_observed<S: SyscallInfo>(
        &mut self,
        call: S,
        receipt: PhysicalInjectionReceipt,
    ) -> Result<Result<i64, Errno>, InputInjectionError> {
        let (number, args) = call.into_parts();
        self.input_control.backend_bind_injection(
            &receipt,
            self.input_task,
            number as u64,
            [
                args.arg0 as u64,
                args.arg1 as u64,
                args.arg2 as u64,
                args.arg3 as u64,
                args.arg4 as u64,
                args.arg5 as u64,
            ],
        )?;
        self.input_control.backend_issued(receipt.id())?;
        let result = self.scripted_call(Syscall::from_raw(number, args), None);
        if self.retain_original_birth
            && let Ok(child) = result
        {
            // Explicit scripted first-retention event, distinct from merely
            // returning a positive PID. Native ptrace ownership is not proved
            // by this fixture; the production policy and registry are real.
            let child = i32::try_from(child).unwrap();
            let retained = self.input_control.backend_register_child(
                self.input_task,
                child,
                child,
                Some(false),
                Some(false),
            )?;
            self.input_control.backend_retained_birth(
                receipt.id(),
                retained.task,
                Some(false),
                Some(false),
            )?;
        } else if result.is_ok() {
            // A mock numeric PID is not a native retained birth.
            self.input_control.backend_unknown_outcome(receipt.id())?;
        } else {
            self.input_control.backend_no_child(receipt.id())?;
        }
        Ok(result)
    }
    async fn inject_staged_input<S: SyscallInfo>(
        &mut self,
        call: S,
        scope: &InputOperationScope,
        image: ReservedInputImage,
        receipt: PhysicalInjectionReceipt,
    ) -> Result<Result<i64, Errno>, InputInjectionError> {
        self.input_control
            .backend_begin_preparation(&receipt, scope, image.clone())?;
        let prepared = match self.input_control.backend_prepared_stopped(receipt.id()) {
            Ok(prepared) => prepared,
            Err(error) => {
                // This fixture has not written or issued anything. Model the
                // backend's proved pre-write refusal settlement, not Drop.
                assert_eq!(error.reason(), "clone-lifecycle-unverified");
                self.input_control.backend_settled_not_run(receipt.id())?;
                return Err(error.into());
            }
        };
        assert_eq!(prepared, image);
        self.input_control.backend_initializing(receipt.id())?;
        // Scripted transport only. The real backend must independently read
        // these bytes through the stopped guest before issuing its syscall.
        self.input_control
            .backend_verified_image(receipt.id(), image.bytes())?;
        self.input_control.backend_issued(receipt.id())?;
        let result = self.scripted_call(call, Some(&image));
        assert!(
            result.is_err(),
            "the attribution fixture scripts no child birth"
        );
        self.input_control.backend_no_child(receipt.id())?;
        Ok(result)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("the chosen-ID path must not tail-inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("the chosen-ID path must not set a timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), Error> {
        panic!("the chosen-ID path must not set a timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("the chosen-ID path must not read a clock")
    }
}

impl CloneGuest {
    fn scripted_call<S: SyscallInfo>(
        &mut self,
        call: S,
        image: Option<&ReservedInputImage>,
    ) -> Result<i64, Errno> {
        let (number, args) = call.into_parts();
        let Syscall::Clone3(clone) = Syscall::from_raw(number, args) else {
            panic!("expected clone3, got {number}")
        };
        assert_eq!(clone.size(), CLONE_ARGS_SIZE_VER2);
        let original = clone.args().unwrap().as_raw() == self.original_args.as_ptr() as usize;
        let mut bytes = [0; CLONE_ARGS_SIZE_VER2];
        if let Some(image) = image {
            assert!(image.read(clone.args().unwrap().as_raw() as u64, &mut bytes));
        } else {
            self.memory()
                .read_exact(clone.args().unwrap().cast::<u8>(), &mut bytes)
                .unwrap();
        }
        self.attempts.push(original);
        if original {
            assert_eq!(bytes, self.original_expected);
            if let Some(ids) = &self.original_set_tid {
                let address = u64::from_ne_bytes(bytes[SET_TID_OFFSET..72].try_into().unwrap());
                assert_eq!(address as usize, ids.as_ptr() as usize);
                assert_eq!(&bytes[SET_TID_SIZE_OFFSET..80], &1_u64.to_ne_bytes());
            } else {
                assert_eq!(&bytes[SET_TID_OFFSET..SET_TID_SIZE_OFFSET + 8], &[0; 16]);
            }
            self.original_result
        } else {
            assert_eq!(
                &bytes[..SET_TID_OFFSET],
                &self.original_args[..SET_TID_OFFSET]
            );
            assert_eq!(&bytes[80..], &self.original_args[80..]);
            let count = if self.proof_context { 2_u64 } else { 1_u64 };
            assert_eq!(&bytes[SET_TID_SIZE_OFFSET..80], &count.to_ne_bytes());
            let address = u64::from_ne_bytes(bytes[SET_TID_OFFSET..72].try_into().unwrap());
            let mut observed = vec![0; count as usize];
            if let Some(image) = image {
                let mut bytes = vec![0; count as usize * 4];
                assert!(image.read(address, &mut bytes));
                for (entry, bytes) in observed.iter_mut().zip(bytes.as_chunks::<4>().0) {
                    *entry = i32::from_ne_bytes(*bytes);
                }
            } else {
                read_pid_array(&self.memory(), address as usize, &mut observed).unwrap();
            }
            let expected = if self.proof_context {
                vec![1, 5001]
            } else {
                vec![5001]
            };
            assert_eq!(observed, expected);
            Err(self.rewritten_errno)
        }
    }
}

#[tokio::test]
async fn an_added_id_einval_takes_the_original_calls_own_result() {
    for native_result in [Ok(6000), Err(Errno::EINVAL), Err(Errno::EFAULT)] {
        let mut guest = CloneGuest::new(false, native_result);
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.chosen(5001);
        let result = tool
            .inject_clone_planned(&mut guest, call, plan, false)
            .await;
        assert_eq!(result, native_result);
        assert_eq!(guest.attempts, [false, true]);
        assert_eq!(
            guest.host_id_reports.load(SeqCst),
            usize::from(native_result.is_ok())
        );
    }

    // A call with no added entry is still answered directly by the kernel.
    let mut guest = CloneGuest::new(false, Err(Errno::EINVAL));
    let ids = Box::new([i32::MAX]);
    *guest.original_args = ChosenIdCall::with_set_tid(*guest.original_args, ids.as_ptr() as u64, 1);
    guest.original_expected = *guest.original_args;
    guest.original_set_tid = Some(ids);
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    assert_eq!(
        tool.inject_clone_planned(&mut guest, call, ClonePlan::GuestChosen, false)
            .await,
        Err(Errno::EINVAL)
    );
    assert_eq!(guest.attempts, [true]);
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);

    // Other native failures keep their original error attribution and do
    // not trigger a host-ID retry merely because an ID was added.
    for errno in [Errno::EFAULT, Errno::EAGAIN] {
        let mut guest = CloneGuest::new(false, Ok(6000));
        guest.rewritten_errno = errno;
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.chosen(5001);
        assert_eq!(
            tool.inject_clone_planned(&mut guest, call, plan, false)
                .await,
            Err(errno)
        );
        assert_eq!(guest.attempts, [false]);
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);
    }
}

#[tokio::test]
async fn an_added_id_einval_is_a_named_refusal_when_host_ids_are_forbidden() {
    let mut guest = CloneGuest::new(true, Ok(6000));
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    let plan = guest.chosen(5001);
    let refused =
        std::panic::AssertUnwindSafe(tool.inject_clone_planned(&mut guest, call, plan, false))
            .catch_unwind()
            .await
            .expect_err("an ambiguous rewritten EINVAL must refuse the chosen-ID guarantee");
    let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
    assert!(
        reason.0.contains("needs every guest process ID chosen"),
        "{reason:?}"
    );
    assert!(reason.0.contains("EINVAL"), "{reason:?}");
    assert_eq!(guest.attempts, [false]);
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);
}

/// The real injection path checks the captured original fields, staged
/// guest-readable bytes, namespace depth and both registry responses. The
/// scripted kernel's EINVAL is independent of the unused original-call result.
/// This tests conditional attribution, not atomic sysctl stability or F1.
#[tokio::test]
async fn a_fresh_clone3_einval_keeps_the_guests_error_under_the_stable_pid_limit_contract() {
    let mut guest = CloneGuest::new(true, Ok(6000));
    guest.enable_fresh_attempt();
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    let plan = guest.fresh_plan(5001);
    assert_eq!(
        tool.inject_clone_planned(&mut guest, call, plan, false)
            .await,
        Err(Errno::EINVAL)
    );
    assert_eq!(guest.attempts, [false]);
    assert_eq!(*guest.registry_queries.lock().unwrap(), [5001, 5001]);
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);
}

/// A fresh value's evidence can become invalid. In particular a negative
/// post-attempt registry refusal must not be mistaken for the guest's EINVAL.
#[tokio::test]
async fn an_added_id_without_attribution_evidence_still_refuses_strict_runs() {
    // Start with the post-attempt failure so removing the attribution guard
    // is caught at this actual boundary, after both exact-ID queries.
    for refused_query in [2, 1] {
        let mut guest = CloneGuest::new(true, Ok(6000));
        guest.enable_fresh_attempt();
        guest.refused_registry_query = Some(refused_query);
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.fresh_plan(5001);
        let refused =
            std::panic::AssertUnwindSafe(tool.inject_clone_planned(&mut guest, call, plan, false))
                .catch_unwind()
                .await
                .expect_err("an occupied added ID is not evidence of the guest's own EINVAL");
        let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
        assert!(reason.0.contains("needs every guest process ID chosen"));
        assert!(reason.0.contains("EINVAL"));
        assert_eq!(guest.attempts, [false]);
        assert_eq!(
            *guest.registry_queries.lock().unwrap(),
            vec![5001; refused_query]
        );
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);
    }

    // Reusing a restart ID does not inherit a fresh allocation's proof.
    let mut guest = CloneGuest::new(true, Ok(6000));
    guest.enable_fresh_attempt();
    let tool = Detcore::new(guest.tid(), &guest.config);
    let call = guest.original_call();
    let ClonePlan::Chosen(mut chosen) = guest.fresh_plan(5001) else {
        unreachable!()
    };
    chosen.fresh_allocation = false;
    let refused = std::panic::AssertUnwindSafe(tool.inject_clone_planned(
        &mut guest,
        call,
        ClonePlan::Chosen(chosen),
        false,
    ))
    .catch_unwind()
    .await
    .expect_err("a restarted ID needs its own registration proof");
    assert!(refused.downcast_ref::<PolicyRefused>().is_some());
    assert_eq!(guest.attempts, [false]);
    assert!(guest.registry_queries.lock().unwrap().is_empty());
    assert_eq!(guest.host_id_reports.load(SeqCst), 0);

    // An inherited filter may return EINVAL only for the staged pointer or
    // private injection IP. A filter count does not prove that errno also
    // belongs to the original call, even with otherwise complete evidence.
    use detcore_model::config::InheritedSeccomp;
    for inherited in [
        InheritedSeccomp::Unknown,
        InheritedSeccomp::Strict,
        InheritedSeccomp::Filter(None),
        InheritedSeccomp::Filter(Some(0)),
        InheritedSeccomp::Filter(Some(1)),
    ] {
        let mut guest = CloneGuest::new(true, Ok(6000));
        guest.enable_fresh_attempt();
        guest.config.inherited_seccomp = inherited;
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.fresh_plan(5001);
        let refused =
            std::panic::AssertUnwindSafe(tool.inject_clone_planned(&mut guest, call, plan, false))
                .catch_unwind()
                .await
                .expect_err("an inherited filter can distinguish the rewritten call");
        let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
        assert!(reason.0.contains("needs every guest process ID chosen"));
        assert!(reason.0.contains("EINVAL"));
        assert_eq!(guest.attempts, [false]);
        assert!(guest.registry_queries.lock().unwrap().is_empty());
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);
    }

    // A definitely invalid added ID refuses before staging or injection.
    for id in [
        0,
        -1,
        pid_max().expect("the Linux test host exposes pid_max"),
    ] {
        let mut guest = CloneGuest::new(true, Ok(6000));
        guest.enable_fresh_attempt();
        let tool = Detcore::new(guest.tid(), &guest.config);
        let call = guest.original_call();
        let plan = guest.fresh_plan(id);
        let refused =
            std::panic::AssertUnwindSafe(tool.inject_clone_planned(&mut guest, call, plan, false))
                .catch_unwind()
                .await
                .expect_err("an invalid added ID must not become the guest's EINVAL");
        let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
        assert!(reason.0.contains("needs every guest process ID chosen"));
        assert!(reason.0.contains("not positive") || reason.0.contains("not below pid_max"));
        assert!(guest.attempts.is_empty());
        assert!(guest.registry_queries.lock().unwrap().is_empty());
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);
    }
}

/// Real GuestChosen planning and injection with a scripted retained birth.
/// Later staging must honor the accepted C4 cost, even though KCMP-style
/// copied-memory/table evidence and a successful native result are supplied.
#[tokio::test]
async fn retained_guest_chosen_clone3_refuses_later_staging_or_reports_native_ids() {
    for strict in [true, false] {
        let mut guest = CloneGuest::new(strict, Ok(6000));
        guest.set_guest_choice(6000);
        guest.retain_original_birth = true;
        let tool = Detcore::new(guest.tid(), &guest.config);
        let original = guest.original_call();
        let plan = tool.plan_clone_id(&mut guest, &original).await.unwrap();
        assert!(matches!(&plan, ClonePlan::GuestChosen));
        assert_eq!(
            tool.inject_clone_planned(&mut guest, original, plan, false)
                .await,
            Ok(6000)
        );
        assert_eq!(guest.attempts, [true]);
        assert_eq!(guest.host_id_reports.load(SeqCst), 0);

        // The first call is allowed to return unchanged. A later operation
        // must not reopen staging after its temporary access was readmitted.
        guest.set_guest_choice(6001);
        guest.original_result = Ok(6001);
        let original = guest.original_call();
        let chosen = guest.chosen(5001);
        if strict {
            let refused = std::panic::AssertUnwindSafe(
                tool.inject_clone_planned(&mut guest, original, chosen, false),
            )
            .catch_unwind()
            .await
            .expect_err("retained unknown lifecycle must prohibit later strict staging");
            let reason = refused.downcast_ref::<PolicyRefused>().unwrap();
            assert!(
                reason.0.contains("clone-lifecycle-unverified"),
                "{reason:?}"
            );
            assert_eq!(guest.attempts, [true]);
            assert_eq!(guest.host_id_reports.load(SeqCst), 0);
        } else {
            assert_eq!(
                tool.inject_clone_planned(&mut guest, original, chosen, false)
                    .await,
                Ok(6001)
            );
            assert_eq!(guest.attempts, [true, true]);
            assert_eq!(guest.host_id_reports.load(SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn proved_no_child_guest_chosen_clone3_preserves_errno_and_later_staging() {
    for strict in [true, false] {
        for errno in [Errno::EINVAL, Errno::EFAULT, Errno::EAGAIN] {
            let mut guest = CloneGuest::new(strict, Err(errno));
            guest.set_guest_choice(6000);
            guest.retain_original_birth = true;
            let tool = Detcore::new(guest.tid(), &guest.config);
            let original = guest.original_call();
            let plan = tool.plan_clone_id(&mut guest, &original).await.unwrap();
            assert!(matches!(&plan, ClonePlan::GuestChosen));
            assert_eq!(
                tool.inject_clone_planned(&mut guest, original, plan, false)
                    .await,
                Err(errno)
            );
            // EAGAIN is neither an added-ID fallback nor an attribution case.
            // Reaching the real staged route proves no prospective cause was
            // committed merely because this original pointer was unknown.
            guest.rewritten_errno = Errno::EAGAIN;
            let original = guest.original_call();
            let chosen = guest.chosen(5001);
            assert_eq!(
                tool.inject_clone_planned(&mut guest, original, chosen, false)
                    .await,
                Err(Errno::EAGAIN)
            );
            assert_eq!(guest.attempts, [true, false]);
            assert_eq!(guest.host_id_reports.load(SeqCst), 0);
        }
    }
}
