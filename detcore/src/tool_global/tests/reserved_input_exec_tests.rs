/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the root LICENSE file.
 */

//! Production startup, exec RPC and timer-copy handler with scripted backend
//! observations. This does not certify a ptrace exec stop or native signal send.

use reverie::Tool;
use reverie::native_syscall::NativeDescriptorSnapshot;
use reverie::native_syscall::NativeSyscallAbi;
use reverie::native_syscall::NativeSyscallAuthority;
use reverie::native_syscall::NativeSyscallCall;
use reverie::native_syscall::NativeSyscallFacts;
use reverie::native_syscall::NativeSyscallId;
use reverie::native_syscall::NativeSyscallPurpose;
use reverie::reserved_input::InputDescriptorFacts;
use reverie::reserved_input::InputTaskId;
use reverie::reserved_input::InputUnavailable;
use reverie::reserved_input::ReservedInputControl;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::SyscallInfo;

use super::*;

// Only the initialized sigset/timespec used by take_pending_signal are staged
// here. Storage is owned by this fixture and retained through the injected call.
struct TimerStack {
    bytes: Box<[u128; 16]>,
    used: usize,
}
struct TimerGuard {
    _bytes: Box<[u128; 16]>,
}
impl Drop for TimerGuard {
    fn drop(&mut self) {}
}
impl reverie::Stack for TimerStack {
    type StackGuard = TimerGuard;
    fn size(&self) -> usize {
        self.used
    }
    fn capacity(&self) -> usize {
        std::mem::size_of_val(&*self.bytes)
    }
    fn push<'a, T>(&mut self, value: T) -> Addr<'a, T> {
        let align = std::mem::align_of::<T>();
        assert!(align <= std::mem::align_of::<u128>());
        let start = self.used.next_multiple_of(align);
        self.used = start.checked_add(std::mem::size_of::<T>()).unwrap();
        assert!(self.used <= self.capacity());
        // SAFETY: aligned, in-bounds owned allocation; the guard retains it.
        let ptr = unsafe { self.bytes.as_mut_ptr().cast::<u8>().add(start).cast::<T>() };
        unsafe { ptr.write(value) };
        Addr::from_ptr(ptr).unwrap()
    }
    fn reserve<'a, T>(&mut self) -> AddrMut<'a, T> {
        panic!("timer take-back does not reserve output storage")
    }
    fn commit(self) -> Result<TimerGuard, Errno> {
        Ok(TimerGuard { _bytes: self.bytes })
    }
}

struct ExecInputGuest<'a> {
    inner: ExternalRegistrationGuest<'a>,
    tool: &'a Detcore,
    control: ReservedInputControl,
    task: InputTaskId,
    injections: Vec<Sysno>,
}

impl NativeSyscallFacts for ExecInputGuest<'_> {
    fn control(&self) -> Option<ReservedInputControl> {
        Some(self.control.clone())
    }
    fn task(&self) -> Option<InputTaskId> {
        Some(self.task)
    }
    fn authority(&self) -> Result<NativeSyscallAuthority, InputUnavailable> {
        self.control.native_syscall_authority(self.task)
    }
    fn descriptor(&self, _: i32) -> Result<InputDescriptorFacts, InputUnavailable> {
        panic!("timer take-back has no descriptor effect")
    }
    fn descriptors(&self) -> Result<NativeDescriptorSnapshot, InputUnavailable> {
        panic!("timer take-back has no descriptor acquisition")
    }
}

#[reverie::tool]
impl GlobalRPC<GlobalState> for ExecInputGuest<'_> {
    fn config(&self) -> &Config {
        self.inner.config
    }
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.inner.requests.lock().unwrap().push(message.2.clone());
        self.inner.global.receive_rpc(self.tid(), message).await
    }
}

#[reverie::tool]
impl Guest<Detcore> for ExecInputGuest<'_> {
    type Memory = reverie::syscalls::LocalMemory;
    type Stack = TimerStack;
    fn tid(&self) -> reverie::Pid {
        reverie::Pid::from_raw(
            self.control
                .registration(self.task)
                .unwrap()
                .tool_tid
                .unwrap(),
        )
    }
    fn pid(&self) -> reverie::Pid {
        reverie::Pid::from_raw(
            self.control
                .registration(self.task)
                .unwrap()
                .tool_pid
                .unwrap(),
        )
    }
    fn ppid(&self) -> Option<reverie::Pid> {
        None
    }
    fn auxv(&self) -> reverie::Auxv {
        reverie::Auxv::from_entries([])
    }
    fn memory(&self) -> Self::Memory {
        Default::default()
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.inner.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.inner.thread
    }
    fn reserved_input_control(&self) -> Option<ReservedInputControl> {
        Some(self.control.clone())
    }
    fn reserved_input_task(&self) -> Option<InputTaskId> {
        Some(self.task)
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        // SAFETY: integer words, logged but not executed by this fixture.
        unsafe { std::mem::zeroed() }
    }
    async fn stack(&mut self) -> TimerStack {
        TimerStack {
            bytes: Box::new([0; 16]),
            used: 0,
        }
    }
    async fn daemonize(&mut self) {
        panic!("exec control must not daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        let (number, args) = syscall.into_parts();
        assert_eq!(number, Sysno::rt_sigtimedwait);
        #[cfg(target_arch = "x86_64")]
        let audit_arch = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        let audit_arch = 0xc000_00b7;
        let call = NativeSyscallCall {
            id: NativeSyscallId::from_backend_counter(1 + self.injections.len() as u64),
            abi: NativeSyscallAbi::Linux { audit_arch },
            number: number as u64,
            args: [
                args.arg0 as u64,
                args.arg1 as u64,
                args.arg2 as u64,
                args.arg3 as u64,
                args.arg4 as u64,
                args.arg5 as u64,
            ],
            purpose: NativeSyscallPurpose::Application,
            operation_bound: false,
            input_receipt: None,
            staged_image: None,
        };
        // Exercise the real final observer: without Mark publishing the
        // authenticated replacement's existing turn, this is unbound/no-grant.
        assert!(
            self.tool
                .observe_native_syscall(&self.inner.thread, self, &call)
                .unwrap()
                .is_none()
        );
        assert!(self.control.current_grant(self.task).is_some());
        let mask: u64 = self
            .memory()
            .read_value(Addr::from_raw(args.arg0).unwrap())?;
        let timeout: reverie::syscalls::Timespec = self
            .memory()
            .read_value(Addr::from_raw(args.arg2).unwrap())?;
        assert_eq!(mask, 1u64 << (libc::SIGUSR1 - 1));
        assert_eq!(args.arg1, 0);
        assert_eq!(args.arg3, std::mem::size_of::<u64>());
        assert_eq!(timeout.tv_sec, 0);
        assert_eq!(timeout.tv_nsec, 0);
        self.injections.push(number);
        Ok(libc::SIGUSR1 as i64)
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("exec control must not tail inject")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("exec control disables preemption timers")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("exec control disables preemption timers")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        Ok(0)
    }
}

#[tokio::test]
async fn actual_root_start_and_same_leader_exec_retain_turn_for_timer_take_back() {
    let config = Config {
        guest_ids: detcore_model::pid::GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        },
        sequentialize_threads: true,
        max_timeslice: None,
        // No image is launched, so nothing resolves auto: post-exec runs
        // under the named profile a launch would have resolved.
        cpu_profile: detcore_model::cpu_profile::CpuProfile::X86_64V2,
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, false);
    let control = ReservedInputControl::new_backend().unwrap();
    state
        .install_reserved_input_control(control.clone())
        .unwrap();
    let root = DetTid::from_raw(5000);
    // Physical and Tool identities deliberately differ.
    let native = control.backend_register_root(123, 123).unwrap();
    let native = control
        .backend_bind_tool_identity(native.task, root.as_raw(), root.as_raw())
        .unwrap();
    state.on_reserved_input_task(native).unwrap();
    crate::reserved_input::register_initial_history(&control, native.task).unwrap();
    let tool = Detcore::new(Tid::from_raw(root.as_raw()), &config);
    let mut thread = tool.init_thread_state(Tid::from_raw(root.as_raw()), None);
    thread.detpid = Some(root);
    let mut guest = ExecInputGuest {
        inner: ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        },
        tool: &tool,
        control: control.clone(),
        task: native.task,
        injections: Vec::new(),
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut start = std::pin::pin!(tool.handle_thread_start(&mut guest));
        assert!(futures::poll!(start.as_mut()).is_pending());
        let grants = async {
            let mut last = Err(crate::scheduler::SkipTurn);
            for expected in [
                ResourceID::ParentContinue {
                    parent: root,
                    child: root,
                },
                ResourceID::MemAddrSpace(root),
            ] {
                let granted = crate::scheduler::do_a_turn_blocking(
                    state.sched.clone(),
                    state.global_time.clone(),
                    &last,
                )
                .await
                .unwrap();
                assert_eq!(granted.tid, root);
                assert!(granted.resources.contains_key(&expected));
                last = Ok(granted);
            }
        };
        let (started, ()) = tokio::join!(start, grants);
        started.unwrap();
    })
    .await
    .unwrap();
    assert!(control.current_grant(native.task).is_some());
    guest.inner.thread.stats.count_syscall(Sysno::execve);
    guest
        .inner
        .thread
        .thread_logical_time
        .add_syscall_with_cost(137);
    let old_mm = guest.inner.thread.mm_id;
    crate::tool_global::prepare_exec(&mut guest, old_mm, Default::default(), Vec::new(), None)
        .await;
    let replacement = control
        .backend_exec_replacement(native.task, native.task)
        .unwrap();
    let replacement = control
        .backend_bind_tool_identity(replacement.task, root.as_raw(), root.as_raw())
        .unwrap();
    state.on_reserved_input_task(replacement).unwrap();
    guest.task = replacement.task;
    guest.inner.thread.mm_id = old_mm.for_exec(root);
    assert_ne!(native.task, replacement.task);
    assert_eq!(control.current_grant(replacement.task), None);
    state
        .sched
        .lock()
        .unwrap()
        .install_exec_input_timer_take_back_for_test(root);
    let before_turn = state.sched.lock().unwrap().turn;
    let before_time = state.global_time.lock().unwrap().as_nanos();
    let before_thread = guest.inner.thread.thread_logical_time.as_nanos();
    let before_requests = guest.inner.requests.lock().unwrap().len();
    tokio::time::timeout(Duration::from_secs(2), tool.handle_post_exec(&mut guest))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(guest.injections, [Sysno::rt_sigtimedwait]);
    assert!(control.current_grant(replacement.task).is_some());
    assert_eq!(control.current_grant(native.task), None);
    assert_eq!(state.sched.lock().unwrap().turn, before_turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_time);
    assert_eq!(
        guest.inner.thread.thread_logical_time.as_nanos(),
        before_thread
    );
    let requests = guest.inner.requests.lock().unwrap();
    assert!(
        requests[before_requests..]
            .iter()
            .any(|request| matches!(request, GlobalRequest::MarkPastFirstExecve(..)))
    );
    assert!(!requests[before_requests..].iter().any(|request| matches!(
        request,
        GlobalRequest::RequestResources(..) | GlobalRequest::ResumeExec(..)
    )));
}

#[tokio::test]
async fn actual_nonleader_exec_resumes_once_before_new_image_timer_take_back() {
    let config = Config {
        guest_ids: detcore_model::pid::GuestIds::Chosen {
            base: 17,
            refuse_host_ids: true,
        },
        sequentialize_threads: true,
        max_timeslice: None,
        // No image is launched, so nothing resolves auto: post-exec runs
        // under the named profile a launch would have resolved.
        cpu_profile: detcore_model::cpu_profile::CpuProfile::X86_64V2,
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, false);
    let control = ReservedInputControl::new_backend().unwrap();
    state
        .install_reserved_input_control(control.clone())
        .unwrap();
    let leader = DetTid::from_raw(17);
    let worker = DetTid::from_raw(18);
    let native_leader = control.backend_register_root(123, 123).unwrap();
    crate::reserved_input::register_initial_history(&control, native_leader.task).unwrap();
    let native_worker = control
        .backend_register_child(native_leader.task, 124, 123, Some(true), Some(true))
        .unwrap();
    for (tid, native) in [(leader, native_leader), (worker, native_worker)] {
        let bound = control
            .backend_bind_tool_identity(native.task, tid.as_raw(), leader.as_raw())
            .unwrap();
        state.on_reserved_input_task(bound).unwrap();
    }
    let tool = Detcore::new(Tid::from_raw(leader.as_raw()), &config);
    let mut parent = tool.init_thread_state(Tid::from_raw(leader.as_raw()), None);
    parent.detpid = Some(leader);
    parent.thread_start_entered = true;
    parent.clone_flags = Some(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_VM);
    let mut thread = tool.init_thread_state(
        Tid::from_raw(worker.as_raw()),
        Some((Tid::from_raw(leader.as_raw()), &parent)),
    );
    thread.detpid = Some(leader);
    thread.thread_start_entered = true;
    thread.stats.count_syscall(Sysno::execve);
    thread.thread_logical_time.add_syscall_with_cost(137);
    let old_mm = thread.mm_id;
    {
        let mut sched = state.sched.lock().unwrap();
        sched.thread_tree.add_child(leader, leader, true);
        sched.thread_tree.add_child(leader, worker, false);
        for tid in [leader, worker] {
            sched.record_thread_mm(tid, old_mm);
            sched.install_test_exec_incarnation(tid, old_mm);
        }
    }
    for current in [&parent, &thread] {
        install_test_registration(&state, current.dettid, Ivar::new());
        state.global_time.lock().unwrap().update_global_time(
            current.dettid,
            current.thread_logical_time.as_nanos(),
            current.thread_logical_time.inherited_nanos(),
        );
    }
    // Only the pre-exec turn/native birth are scripted. Reconnect, its one
    // existing ResumeExec request, real scheduler grant, Mark and take-back
    // below all execute production code and cannot invent successful replies.
    state.sched.lock().unwrap().set_running_for_test(worker);
    control
        .grant(
            native_worker.task,
            reverie::reserved_input::InputGrantKind::Ordinary,
        )
        .unwrap();
    let mut guest = ExecInputGuest {
        inner: ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        },
        tool: &tool,
        control: control.clone(),
        task: native_worker.task,
        injections: Vec::new(),
    };
    crate::tool_global::prepare_exec(&mut guest, old_mm, Default::default(), Vec::new(), None)
        .await;
    {
        let mut sched = state.sched.lock().unwrap();
        assert!(sched.defer_exec_sibling_retirement(leader, leader, old_mm));
        sched.install_exec_input_timer_take_back_for_test(worker);
    }
    let replacement = control
        .backend_exec_replacement(native_leader.task, native_worker.task)
        .unwrap();
    let replacement = control
        .backend_bind_tool_identity(replacement.task, leader.as_raw(), leader.as_raw())
        .unwrap();
    state.on_reserved_input_task(replacement).unwrap();
    guest.task = replacement.task;
    guest.inner.thread.mm_id = old_mm.for_exec(leader);
    let before_turn = state.sched.lock().unwrap().turn;
    let before_time = state.global_time.lock().unwrap().as_nanos();
    let before_thread = guest.inner.thread.thread_logical_time.as_nanos();
    let before_requests = guest.inner.requests.lock().unwrap().len();
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut post_exec = std::pin::pin!(tool.handle_post_exec(&mut guest));
        assert!(futures::poll!(post_exec.as_mut()).is_pending());
        assert!(futures::poll!(post_exec.as_mut()).is_pending());
        assert_eq!(control.current_grant(replacement.task), None);
        let grant = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .unwrap();
        assert_eq!(grant.tid, leader);
        assert_eq!(grant.resources.len(), 1);
        assert!(
            grant
                .resources
                .contains_key(&ResourceID::MemAddrSpace(leader))
        );
        post_exec.await.unwrap();
    })
    .await
    .unwrap();
    assert_eq!(guest.inner.thread.dettid, leader);
    assert_eq!(guest.injections, [Sysno::rt_sigtimedwait]);
    assert!(control.current_grant(replacement.task).is_some());
    assert_eq!(control.current_grant(native_leader.task), None);
    assert_eq!(control.current_grant(native_worker.task), None);
    assert_eq!(state.sched.lock().unwrap().turn, before_turn + 1);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), before_time);
    assert_eq!(
        guest.inner.thread.thread_logical_time.as_nanos(),
        before_thread
    );
    let requests = guest.inner.requests.lock().unwrap();
    let requests = &requests[before_requests..];
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, GlobalRequest::ResumeExec(..)))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, GlobalRequest::ReconnectExec { .. }))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(r, GlobalRequest::MarkPastFirstExecve(..)))
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|r| matches!(r, GlobalRequest::RequestResources(..)))
    );
}

#[tokio::test]
async fn resource_bookkeeping_preserves_only_the_existing_native_operation_grant() {
    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;

    use crate::reserved_input::NativeCallOperation;

    let config = Config {
        guest_ids: detcore_model::pid::GuestIds::Chosen {
            base: 5000,
            refuse_host_ids: true,
        },
        sequentialize_threads: true,
        max_timeslice: None,
        ..Config::default()
    };
    let mut state = GlobalState::initialize(&config, false);
    let control = ReservedInputControl::new_backend().unwrap();
    state
        .install_reserved_input_control(control.clone())
        .unwrap();
    let root = DetTid::from_raw(5000);
    let native = control.backend_register_root(123, 123).unwrap();
    let native = control
        .backend_bind_tool_identity(native.task, root.as_raw(), root.as_raw())
        .unwrap();
    state.on_reserved_input_task(native).unwrap();
    crate::reserved_input::register_initial_history(&control, native.task).unwrap();
    let tool = Detcore::new(Tid::from_raw(root.as_raw()), &config);
    let mut thread = tool.init_thread_state(Tid::from_raw(root.as_raw()), None);
    thread.detpid = Some(root);
    let mut guest = ExecInputGuest {
        inner: ExternalRegistrationGuest {
            global: &state,
            config: &config,
            thread,
            requests: Mutex::new(Vec::new()),
            post_exec: true,
            ppid: None,
            private_memory: None,
        },
        tool: &tool,
        control: control.clone(),
        task: native.task,
        injections: Vec::new(),
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        let start = tool.handle_thread_start(&mut guest);
        let grants = async {
            let mut last = Err(crate::scheduler::SkipTurn);
            for expected in [
                ResourceID::ParentContinue {
                    parent: root,
                    child: root,
                },
                ResourceID::MemAddrSpace(root),
            ] {
                let granted = crate::scheduler::do_a_turn_blocking(
                    state.sched.clone(),
                    state.global_time.clone(),
                    &last,
                )
                .await
                .unwrap();
                assert_eq!(granted.tid, root);
                assert!(granted.resources.contains_key(&expected));
                last = Ok(granted);
            }
        };
        let (started, ()) = tokio::join!(start, grants);
        started.unwrap();
    })
    .await
    .unwrap();

    let syscall = Syscall::from_raw(Sysno::getpid, SyscallArgs::new(0, 0, 0, 0, 0, 0));
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    // Native completion is scripted, but both RPCs below use the actual
    // scheduler and the operation's real completion/readmission checks.
    let physical = operation.binding().begin_native_attempt().unwrap();
    physical.transition_started();
    operation.close();
    let grant = control.current_grant(native.task).unwrap();
    let turn = state.sched.lock().unwrap().turn;
    let time = state.global_time.lock().unwrap().as_nanos();
    crate::tool_global::resource_release_all(&mut guest).await;
    assert_eq!(control.current_grant(native.task), Some(grant));
    assert_eq!(state.sched.lock().unwrap().turn, turn);
    assert_eq!(state.global_time.lock().unwrap().as_nanos(), time);
    assert_eq!(
        operation.readmitted().unwrap_err().to_string(),
        "native operation effects are not closed and complete"
    );
    physical.effects_ended();
    operation.readmitted().unwrap();
    assert_eq!(control.current_grant(native.task), Some(grant));

    // A real pending resource request must revoke that grant. Only its
    // committed scheduler reply can permit the completed operation to retire.
    let operation = NativeCallOperation::begin(&guest, syscall)
        .unwrap()
        .unwrap();
    operation.close();
    let mut request = Resources::new(root);
    request.insert(ResourceID::MemAddrSpace(root), Permission::W);
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut request = std::pin::pin!(crate::tool_global::resource_request(&mut guest, request));
        assert!(futures::poll!(request.as_mut()).is_pending());
        assert_eq!(control.current_grant(native.task), None);
        assert_eq!(
            operation.readmitted().unwrap_err().to_string(),
            "native operation lacks its logical readmission"
        );
        let granted = crate::scheduler::do_a_turn_blocking(
            state.sched.clone(),
            state.global_time.clone(),
            &Err(crate::scheduler::SkipTurn),
        )
        .await
        .unwrap();
        assert_eq!(granted.tid, root);
        assert!(
            granted
                .resources
                .contains_key(&ResourceID::MemAddrSpace(root))
        );
        assert!(matches!(request.await, ResumeStatus::Normal));
    })
    .await
    .unwrap();
    assert_ne!(control.current_grant(native.task), Some(grant));
    operation.readmitted().unwrap();

    // Neither a missing grant nor a frozen owner can acquire authority from
    // the bookkeeping reply. No replacement grant or Drop cleanup is used.
    for frozen in [false, true] {
        let operation = NativeCallOperation::begin(&guest, syscall)
            .unwrap()
            .unwrap();
        operation.close();
        let cohort = if frozen {
            Some(control.freeze_tasks(&[native.task]).unwrap())
        } else {
            control.invalidate_operation(native.task);
            None
        };
        let turn = state.sched.lock().unwrap().turn;
        crate::tool_global::resource_release_all(&mut guest).await;
        assert_eq!(control.current_grant(native.task), None);
        assert_eq!(state.sched.lock().unwrap().turn, turn);
        assert_eq!(
            operation.readmitted().unwrap_err().to_string(),
            "native operation lacks its logical readmission"
        );
        if cohort.is_none() {
            let mut request = Resources::new(root);
            request.insert(ResourceID::MemAddrSpace(root), Permission::W);
            tokio::time::timeout(Duration::from_secs(2), async {
                let last = Err(crate::scheduler::SkipTurn);
                let grant = crate::scheduler::do_a_turn_blocking(
                    state.sched.clone(),
                    state.global_time.clone(),
                    &last,
                );
                let (resumed, granted) = tokio::join!(
                    crate::tool_global::resource_request(&mut guest, request),
                    grant,
                );
                assert!(matches!(resumed, ResumeStatus::Normal));
                assert_eq!(granted.unwrap().tid, root);
            })
            .await
            .unwrap();
            operation.readmitted().unwrap();
        } else {
            drop(operation);
            assert_eq!(control.current_grant(native.task), None);
        }
    }
}
